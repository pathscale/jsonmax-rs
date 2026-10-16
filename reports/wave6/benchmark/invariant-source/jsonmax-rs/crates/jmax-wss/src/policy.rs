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
                    let reason = frame.payload.slice(reason_start..frame.payload.len())?;
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
