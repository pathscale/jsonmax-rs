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

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingSink {
        bytes: [u8; 512],
        used: usize,
        reserves: usize,
        reserved: usize,
        writes: usize,
        fail_reserve: bool,
        fail_write: Option<usize>,
    }

    fn sink_error() -> Error {
        Error::new(ErrorCode::BudgetExhausted, ErrorSource::Wire, 123, "sink failure")
            .with_schema(456, 789)
    }

    impl RecordingSink {
        fn new(prefix: &[u8]) -> Self {
            let mut sink = Self {
                bytes: [0; 512], used: prefix.len(), reserves: 0, reserved: 0,
                writes: 0, fail_reserve: false, fail_write: None,
            };
            sink.bytes[..prefix.len()].copy_from_slice(prefix);
            sink
        }

        fn output(&self) -> &[u8] {
            &self.bytes[..self.used]
        }
    }

    impl ByteSink for RecordingSink {
        fn reserve(&mut self, additional: usize) -> Result<()> {
            self.reserves += 1;
            self.reserved = additional;
            if self.fail_reserve || additional > self.bytes.len() - self.used {
                return Err(sink_error());
            }
            Ok(())
        }

        fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
            self.writes += 1;
            if self.fail_write == Some(self.writes) || bytes.len() > self.bytes.len() - self.used {
                return Err(sink_error());
            }
            self.bytes[self.used..self.used + bytes.len()].copy_from_slice(bytes);
            self.used += bytes.len();
            Ok(())
        }

        fn len(&self) -> usize {
            self.used
        }
    }

    fn assert_error(error: Error, code: ErrorCode, source: ErrorSource, offset: u32) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, source);
        assert_eq!(error.byte_offset, offset);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    fn decode_error(input: &[u8], offset: u32, code: ErrorCode, expected: u32) {
        assert_error(decode_escape(input, offset).unwrap_err(), code, ErrorSource::Decode, expected);
    }

    fn content_error(input: &[u8], span: Span32, code: ErrorCode, source: ErrorSource, offset: u32) {
        let mut sink = RecordingSink::new(b"existing");
        // Invalid input must precede even an otherwise failing reservation.
        sink.fail_reserve = true;
        assert_error(unescape(input, span, &mut sink).unwrap_err(), code, source, offset);
        assert_eq!(sink.output(), b"existing");
        assert_eq!((sink.reserves, sink.writes), (0, 0));
    }

    fn unicode_escape(unit: u16) -> [u8; 6] {
        let hex = b"0123456789abcdef";
        [BACKSLASH, b'u', hex[usize::from(unit >> 12)],
            hex[usize::from((unit >> 8) & 15)], hex[usize::from((unit >> 4) & 15)],
            hex[usize::from(unit & 15)]]
    }

    fn assert_pair(high: u16, low: u16) {
        let mut input = [0; 12];
        input[..6].copy_from_slice(&unicode_escape(high));
        input[6..].copy_from_slice(&unicode_escape(low));
        let value = 0x10000 + ((u32::from(high) - 0xd800) << 10) + u32::from(low) - 0xdc00;
        assert_eq!(decode_escape(&input, 0).unwrap(), DecodedEscape {
            scalar: char::from_u32(value).unwrap(), consumed: 12,
        });
    }

    #[test]
    fn all_simple_escapes_decode_exactly_one_scalar() {
        for (letter, scalar) in [
            (b'"', '"'), (b'\\', '\\'), (b'/', '/'), (b'b', '\u{8}'),
            (b'f', '\u{c}'), (b'n', '\n'), (b'r', '\r'), (b't', '\t'),
        ] {
            let input = [0xff, BACKSLASH, letter, 0xff];
            assert_eq!(decode_escape(&input, 1).unwrap(), DecodedEscape { scalar, consumed: 2 });
        }
    }

    #[test]
    fn every_bmp_code_unit_is_decoded_or_rejected_as_a_surrogate() {
        for unit in 0u16..=u16::MAX {
            let input = unicode_escape(unit);
            if (0xd800..=0xdfff).contains(&unit) {
                decode_error(&input, 0, ErrorCode::LoneSurrogate, 0);
            } else {
                assert_eq!(decode_escape(&input, 0).unwrap(), DecodedEscape {
                    scalar: char::from_u32(u32::from(unit)).unwrap(), consumed: 6,
                });
            }
        }
    }

    #[test]
    fn mixed_case_hex_and_scalar_width_boundaries_are_accepted() {
        for (input, scalar) in [
            (b"\\u0000".as_slice(), '\0'), (b"\\u007F", '\u{7f}'),
            (b"\\u0080", '\u{80}'), (b"\\u07fF", '\u{7ff}'),
            (b"\\u0800", '\u{800}'), (b"\\uD7fF", '\u{d7ff}'),
            (b"\\uE000", '\u{e000}'), (b"\\uFfFf", '\u{ffff}'),
            (b"\\uD800\\udC00", '\u{10000}'), (b"\\uDBff\\uDfFF", '\u{10ffff}'),
            (b"\\ud83D\\uDE42", '🙂'),
        ] {
            assert_eq!(decode_escape(input, 0).unwrap().scalar, scalar);
        }
    }

    #[test]
    fn every_surrogate_half_combines_at_both_partner_boundaries() {
        for high in 0xd800..=0xdbff {
            assert_pair(high, 0xdc00);
            assert_pair(high, 0xdfff);
        }
        for low in 0xdc00..=0xdfff {
            assert_pair(0xd800, low);
            assert_pair(0xdbff, low);
        }
    }

    #[test]
    fn offset_and_escape_letter_errors_are_precise() {
        for offset in [0, 1, u32::MAX] {
            decode_error(b"", offset, ErrorCode::UnexpectedEof, 0);
        }
        for offset in [2, 3, u32::MAX] {
            decode_error(b"\\n", offset, ErrorCode::UnexpectedEof, 2);
        }
        decode_error(b"x", 0, ErrorCode::InvalidEscape, 0);
        decode_error(b"xx", 1, ErrorCode::InvalidEscape, 1);
        for letter in 0u8..=255 {
            if !b"\"\\/bfnrtu".contains(&letter) {
                decode_error(&[b'x', BACKSLASH, letter], 1, ErrorCode::InvalidEscape, 2);
            }
        }
    }

    #[test]
    fn truncation_distinguishes_missing_pair_prefix_from_missing_digits() {
        let input = b"xx\\uD800\\uDC00";
        for len in 3..input.len() {
            let code = if len == 8 || len == 9 {
                ErrorCode::LoneSurrogate
            } else {
                ErrorCode::UnexpectedEof
            };
            let offset = if code == ErrorCode::LoneSurrogate { 2 } else { len as u32 };
            decode_error(&input[..len], 2, code, offset);
        }
        assert_eq!(decode_escape(input, 2).unwrap().consumed, 12);
        for suffix in [b"".as_slice(), b"\\", b"x", b"\\n", b"\\Udc00", b" \\uDC00"] {
            let mut bytes = [0; 32];
            bytes[..6].copy_from_slice(b"\\uD800");
            bytes[6..6 + suffix.len()].copy_from_slice(suffix);
            decode_error(&bytes[..6 + suffix.len()], 0, ErrorCode::LoneSurrogate, 0);
        }
    }

    #[test]
    fn invalid_hex_reports_each_offending_digit_before_later_truncation() {
        for start in [0, 6] {
            for digit in 2..6 {
                for invalid in [b'g', b'G', b'/', b':', b' ', 0, 0xff] {
                    let mut input = *b"\\uD800\\uDC00";
                    input[start + digit] = invalid;
                    decode_error(&input, 0, ErrorCode::InvalidEscape, (start + digit) as u32);
                    decode_error(&input[..start + digit + 1], 0,
                        ErrorCode::InvalidEscape, (start + digit) as u32);
                }
            }
        }
    }

    #[test]
    fn wrong_second_units_report_the_second_backslash() {
        for low in [0, 0x41, 0xd7ff, 0xd800, 0xdbff, 0xe000, 0xffff] {
            let mut input = *b"xx\\uD800\\uDC00";
            input[8..].copy_from_slice(&unicode_escape(low));
            decode_error(&input, 2, ErrorCode::LoneSurrogate, 8);
        }
        decode_error(b"xx\\uDC00\\uD800", 2, ErrorCode::LoneSurrogate, 2);
    }

    #[test]
    fn unescape_preserves_raw_utf8_and_measures_appended_scalars() {
        for text in ["", "ASCII /\u{7f}", "é", "e\u{301}", "中文", "🙂", "\u{feff}", "\u{10ffff}"] {
            let mut sink = RecordingSink::new(b"prefix:");
            let lengths = unescape(text.as_bytes(), Span32::new(0, text.len() as u32), &mut sink).unwrap();
            assert_eq!(lengths, StringLengths::new(text.len() as u32, text.chars().count() as u32));
            assert_eq!(&sink.output()[7..], text.as_bytes());
            assert_eq!(&sink.output()[..7], b"prefix:");
            assert_eq!((sink.reserves, sink.reserved), (1, text.len()));
            assert_eq!(sink.writes, usize::from(!text.is_empty()));
        }
    }

    #[test]
    fn mixed_escapes_and_embedded_nul_have_exact_output_and_reservation() {
        let input = "xx\"é\\n\\u0000\\uD83D\\uDE42\\\"\\\\\\/\\b\\f\\r\\t中\"tail";
        let expected = "é\n\0🙂\"\\/\u{8}\u{c}\r\t中";
        let content = Span32::new(3, (input.len() - 8) as u32);
        let mut sink = RecordingSink::new(b"prior");
        let lengths = unescape(input.as_bytes(), content, &mut sink).unwrap();
        assert_eq!(lengths, StringLengths::new(expected.len() as u32, expected.chars().count() as u32));
        assert_eq!(&sink.output()[5..], expected.as_bytes());
        assert_eq!((sink.reserves, sink.reserved), (1, expected.len()));
    }

    #[test]
    fn raw_runs_and_escape_tails_work_at_every_short_boundary() {
        let mut input = [b'a'; 160];
        for start in 0..=16 {
            for run_len in 0..=128 {
                input.fill(b'a');
                let escape = start + run_len;
                input[escape..escape + 6].copy_from_slice(b"\\u20ac");
                // A quote immediately outside the span must not be emitted.
                input[escape + 6] = QUOTE;
                let mut sink = RecordingSink::new(b"");
                let lengths = unescape(&input,
                    Span32::new(start as u32, (run_len + 6) as u32), &mut sink).unwrap();
                assert_eq!(lengths, StringLengths::new((run_len + 3) as u32, (run_len + 1) as u32));
                assert_eq!(&sink.output()[..run_len], &input[start..escape]);
                assert_eq!(&sink.output()[run_len..], "€".as_bytes());
                assert_eq!((sink.reserves, sink.reserved), (1, run_len + 3));
            }
        }
    }

    #[test]
    fn scan_spans_decode_without_quotes_and_still_require_strict_validation() {
        let input = b"xx\"a\\\"\\\\\\uD800\\uDC00\"tail";
        let scan = crate::scan::scan_string(input, 2).unwrap();
        let mut sink = RecordingSink::new(b"");
        assert_eq!(unescape(input, scan.content, &mut sink).unwrap(), StringLengths::new(7, 4));
        assert_eq!(sink.output(), "a\"\\\u{10000}".as_bytes());

        for input in [b"\"\\q\"".as_slice(), b"\"\\uD800\"", b"\"\\uDC00\""] {
            let scan = crate::scan::scan_string(input, 0).unwrap();
            let mut sink = RecordingSink::new(b"prefix");
            assert!(unescape(input, scan.content, &mut sink).is_err());
            assert_eq!(sink.output(), b"prefix");
            assert_eq!((sink.reserves, sink.writes), (0, 0));
        }
    }

    #[test]
    fn span_validation_precedes_sink_calls() {
        for (span, code) in [
            (Span32::new(u32::MAX, 1), ErrorCode::PayloadTooLarge),
            (Span32::new(1, u32::MAX), ErrorCode::PayloadTooLarge),
            (Span32::new(4, 0), ErrorCode::UnexpectedEof),
            (Span32::new(2, 2), ErrorCode::UnexpectedEof),
            (Span32::new(u32::MAX, 0), ErrorCode::UnexpectedEof),
        ] {
            content_error(b"abc", span, code, ErrorSource::Decode, span.offset);
        }
        let mut sink = RecordingSink::new(b"prefix");
        assert_eq!(unescape(b"abc", Span32::new(3, 0), &mut sink).unwrap(), StringLengths::default());
        assert_eq!(sink.output(), b"prefix");
        assert_eq!((sink.reserves, sink.reserved, sink.writes), (1, 0, 0));
    }

    #[test]
    fn lookahead_never_borrows_bytes_outside_content() {
        let input = b"xx\\uD800\\uDC00tail";
        for len in 1..12 {
            let code = if len == 6 || len == 7 {
                ErrorCode::LoneSurrogate
            } else {
                ErrorCode::UnexpectedEof
            };
            let offset = if code == ErrorCode::LoneSurrogate { 2 } else { 2 + len };
            content_error(input, Span32::new(2, len), code, ErrorSource::Decode, offset);
        }
        content_error(b"xx\\ntail", Span32::new(2, 1),
            ErrorCode::UnexpectedEof, ErrorSource::Decode, 3);
        // Invalid surroundings, including the rest of a UTF-8 sequence, are
        // outside the span and can neither invalidate nor complete its bytes.
        let mut sink = RecordingSink::new(b"");
        assert_eq!(unescape(&[0xff, b'a', 0xff], Span32::new(1, 1), &mut sink).unwrap(),
            StringLengths::new(1, 1));
        assert_eq!(sink.output(), b"a");
        content_error(&[b'x', 0xc3, 0xa9], Span32::new(1, 1),
            ErrorCode::InvalidUtf8, ErrorSource::Utf8, 1);
    }

    #[test]
    fn raw_quotes_and_every_raw_control_are_rejected() {
        content_error(b"xxok\"tail", Span32::new(2, 7),
            ErrorCode::InvalidSyntax, ErrorSource::Decode, 4);
        for control in 0..0x20 {
            content_error(&[b'x', b'a', control, b'z'], Span32::new(1, 3),
                ErrorCode::ControlCharInString, ErrorSource::Decode, 2);
            content_error(&[b'x', BACKSLASH, control], Span32::new(1, 2),
                ErrorCode::InvalidEscape, ErrorSource::Decode, 2);
        }
    }

    #[test]
    fn malformed_utf8_is_rejected_without_sink_side_effects() {
        let malformed: &[&[u8]] = &[
            &[0x80], &[0xbf], &[0xc0, 0x80], &[0xc1, 0xbf], &[0xc2],
            &[0xc2, b'a'], &[0xe0, 0x80, 0x80], &[0xe2, 0x82],
            &[0xe2, b'a', 0xa1], &[0xed, 0xa0, 0x80],
            &[0xf0, 0x80, 0x80, 0x80], &[0xf0, 0x9f, 0x99],
            &[0xf4, 0x90, 0x80, 0x80], &[0xf5, 0x80, 0x80, 0x80], &[0xff],
        ];
        for &bad in malformed {
            let mut input = [b'a'; 16];
            input[3..3 + bad.len()].copy_from_slice(bad);
            content_error(&input[..3 + bad.len()], Span32::new(1, (2 + bad.len()) as u32),
                ErrorCode::InvalidUtf8, ErrorSource::Utf8, 3);
        }
    }

    #[test]
    fn errors_follow_source_order_even_after_a_valid_decoded_prefix() {
        for (input, code, source, offset) in [
            (b"\\n\\q".as_slice(), ErrorCode::InvalidEscape, ErrorSource::Decode, 3),
            (b"\\n\\uD800", ErrorCode::LoneSurrogate, ErrorSource::Decode, 2),
            (b"\\n\\uD800\\u0041", ErrorCode::LoneSurrogate, ErrorSource::Decode, 8),
            (b"\\n\\uDC00", ErrorCode::LoneSurrogate, ErrorSource::Decode, 2),
            (b"\\n\\u00", ErrorCode::UnexpectedEof, ErrorSource::Decode, 6),
            (b"\\n\xff\\q", ErrorCode::InvalidUtf8, ErrorSource::Utf8, 2),
            (b"\\q\xff", ErrorCode::InvalidEscape, ErrorSource::Decode, 1),
            (b"\xc3\\n", ErrorCode::InvalidUtf8, ErrorSource::Utf8, 0),
            (b"\xff\"", ErrorCode::InvalidUtf8, ErrorSource::Utf8, 0),
            (b"\"\xff", ErrorCode::InvalidSyntax, ErrorSource::Decode, 0),
            (b"\0\xff", ErrorCode::ControlCharInString, ErrorSource::Decode, 0),
        ] {
            content_error(input, Span32::new(0, input.len() as u32), code, source, offset);
        }
    }

    #[test]
    fn sink_failures_propagate_unchanged_and_stop_emission() {
        let input = b"a\\nb";
        let mut sink = RecordingSink::new(b"prior");
        sink.fail_reserve = true;
        assert_eq!(unescape(input, Span32::new(0, 4), &mut sink).unwrap_err(), sink_error());
        assert_eq!(sink.output(), b"prior");
        assert_eq!((sink.reserves, sink.reserved, sink.writes), (1, 3, 0));
        for (failure, prefix) in [(1, b"prior".as_slice()), (2, b"priora"), (3, b"priora\n")] {
            let mut sink = RecordingSink::new(b"prior");
            sink.fail_write = Some(failure);
            assert_eq!(unescape(input, Span32::new(0, 4), &mut sink).unwrap_err(), sink_error());
            assert_eq!(sink.output(), prefix);
            assert_eq!((sink.reserves, sink.reserved, sink.writes), (1, 3, failure));
        }
    }

    #[test]
    fn counter_boundaries_do_not_wrap_or_partially_update() {
        let mut lengths = StringLengths::new(u32::MAX - 4, u32::MAX - 1);
        add_scalar(&mut lengths, '🙂', 17).unwrap();
        assert_eq!(lengths, StringLengths::new(u32::MAX, u32::MAX));
        for initial in [
            StringLengths::new(u32::MAX, 0),
            StringLengths::new(u32::MAX - 3, 0),
            StringLengths::new(0, u32::MAX),
            StringLengths::new(u32::MAX, u32::MAX),
        ] {
            let mut lengths = initial;
            assert_error(add_scalar(&mut lengths, '🙂', u32::MAX - 1).unwrap_err(),
                ErrorCode::DecodedArenaOverflow, ErrorSource::Decode, u32::MAX - 1);
            assert_eq!(lengths, initial);
        }
    }
}
