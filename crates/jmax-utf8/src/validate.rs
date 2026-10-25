//! Complete, allocation-free UTF-8 validation through safe backend facades.
//!
//! Backend selection is delegated to the no_std-capable `ps-simdutf8`.
//! Validation preserves the input borrow and reports the first malformed
//! sequence's start; the `simd-utf8` feature remains a compatibility flag.

pub use super::count::{count_scalars_trusted_utf8, validate_and_count_scalars};
use super::{checked_len, Error, ErrorCode, ErrorSource, Result, Utf8Summary};

/// Validate an entire slice and count its bytes and Unicode scalar values.
///
/// Empty input is valid and ASCII-only. Counts include control characters,
/// byte-order marks, and combining characters without normalization or JSON
/// escape processing. A non-BMP scalar counts once, regardless of its width.
///
/// # Errors
/// Returns `PayloadTooLarge` before validation if the input exceeds the ABI's
/// 32-bit length limit. Invalid or incomplete UTF-8 returns `InvalidUtf8` at
/// the leading byte of the first malformed sequence.
pub fn validate(bytes: &[u8]) -> Result<Utf8Summary> {
    let text = validate_str(bytes)?;
    let byte_len = checked_len(text.len())?;
    let scalar_len = super::count_scalars_trusted_utf8(text.as_bytes());

    // In valid UTF-8 each scalar occupies one to four bytes. Equality holds
    // exactly when every scalar is ASCII, avoiding another pass over the data.
    Ok(Utf8Summary {
        byte_len,
        scalar_len,
        ascii_only: scalar_len == byte_len,
    })
}

/// Validate an entire slice and borrow it as a string without allocation.
///
/// The returned string covers the exact input bytes with the input's lifetime.
/// No byte-order mark is stripped and no replacement or decoding is performed.
///
/// # Errors
/// Returns `PayloadTooLarge` for input longer than `u32::MAX` before invoking
/// the backend. Invalid or incomplete UTF-8 returns `InvalidUtf8`, with source
/// `Utf8` and the same byte offset as `core::str::Utf8Error::valid_up_to()`.
pub fn validate_str(bytes: &[u8]) -> Result<&str> {
    let _t = jmax_trace::span!(UTF8_VALIDATE);
    checked_len(bytes.len())?;

    // The package is ps-simdutf8, but its library target is named simdutf8.
    // The compat entry point preserves precise offsets for malformed input.
    let validated = simdutf8::compat::from_utf8(bytes);

    match validated {
        Ok(text) => Ok(text),
        Err(error) => Err(Error::new(
            ErrorCode::InvalidUtf8,
            ErrorSource::Utf8,
            checked_len(error.valid_up_to())?,
            if error.error_len().is_none() {
                "incomplete UTF-8 sequence"
            } else {
                "invalid UTF-8 sequence"
            },
        )),
    }
}
