//! Bounded WebSocket frame assembly and control-message policy.
//!
//! Raw header validation belongs to the transport. This module accepts owned
//! frames, publishes only complete UTF-8 data, and never issues JSON proofs.

use super::*;

/// Checks the policy fields required for a usable WebSocket data boundary.
///
/// Frame and message ceilings are independent. Other syntax limits may be zero
/// and are applied by the configured message validator, not by frame assembly.
pub fn validate_policy(policy: &WssPolicy) -> Result<()> {
    if policy.max_frame_bytes == 0
        || policy.max_message_bytes == 0
        || policy.max_fragments == 0
        || policy.limits.max_payload_bytes == 0
        || policy.limits.max_depth == 0
    {
        return Err(transport_error(
            ErrorCode::ConstraintViolation,
            "WebSocket frame, message, fragment, payload, and depth limits must be nonzero",
        ));
    }
    Ok(())
}

/// Accepts one transport-verified frame and advances message assembly.
///
/// A final data frame keeps its original payload owner. Fragmented messages
/// reserve storage fallibly and validate UTF-8 only after the final continuation,
/// allowing Unicode encodings to cross frame boundaries. Control payloads are
/// independent of JSON limits. Successful Ping and Pong leave assembly intact;
/// successful Close discards it and permanently closes this state.
///
/// Every error clears message assembly while preserving the closed flag and
/// reusable allocation. A caller may start a fresh message after an error on an
/// open state, but no frame is accepted after a successful Close.
pub fn accept_frame(
    state: &mut FragmentState,
    frame: IncomingFrame,
    policy: &WssPolicy,
) -> Result<ReceiveEvent> {
    let result = accept_frame_inner(state, frame, policy);
    if result.is_err() {
        state.reset_message();
    }
    result
}

/// Checks a complete close payload and borrows its UTF-8 reason.
///
/// An empty payload has no status. Otherwise a two-byte big-endian status is
/// required, followed by at most 123 reason bytes. Only 1000 through 1003,
/// 1007 through 1014, and 3000 through 4999 are accepted. UTF-8 error offsets
/// refer to the whole close payload, including its two-byte status prefix.
pub fn validate_close_payload(payload: &[u8]) -> Result<(Option<u16>, &str)> {
    ensure_control_len(payload.len())?;
    if payload.is_empty() {
        return Ok((None, ""));
    }
    if payload.len() == 1 {
        return Err(transport_error(
            ErrorCode::FramingError,
            "close payload must contain either no bytes or a complete status code",
        ));
    }
    let code = u16::from_be_bytes([payload[0], payload[1]]);
    if !matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999) {
        return Err(transport_error(ErrorCode::FramingError, "invalid WebSocket close code"));
    }
    let reason = decode_utf8(&payload[2..], 2)?;
    Ok((Some(code), reason))
}

fn accept_frame_inner(
    state: &mut FragmentState,
    frame: IncomingFrame,
    policy: &WssPolicy,
) -> Result<ReceiveEvent> {
    // Closure takes precedence even if the caller supplies a different policy.
    if state.closed {
        return Err(transport_error(ErrorCode::FramingError, "frame received after close"));
    }
    validate_policy(policy)?;
    ensure_payload_len(frame.payload.len(), policy.max_frame_bytes)?;

    match frame.opcode {
        Opcode::Ping | Opcode::Pong | Opcode::Close => {
            if !frame.final_fragment {
                return Err(transport_error(
                    ErrorCode::FramingError,
                    "WebSocket control frames must be final",
                ));
            }
            ensure_control_len(frame.payload.len())?;
            match frame.opcode {
                Opcode::Ping => Ok(ReceiveEvent::Ping(frame.payload)),
                Opcode::Pong => Ok(ReceiveEvent::Pong(frame.payload)),
                _ => {
                    let (code, reason) = validate_close_payload(frame.payload.as_slice())?;
                    let reason_start = frame.payload.len() - reason.len();
                    // Bytes can share a reason-only view. Shared payloads need a
                    // bounded, fallibly reserved copy because Arc has no slice.
                    let reason = match frame.payload {
                        Payload::Bytes(bytes) => Payload::Bytes(bytes.slice(reason_start..)),
                        Payload::Shared(bytes) => {
                            let mut reason = Vec::new();
                            append_bytes(&mut reason, &bytes[reason_start..])?;
                            Payload::from_vec(reason)
                        }
                    };
                    state.reset_message();
                    state.closed = true;
                    Ok(ReceiveEvent::Close { code, reason })
                }
            }
        }
        Opcode::Text | Opcode::Binary => {
            if state.opcode.is_some() {
                return Err(transport_error(
                    ErrorCode::FramingError,
                    "new data frame received during a fragmented message",
                ));
            }
            ensure_data_opcode(frame.opcode, policy)?;
            assembled_len(0, frame.payload.len(), policy)?;
            let fragments = next_fragment_count(0, policy.max_fragments)?;
            if frame.final_fragment {
                decode_utf8(frame.payload.as_slice(), 0)?;
                return Ok(ReceiveEvent::Message(ProtectedInput {
                    payload: frame.payload,
                    opcode: frame.opcode,
                }));
            }
            state.reset_message();
            append_bytes(&mut state.bytes, frame.payload.as_slice())?;
            state.opcode = Some(frame.opcode);
            state.fragments = fragments;
            Ok(ReceiveEvent::Pending)
        }
        Opcode::Continuation => {
            let opcode = state.opcode.ok_or_else(|| {
                transport_error(ErrorCode::FramingError, "continuation without an active message")
            })?;
            // Recheck policy on each data fragment, including a binary message
            // whose policy may have changed since its first frame.
            ensure_data_opcode(opcode, policy)?;
            let fragments = next_fragment_count(state.fragments, policy.max_fragments)?;
            assembled_len(state.bytes.len(), frame.payload.len(), policy)?;
            append_bytes(&mut state.bytes, frame.payload.as_slice())?;
            state.fragments = fragments;
            if !frame.final_fragment {
                return Ok(ReceiveEvent::Pending);
            }
            decode_utf8(&state.bytes, 0)?;
            let payload = Payload::from_vec(core::mem::take(&mut state.bytes));
            state.reset_message();
            Ok(ReceiveEvent::Message(ProtectedInput { payload, opcode }))
        }
    }
}

fn ensure_data_opcode(opcode: Opcode, policy: &WssPolicy) -> Result<()> {
    match opcode {
        Opcode::Text => Ok(()),
        Opcode::Binary if policy.binary == BinaryPolicy::Json => Ok(()),
        Opcode::Binary => Err(transport_error(ErrorCode::TypeMismatch, "binary JSON is disabled")),
        _ => Err(transport_error(ErrorCode::FramingError, "invalid fragmented data opcode")),
    }
}

fn ensure_control_len(len: usize) -> Result<()> {
    if len > MAX_CONTROL_BYTES {
        return Err(transport_error(
            ErrorCode::PayloadTooLarge,
            "WebSocket control payload exceeds 125 bytes",
        ));
    }
    Ok(())
}

fn next_fragment_count(current: u32, maximum: u32) -> Result<u32> {
    let next = current.checked_add(1).ok_or_else(|| {
        transport_error(ErrorCode::BudgetExhausted, "WebSocket fragment count overflow")
    })?;
    if next > maximum {
        return Err(transport_error(ErrorCode::BudgetExhausted, "WebSocket fragment limit exceeded"));
    }
    Ok(next)
}

fn assembled_len(current: usize, additional: usize, policy: &WssPolicy) -> Result<u32> {
    let len = current.checked_add(additional).ok_or_else(|| {
        transport_error(ErrorCode::PayloadTooLarge, "WebSocket assembled byte length overflow")
    })?;
    ensure_payload_len(len, policy.max_message_bytes.min(policy.limits.max_payload_bytes))
}

fn reserve_bytes(bytes: &mut Vec<u8>, additional: usize) -> Result<()> {
    // Exact reservation avoids intentional geometric growth beyond a message's
    // configured byte ceiling. Capacity retained after reset can be reused.
    bytes.try_reserve_exact(additional).map_err(|_| {
        transport_error(ErrorCode::BudgetExhausted, "WebSocket buffer allocation failed")
    })
}

fn append_bytes(bytes: &mut Vec<u8>, additional: &[u8]) -> Result<()> {
    reserve_bytes(bytes, additional.len())?;
    bytes.extend_from_slice(additional);
    Ok(())
}

fn decode_utf8(bytes: &[u8], base_offset: u32) -> Result<&str> {
    match core::str::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(error) => {
            let offset = u32::try_from(error.valid_up_to()).ok()
                .and_then(|offset| base_offset.checked_add(offset))
                .ok_or_else(|| {
                    transport_error(ErrorCode::PayloadTooLarge, "UTF-8 offset exceeds ABI length")
                })?;
            Err(Error::new(
                ErrorCode::InvalidUtf8,
                ErrorSource::Utf8,
                offset,
                "WebSocket payload is not valid UTF-8",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(opcode: Opcode, final_fragment: bool, bytes: &[u8]) -> IncomingFrame {
        IncomingFrame { opcode, final_fragment, payload: Payload::from_slice(bytes) }
    }

    fn message(event: ReceiveEvent) -> ProtectedInput {
        match event {
            ReceiveEvent::Message(input) => input,
            other => panic!("expected a complete message, got {other:?}"),
        }
    }

    fn assert_error<T: core::fmt::Debug>(
        result: Result<T>,
        code: ErrorCode,
        source: ErrorSource,
        offset: u32,
    ) {
        let error = result.expect_err("frame policy must reject this input");
        assert_eq!(error.code, code);
        assert_eq!(error.source, source);
        assert_eq!(error.byte_offset, offset);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    fn assert_reset(state: &FragmentState, closed: bool) {
        assert!(!state.is_fragmented());
        assert_eq!(state.fragments, 0);
        assert!(state.bytes.is_empty());
        assert_eq!(state.is_closed(), closed);
    }

    fn partial(policy: &WssPolicy) -> FragmentState {
        let mut state = FragmentState::new();
        assert!(matches!(
            accept_frame(&mut state, frame(Opcode::Text, false, b"nu"), policy).unwrap(),
            ReceiveEvent::Pending
        ));
        state
    }

    #[test]
    fn required_policy_ceilings_are_nonzero() {
        validate_policy(&WssPolicy::default()).unwrap();
        for field in 0..5 {
            let mut policy = WssPolicy::default();
            match field {
                0 => policy.max_frame_bytes = 0,
                1 => policy.max_message_bytes = 0,
                2 => policy.max_fragments = 0,
                3 => policy.limits.max_payload_bytes = 0,
                _ => policy.limits.max_depth = 0,
            }
            assert_error(
                validate_policy(&policy), ErrorCode::ConstraintViolation, ErrorSource::Transport, 0,
            );
            let mut state = partial(&WssPolicy::default());
            let capacity = state.retained_bytes();
            assert_error(
                accept_frame(&mut state, frame(Opcode::Ping, true, b""), &policy),
                ErrorCode::ConstraintViolation, ErrorSource::Transport, 0,
            );
            assert_reset(&state, false);
            assert_eq!(state.retained_bytes(), capacity);
        }
    }

    #[test]
    fn unrelated_syntax_limits_can_be_zero_and_frame_ceiling_can_be_larger() {
        let mut policy = WssPolicy {
            max_frame_bytes: u32::MAX, max_message_bytes: 1, max_fragments: 1,
            ..WssPolicy::default()
        };
        policy.limits = Limits {
            max_payload_bytes: 1, max_decoded_bytes: 0, max_depth: 1,
            max_fields_per_object: 0, max_string_bytes: 0, max_number_digits: 0,
            max_array_items: 0, reject_duplicate_keys: false, reject_unknown_keys: false,
        };
        validate_policy(&policy).unwrap();
        let input = message(accept_frame(
            &mut FragmentState::new(), frame(Opcode::Text, true, b"0"), &policy,
        ).unwrap());
        assert_eq!(input.payload().as_slice(), b"0");
    }

    #[test]
    fn final_data_frames_keep_their_original_owners() {
        let policy = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        for opcode in [Opcode::Text, Opcode::Binary] {
            for payload in [Payload::from_slice(b"null"), Payload::from_vec(b"null".to_vec())] {
                let pointer = payload.as_slice().as_ptr();
                let shared = matches!(&payload, Payload::Shared(_));
                let original = payload.clone();
                let mut state = FragmentState::new();
                let input = message(accept_frame(
                    &mut state, IncomingFrame { opcode, final_fragment: true, payload }, &policy,
                ).unwrap());
                drop(original);
                assert_eq!(input.opcode(), opcode);
                assert_eq!(input.payload().as_slice(), b"null");
                assert_eq!(input.payload().as_slice().as_ptr(), pointer);
                assert_eq!(matches!(input.payload(), Payload::Shared(_)), shared);
                assert_reset(&state, false);
            }
        }
    }

    #[test]
    fn sliced_bytes_remain_owned_after_transport_buffers_are_dropped() {
        let backing = Bytes::from(b"prefixnullsuffix".to_vec());
        let bytes = backing.slice(6..10);
        let pointer = bytes.as_ptr();
        let input = message(accept_frame(
            &mut FragmentState::new(),
            IncomingFrame { opcode: Opcode::Text, final_fragment: true, payload: Payload::Bytes(bytes) },
            &WssPolicy::default(),
        ).unwrap());
        drop(backing);
        let clone = input.clone();
        drop(input);
        assert_eq!(clone.payload().as_slice(), b"null");
        assert_eq!(clone.payload().as_slice().as_ptr(), pointer);
    }

    #[test]
    fn transport_acceptance_does_not_claim_json_validity() {
        let policy = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        for bytes in [b"".as_slice(), b"{", b"null true", b"not JSON", b"\0", b"[1,]"] {
            for opcode in [Opcode::Text, Opcode::Binary] {
                let input = message(accept_frame(
                    &mut FragmentState::new(), frame(opcode, true, bytes), &policy,
                ).unwrap());
                assert_eq!(input.payload().as_slice(), bytes);
            }
        }
    }

    #[test]
    fn binary_rejection_covers_final_and_nonfinal_data() {
        for final_fragment in [false, true] {
            let mut state = FragmentState::new();
            assert_error(
                accept_frame(&mut state, frame(Opcode::Binary, final_fragment, b"null"), &WssPolicy::default()),
                ErrorCode::TypeMismatch, ErrorSource::Transport, 0,
            );
            assert_reset(&state, false);
        }
    }

    #[test]
    fn each_byte_ceiling_is_inclusive_and_independent() {
        for ceiling in 0..3 {
            let mut policy = WssPolicy {
                max_frame_bytes: 8, max_message_bytes: 8, binary: BinaryPolicy::Json,
                ..WssPolicy::default()
            };
            policy.limits.max_payload_bytes = 8;
            match ceiling {
                0 => policy.max_frame_bytes = 4,
                1 => policy.max_message_bytes = 4,
                _ => policy.limits.max_payload_bytes = 4,
            }
            for opcode in [Opcode::Text, Opcode::Binary] {
                for bytes in [b"nul".as_slice(), b"null"] {
                    let input = message(accept_frame(
                        &mut FragmentState::new(), frame(opcode, true, bytes), &policy,
                    ).unwrap());
                    assert_eq!(input.payload().as_slice(), bytes);
                }
                let mut state = FragmentState::new();
                assert_error(
                    accept_frame(&mut state, frame(opcode, true, b"false"), &policy),
                    ErrorCode::PayloadTooLarge, ErrorSource::Transport, 0,
                );
                assert_reset(&state, false);
            }
        }
    }

    #[test]
    fn assembled_data_can_exceed_the_individual_frame_limit() {
        let mut policy = WssPolicy {
            max_frame_bytes: 2, max_message_bytes: 4, max_fragments: 2,
            ..WssPolicy::default()
        };
        policy.limits.max_payload_bytes = 4;
        let mut state = partial(&policy);
        assert_eq!(state.fragments, 1);
        let input = message(accept_frame(
            &mut state, frame(Opcode::Continuation, true, b"ll"), &policy,
        ).unwrap());
        assert_eq!(input.payload().as_slice(), b"null");
        assert_reset(&state, false);
        assert_eq!(state.retained_bytes(), 0);
        drop(state);
        assert_eq!(input.payload().as_slice(), b"null");
    }

    #[test]
    fn continuation_must_fit_frame_and_both_assembled_limits() {
        for ceiling in 0..3 {
            let mut policy = WssPolicy {
                max_frame_bytes: 8, max_message_bytes: 8, ..WssPolicy::default()
            };
            policy.limits.max_payload_bytes = 8;
            let mut state = partial(&policy);
            match ceiling {
                0 => policy.max_frame_bytes = 1,
                1 => policy.max_message_bytes = 3,
                _ => policy.limits.max_payload_bytes = 3,
            }
            let capacity = state.retained_bytes();
            assert_error(
                accept_frame(&mut state, frame(Opcode::Continuation, true, b"ll"), &policy),
                ErrorCode::PayloadTooLarge, ErrorSource::Transport, 0,
            );
            assert_reset(&state, false);
            assert_eq!(state.retained_bytes(), capacity);
        }
    }

    #[test]
    fn empty_fragments_count_including_initial_and_final_frames() {
        for maximum in [1, 2, 3] {
            let policy = WssPolicy { max_fragments: maximum, ..WssPolicy::default() };
            let mut state = FragmentState::new();
            accept_frame(&mut state, frame(Opcode::Text, false, b""), &policy).unwrap();
            for count in 2..=maximum {
                assert!(matches!(accept_frame(
                    &mut state, frame(Opcode::Continuation, false, b""), &policy,
                ).unwrap(), ReceiveEvent::Pending));
                assert_eq!(state.fragments, count);
            }
            assert_error(
                accept_frame(&mut state, frame(Opcode::Continuation, true, b""), &policy),
                ErrorCode::BudgetExhausted, ErrorSource::Transport, 0,
            );
            assert_reset(&state, false);
            let input = message(accept_frame(&mut state, frame(Opcode::Text, true, b""), &policy).unwrap());
            assert!(input.payload().is_empty());
        }
        let policy = WssPolicy { max_fragments: 2, ..WssPolicy::default() };
        let mut state = FragmentState::new();
        accept_frame(&mut state, frame(Opcode::Text, false, b""), &policy).unwrap();
        assert!(message(accept_frame(
            &mut state, frame(Opcode::Continuation, true, b""), &policy,
        ).unwrap()).payload().is_empty());
        assert_reset(&state, false);
    }

    #[test]
    fn fragment_arithmetic_cannot_wrap() {
        assert_eq!(next_fragment_count(0, 1).unwrap(), 1);
        assert_eq!(next_fragment_count(u32::MAX - 1, u32::MAX).unwrap(), u32::MAX);
        for (current, maximum) in [(0, 0), (1, 1), (7, 3), (u32::MAX, u32::MAX)] {
            assert_error(
                next_fragment_count(current, maximum),
                ErrorCode::BudgetExhausted, ErrorSource::Transport, 0,
            );
        }
        let policy = WssPolicy { max_fragments: u32::MAX, ..WssPolicy::default() };
        let mut state = partial(&policy);
        state.fragments = u32::MAX;
        assert_error(
            accept_frame(&mut state, frame(Opcode::Continuation, true, b""), &policy),
            ErrorCode::BudgetExhausted, ErrorSource::Transport, 0,
        );
        assert_reset(&state, false);
    }

    #[test]
    fn accumulated_lengths_are_checked_without_large_allocations() {
        let mut policy = WssPolicy {
            max_message_bytes: u32::MAX, ..WssPolicy::default()
        };
        policy.limits.max_payload_bytes = u32::MAX;
        assert_eq!(assembled_len(0, 0, &policy).unwrap(), 0);
        assert_eq!(assembled_len(u32::MAX as usize - 1, 1, &policy).unwrap(), u32::MAX);
        assert_error(
            assembled_len(usize::MAX, 1, &policy),
            ErrorCode::PayloadTooLarge, ErrorSource::Transport, 0,
        );
        #[cfg(target_pointer_width = "64")]
        assert_error(
            assembled_len(u32::MAX as usize, 1, &policy),
            ErrorCode::PayloadTooLarge, ErrorSource::Transport, 0,
        );
        policy.limits.max_payload_bytes = 4;
        assert_eq!(assembled_len(2, 2, &policy).unwrap(), 4);
        assert_error(
            assembled_len(4, 1, &policy),
            ErrorCode::PayloadTooLarge, ErrorSource::Transport, 0,
        );
    }

    #[test]
    fn fallible_reservation_maps_capacity_overflow_and_preserves_contents() {
        let mut bytes = b"nu".to_vec();
        let pointer = bytes.as_ptr();
        let capacity = bytes.capacity();
        // Vec rejects this capacity before calling the allocator on every host.
        assert_error(
            reserve_bytes(&mut bytes, usize::MAX),
            ErrorCode::BudgetExhausted, ErrorSource::Transport, 0,
        );
        assert_eq!(bytes, b"nu");
        assert_eq!(bytes.as_ptr(), pointer);
        assert_eq!(bytes.capacity(), capacity);
        append_bytes(&mut bytes, b"ll").unwrap();
        assert_eq!(bytes, b"null");
        append_bytes(&mut bytes, b"").unwrap();
        assert_eq!(bytes, b"null");
    }

    #[test]
    fn ordering_errors_discard_assembly_and_allow_a_fresh_message() {
        let policy = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        for final_fragment in [false, true] {
            assert_error(
                accept_frame(&mut FragmentState::new(), frame(Opcode::Continuation, final_fragment, b""), &policy),
                ErrorCode::FramingError, ErrorSource::Transport, 0,
            );
            for opcode in [Opcode::Text, Opcode::Binary] {
                let mut state = partial(&policy);
                assert_error(
                    accept_frame(&mut state, frame(opcode, final_fragment, b"null"), &policy),
                    ErrorCode::FramingError, ErrorSource::Transport, 0,
                );
                assert_reset(&state, false);
                assert_eq!(message(accept_frame(
                    &mut state, frame(Opcode::Text, true, b"null"), &policy,
                ).unwrap()).payload().as_slice(), b"null");
            }
        }
    }

    #[test]
    fn every_unicode_split_is_accepted_for_text_and_binary_json() {
        let policy = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        for text in ["\"é\"", "\"水\"", "\"😀\"", "\"\u{10ffff}\""] {
            for split in 0..=text.len() {
                for opcode in [Opcode::Text, Opcode::Binary] {
                    let mut state = FragmentState::new();
                    assert!(matches!(accept_frame(
                        &mut state, frame(opcode, false, &text.as_bytes()[..split]), &policy,
                    ).unwrap(), ReceiveEvent::Pending));
                    let input = message(accept_frame(
                        &mut state, frame(Opcode::Continuation, true, &text.as_bytes()[split..]), &policy,
                    ).unwrap());
                    assert_eq!(input.opcode(), opcode);
                    assert_eq!(input.payload().as_slice(), text.as_bytes());
                    assert_reset(&state, false);
                }
            }
        }
    }

    #[test]
    fn invalid_utf8_is_rejected_on_completion_with_the_first_byte_offset() {
        let policy = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        let vectors: &[(&[u8], u32)] = &[
            (b"\xff", 0), (b"\"\x80\"", 1), (b"ok\xc0\xaf", 2),
            (b"ok\xe2\x28\xa1", 2), (b"\"\xed\xa0\x80\"", 1),
            (b"\"\xf4\x90\x80\x80\"", 1), (b"null \xf0\x9f", 5),
            (b"\xc3\xa9\xff", 2),
        ];
        for &(bytes, offset) in vectors {
            for opcode in [Opcode::Text, Opcode::Binary] {
                let mut state = FragmentState::new();
                assert_error(
                    accept_frame(&mut state, frame(opcode, true, bytes), &policy),
                    ErrorCode::InvalidUtf8, ErrorSource::Utf8, offset,
                );
                assert_reset(&state, false);
                assert!(matches!(accept_frame(
                    &mut state, frame(opcode, false, bytes), &policy,
                ).unwrap(), ReceiveEvent::Pending));
                assert_error(
                    accept_frame(&mut state, frame(Opcode::Continuation, true, b""), &policy),
                    ErrorCode::InvalidUtf8, ErrorSource::Utf8, offset,
                );
                assert_reset(&state, false);
            }
        }
    }

    #[test]
    fn multiple_continuations_can_split_each_byte_of_a_scalar() {
        let policy = WssPolicy { max_fragments: 4, ..WssPolicy::default() };
        let mut state = FragmentState::new();
        accept_frame(&mut state, frame(Opcode::Text, false, b"\xf0"), &policy).unwrap();
        for bytes in [b"\x9f".as_slice(), b"\x98"] {
            assert!(matches!(accept_frame(
                &mut state, frame(Opcode::Continuation, false, bytes), &policy,
            ).unwrap(), ReceiveEvent::Pending));
        }
        let input = message(accept_frame(
            &mut state, frame(Opcode::Continuation, true, b"\x80"), &policy,
        ).unwrap());
        assert_eq!(input.payload().as_slice(), "😀".as_bytes());
        assert_reset(&state, false);
    }

    #[test]
    fn ping_and_pong_are_opaque_owned_and_do_not_change_fragment_state() {
        let mut policy = WssPolicy { max_fragments: 2, ..WssPolicy::default() };
        policy.max_message_bytes = 4;
        policy.limits.max_payload_bytes = 4;
        let mut state = partial(&policy);
        let pointer = state.bytes.as_ptr();
        let capacity = state.retained_bytes();
        for opcode in [Opcode::Ping, Opcode::Pong] {
            for len in [0, 1, 124, 125] {
                for payload in [
                    Payload::from_slice(&alloc::vec![0xff; len]),
                    Payload::from_vec(alloc::vec![0xff; len]),
                ] {
                    let original = payload.clone();
                    let event = accept_frame(
                        &mut state, IncomingFrame { opcode, final_fragment: true, payload }, &policy,
                    ).unwrap();
                    let returned = match (opcode, event) {
                        (Opcode::Ping, ReceiveEvent::Ping(payload)) => payload,
                        (Opcode::Pong, ReceiveEvent::Pong(payload)) => payload,
                        (_, other) => panic!("unexpected control event: {other:?}"),
                    };
                    assert_eq!(returned.as_slice(), original.as_slice());
                    assert_eq!(returned.as_slice().as_ptr(), original.as_slice().as_ptr());
                    assert_eq!(state.opcode, Some(Opcode::Text));
                    assert_eq!(state.fragments, 1);
                    assert_eq!(state.bytes, b"nu");
                    assert_eq!(state.bytes.as_ptr(), pointer);
                    assert_eq!(state.retained_bytes(), capacity);
                    assert!(!state.closed);
                }
            }
        }
        assert_eq!(message(accept_frame(
            &mut state, frame(Opcode::Continuation, true, b"ll"), &policy,
        ).unwrap()).payload().as_slice(), b"null");
    }

    #[test]
    fn control_frames_must_be_final_and_fit_both_control_and_frame_limits() {
        for opcode in [Opcode::Ping, Opcode::Pong, Opcode::Close] {
            let policy = WssPolicy::default();
            let mut state = partial(&policy);
            assert_error(
                accept_frame(&mut state, frame(opcode, false, b""), &policy),
                ErrorCode::FramingError, ErrorSource::Transport, 0,
            );
            assert_reset(&state, false);
            let mut state = partial(&policy);
            assert_error(
                accept_frame(&mut state, frame(opcode, true, &[0; 126]), &policy),
                ErrorCode::PayloadTooLarge, ErrorSource::Transport, 0,
            );
            assert_reset(&state, false);
            let tight = WssPolicy { max_frame_bytes: 2, ..policy };
            let mut state = partial(&tight);
            assert_error(
                accept_frame(&mut state, frame(opcode, true, b"\x03\xe8a"), &tight),
                ErrorCode::PayloadTooLarge, ErrorSource::Transport, 0,
            );
            assert_reset(&state, false);
        }
    }

    #[test]
    fn all_close_codes_are_classified_exhaustively() {
        assert_eq!(validate_close_payload(b"").unwrap(), (None, ""));
        assert_error(
            validate_close_payload(b"\x03"), ErrorCode::FramingError, ErrorSource::Transport, 0,
        );
        for code in u16::MIN..=u16::MAX {
            let expected = (1000..=1003).contains(&code)
                || (1007..=1014).contains(&code)
                || (3000..=4999).contains(&code);
            let bytes = code.to_be_bytes();
            if expected {
                assert_eq!(validate_close_payload(&bytes).unwrap(), (Some(code), ""));
            } else {
                assert_error(
                    validate_close_payload(&bytes), ErrorCode::FramingError, ErrorSource::Transport, 0,
                );
            }
        }
    }

    #[test]
    fn close_reason_is_borrowed_utf8_without_json_requirements() {
        for reason in ["", "bye", "é水😀", "\0\n", "{not JSON", "\u{10ffff}"] {
            let mut bytes = 1000_u16.to_be_bytes().to_vec();
            bytes.extend_from_slice(reason.as_bytes());
            let (code, returned) = validate_close_payload(&bytes).unwrap();
            assert_eq!(code, Some(1000));
            assert_eq!(returned, reason);
            assert_eq!(returned.as_ptr(), bytes[2..].as_ptr());
        }
    }

    #[test]
    fn close_reason_byte_limit_is_inclusive_and_measures_utf8_bytes() {
        for len in [0, 1, 122, 123] {
            let mut bytes = 1000_u16.to_be_bytes().to_vec();
            bytes.extend_from_slice(&alloc::vec![b'a'; len]);
            assert_eq!(validate_close_payload(&bytes).unwrap().1.len(), len);
        }
        let mut bytes = 1000_u16.to_be_bytes().to_vec();
        for _ in 0..41 {
            bytes.extend_from_slice("水".as_bytes());
        }
        assert_eq!(bytes.len(), 125);
        assert_eq!(validate_close_payload(&bytes).unwrap().1.len(), 123);
        bytes.push(b'a');
        assert_error(
            validate_close_payload(&bytes), ErrorCode::PayloadTooLarge, ErrorSource::Transport, 0,
        );
    }

    #[test]
    fn malformed_close_reasons_report_offsets_in_the_full_payload() {
        for (reason, offset) in [(b"\xff".as_slice(), 2), (b"ok\xf0\x9f", 4), (b"\xc3\xa9\xff", 4)] {
            let mut bytes = 1000_u16.to_be_bytes().to_vec();
            bytes.extend_from_slice(reason);
            assert_error(
                validate_close_payload(&bytes), ErrorCode::InvalidUtf8, ErrorSource::Utf8, offset,
            );
            let policy = WssPolicy::default();
            let mut state = partial(&policy);
            assert_error(
                accept_frame(&mut state, frame(Opcode::Close, true, &bytes), &policy),
                ErrorCode::InvalidUtf8, ErrorSource::Utf8, offset,
            );
            assert_reset(&state, false);
        }
    }

    #[test]
    fn malformed_close_does_not_close_and_discards_partial_data() {
        let policy = WssPolicy::default();
        for bytes in [b"\x03".as_slice(), b"\x03\xed", b"\x07\xd0", b"\x13\x88"] {
            let mut state = partial(&policy);
            assert_error(
                accept_frame(&mut state, frame(Opcode::Close, true, bytes), &policy),
                ErrorCode::FramingError, ErrorSource::Transport, 0,
            );
            assert_reset(&state, false);
            assert_eq!(message(accept_frame(
                &mut state, frame(Opcode::Text, true, b"null"), &policy,
            ).unwrap()).payload().as_slice(), b"null");
        }
    }

    #[test]
    fn close_publishes_only_owned_reason_bytes_and_permanently_closes() {
        let policy = WssPolicy::default();
        for bytes in [b"".as_slice(), b"\x03\xe8", b"\x03\xe8bye", b"\x0b\xb8\xe6\xb0\xb4"] {
            for payload in [Payload::from_slice(bytes), Payload::from_vec(bytes.to_vec())] {
                let mut state = partial(&policy);
                let capacity = state.retained_bytes();
                let (expected_code, expected_reason) = validate_close_payload(bytes).unwrap();
                match accept_frame(
                    &mut state, IncomingFrame { opcode: Opcode::Close, final_fragment: true, payload }, &policy,
                ).unwrap() {
                    ReceiveEvent::Close { code, reason } => {
                        assert_eq!(code, expected_code);
                        assert_eq!(reason.as_slice(), expected_reason.as_bytes());
                        assert_eq!(reason.len(), expected_reason.len());
                        let clone = reason.clone();
                        drop(reason);
                        assert_eq!(clone.as_slice(), expected_reason.as_bytes());
                    }
                    other => panic!("expected close event, got {other:?}"),
                }
                assert_reset(&state, true);
                assert_eq!(state.retained_bytes(), capacity);
                state.reset_message();
                assert_reset(&state, true);
                for opcode in [Opcode::Continuation, Opcode::Text, Opcode::Binary, Opcode::Ping, Opcode::Pong, Opcode::Close] {
                    for final_fragment in [false, true] {
                        assert_error(
                            accept_frame(&mut state, frame(opcode, final_fragment, b""), &policy),
                            ErrorCode::FramingError, ErrorSource::Transport, 0,
                        );
                        assert_reset(&state, true);
                    }
                }
                let invalid = WssPolicy { max_frame_bytes: 0, ..policy };
                assert_error(
                    accept_frame(&mut state, frame(Opcode::Ping, true, &[0; 126]), &invalid),
                    ErrorCode::FramingError, ErrorSource::Transport, 0,
                );
            }
        }
    }

    #[test]
    fn bytes_close_reason_uses_a_view_of_the_original_owner() {
        let backing = Bytes::from(b"prefix\x03\xe8byesuffix".to_vec());
        let bytes = backing.slice(6..11);
        let pointer = bytes[2..].as_ptr();
        let event = accept_frame(
            &mut FragmentState::new(),
            IncomingFrame { opcode: Opcode::Close, final_fragment: true, payload: Payload::Bytes(bytes) },
            &WssPolicy::default(),
        ).unwrap();
        drop(backing);
        match event {
            ReceiveEvent::Close { code, reason } => {
                assert_eq!(code, Some(1000));
                assert_eq!(reason.as_slice(), b"bye");
                assert_eq!(reason.as_slice().as_ptr(), pointer);
            }
            other => panic!("expected close event, got {other:?}"),
        }
    }

    #[test]
    fn changing_policy_rechecks_active_binary_and_fragment_budgets() {
        let original = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        for field in 0..4 {
            let mut state = FragmentState::new();
            accept_frame(&mut state, frame(Opcode::Binary, false, b"nu"), &original).unwrap();
            let mut policy = original;
            let expected = match field {
                0 => { policy.binary = BinaryPolicy::Reject; ErrorCode::TypeMismatch }
                1 => { policy.max_fragments = 1; ErrorCode::BudgetExhausted }
                2 => { policy.max_message_bytes = 1; ErrorCode::PayloadTooLarge }
                _ => { policy.limits.max_payload_bytes = 1; ErrorCode::PayloadTooLarge }
            };
            assert_error(
                accept_frame(&mut state, frame(Opcode::Continuation, true, b""), &policy),
                expected, ErrorSource::Transport, 0,
            );
            assert_reset(&state, false);
        }
    }

    #[test]
    fn byte_errors_precede_utf8_and_reset_storage_without_publishing() {
        let policy = WssPolicy { max_message_bytes: 3, ..WssPolicy::default() };
        let mut state = partial(&policy);
        let capacity = state.retained_bytes();
        assert_error(
            accept_frame(&mut state, frame(Opcode::Continuation, true, b"\xff\xff"), &policy),
            ErrorCode::PayloadTooLarge, ErrorSource::Transport, 0,
        );
        assert_reset(&state, false);
        assert_eq!(state.retained_bytes(), capacity);
        accept_frame(&mut state, frame(Opcode::Text, false, b"x"), &policy).unwrap();
        assert_eq!(state.retained_bytes(), capacity);
        assert_eq!(message(accept_frame(
            &mut state, frame(Opcode::Continuation, true, b"y"), &policy,
        ).unwrap()).payload().as_slice(), b"xy");
    }

    #[test]
    fn utf8_offset_arithmetic_is_checked() {
        assert_error(
            decode_utf8(b"a\xff", u32::MAX),
            ErrorCode::PayloadTooLarge, ErrorSource::Transport, 0,
        );
        assert_error(
            decode_utf8(b"\xff", u32::MAX),
            ErrorCode::InvalidUtf8, ErrorSource::Utf8, u32::MAX,
        );
    }
}
