//! Complete, owned WebSocket input at the transport and validator boundaries.

use super::*;
use alloc::sync::Arc;
use jmax_abi::span::Span32;

/// Adopts a complete message already reassembled by the transport without copying.
///
/// The caller must supply exactly one complete message, never a frame fragment.
/// Use [`receive_frame`] when fragment boundaries are still present. This checks
/// transport policy and UTF-8, but does not assert that the bytes are valid JSON.
/// The frame byte limit is enforced by frame processing, not on a reassembled
/// message, which may legitimately be larger than any individual frame.
pub fn receive_bytes(bytes: Bytes, opcode: Opcode, policy: &WssPolicy) -> Result<ProtectedInput> {
    receive_payload(Payload::Bytes(bytes), opcode, policy)
}

/// Adopts a complete immutable Arc-backed transport message without copying.
pub fn receive_arc_owner<T: AsRef<[u8]> + Send + Sync + 'static>(
    owner: Arc<T>, opcode: Opcode, policy: &WssPolicy,
) -> Result<ProtectedInput> {
    receive_payload(Payload::from_arc_owner(owner)?, opcode, policy)
}

/// Accepts any ABI owner, including an Arc slice or a nested shared subview.
pub fn receive_payload(payload: Payload, opcode: Opcode, policy: &WssPolicy) -> Result<ProtectedInput> {
    validate_message(payload.as_slice(), opcode, policy)?;
    Ok(ProtectedInput { payload, opcode })
}

/// Adopts a complete message subrange within a larger transport allocation.
/// Framing, unmasking, and decompression must already have completed.
pub fn receive_subview(
    payload: &Payload, span: Span32, opcode: Opcode, policy: &WssPolicy,
) -> Result<ProtectedInput> {
    receive_payload(payload.try_slice(span)?, opcode, policy)
}

/// Transfers a protected complete message directly to owned CPU admission.
/// Current transport policy is checked again before the owner is moved.
pub fn into_parse_job(
    input: ProtectedInput,
    policy: &WssPolicy,
    cancellation: jmax_nagoya::CancellationToken,
) -> Result<jmax_nagoya::ParseJob> {
    validate_message(input.payload.as_slice(), input.opcode, policy)?;
    Ok(jmax_nagoya::ParseJob { payload: input.payload, limits: policy.limits, cancellation })
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
    ensure_validated_slots(&slots, &input.payload)?;
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
