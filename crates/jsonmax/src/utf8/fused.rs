//! Safe integration of scanner classifications with incremental UTF-8 validation.
//!
//! Public masks are untrusted metadata. Only the provided slice is read, and
//! its high bits are checked before the ordinary stream validator is called.
//! This adapter allocates nothing and requires no optional backend dependency.

use super::{BlockMasks, Error, ErrorCode, ErrorSource, Result, Utf8Stream, BLOCK_BYTES};

/// Validate one classified block of at most 64 actual bytes.
///
/// `valid_len` must equal `bytes.len()`, `valid_mask` must contain exactly that
/// many low bits, and `non_ascii_mask` must exactly describe the bytes whose
/// high bit is set. Other classifications and the backend tag are ignored.
/// Masks never authorize reads beyond `bytes` or establish a borrowed proof.
///
/// Once metadata is accepted, this is exactly [`super::stream::validate_chunk`],
/// including validation of an incoming incomplete scalar for an ASCII block.
/// Call [`super::stream::finish`] to reject a truncated final sequence.
///
/// # Errors
/// Oversized blocks or mismatched metadata return `InvalidToken`, source
/// `Utf8`, at the current stream byte offset, without changing any state.
/// These checks precede saved errors, finalization, and cumulative overflow.
/// With accepted metadata, stream errors and their poisoning behavior apply.
pub fn validate_classified(
    bytes: &[u8],
    masks: &BlockMasks,
    state: &mut Utf8Stream,
) -> Result<()> {
    if bytes.len() > BLOCK_BYTES || usize::from(masks.valid_len) != bytes.len() {
        return Err(invalid_metadata(state, "UTF-8 block length does not match classification"));
    }

    // Handle the full block explicitly so a shift by 64 is never evaluated.
    let valid_mask = if bytes.len() == BLOCK_BYTES {
        u64::MAX
    } else {
        (1u64 << bytes.len()) - 1
    };
    if masks.valid_mask != valid_mask || masks.non_ascii_mask & !valid_mask != 0 {
        return Err(invalid_metadata(state, "UTF-8 classification has invalid occupied lanes"));
    }

    let mut non_ascii_mask = 0u64;
    for (index, &byte) in bytes.iter().enumerate() {
        // The length guard bounds index to 0..64, including unaligned tails.
        if !byte.is_ascii() {
            non_ascii_mask |= 1u64 << index;
        }
    }
    if masks.non_ascii_mask != non_ascii_mask {
        return Err(invalid_metadata(state, "UTF-8 classification does not match byte high bits"));
    }

    super::stream::validate_chunk(state, bytes)
}

fn invalid_metadata(state: &Utf8Stream, detail: &'static str) -> Error {
    Error::new(ErrorCode::InvalidToken, ErrorSource::Utf8, state.bytes_seen(), detail)
}
