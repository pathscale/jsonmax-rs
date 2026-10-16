//! Allocation-free decoded UTF-8 byte and Unicode scalar measurement.
//!
//! JSON content is validated within its checked source span. Measurement never
//! retains decoded output, normalizes text, or counts UTF-16 code units as scalars.

use super::unescape::decode_escape;
use super::{
    BACKSLASH, Error, ErrorCode, ErrorSource, QUOTE, Result, Span32, StringLengths, content_bytes,
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
    // Span validation proves that both indices fit usize and this addition is
    // bounded by input.len(), even on a 32-bit host. Keeping the original prefix
    // lets decode_escape report absolute offsets while limiting its lookahead.
    let bounded_input = &input[..content.offset as usize + bytes.len()];
    let mut lengths = StringLengths::default();
    let mut cursor = 0;
    let mut finder = super::masks::EscapeMaskCursor::new(bytes);

    while cursor < bytes.len() {
        // The checked span ends at or below u32::MAX, so every source index and
        // rebase below is representable without truncation or overflow.
        let offset = content.offset + cursor as u32;
        match bytes[cursor] {
            BACKSLASH => {
                let decoded = decode_escape(bounded_input, offset)?;
                add_scalar(&mut lengths, decoded.scalar, offset)?;
                cursor += usize::from(decoded.consumed);
            }
            QUOTE => {
                return Err(Error::new(
                    ErrorCode::InvalidSyntax,
                    ErrorSource::Decode,
                    offset,
                    "unescaped double quote in string content",
                ));
            }
            0..=0x1f => {
                return Err(Error::new(
                    ErrorCode::ControlCharInString,
                    ErrorSource::Decode,
                    offset,
                    "unescaped control byte in string content",
                ));
            }
            _ => {
                let tail = &bytes[cursor..];
                let run_len = finder.next_special(cursor) - cursor;
                // As in unescape, validate a complete raw run before measuring
                // its scalars. A split sequence fails at its leading byte.
                let text = core::str::from_utf8(&tail[..run_len]).map_err(|error| {
                    Error::new(
                        ErrorCode::InvalidUtf8,
                        ErrorSource::Utf8,
                        offset + error.valid_up_to() as u32,
                        "string content contains invalid UTF-8",
                    )
                })?;
                for (relative, scalar) in text.char_indices() {
                    add_scalar(&mut lengths, scalar, offset + relative as u32)?;
                }
                cursor += run_len;
            }
        }
    }
    Ok(lengths)
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

/// Update both counts together, preserving the source scalar on overflow.
fn add_scalar(lengths: &mut StringLengths, scalar: char, offset: u32) -> Result<()> {
    match (
        lengths.byte_len.checked_add(scalar.len_utf8() as u32),
        lengths.scalar_len.checked_add(1),
    ) {
        (Some(byte_len), Some(scalar_len)) => {
            *lengths = StringLengths::new(byte_len, scalar_len);
            Ok(())
        }
        _ => Err(Error::new(
            ErrorCode::DecodedArenaOverflow,
            ErrorSource::Decode,
            offset,
            "decoded string length exceeds the 32-bit ABI limit",
        )),
    }
}
