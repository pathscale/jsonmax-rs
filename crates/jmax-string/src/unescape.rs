//! Strict, allocation-free JSON escape and UTF-16 surrogate decoding.
//!
//! Validation and measurement finish before reserving or writing to a sink.
//! Every lookahead is confined to the caller's content span.

use super::{
    BACKSLASH, ByteSink, DecodedEscape, Error, ErrorCode, ErrorSource, QUOTE, Result, Span32,
    StringLengths, checked_input_len, content_bytes,
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
pub fn unescape(input: &[u8], content: Span32, sink: &mut dyn ByteSink) -> Result<StringLengths> {
    let _t = jmax_trace::span!(STRING_UNESCAPE);
    let bytes = content_bytes(input, content)?;
    // content_bytes checked span addition and slicing. These sums cannot exceed
    // u32::MAX or the source slice length, on either 32-bit or 64-bit hosts.
    let end = content.offset + content.len;
    let lengths = measure_content(bytes, content.offset, end)?;
    sink.reserve(lengths.byte_len as usize)?;

    let mut cursor = 0;
    let mut finder = super::masks::EscapeMaskCursor::new(bytes);
    while cursor < bytes.len() {
        if bytes[cursor] == BACKSLASH {
            let decoded = decode_tail(&bytes[cursor..], content.offset + cursor as u32, end)?;
            let mut encoded = [0; 4];
            sink.write_bytes(decoded.scalar.encode_utf8(&mut encoded).as_bytes())?;
            cursor += usize::from(decoded.consumed);
        } else {
            // The complete first pass already validated this run. Borrow its
            // original bytes instead of allocating or encoding each scalar.
            let run_len = finder.next_special(cursor) - cursor;
            sink.write_bytes(&bytes[cursor..cursor + run_len])?;
            cursor += run_len;
        }
    }
    Ok(lengths)
}

fn unexpected_eof(end: u32) -> Error {
    Error::new(
        ErrorCode::UnexpectedEof,
        ErrorSource::Decode,
        end,
        "incomplete JSON string escape",
    )
}

fn invalid_escape(offset: u32) -> Error {
    Error::new(
        ErrorCode::InvalidEscape,
        ErrorSource::Decode,
        offset,
        "invalid JSON string escape spelling",
    )
}

fn lone_surrogate(offset: u32) -> Error {
    Error::new(
        ErrorCode::LoneSurrogate,
        ErrorSource::Decode,
        offset,
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
                    (
                        0x10000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(low) - 0xdc00),
                        12,
                    )
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
    Ok(DecodedEscape {
        scalar,
        consumed: 2,
    })
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

pub(crate) fn measure_content(bytes: &[u8], base: u32, end: u32) -> Result<StringLengths> {
    let mut lengths = StringLengths::default();
    let mut cursor = 0;
    let mut finder = super::masks::EscapeMaskCursor::new(bytes);
    while cursor < bytes.len() {
        let offset = base + cursor as u32;
        match bytes[cursor] {
            BACKSLASH => {
                let decoded = decode_tail(&bytes[cursor..], offset, end)?;
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
                let run_len = finder.next_special(cursor) - cursor;
                let text =
                    core::str::from_utf8(&bytes[cursor..cursor + run_len]).map_err(|error| {
                        Error::new(
                            ErrorCode::InvalidUtf8,
                            ErrorSource::Utf8,
                            offset + error.valid_up_to() as u32,
                            "string content contains invalid UTF-8",
                        )
                    })?;
                // Decoding never expands an ordinary run and each escape
                // consumes at least its decoded length. The checked raw length
                // therefore bounds both cumulative counters by u32::MAX.
                lengths.byte_len += run_len as u32;
                lengths.scalar_len += jmax_utf8::count_scalars_trusted_utf8(text.as_bytes());
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
            ErrorCode::DecodedArenaOverflow,
            ErrorSource::Decode,
            offset,
            "decoded string length exceeds the 32-bit ABI limit",
        )),
    }
}

/// Cook one complete raw content slice in a single transactional traversal.
///
/// `raw_content` excludes wrapper quotes. Ordinary UTF-8 runs are validated and
/// counted in bulk, then appended once; only escapes are decoded individually.
/// Decoded byte and scalar lengths are accumulated inline while cooking.
/// `max_string_bytes` bounds decoded bytes for this string, independently of
/// the arena's total limit. The caller enforces any separate raw-token limit.
/// Every error restores the arena's original logical length and bytes (retained
/// capacity may grow). Returned offsets refer to the supplied absolute base.
pub fn cook_into(
    raw_content: &[u8],
    raw_content_offset: u32,
    arena: &mut jmax_buffer::DecodedArena,
    max_string_bytes: u32,
) -> Result<jmax_abi::span::StrRef> {
    let _t = jmax_trace::span!(STRING_UNESCAPE);
    let len = checked_input_len(raw_content)?;
    let end = raw_content_offset.checked_add(len).ok_or_else(|| {
        Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Decode,
            raw_content_offset,
            "string content span overflows its ABI offset",
        )
    })?;
    let checkpoint = u32::try_from(arena.len()).map_err(|_| {
        Error::new(
            ErrorCode::DecodedArenaOverflow,
            ErrorSource::Decode,
            raw_content_offset,
            "decoded arena length exceeds the 32-bit ABI limit",
        )
    })?;
    let result = (|| {
        let mut finder = super::masks::EscapeMaskCursor::new(raw_content);
        let mut cursor = 0usize;
        let mut lengths = StringLengths::default();
        while cursor < raw_content.len() {
            let offset = raw_content_offset + cursor as u32;
            let tail = &raw_content[cursor..];
            let run_len = finder.next_special(cursor) - cursor;
            if run_len != 0 {
                let run = &tail[..run_len];
                let validated = if run.is_ascii() {
                    Ok(run_len as u32)
                } else {
                    jmax_utf8::validate_and_count_scalars(run, offset)
                };
                let remaining = max_string_bytes.saturating_sub(lengths.byte_len) as usize;
                let arena_remaining = arena.limit().saturating_sub(arena.len() as u32) as usize;
                let first_limit = remaining.min(arena_remaining);
                if run_len > first_limit {
                    // Match scalar-by-scalar error ordering: malformed UTF-8
                    // in/before the first excess scalar takes precedence over
                    // its decoded-length limit, but later errors do not.
                    let mut scalar_start = first_limit;
                    while scalar_start != 0 && run[scalar_start] & 0xc0 == 0x80 {
                        scalar_start -= 1;
                    }
                    if let Err(error) = &validated {
                        if error.byte_offset <= offset + scalar_start as u32 {
                            return Err(error.clone());
                        }
                    }
                    if remaining <= arena_remaining {
                        return Err(string_byte_limit(offset + scalar_start as u32));
                    }
                    return Err(Error::new(
                        ErrorCode::DecodedArenaOverflow,
                        ErrorSource::Decode,
                        offset + arena_remaining as u32,
                        "decoded arena length exceeds configured or ABI limit",
                    ));
                }
                let scalars = validated?;
                arena.append(run).map_err(|mut error| {
                    error.byte_offset = offset;
                    error
                })?;
                lengths.byte_len += run_len as u32;
                lengths.scalar_len += scalars;
                cursor += run_len;
                continue;
            }
            match tail[0] {
                BACKSLASH => {
                    let decoded = decode_tail(tail, offset, end)?;
                    let mut encoded = [0u8; 4];
                    let bytes = decoded.scalar.encode_utf8(&mut encoded).as_bytes();
                    let byte_len = lengths
                        .byte_len
                        .checked_add(bytes.len() as u32)
                        .ok_or_else(|| string_byte_limit(offset))?;
                    if byte_len > max_string_bytes {
                        return Err(string_byte_limit(offset));
                    }
                    arena.append(bytes).map_err(|mut error| {
                        error.byte_offset = offset;
                        error
                    })?;
                    lengths.byte_len = byte_len;
                    // Every scalar consumes at least one raw byte; checked raw
                    // length guarantees that this sum cannot exceed u32::MAX.
                    lengths.scalar_len += 1;
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
                _ => {
                    return Err(Error::new(
                        ErrorCode::ControlCharInString,
                        ErrorSource::Decode,
                        offset,
                        "unescaped control byte in string content",
                    ));
                }
            }
        }
        Ok(jmax_abi::span::StrRef {
            span: Span32::new(checkpoint, lengths.byte_len),
            source: jmax_abi::span::SpanSource::DecodedArena,
            scalar_len: lengths.scalar_len,
        })
    })();
    if result.is_err() {
        // Only appends occur above, so checkpoint always remains a valid
        // truncation point. Propagate an invariant violation rather than panic.
        arena.truncate(checkpoint)?;
    }
    result
}

/// Compatibility name for [`cook_into`].
pub fn decode_escaped_slice_into(
    raw_content: &[u8],
    raw_content_offset: u32,
    arena: &mut jmax_buffer::DecodedArena,
    max_string_bytes: u32,
) -> Result<jmax_abi::span::StrRef> {
    cook_into(raw_content, raw_content_offset, arena, max_string_bytes)
}

fn string_byte_limit(offset: u32) -> Error {
    Error::new(
        ErrorCode::ConstraintViolation,
        ErrorSource::Decode,
        offset,
        "decoded string byte limit exceeded",
    )
}
