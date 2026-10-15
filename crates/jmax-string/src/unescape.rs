//! Strict, allocation-free JSON escape and UTF-16 surrogate decoding.
//!
//! Validation and measurement finish before reserving or writing to a sink.
//! Every lookahead is confined to the caller's content span.

use super::{
    checked_input_len, content_bytes, ByteSink, DecodedEscape, Error, ErrorCode,
    ErrorSource, Result, Span32, StringLengths, BACKSLASH, QUOTE,
};

/// Decode one escape at an absolute input offset.
///
/// A surrogate pair is consumed as one scalar and twelve source bytes. Invalid
/// surrogate spellings are rejected without replacement. EOF errors point to
/// the exclusive end of `input`, including when the requested offset is beyond
/// that end. Bytes after the decoded escape are not validated.
pub fn decode_escape(input: &[u8], backslash_offset: u32) -> Result<DecodedEscape> {
    let end = checked_input_len(input)?;
    if backslash_offset >= end {
        return Err(unexpected_eof(end));
    }
    // The offset is inside a real slice, so it is representable on this host.
    decode_tail(&input[backslash_offset as usize..], backslash_offset, end)
}

/// Decode a checked content span into an ABI byte sink.
///
/// Raw UTF-8 is preserved byte for byte; wrapper quotes are not accepted as
/// content. Invalid input leaves the sink untouched, including its reservation
/// state. Exactly the decoded byte length is reserved before emission. Sink
/// errors retain all metadata and may leave an emitted prefix. Returned counts
/// describe only this append, independent of the sink's existing length.
pub fn unescape(
    input: &[u8],
    content: Span32,
    sink: &mut dyn ByteSink,
) -> Result<StringLengths> {
    let bytes = content_bytes(input, content)?;
    // content_bytes checked span addition and slicing. These sums cannot exceed
    // u32::MAX or the source slice length, on either 32-bit or 64-bit hosts.
    let end = content.offset + content.len;
    let lengths = measure_content(bytes, content.offset, end)?;
    sink.reserve(lengths.byte_len as usize)?;

    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == BACKSLASH {
            let decoded = decode_tail(&bytes[cursor..], content.offset + cursor as u32, end)?;
            let mut encoded = [0; 4];
            sink.write_bytes(decoded.scalar.encode_utf8(&mut encoded).as_bytes())?;
            cursor += usize::from(decoded.consumed);
        } else {
            // The complete first pass already validated this run. Borrow its
            // original bytes instead of allocating or encoding each scalar.
            let run_len = raw_run_len(&bytes[cursor..]);
            sink.write_bytes(&bytes[cursor..cursor + run_len])?;
            cursor += run_len;
        }
    }
    Ok(lengths)
}

fn unexpected_eof(end: u32) -> Error {
    Error::new(
        ErrorCode::UnexpectedEof, ErrorSource::Decode, end,
        "incomplete JSON string escape",
    )
}

fn invalid_escape(offset: u32) -> Error {
    Error::new(
        ErrorCode::InvalidEscape, ErrorSource::Decode, offset,
        "invalid JSON string escape spelling",
    )
}

fn lone_surrogate(offset: u32) -> Error {
    Error::new(
        ErrorCode::LoneSurrogate, ErrorSource::Decode, offset,
        "UTF-16 surrogate requires an adjacent matching escape",
    )
}

/// `tail` begins at `offset` and ends at the validated absolute `end`.
fn decode_tail(tail: &[u8], offset: u32, end: u32) -> Result<DecodedEscape> {
    match tail.first() {
        None => return Err(unexpected_eof(end)),
        Some(&BACKSLASH) => {}
        Some(_) => return Err(invalid_escape(offset)),
    }
    let letter = *tail.get(1).ok_or_else(|| unexpected_eof(end))?;
    let scalar = match letter {
        b'"' => '"',
        b'\\' => '\\',
        b'/' => '/',
        b'b' => '\u{0008}',
        b'f' => '\u{000c}',
        b'n' => '\n',
        b'r' => '\r',
        b't' => '\t',
        b'u' => {
            let high = read_code_unit(tail, offset, end)?;
            let (value, consumed) = match high {
                0xdc00..=0xdfff => return Err(lone_surrogate(offset)),
                0xd800..=0xdbff => {
                    // A partial or absent second prefix is a lone high
                    // surrogate. Only a complete adjacent backslash-u prefix
                    // changes subsequent truncation into UnexpectedEof.
                    if tail.get(6..8) != Some(b"\\u".as_slice()) {
                        return Err(lone_surrogate(offset));
                    }
                    let low_offset = offset + 6;
                    let low = read_code_unit(&tail[6..], low_offset, end)?;
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return Err(lone_surrogate(low_offset));
                    }
                    (0x10000 + ((u32::from(high) - 0xd800) << 10)
                        + (u32::from(low) - 0xdc00), 12)
                }
                _ => (u32::from(high), 6),
            };
            // The ranges above guarantee a scalar. Keep the conversion checked
            // so no unsafe construction or surrogate UTF-8 encoding is needed.
            let scalar = char::from_u32(value).ok_or_else(|| lone_surrogate(offset))?;
            return Ok(DecodedEscape { scalar, consumed });
        }
        _ => return Err(invalid_escape(offset + 1)),
    };
    Ok(DecodedEscape { scalar, consumed: 2 })
}

/// Read exactly four hexadecimal digits after an already checked `\\u` prefix.
fn read_code_unit(tail: &[u8], offset: u32, end: u32) -> Result<u16> {
    let mut value = 0u16;
    for index in 2..6 {
        let byte = *tail.get(index).ok_or_else(|| unexpected_eof(end))?;
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return Err(invalid_escape(offset + index as u32)),
        };
        value = (value << 4) | u16::from(digit);
    }
    Ok(value)
}

fn raw_run_len(bytes: &[u8]) -> usize {
    bytes.iter().position(|&byte| byte == BACKSLASH || byte == QUOTE || byte < 0x20)
        .unwrap_or(bytes.len())
}

fn measure_content(bytes: &[u8], base: u32, end: u32) -> Result<StringLengths> {
    let mut lengths = StringLengths::default();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let offset = base + cursor as u32;
        match bytes[cursor] {
            BACKSLASH => {
                let decoded = decode_tail(&bytes[cursor..], offset, end)?;
                add_scalar(&mut lengths, decoded.scalar, offset)?;
                cursor += usize::from(decoded.consumed);
            }
            QUOTE => return Err(Error::new(
                ErrorCode::InvalidSyntax, ErrorSource::Decode, offset,
                "unescaped double quote in string content",
            )),
            0..=0x1f => return Err(Error::new(
                ErrorCode::ControlCharInString, ErrorSource::Decode, offset,
                "unescaped control byte in string content",
            )),
            _ => {
                let run_len = raw_run_len(&bytes[cursor..]);
                let text = core::str::from_utf8(&bytes[cursor..cursor + run_len])
                    .map_err(|error| Error::new(
                        ErrorCode::InvalidUtf8, ErrorSource::Utf8,
                        offset + error.valid_up_to() as u32,
                        "string content contains invalid UTF-8",
                    ))?;
                for (relative, scalar) in text.char_indices() {
                    add_scalar(&mut lengths, scalar, offset + relative as u32)?;
                }
                cursor += run_len;
            }
        }
    }
    Ok(lengths)
}

/// Commit both counters together, reporting the source scalar on overflow.
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
            ErrorCode::DecodedArenaOverflow, ErrorSource::Decode, offset,
            "decoded string length exceeds the 32-bit ABI limit",
        )),
    }
}
