//! UTF-8 evidence tied to the immutable bytes that were actually validated.
//!
//! A proof stores a borrowed string, never an owner identifier or a reconstructed
//! pointer. Copying it preserves the same borrow. Slices use proof-relative byte
//! offsets; payload validation reports errors relative to the payload.

use core::ops::Range;

#[cfg(feature = "alloc")]
use super::Payload;
use super::{checked_len, Error, ErrorCode, ErrorSource, Result, Span32, Utf8Proof, Utf8Summary};

/// Validate the complete input and bind its exact summary to its immutable borrow.
///
/// No bytes are copied, normalized, decoded, or retained independently of the
/// input. Empty input is valid and ASCII-only. Validation uses the same safe
/// backend as [`super::validate::validate_str`].
///
/// # Errors
/// Input exceeding `u32::MAX` bytes returns `PayloadTooLarge` before validation.
/// Malformed or incomplete UTF-8 returns `InvalidUtf8` at the first malformed
/// sequence's leading byte, relative to `bytes`.
///
/// A proof cannot outlive its input:
///
/// ```compile_fail
/// let proof;
/// {
///     let bytes = [b'a', b'b'];
///     let Ok(validated) = jmax_utf8::proof::validate(&bytes) else { return; };
///     proof = validated;
/// }
/// assert_eq!(proof.as_str(), "ab");
/// ```
pub fn validate(bytes: &[u8]) -> Result<Utf8Proof<'_>> {
    let text = super::validate::validate_str(bytes)?;
    let byte_len = checked_len(text.len())?;
    let scalar_len = super::count_scalars_trusted_utf8(text.as_bytes());
    // Every scalar takes at least one byte. Equal counts therefore mean that
    // all scalars are ASCII, including the empty string.
    let summary = Utf8Summary {
        byte_len,
        scalar_len,
        ascii_only: byte_len == scalar_len,
    };
    Ok(Utf8Proof { text, summary })
}

/// Validate exactly `span` within `owner` and borrow that payload sub-slice.
///
/// The summary describes only the selected bytes. Bytes outside the span need
/// not be UTF-8. Even an empty span inside an otherwise malformed payload is
/// valid, provided it is within bounds. This operation itself does not allocate.
///
/// # Errors
/// Checks the complete owner length first. Span arithmetic overflow returns
/// `PayloadTooLarge` at `span.offset`; an out-of-bounds span returns
/// `UnexpectedEof` there. UTF-8 errors report the malformed sequence's leading
/// byte relative to the owner, including sequences cut short by the span.
#[cfg(feature = "alloc")]
pub fn validate_span<'a>(owner: &'a Payload, span: Span32) -> Result<Utf8Proof<'a>> {
    let bytes = owner.as_slice();
    let range = checked_range(bytes.len(), span)?;
    let selected = bytes.get(range).ok_or_else(|| out_of_bounds(span.offset))?;
    validate(selected).map_err(|error| rebase_error(error, span.offset))
}

/// Borrow a string view using byte offsets relative to the proof's bytes.
///
/// Both endpoints must be Unicode scalar boundaries. Empty ranges are allowed
/// at any such boundary, including the end. The returned view borrows the
/// original bytes and may outlive the proof value itself.
///
/// # Errors
/// Span arithmetic overflow returns `PayloadTooLarge` at `span.offset`.
/// Out-of-bounds spans return `UnexpectedEof` there, before boundary checks.
/// A non-boundary endpoint returns `InvalidUtf8` at that endpoint, checking the
/// start before the end. All offsets are relative to the proof, even when it
/// was obtained from a nonzero payload offset.
pub fn slice<'a>(proof: &Utf8Proof<'a>, span: Span32) -> Result<&'a str> {
    let text = proof.as_str();
    let range = checked_range(text.len(), span)?;
    if !text.is_char_boundary(range.start) {
        return Err(non_boundary(span.offset));
    }
    // Bounds and the starting boundary are already checked. Safe str slicing
    // can now fail only because the exclusive end splits a scalar.
    text.get(range)
        .ok_or_else(|| non_boundary(span.offset + span.len))
}

fn checked_range(byte_len: usize, span: Span32) -> Result<Range<usize>> {
    let byte_len = checked_len(byte_len)?;
    let end = span.end().ok_or_else(|| span_overflow(span.offset))?;
    if end > byte_len {
        return Err(out_of_bounds(span.offset));
    }
    // Both endpoints are bounded by the original usize length. Checked
    // conversions also keep this code portable to narrower usize targets.
    let start = usize::try_from(span.offset).map_err(|_| out_of_bounds(span.offset))?;
    let end = usize::try_from(end).map_err(|_| out_of_bounds(span.offset))?;
    Ok(start..end)
}

fn span_overflow(offset: u32) -> Error {
    Error::new(
        ErrorCode::PayloadTooLarge,
        ErrorSource::Utf8,
        offset,
        "UTF-8 span exceeds the 32-bit ABI offset limit",
    )
}

fn out_of_bounds(offset: u32) -> Error {
    Error::new(
        ErrorCode::UnexpectedEof,
        ErrorSource::Utf8,
        offset,
        "UTF-8 span is outside the borrowed bytes",
    )
}

fn non_boundary(offset: u32) -> Error {
    Error::new(
        ErrorCode::InvalidUtf8,
        ErrorSource::Utf8,
        offset,
        "UTF-8 slice endpoint is not a Unicode scalar boundary",
    )
}

#[cfg(feature = "alloc")]
fn rebase_error(mut error: Error, offset: u32) -> Error {
    if error.code == ErrorCode::InvalidUtf8 {
        // Validated owner bounds imply this addition fits. Keep it checked so
        // even error translation never relies on wrapping offset arithmetic.
        match offset.checked_add(error.byte_offset) {
            Some(absolute) => error.byte_offset = absolute,
            None => return span_overflow(offset),
        }
    }
    error
}
