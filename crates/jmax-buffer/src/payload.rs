//! Length-checked adoption and construction of canonical ABI payloads.
//!
//! These functions establish ownership and payload length limits only. Arbitrary
//! bytes, including invalid UTF-8 and invalid JSON, are accepted within the limit.
//! Allocation follows the infallible ABI constructors' normal allocator behavior.

use super::{Arc, BufferLimits, Error, ErrorCode, ErrorSource, Payload, Result, Vec};

/// Adopts an existing payload without copying or changing its visible range.
///
/// Both shared slices and the ABI's Bytes variant retain their storage variant.
/// Only visible length is checked; a sliced payload may retain a larger backing
/// allocation. This function neither validates contents nor issues a proof.
pub fn adopt(payload: Payload, limits: BufferLimits) -> Result<Payload> {
    validate_len(payload.len(), limits)?;
    Ok(payload)
}

/// Adopts a shared byte slice without copying its byte allocation.
pub fn from_arc(bytes: Arc<[u8]>, limits: BufferLimits) -> Result<Payload> {
    validate_len(bytes.len(), limits)?;
    Ok(Payload::Shared(bytes))
}

/// Transfers a vector through the canonical ABI ownership constructor.
///
/// Spare capacity does not count toward the payload limit. No capacity
/// preservation guarantee is made by this interface.
pub fn from_vec(bytes: Vec<u8>, limits: BufferLimits) -> Result<Payload> {
    validate_len(bytes.len(), limits)?;
    Ok(Payload::from_vec(bytes))
}

/// Copies borrowed bytes into a shared payload after checking their length.
pub fn from_slice(bytes: &[u8], limits: BufferLimits) -> Result<Payload> {
    validate_len(bytes.len(), limits)?;
    Ok(Payload::from_slice(bytes))
}

/// Checks both ABI representability and the configured payload limit.
///
/// Equality is allowed, including an empty payload with a zero limit. The
/// decoded arena limit is independent and does not affect payload admission.
/// Errors contain static, payload-free metadata with an unknown byte offset.
pub fn validate_len(len: usize, limits: BufferLimits) -> Result<u32> {
    let too_large = || {
        Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Decode,
            u32::MAX,
            "payload length exceeds configured or ABI limit",
        )
    };
    let len = u32::try_from(len).map_err(|_| too_large())?;
    if len > limits.max_payload_bytes {
        return Err(too_large());
    }
    Ok(len)
}
