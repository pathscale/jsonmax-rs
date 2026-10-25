//! Allocation-free decoded UTF-8 byte and Unicode scalar measurement.
//!
//! JSON content is validated within its checked source span. Measurement never
//! retains decoded output, normalizes text, or counts UTF-16 code units as scalars.

use super::{
    Error, ErrorCode, ErrorSource, Result, Span32, StringLengths, content_bytes,
};

/// Validate and measure JSON string content without its wrapper quotes.
///
/// Counts and errors match `unescape` with an infallible sink, including absolute
/// source offsets and left-to-right error precedence. Escape and UTF-8 lookahead
/// cannot use bytes after `content.end()`. Caller-constructed spans are checked
/// before any content is inspected. A surrogate pair contributes one scalar and
/// four decoded bytes; escaped controls, including NUL, are valid scalars.
pub fn decoded_length(input: &[u8], content: Span32) -> Result<StringLengths> {
    let bytes = content_bytes(input, content)?;
    super::unescape::measure_content(bytes, content.offset, content.offset + content.len)
}

/// Measure valid UTF-8 directly, without applying JSON content restrictions.
///
/// Bytes are the actual UTF-8 length; scalars are `chars().count()`, independent
/// of combining marks, grapheme clusters, and UTF-16 surrogate representation.
/// Text exceeding the ABI's byte limit fails before counting its scalars.
pub fn utf8_length(text: &str) -> Result<StringLengths> {
    let byte_len = checked_text_len(text.len())?;
    // Every scalar occupies at least one byte, so this count also fits the ABI
    // after the byte-length check. Keep its narrowing explicitly checked.
    let scalar_len = jmax_utf8::count_scalars_trusted_utf8(text.as_bytes());
    Ok(StringLengths::new(byte_len, scalar_len))
}

fn checked_text_len(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| {
        Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Decode,
            u32::MAX,
            "string input exceeds the 32-bit ABI length limit",
        )
    })
}
