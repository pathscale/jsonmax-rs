//! Allocation-free discovery of JSON string boundaries and escape positions.
//!
//! Discovery does not validate escape spellings. A successful scan establishes
//! token boundaries and raw UTF-8 validity, but callers must still decode or
//! measure the content before accepting its JSON escape syntax.

use super::{BACKSLASH, Error, ErrorCode, ErrorSource, QUOTE, Result, Span32, StringScan};

/// Find the first quote, backslash, or raw control byte below `0x20`.
///
/// Arbitrary byte slices are accepted, including invalid UTF-8 and slices whose
/// lengths exceed the ABI's 32-bit limit. The returned index is slice-relative.
/// All reads are bounded by `bytes`; no padding or scanner carry is required.
#[inline]
pub fn first_special(bytes: &[u8]) -> Option<usize> {
    let index = super::first_escape_or_control(bytes);
    if index == bytes.len() {
        None
    } else {
        Some(index)
    }
}

/// Discover one string in a complete input, starting at `opening_quote`.
///
/// All returned spans and error positions are absolute input offsets. Quotes
/// terminate the token only after an even run of consecutive backslashes.
/// Escaped bytes are skipped for delimiter discovery without interpreting the
/// escape, so invalid spellings and surrogate pairs require later validation.
///
/// Opening-position, raw-control, and closure errors have source `Scanner`.
/// Once a closing quote is found, the entire raw content is validated as UTF-8;
/// malformed sequences have source `Utf8` and point to their leading byte.
pub fn scan_string(input: &[u8], opening_quote: u32) -> Result<StringScan> {
    let input_len = checked_scan_len(input.len())?;
    if opening_quote >= input_len {
        return Err(Error::new(
            ErrorCode::UnexpectedEof,
            ErrorSource::Scanner,
            opening_quote,
            "opening quote is outside the input",
        ));
    }

    // The opening offset is below input.len(), so it fits usize on this host.
    let opening = opening_quote as usize;
    if input[opening] != QUOTE {
        return Err(Error::new(
            ErrorCode::InvalidToken,
            ErrorSource::Scanner,
            opening_quote,
            "expected an opening double quote",
        ));
    }

    // Both additions fit: opening_quote < input_len <= u32::MAX, and the
    // opening byte is within a real slice whose length is representable.
    let content_offset = opening_quote + 1;
    let content_start = opening + 1;
    let mut cursor = content_start;
    let mut first_escape = None;

    let mut finder = super::masks::EscapeMaskCursor::new(&input[content_start..]);
    loop {
        cursor = content_start + finder.next_special(cursor - content_start);
        if cursor == input.len() {
            break;
        }
        let offset = checked_scan_len(cursor)?;
        match input[cursor] {
            QUOTE => {
                if let Err(error) = core::str::from_utf8(&input[content_start..cursor]) {
                    // valid_up_to is content-relative and within the bounded
                    // token, so the rebased position cannot exceed input_len.
                    let invalid_offset = checked_scan_len(content_start + error.valid_up_to())?;
                    return Err(Error::new(
                        ErrorCode::InvalidUtf8,
                        ErrorSource::Utf8,
                        invalid_offset,
                        "string content contains invalid UTF-8",
                    ));
                }

                // The closing quote is inside input, hence offset + 1 fits
                // u32 and both span differences are nonnegative.
                let next_offset = offset + 1;
                return Ok(StringScan {
                    raw: Span32::new(opening_quote, next_offset - opening_quote),
                    content: Span32::new(content_offset, offset - content_offset),
                    next_offset,
                    first_escape,
                });
            }
            BACKSLASH => {
                if first_escape.is_none() {
                    first_escape = Some(offset);
                }
                cursor += 1;
                if cursor < input.len() {
                    // Consume exactly one escaped byte. Pairwise consumption
                    // handles arbitrary backslash runs without a parity counter
                    // and leaves escape spelling checks to the decoder.
                    cursor += 1;
                }
            }
            _ => {
                // first_special only discovers quotes, backslashes, and
                // controls. The first two cases were handled above.
                return Err(Error::new(
                    ErrorCode::ControlCharInString,
                    ErrorSource::Scanner,
                    offset,
                    "unescaped control byte in string content",
                ));
            }
        }
    }

    Err(Error::new(
        ErrorCode::UnclosedString,
        ErrorSource::Scanner,
        opening_quote,
        "string has no closing double quote",
    ))
}

/// Keep scanner size errors separate from the root's Decode-source helper.
/// Taking a length also permits testing the ABI limit without a huge allocation.
#[inline]
fn checked_scan_len(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| {
        Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Scanner,
            u32::MAX,
            "string input exceeds the 32-bit ABI length limit",
        )
    })
}
