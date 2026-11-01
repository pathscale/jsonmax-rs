//! Bulk scalar counting without decoding individual code points.

use super::{checked_len, Error, ErrorCode, ErrorSource, Result};

/// Validate a complete slice once through ps-simdutf8 and count Unicode scalars.
/// Errors identify the first malformed sequence relative to `base_offset`.
pub fn validate_and_count_scalars(bytes: &[u8], base_offset: u32) -> Result<u32> {
    let len = checked_len(bytes.len())?;
    base_offset.checked_add(len).ok_or_else(|| {
        Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Utf8,
            base_offset,
            "UTF-8 span exceeds the 32-bit ABI offset limit",
        )
    })?;
    simdutf8::compat::from_utf8(bytes).map_err(|error| {
        Error::new(
            ErrorCode::InvalidUtf8,
            ErrorSource::Utf8,
            base_offset + error.valid_up_to() as u32,
            if error.error_len().is_none() {
                "incomplete UTF-8 sequence"
            } else {
                "invalid UTF-8 sequence"
            },
        )
    })?;
    Ok(count_scalars_trusted_utf8(bytes))
}

/// Count scalars in already validated UTF-8, without validating again.
///
/// The caller must supply valid UTF-8 with byte length at most `u32::MAX`.
/// This is a counting contract only; arbitrary bytes cannot authorize UTF-8
/// access or create a validation proof. Counts saturate for oversized slices.
#[inline]
pub fn count_scalars_trusted_utf8(bytes: &[u8]) -> u32 {
    u32::try_from(bytes.len() - count_continuation_bytes(bytes)).unwrap_or(u32::MAX)
}

/// SWAR counts eight independent `(byte & 0xc0) == 0x80` predicates per word.
/// No subtraction or addition can borrow/carry across bytes. Native endianness
/// only permutes the counted bits; short tails never overread their slice.
pub fn count_continuation_bytes(bytes: &[u8]) -> usize {
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let mut count = 0usize;
    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        let mut word = [0u8; 8];
        word.copy_from_slice(chunk);
        let word = u64::from_ne_bytes(word);
        count += ((word & HIGH) & !((word << 1) & HIGH)).count_ones() as usize;
    }
    count
        + chunks
            .remainder()
            .iter()
            .filter(|&&b| b & 0xc0 == 0x80)
            .count()
}
