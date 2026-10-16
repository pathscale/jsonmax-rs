//! Validated, immutable ownership at the outbound WebSocket frame boundary.

use super::*;

/// Owns an already encoded transport frame and its validated JSON source across
/// partial writes. `encoded` includes any wire header/masking required by the
/// transport; constructing this cursor never re-encodes or copies either owner.
/// The transport is responsible for encoding the matching WebSocket frame.
#[derive(Debug)]
pub struct OutboundFrame {
    output: ValidatedOutput,
    encoded: Payload,
    offset: usize,
}

impl OutboundFrame {
    pub fn new(output: ValidatedOutput, encoded: Payload) -> Self {
        Self { output, encoded, offset: 0 }
    }

    pub fn output(&self) -> &ValidatedOutput { &self.output }
    pub fn encoded(&self) -> &Payload { &self.encoded }
    pub fn offset(&self) -> usize { self.offset }
    pub fn remaining(&self) -> &[u8] { &self.encoded.as_slice()[self.offset..] }
    pub fn is_complete(&self) -> bool { self.offset == self.encoded.len() }

    /// Retains a subview owner if the transport needs an owned pending write.
    pub fn remaining_payload(&self) -> Result<Payload> {
        self.encoded.slice(self.offset..self.encoded.len())
    }

    /// Report only the bytes actually accepted by the transport. A zero write
    /// leaves the cursor intact for a later readiness wake; an invalid count
    /// fails atomically without advancing. Never retry a consumed prefix.
    pub fn advance(&mut self, written: usize) -> Result<()> {
        if written > self.encoded.len() - self.offset {
            return Err(transport_error(ErrorCode::TransportIncomplete, "write progress exceeds pending frame"));
        }
        self.offset += written;
        Ok(())
    }

    /// Return all owners and the cursor for transfer to another transport task.
    pub fn into_parts(self) -> (ValidatedOutput, Payload, usize) {
        (self.output, self.encoded, self.offset)
    }
}

/// Validates one complete JSON document before publishing an owned data frame.
///
/// The validator receives a shared clone of the immutable payload and the exact
/// configured syntax limits. Its implementation must validate the whole document,
/// including unselected data and strict EOF, under its configured schema. The
/// original owner stays alive while the returned buffers and proof are checked.
/// Only that original owner, the opcode, and the unchanged proof are retained;
/// decoded storage and selected slots are dropped before returning.
///
/// This module allocates no payload copies and never issues a validation proof.
/// Validator errors, including schema context and offsets, propagate unchanged.
pub fn prepare_output(
    payload: Payload,
    opcode: Opcode,
    validator: &mut dyn MessageValidator,
    policy: &WssPolicy,
) -> Result<ValidatedOutput> {
    validate_frame(payload.as_slice(), opcode, policy)?;
    let validated = validator.validate(payload.clone(), policy.limits)?;
    ensure_validated_slots(&validated, &payload)?;
    ensure_payload_len(validated.decoded.len(), policy.limits.max_decoded_bytes)?;
    ensure_utf8(&validated.decoded)?;
    Ok(ValidatedOutput { payload, opcode, proof: validated.proof })
}

/// Rechecks an existing validated frame against the current transport policy.
///
/// This checks its data opcode, all byte ceilings, UTF-8, and proof lengths. The
/// decoded length remains bounded even though decoded storage is not retained in
/// an outbound frame. The existing schema fingerprint is never replaced. These
/// boundary checks do not rerun JSON or schema validation; a caller needing new
/// schema validation must use [`prepare_output`] with its configured validator.
pub fn validate_output(output: &ValidatedOutput, policy: &WssPolicy) -> Result<()> {
    validate_frame(output.payload.as_slice(), output.opcode, policy)?;
    ensure_proof_lengths(
        output.payload.len(),
        output.proof.payload_len(),
        output.proof.decoded_len(),
        policy.limits.max_decoded_bytes,
    )
}

/// Unlike reassembled input, an output is handed to the transport as one atomic
/// frame, so the frame ceiling applies in addition to both message ceilings.
fn validate_frame(bytes: &[u8], opcode: Opcode, policy: &WssPolicy) -> Result<()> {
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
                "validated JSON output requires a data opcode",
            ));
        }
    }
    ensure_payload_len(bytes.len(), policy.max_frame_bytes)?;
    ensure_payload_len(bytes.len(), policy.max_message_bytes)?;
    ensure_payload_len(bytes.len(), policy.limits.max_payload_bytes)?;
    ensure_utf8(bytes)
}

/// Checks existing evidence only. Metadata cannot establish JSON validity.
fn ensure_proof_lengths(
    payload_len: usize,
    proof_payload_len: u32,
    proof_decoded_len: u32,
    max_decoded_bytes: u32,
) -> Result<()> {
    if usize::try_from(proof_payload_len).ok() != Some(payload_len) {
        return Err(transport_error(
            ErrorCode::InternalError,
            "validated output payload and proof lengths differ",
        ));
    }
    if proof_decoded_len > max_decoded_bytes {
        return Err(transport_error(
            ErrorCode::PayloadTooLarge,
            "validated output decoded byte limit exceeded",
        ));
    }
    Ok(())
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
            "WebSocket JSON output is not valid UTF-8",
        ));
    }
    Ok(())
}
