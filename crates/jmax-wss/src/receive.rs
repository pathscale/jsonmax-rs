//! Complete, owned WebSocket input at the transport and validator boundaries.

use super::*;

/// Adopts a complete message already reassembled by the transport without copying.
///
/// The caller must supply exactly one complete message, never a frame fragment.
/// Use [`receive_frame`] when fragment boundaries are still present. This checks
/// transport policy and UTF-8, but does not assert that the bytes are valid JSON.
/// The frame byte limit is enforced by frame processing, not on a reassembled
/// message, which may legitimately be larger than any individual frame.
pub fn receive_bytes(bytes: Bytes, opcode: Opcode, policy: &WssPolicy) -> Result<ProtectedInput> {
    validate_message(bytes.as_ref(), opcode, policy)?;
    Ok(ProtectedInput { payload: Payload::Bytes(bytes), opcode })
}

/// Processes one frame through the shared assembly and control-message policy.
///
/// Only a completed data message can produce a [`ReceiveEvent::Message`].
/// Ordering, control frames, assembly limits, and error recovery all belong to
/// the policy module so both transport entry points use the same rules.
pub fn receive_frame(
    state: &mut FragmentState,
    frame: IncomingFrame,
    policy: &WssPolicy,
) -> Result<ReceiveEvent> {
    policy::accept_frame(state, frame, policy)
}

/// Validates one complete protected message and retains the validator's proof.
///
/// The original immutable owner remains alive across validation, allowing the
/// returned payload to be compared with the exact input bytes. A validator may
/// return an equivalent owner, but cannot substitute other contents or report
/// lengths inconsistent with its owners. No proof is issued by this module.
/// Current policy is checked again because it may differ from receive policy.
pub fn validate_input(
    input: ProtectedInput,
    validator: &mut dyn MessageValidator,
    policy: &WssPolicy,
) -> Result<ValidatedSlots> {
    validate_message(input.payload.as_slice(), input.opcode, policy)?;
    let slots = validator.validate(input.payload.clone(), policy.limits)?;
    ensure_validated_slots(&slots, input.payload.as_slice())?;
    ensure_payload_len(slots.decoded.len(), policy.limits.max_decoded_bytes)?;
    ensure_utf8(&slots.decoded)?;
    Ok(slots)
}

/// Applies the same complete-message boundary to both owned input paths.
fn validate_message(bytes: &[u8], opcode: Opcode, policy: &WssPolicy) -> Result<()> {
    policy::validate_policy(policy)?;
    match opcode {
        Opcode::Text => {}
        Opcode::Binary if policy.binary == BinaryPolicy::Json => {}
        Opcode::Binary => {
            return Err(transport_error(ErrorCode::TypeMismatch, "binary JSON is disabled"));
        }
        _ => {
            return Err(transport_error(
                ErrorCode::FramingError,
                "complete JSON input requires a data opcode",
            ));
        }
    }
    ensure_payload_len(bytes.len(), policy.max_message_bytes)?;
    ensure_payload_len(bytes.len(), policy.limits.max_payload_bytes)?;
    ensure_utf8(bytes)
}

fn ensure_utf8(bytes: &[u8]) -> Result<()> {
    if let Err(error) = core::str::from_utf8(bytes) {
        let offset = u32::try_from(error.valid_up_to()).map_err(|_| {
            transport_error(ErrorCode::PayloadTooLarge, "UTF-8 offset exceeds ABI length")
        })?;
        return Err(Error::new(
            ErrorCode::InvalidUtf8,
            ErrorSource::Utf8,
            offset,
            "WebSocket JSON bytes are not valid UTF-8",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_error<T: core::fmt::Debug>(
        result: Result<T>,
        code: ErrorCode,
        source: ErrorSource,
        offset: u32,
    ) {
        let error = result.expect_err("the boundary must reject this input");
        assert_eq!(error.code, code);
        assert_eq!(error.source, source);
        assert_eq!(error.byte_offset, offset);
        assert!(!error.detail.is_empty());
    }

    fn frame(opcode: Opcode, final_fragment: bool, bytes: &'static [u8]) -> IncomingFrame {
        IncomingFrame {
            opcode,
            final_fragment,
            payload: Payload::Bytes(Bytes::from_static(bytes)),
        }
    }

    fn message(event: ReceiveEvent) -> ProtectedInput {
        match event {
            ReceiveEvent::Message(input) => input,
            other => panic!("expected a complete message, got {other:?}"),
        }
    }

    // This validator deliberately fails with caller-selected diagnostics. It
    // records the ownership seam without fabricating a validation proof. The
    // frozen ABI has no safe unchecked constructor for proof-bearing slots.
    struct RejectingValidator {
        error: Error,
        calls: usize,
        received: Option<Payload>,
        limits: Option<Limits>,
    }

    impl RejectingValidator {
        fn new(error: Error) -> Self {
            Self { error, calls: 0, received: None, limits: None }
        }
    }

    impl MessageValidator for RejectingValidator {
        fn validate(&mut self, payload: Payload, limits: Limits) -> Result<ValidatedSlots> {
            self.calls += 1;
            self.received = Some(payload);
            self.limits = Some(limits);
            Err(self.error.clone())
        }
    }

    fn validator() -> RejectingValidator {
        RejectingValidator::new(Error::new(
            ErrorCode::InvalidSyntax,
            ErrorSource::Syntax,
            0,
            "test validator rejected document",
        ))
    }

    #[test]
    fn adopts_bytes_slice_and_preserves_ownership_across_clones_and_drops() {
        let backing = Bytes::from(b"prefix{\"v\":1}suffix".to_vec());
        let bytes = backing.slice(6..13);
        let pointer = bytes.as_ptr();
        let input = receive_bytes(bytes.clone(), Opcode::Text, &WssPolicy::default()).unwrap();
        assert_eq!(input.opcode(), Opcode::Text);
        assert!(matches!(input.payload(), Payload::Bytes(_)));
        assert_eq!(input.payload().as_slice(), b"{\"v\":1}");
        assert_eq!(input.payload().as_slice().as_ptr(), pointer);
        let clone = input.clone();
        drop(backing);
        drop(bytes);
        drop(input);
        let payload = clone.into_payload();
        assert_eq!(payload.as_slice(), b"{\"v\":1}");
        assert_eq!(payload.as_slice().as_ptr(), pointer);
    }

    #[test]
    fn accepts_binary_only_under_explicit_json_policy() {
        let bytes = Bytes::from(b"null".to_vec());
        let pointer = bytes.as_ptr();
        assert_error(
            receive_bytes(bytes.clone(), Opcode::Binary, &WssPolicy::default()),
            ErrorCode::TypeMismatch,
            ErrorSource::Transport,
            0,
        );
        let policy = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        let input = receive_bytes(bytes, Opcode::Binary, &policy).unwrap();
        assert_eq!(input.opcode(), Opcode::Binary);
        assert!(matches!(input.payload(), Payload::Bytes(_)));
        assert_eq!(input.payload().as_slice().as_ptr(), pointer);
    }

    #[test]
    fn complete_bytes_reject_every_non_data_opcode() {
        for opcode in [Opcode::Continuation, Opcode::Close, Opcode::Ping, Opcode::Pong] {
            assert_error(
                receive_bytes(Bytes::from_static(b"null"), opcode, &WssPolicy::default()),
                ErrorCode::FramingError,
                ErrorSource::Transport,
                0,
            );
        }
    }

    #[test]
    fn transport_acceptance_does_not_claim_json_validity() {
        let vectors: &[&[u8]] = &[
            b"", b" ", b"not json", b"{", b"null true", b"\"\\uD800\"", b"\0",
        ];
        let policy = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        for &bytes in vectors {
            for opcode in [Opcode::Text, Opcode::Binary] {
                let input = receive_bytes(Bytes::copy_from_slice(bytes), opcode, &policy).unwrap();
                assert_eq!(input.payload().as_slice(), bytes);
                assert_eq!(input.opcode(), opcode);
            }
        }
    }

    #[test]
    fn message_and_syntax_limits_are_inclusive_and_independent() {
        for syntax_is_tighter in [false, true] {
            let mut policy = WssPolicy::default();
            policy.max_message_bytes = if syntax_is_tighter { 8 } else { 4 };
            policy.limits.max_payload_bytes = if syntax_is_tighter { 4 } else { 8 };
            assert!(receive_bytes(Bytes::from_static(b"nul"), Opcode::Text, &policy).is_ok());
            assert!(receive_bytes(Bytes::from_static(b"null"), Opcode::Text, &policy).is_ok());
            assert_error(
                receive_bytes(Bytes::from_static(b"null "), Opcode::Text, &policy),
                ErrorCode::PayloadTooLarge,
                ErrorSource::Transport,
                0,
            );
        }
    }

    #[test]
    fn complete_transport_messages_can_exceed_the_frame_limit() {
        let mut policy = WssPolicy::default();
        policy.max_frame_bytes = 1;
        policy.max_message_bytes = 4;
        policy.limits.max_payload_bytes = 4;
        let input = receive_bytes(Bytes::from_static(b"null"), Opcode::Text, &policy).unwrap();
        assert_eq!(input.payload().as_slice(), b"null");
        assert_error(
            receive_frame(&mut FragmentState::new(), frame(Opcode::Text, true, b"null"), &policy),
            ErrorCode::PayloadTooLarge,
            ErrorSource::Transport,
            0,
        );
    }

    #[test]
    fn invalid_required_policy_limits_fail_before_adoption_or_validation() {
        let default = WssPolicy::default();
        let mut policies = [default; 5];
        policies[0].max_frame_bytes = 0;
        policies[1].max_message_bytes = 0;
        policies[2].max_fragments = 0;
        policies[3].limits.max_payload_bytes = 0;
        policies[4].limits.max_depth = 0;
        for policy in policies {
            assert_error(
                receive_bytes(Bytes::from_static(b"null"), Opcode::Text, &policy),
                ErrorCode::ConstraintViolation,
                ErrorSource::Transport,
                0,
            );
            let input = receive_bytes(Bytes::from_static(b"null"), Opcode::Text, &default).unwrap();
            let mut validator = validator();
            assert_error(
                validate_input(input, &mut validator, &policy),
                ErrorCode::ConstraintViolation,
                ErrorSource::Transport,
                0,
            );
            assert_eq!(validator.calls, 0);
        }
    }

    #[test]
    fn non_transport_limits_are_left_for_the_configured_validator() {
        let mut policy = WssPolicy::default();
        policy.limits.max_decoded_bytes = 0;
        policy.limits.max_fields_per_object = 0;
        policy.limits.max_string_bytes = 0;
        policy.limits.max_number_digits = 0;
        policy.limits.max_array_items = 0;
        policy.limits.reject_duplicate_keys = false;
        policy.limits.reject_unknown_keys = false;
        let input = receive_bytes(Bytes::from_static(b"null"), Opcode::Text, &policy).unwrap();
        let mut validator = validator();
        assert_eq!(validate_input(input, &mut validator, &policy).unwrap_err(), validator.error);
        assert_eq!(validator.calls, 1);
        assert_eq!(validator.limits, Some(policy.limits));
    }

    #[test]
    fn utf8_vectors_report_the_first_invalid_sequence_offset_for_both_opcodes() {
        let vectors: &[(&[u8], u32)] = &[
            (b"\xff", 0),
            (b"\"\x80\"", 1),
            (b"ok\xc0\xaf", 2),
            (b"ok\xe2\x28\xa1", 2),
            (b"\"\xed\xa0\x80\"", 1),
            (b"\"\xf4\x90\x80\x80\"", 1),
            (b"null \xf0\x9f", 5),
            (b"\xc3\xa9\xff", 2),
        ];
        let policy = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        for &(bytes, offset) in vectors {
            for opcode in [Opcode::Text, Opcode::Binary] {
                assert_error(
                    receive_bytes(Bytes::copy_from_slice(bytes), opcode, &policy),
                    ErrorCode::InvalidUtf8,
                    ErrorSource::Utf8,
                    offset,
                );
            }
            // The decoded arena uses the same checked UTF-8 boundary.
            assert_error(ensure_utf8(bytes), ErrorCode::InvalidUtf8, ErrorSource::Utf8, offset);
        }
    }

    #[test]
    fn valid_unicode_and_empty_decoded_storage_pass_utf8_checks() {
        let vectors = ["", "\0", "ASCII", "é", "水", "😀", "\u{10ffff}", "\u{feff}null"];
        let policy = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        for text in vectors {
            ensure_utf8(text.as_bytes()).unwrap();
            for opcode in [Opcode::Text, Opcode::Binary] {
                let input = receive_bytes(Bytes::copy_from_slice(text.as_bytes()), opcode, &policy)
                    .unwrap();
                assert_eq!(input.payload().as_slice(), text.as_bytes());
            }
        }
    }

    #[test]
    fn byte_limits_and_opcode_policy_precede_utf8_work() {
        let policy = WssPolicy { max_message_bytes: 1, ..WssPolicy::default() };
        assert_error(
            receive_bytes(Bytes::from_static(b"\xff\xff"), Opcode::Text, &policy),
            ErrorCode::PayloadTooLarge,
            ErrorSource::Transport,
            0,
        );
        assert_error(
            receive_bytes(Bytes::from_static(b"\xff"), Opcode::Binary, &policy),
            ErrorCode::TypeMismatch,
            ErrorSource::Transport,
            0,
        );
    }

    #[test]
    fn validation_receives_the_original_owner_and_exact_configured_limits_once() {
        let policy = WssPolicy::default();
        let bytes = Bytes::from(b"null true".to_vec());
        let pointer = bytes.as_ptr();
        let input = receive_bytes(bytes, Opcode::Text, &policy).unwrap();
        let mut validator = RejectingValidator::new(Error::new(
            ErrorCode::TrailingContent,
            ErrorSource::Syntax,
            5,
            "second root is forbidden",
        ).with_schema(17, 23));
        let error = validate_input(input, &mut validator, &policy).unwrap_err();
        assert_eq!(error, validator.error);
        assert_eq!(validator.calls, 1);
        assert_eq!(validator.limits, Some(policy.limits));
        let retained = validator.received.take().unwrap();
        assert!(matches!(&retained, Payload::Bytes(_)));
        assert_eq!(retained.as_slice(), b"null true");
        assert_eq!(retained.as_slice().as_ptr(), pointer);
    }

    #[test]
    fn all_validator_error_metadata_propagates_unchanged() {
        let cases = [
            (ErrorCode::InvalidSyntax, ErrorSource::Syntax),
            (ErrorCode::DepthLimitExceeded, ErrorSource::Cursor),
            (ErrorCode::InvalidUtf8, ErrorSource::Utf8),
            (ErrorCode::MissingRequiredField, ErrorSource::SchemaVm),
            (ErrorCode::DuplicateObjectKey, ErrorSource::SchemaVm),
            (ErrorCode::UnknownObjectKey, ErrorSource::SchemaVm),
            (ErrorCode::DecodedArenaOverflow, ErrorSource::Decode),
            (ErrorCode::BudgetExhausted, ErrorSource::SchemaVm),
            (ErrorCode::Cancelled, ErrorSource::Transport),
            (ErrorCode::InternalError, ErrorSource::SchemaVm),
        ];
        for (code, source) in cases {
            let expected = Error::new(code, source, 3, "configured validation failure")
                .with_schema(91, 7);
            let mut validator = RejectingValidator::new(expected.clone());
            let policy = WssPolicy::default();
            let input = receive_bytes(Bytes::from_static(b"null"), Opcode::Text, &policy).unwrap();
            assert_eq!(validate_input(input, &mut validator, &policy).unwrap_err(), expected);
            assert_eq!(validator.calls, 1);
        }
    }

    #[test]
    fn tightened_message_or_syntax_policy_rejects_before_validator_runs() {
        let original = WssPolicy::default();
        for syntax_is_tighter in [false, true] {
            let input = receive_bytes(Bytes::from_static(b"null"), Opcode::Text, &original).unwrap();
            let mut policy = original;
            if syntax_is_tighter {
                policy.limits.max_payload_bytes = 3;
            } else {
                policy.max_message_bytes = 3;
            }
            let mut validator = validator();
            assert_error(
                validate_input(input, &mut validator, &policy),
                ErrorCode::PayloadTooLarge,
                ErrorSource::Transport,
                0,
            );
            assert_eq!(validator.calls, 0);
            assert!(validator.received.is_none());
        }
    }

    #[test]
    fn changed_binary_policy_rejects_before_validator_runs() {
        let policy = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        let input = receive_bytes(Bytes::from_static(b"null"), Opcode::Binary, &policy).unwrap();
        let mut validator = validator();
        assert_error(
            validate_input(input, &mut validator, &WssPolicy::default()),
            ErrorCode::TypeMismatch,
            ErrorSource::Transport,
            0,
        );
        assert_eq!(validator.calls, 0);
    }

    #[test]
    fn validation_rechecks_internal_inputs_for_utf8_and_data_opcodes() {
        let policy = WssPolicy::default();
        let mut validator = validator();
        let input = ProtectedInput {
            payload: Payload::from_slice(b"\"\xff\""),
            opcode: Opcode::Text,
        };
        assert_error(
            validate_input(input, &mut validator, &policy),
            ErrorCode::InvalidUtf8,
            ErrorSource::Utf8,
            1,
        );
        for opcode in [Opcode::Continuation, Opcode::Ping, Opcode::Pong, Opcode::Close] {
            let input = ProtectedInput { payload: Payload::from_slice(b"null"), opcode };
            assert_error(
                validate_input(input, &mut validator, &policy),
                ErrorCode::FramingError,
                ErrorSource::Transport,
                0,
            );
        }
        assert_eq!(validator.calls, 0);
    }

    #[test]
    fn decoded_length_boundary_is_inclusive_including_zero() {
        // This is the same length guard used after proof/owner consistency.
        assert_eq!(ensure_payload_len(0, 0).unwrap(), 0);
        assert_eq!(ensure_payload_len(3, 3).unwrap(), 3);
        assert_error(
            ensure_payload_len(1, 0),
            ErrorCode::PayloadTooLarge,
            ErrorSource::Transport,
            0,
        );
        assert_error(
            ensure_payload_len(4, 3),
            ErrorCode::PayloadTooLarge,
            ErrorSource::Transport,
            0,
        );
        assert_eq!(ensure_payload_len(u32::MAX as usize, u32::MAX).unwrap(), u32::MAX);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn abi_length_overflow_is_rejected_without_a_large_allocation() {
        assert_error(
            ensure_payload_len(u32::MAX as usize + 1, u32::MAX),
            ErrorCode::PayloadTooLarge,
            ErrorSource::Transport,
            0,
        );
    }

    #[test]
    fn final_frames_preserve_both_payload_storage_variants() {
        let policy = WssPolicy::default();
        for payload in [Payload::from_slice(b"null"), Payload::from_vec(b"null".to_vec())] {
            let pointer = payload.as_slice().as_ptr();
            let shared = matches!(&payload, Payload::Shared(_));
            let mut state = FragmentState::new();
            let input = message(receive_frame(
                &mut state,
                IncomingFrame { opcode: Opcode::Text, final_fragment: true, payload },
                &policy,
            ).unwrap());
            assert_eq!(input.payload().as_slice(), b"null");
            assert_eq!(input.payload().as_slice().as_ptr(), pointer);
            assert_eq!(matches!(input.payload(), Payload::Shared(_)), shared);
            assert!(!state.is_fragmented());
            assert!(!state.is_closed());
            let mut validator = validator();
            assert_eq!(validate_input(input, &mut validator, &policy).unwrap_err(), validator.error);
            assert_eq!(validator.calls, 1);
            let retained = validator.received.unwrap();
            assert_eq!(retained.as_slice().as_ptr(), pointer);
            assert_eq!(matches!(&retained, Payload::Shared(_)), shared);
        }
    }

    #[test]
    fn split_unicode_is_published_only_after_the_final_continuation() {
        let mut policy = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        policy.max_frame_bytes = 3;
        policy.max_message_bytes = 6;
        policy.max_fragments = 3;
        policy.limits.max_payload_bytes = 6;
        for opcode in [Opcode::Text, Opcode::Binary] {
            let mut state = FragmentState::new();
            assert!(matches!(receive_frame(
                &mut state, frame(opcode, false, b"\"\xf0"), &policy,
            ).unwrap(), ReceiveEvent::Pending));
            assert!(state.is_fragmented());
            assert!(matches!(receive_frame(
                &mut state, frame(Opcode::Continuation, false, b"\x9f"), &policy,
            ).unwrap(), ReceiveEvent::Pending));
            let input = message(receive_frame(
                &mut state, frame(Opcode::Continuation, true, b"\x98\x80\""), &policy,
            ).unwrap());
            assert_eq!(input.opcode(), opcode);
            assert_eq!(input.payload().as_slice(), "\"😀\"".as_bytes());
            assert!(!state.is_fragmented());
            let mut validator = validator();
            assert!(validate_input(input, &mut validator, &policy).is_err());
            assert_eq!(validator.calls, 1);
            assert_eq!(validator.received.unwrap().as_slice(), "\"😀\"".as_bytes());
        }
    }

    #[test]
    fn ping_and_pong_preserve_control_owners_and_partial_message() {
        let mut state = FragmentState::new();
        let policy = WssPolicy::default();
        assert!(matches!(receive_frame(
            &mut state, frame(Opcode::Text, false, b"nu"), &policy,
        ).unwrap(), ReceiveEvent::Pending));
        for opcode in [Opcode::Ping, Opcode::Pong] {
            let bytes = Bytes::from(alloc::vec![0xff, 0x00]);
            let pointer = bytes.as_ptr();
            let event = receive_frame(&mut state, IncomingFrame {
                opcode, final_fragment: true, payload: Payload::Bytes(bytes),
            }, &policy).unwrap();
            let payload = match (opcode, event) {
                (Opcode::Ping, ReceiveEvent::Ping(payload)) => payload,
                (Opcode::Pong, ReceiveEvent::Pong(payload)) => payload,
                (_, other) => panic!("unexpected control event: {other:?}"),
            };
            assert_eq!(payload.as_slice(), &[0xff, 0x00]);
            assert_eq!(payload.as_slice().as_ptr(), pointer);
            assert!(state.is_fragmented());
        }
        let input = message(receive_frame(
            &mut state, frame(Opcode::Continuation, true, b"ll"), &policy,
        ).unwrap());
        assert_eq!(input.payload().as_slice(), b"null");
    }

    #[test]
    fn close_discards_partial_input_and_keeps_the_connection_closed() {
        let mut state = FragmentState::new();
        let policy = WssPolicy::default();
        assert!(matches!(receive_frame(
            &mut state, frame(Opcode::Text, false, b"{"), &policy,
        ).unwrap(), ReceiveEvent::Pending));
        match receive_frame(&mut state, frame(Opcode::Close, true, b"\x03\xe8bye"), &policy)
            .unwrap()
        {
            ReceiveEvent::Close { code, reason } => {
                assert_eq!(code, Some(1000));
                assert_eq!(reason.as_slice(), b"bye");
            }
            other => panic!("expected close event, got {other:?}"),
        }
        assert!(state.is_closed());
        assert!(!state.is_fragmented());
        for opcode in [Opcode::Text, Opcode::Continuation, Opcode::Ping, Opcode::Close] {
            assert_error(
                receive_frame(&mut state, frame(opcode, true, b""), &policy),
                ErrorCode::FramingError,
                ErrorSource::Transport,
                0,
            );
            assert!(state.is_closed());
            assert!(!state.is_fragmented());
        }
    }

    #[test]
    fn protocol_errors_reset_fragment_assembly_and_allow_a_fresh_message() {
        let policy = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        for bad_frame in [
            frame(Opcode::Text, true, b"null"),
            frame(Opcode::Binary, true, b"null"),
            frame(Opcode::Ping, false, b""),
            frame(Opcode::Pong, false, b""),
            frame(Opcode::Close, false, b""),
        ] {
            let mut state = FragmentState::new();
            receive_frame(&mut state, frame(Opcode::Text, false, b"{"), &policy).unwrap();
            assert_error(
                receive_frame(&mut state, bad_frame, &policy),
                ErrorCode::FramingError,
                ErrorSource::Transport,
                0,
            );
            assert!(!state.is_fragmented());
            assert!(!state.is_closed());
            let input = message(receive_frame(
                &mut state, frame(Opcode::Text, true, b"null"), &policy,
            ).unwrap());
            assert_eq!(input.payload().as_slice(), b"null");
        }
        assert_error(
            receive_frame(&mut FragmentState::new(), frame(Opcode::Continuation, true, b""), &policy),
            ErrorCode::FramingError,
            ErrorSource::Transport,
            0,
        );
    }

    #[test]
    fn empty_fragments_count_and_fragment_budget_failure_discards_input() {
        let policy = WssPolicy { max_fragments: 2, ..WssPolicy::default() };
        let mut state = FragmentState::new();
        assert!(matches!(receive_frame(
            &mut state, frame(Opcode::Text, false, b""), &policy,
        ).unwrap(), ReceiveEvent::Pending));
        assert!(matches!(receive_frame(
            &mut state, frame(Opcode::Continuation, false, b""), &policy,
        ).unwrap(), ReceiveEvent::Pending));
        assert_error(
            receive_frame(&mut state, frame(Opcode::Continuation, true, b"null"), &policy),
            ErrorCode::BudgetExhausted,
            ErrorSource::Transport,
            0,
        );
        assert!(!state.is_fragmented());
        let input = message(receive_frame(
            &mut state, frame(Opcode::Text, true, b"null"), &policy,
        ).unwrap());
        assert_eq!(input.payload().as_slice(), b"null");
    }

    #[test]
    fn assembled_transport_and_syntax_byte_overflows_discard_partial_input() {
        for syntax_is_tighter in [false, true] {
            let mut policy = WssPolicy::default();
            policy.max_message_bytes = if syntax_is_tighter { 8 } else { 4 };
            policy.limits.max_payload_bytes = if syntax_is_tighter { 4 } else { 8 };
            let mut state = FragmentState::new();
            receive_frame(&mut state, frame(Opcode::Text, false, b"null"), &policy).unwrap();
            assert_error(
                receive_frame(&mut state, frame(Opcode::Continuation, true, b" "), &policy),
                ErrorCode::PayloadTooLarge,
                ErrorSource::Transport,
                0,
            );
            assert!(!state.is_fragmented());
            assert!(!state.is_closed());
        }
    }

    #[test]
    fn incomplete_utf8_is_rejected_only_when_the_message_finishes() {
        let mut state = FragmentState::new();
        let policy = WssPolicy::default();
        assert!(matches!(receive_frame(
            &mut state, frame(Opcode::Text, false, b"\"\xf0\x9f"), &policy,
        ).unwrap(), ReceiveEvent::Pending));
        assert_error(
            receive_frame(&mut state, frame(Opcode::Continuation, true, b""), &policy),
            ErrorCode::InvalidUtf8,
            ErrorSource::Utf8,
            1,
        );
        assert!(!state.is_fragmented());
        assert!(!state.is_closed());
    }
}
