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
