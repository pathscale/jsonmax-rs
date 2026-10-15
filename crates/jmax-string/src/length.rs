//! Allocation-free decoded UTF-8 byte and Unicode scalar measurement.
//!
//! JSON content is validated within its checked source span. Measurement never
//! retains decoded output, normalizes text, or counts UTF-16 code units as scalars.

use super::{
    content_bytes, Error, ErrorCode, ErrorSource, Result, Span32, StringLengths,
    BACKSLASH, QUOTE,
};
use super::scan::first_special;
use super::unescape::decode_escape;

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
            QUOTE => return Err(Error::new(
                ErrorCode::InvalidSyntax, ErrorSource::Decode, offset,
                "unescaped double quote in string content",
            )),
            0..=0x1f => return Err(Error::new(
                ErrorCode::ControlCharInString, ErrorSource::Decode, offset,
                "unescaped control byte in string content",
            )),
            _ => {
                let tail = &bytes[cursor..];
                let run_len = first_special(tail).unwrap_or(tail.len());
                // As in unescape, validate a complete raw run before measuring
                // its scalars. A split sequence fails at its leading byte.
                let text = core::str::from_utf8(&tail[..run_len]).map_err(|error| {
                    Error::new(
                        ErrorCode::InvalidUtf8, ErrorSource::Utf8,
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
    let scalar_len = checked_text_len(text.chars().count())?;
    Ok(StringLengths::new(byte_len, scalar_len))
}

fn checked_text_len(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| Error::new(
        ErrorCode::PayloadTooLarge, ErrorSource::Decode, u32::MAX,
        "string input exceeds the 32-bit ABI length limit",
    ))
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
            ErrorCode::DecodedArenaOverflow, ErrorSource::Decode, offset,
            "decoded string length exceeds the 32-bit ABI limit",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ByteSink;
    use crate::unescape::unescape;

    // All differential vectors fit this stack buffer. Existing bytes ensure
    // that unescape's append measurement is independent of the sink's length.
    struct RecordingSink {
        bytes: [u8; 256],
        used: usize,
        reserved: usize,
        reserve_calls: usize,
        write_calls: usize,
    }

    impl RecordingSink {
        fn new() -> Self {
            let mut bytes = [0; 256];
            bytes[..6].copy_from_slice(b"prior:");
            Self { bytes, used: 6, reserved: 0, reserve_calls: 0, write_calls: 0 }
        }
    }

    impl ByteSink for RecordingSink {
        fn reserve(&mut self, additional: usize) -> Result<()> {
            assert!(additional <= self.bytes.len() - self.used);
            self.reserved = additional;
            self.reserve_calls += 1;
            Ok(())
        }

        fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
            assert!(bytes.len() <= self.bytes.len() - self.used);
            self.bytes[self.used..self.used + bytes.len()].copy_from_slice(bytes);
            self.used += bytes.len();
            self.write_calls += 1;
            Ok(())
        }

        fn len(&self) -> usize {
            self.used
        }
    }

    fn full_span(input: &[u8]) -> Span32 {
        Span32::new(0, u32::try_from(input.len()).unwrap())
    }

    fn assert_measurement(input: &[u8], span: Span32, expected: &str) {
        let lengths = StringLengths::new(
            u32::try_from(expected.len()).unwrap(),
            u32::try_from(expected.chars().count()).unwrap(),
        );
        assert_eq!(decoded_length(input, span).unwrap(), lengths, "input: {input:?}, span: {span:?}");
        assert_eq!(utf8_length(expected).unwrap(), lengths);
        let mut sink = RecordingSink::new();
        assert_eq!(unescape(input, span, &mut sink).unwrap(), lengths);
        assert_eq!(&sink.bytes[..6], b"prior:");
        assert_eq!(&sink.bytes[6..sink.used], expected.as_bytes());
        assert_eq!((sink.reserve_calls, sink.reserved), (1, expected.len()));
    }

    fn assert_error_metadata(error: &Error, code: ErrorCode, source: ErrorSource, offset: u32) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, source);
        assert_eq!(error.byte_offset, offset);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    fn assert_content_error(
        input: &[u8], span: Span32, code: ErrorCode, source: ErrorSource, offset: u32,
    ) {
        let error = decoded_length(input, span).unwrap_err();
        assert_error_metadata(&error, code, source, offset);
        let mut sink = RecordingSink::new();
        assert_eq!(unescape(input, span, &mut sink).unwrap_err(), error);
        assert_eq!(&sink.bytes[..sink.used], b"prior:");
        assert_eq!((sink.reserve_calls, sink.write_calls), (0, 0));
    }

    fn unicode_escape(unit: u16) -> [u8; 6] {
        let hex = b"0123456789abcdef";
        [BACKSLASH, b'u', hex[usize::from(unit >> 12)],
            hex[usize::from((unit >> 8) & 15)], hex[usize::from((unit >> 4) & 15)],
            hex[usize::from(unit & 15)]]
    }

    #[test]
    fn utf8_counts_bytes_and_scalars_without_normalization() {
        for (text, bytes, scalars) in [
            ("", 0, 0), ("ASCII", 5, 5), ("é", 2, 1), ("e\u{301}", 3, 2),
            ("中文", 6, 2), ("🙂", 4, 1), ("🇰🇭", 8, 2),
            ("👩\u{200d}💻", 11, 3), ("✈\u{fe0f}", 6, 2),
            ("\u{feff}", 3, 1), ("\u{10ffff}", 4, 1),
        ] {
            assert_eq!(utf8_length(text).unwrap(), StringLengths::new(bytes, scalars));
            assert_measurement(text.as_bytes(), full_span(text.as_bytes()), text);
        }
    }

    #[test]
    fn utf8_length_accepts_controls_quotes_and_backslashes_as_plain_text() {
        let text = "\0\u{1f}\"\\\n\r\t\u{7f}";
        assert_eq!(utf8_length(text).unwrap(), StringLengths::new(8, 8));
        for byte in 0u8..=0x1f {
            let bytes = [byte];
            let text = core::str::from_utf8(&bytes).unwrap();
            assert_eq!(utf8_length(text).unwrap(), StringLengths::new(1, 1));
            assert_content_error(&bytes, full_span(&bytes), ErrorCode::ControlCharInString,
                ErrorSource::Decode, 0);
        }
        assert_content_error(b"\"", Span32::new(0, 1), ErrorCode::InvalidSyntax,
            ErrorSource::Decode, 0);
        assert_content_error(b"\\", Span32::new(0, 1), ErrorCode::UnexpectedEof,
            ErrorSource::Decode, 1);
    }

    #[test]
    fn every_simple_escape_has_one_decoded_byte_and_scalar() {
        for (letter, scalar) in [
            (b'"', '"'), (b'\\', '\\'), (b'/', '/'), (b'b', '\u{8}'),
            (b'f', '\u{c}'), (b'n', '\n'), (b'r', '\r'), (b't', '\t'),
        ] {
            let input = [0xff, BACKSLASH, letter, 0xff];
            let mut encoded = [0; 4];
            assert_measurement(&input, Span32::new(1, 2), scalar.encode_utf8(&mut encoded));
            assert_eq!(decoded_length(&input, Span32::new(1, 2)).unwrap(), StringLengths::new(1, 1));
        }
    }

    #[test]
    fn unicode_escape_width_boundaries_and_hex_case_are_exact() {
        for (input, scalar) in [
            (b"\\u0000".as_slice(), '\0'), (b"\\u007f", '\u{7f}'),
            (b"\\u0080", '\u{80}'), (b"\\u07fF", '\u{7ff}'),
            (b"\\u0800", '\u{800}'), (b"\\uD7fF", '\u{d7ff}'),
            (b"\\uE000", '\u{e000}'), (b"\\uFfFf", '\u{ffff}'),
            (b"\\uD800\\udC00", '\u{10000}'),
            (b"\\uDBff\\uDfFF", '\u{10ffff}'), (b"\\ud83D\\uDE42", '🙂'),
        ] {
            let mut encoded = [0; 4];
            assert_measurement(input, full_span(input), scalar.encode_utf8(&mut encoded));
        }
    }

    #[test]
    fn every_bmp_unit_is_measured_or_rejected_as_an_isolated_surrogate() {
        for unit in 0u16..=u16::MAX {
            let input = unicode_escape(unit);
            if (0xd800..=0xdfff).contains(&unit) {
                assert_content_error(&input, full_span(&input), ErrorCode::LoneSurrogate,
                    ErrorSource::Decode, 0);
            } else {
                let scalar = char::from_u32(u32::from(unit)).unwrap();
                let mut encoded = [0; 4];
                assert_measurement(&input, full_span(&input), scalar.encode_utf8(&mut encoded));
            }
        }
    }

    #[test]
    fn every_surrogate_half_pairs_with_both_partner_boundaries() {
        fn assert_pair(high: u16, low: u16) {
            let mut input = [0xff; 14];
            input[1..7].copy_from_slice(&unicode_escape(high));
            input[7..13].copy_from_slice(&unicode_escape(low));
            let value = 0x10000 + ((u32::from(high) - 0xd800) << 10)
                + (u32::from(low) - 0xdc00);
            let scalar = char::from_u32(value).unwrap();
            let mut encoded = [0; 4];
            assert_measurement(&input, Span32::new(1, 12), scalar.encode_utf8(&mut encoded));
            assert_eq!(decoded_length(&input, Span32::new(1, 12)).unwrap(), StringLengths::new(4, 1));
        }
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
    fn mixed_raw_and_escaped_content_uses_decoded_widths() {
        let input = "xx\"é\\n\\u0000\\uD83D\\uDE42\\\"\\\\\\/\\b\\f\\r\\t中\"tail";
        let span = Span32::new(3, (input.len() - 8) as u32);
        assert_measurement(input.as_bytes(), span, "é\n\0🙂\"\\/\u{8}\u{c}\r\t中");
        assert_eq!(decoded_length(input.as_bytes(), span).unwrap(), StringLengths::new(18, 12));
        for (input, expected) in [
            (b"\\u0065\\u0301".as_slice(), "e\u{301}"),
            (b"\\u00e9", "é"), (b"\\\\u0041", "\\u0041"),
            (b"\\u0000a\\u0000", "\0a\0"),
        ] {
            assert_measurement(input, full_span(input), expected);
        }
    }

    #[test]
    fn raw_runs_and_escaped_tails_work_at_short_alignment_boundaries() {
        let mut input = [b'a'; 160];
        for start in 0..=16 {
            for run_len in 0..=128 {
                input.fill(b'a');
                let escape = start + run_len;
                input[escape..escape + 6].copy_from_slice(b"\\u20ac");
                input[escape + 6] = QUOTE;
                let span = Span32::new(start as u32, (run_len + 6) as u32);
                let lengths = decoded_length(&input, span).unwrap();
                assert_eq!(lengths, StringLengths::new((run_len + 3) as u32, (run_len + 1) as u32));
                let mut sink = RecordingSink::new();
                assert_eq!(unescape(&input, span, &mut sink).unwrap(), lengths);
                assert_eq!(&sink.bytes[6..6 + run_len], &input[start..escape]);
                assert_eq!(&sink.bytes[6 + run_len..sink.used], "€".as_bytes());
            }
        }
    }

    #[test]
    fn empty_and_nonzero_spans_ignore_invalid_surroundings() {
        assert_measurement(b"", Span32::new(0, 0), "");
        for offset in 0..=3 {
            assert_measurement(&[0xff, 0, QUOTE], Span32::new(offset, 0), "");
        }
        assert_measurement(&[0xff, b'a', 0, QUOTE], Span32::new(1, 1), "a");
        assert_measurement(&[0xff, 0xc3, 0xa9, 0xff], Span32::new(1, 2), "é");
    }

    #[test]
    fn invalid_spans_report_checked_arithmetic_and_bounds_errors_first() {
        for (span, code) in [
            (Span32::new(u32::MAX, 1), ErrorCode::PayloadTooLarge),
            (Span32::new(1, u32::MAX), ErrorCode::PayloadTooLarge),
            (Span32::new(u32::MAX - 1, 2), ErrorCode::PayloadTooLarge),
            (Span32::new(4, 0), ErrorCode::UnexpectedEof),
            (Span32::new(2, 2), ErrorCode::UnexpectedEof),
            (Span32::new(0, 4), ErrorCode::UnexpectedEof),
            (Span32::new(u32::MAX, 0), ErrorCode::UnexpectedEof),
            (Span32::new(u32::MAX - 1, 1), ErrorCode::UnexpectedEof),
        ] {
            assert_content_error(b"\"\0\\", span, code, ErrorSource::Decode, span.offset);
        }
    }

    #[test]
    fn escapes_and_surrogate_pairs_cannot_borrow_past_the_span_end() {
        let input = b"xx\\uD800\\uDC00tail";
        for len in 1..12 {
            let code = if len == 6 || len == 7 {
                ErrorCode::LoneSurrogate
            } else {
                ErrorCode::UnexpectedEof
            };
            let offset = if code == ErrorCode::LoneSurrogate { 2 } else { 2 + len };
            assert_content_error(input, Span32::new(2, len), code, ErrorSource::Decode, offset);
        }
        assert_measurement(input, Span32::new(2, 12), "\u{10000}");
        for spelling in [b"\\n".as_slice(), b"\\u0041"] {
            let mut input = [b'x'; 10];
            input[2..2 + spelling.len()].copy_from_slice(spelling);
            for len in 1..spelling.len() {
                assert_content_error(&input, Span32::new(2, len as u32),
                    ErrorCode::UnexpectedEof, ErrorSource::Decode, 2 + len as u32);
            }
        }
    }

    #[test]
    fn raw_utf8_sequences_cannot_borrow_past_either_span_boundary() {
        for text in ["é", "€", "🙂", "\u{10ffff}"] {
            let mut input = [0xff; 8];
            let bytes = text.as_bytes();
            input[2..2 + bytes.len()].copy_from_slice(bytes);
            assert_measurement(&input, Span32::new(2, bytes.len() as u32), text);
            for len in 1..bytes.len() {
                assert_content_error(&input, Span32::new(2, len as u32),
                    ErrorCode::InvalidUtf8, ErrorSource::Utf8, 2);
                assert_content_error(&input, Span32::new((2 + len) as u32, (bytes.len() - len) as u32),
                    ErrorCode::InvalidUtf8, ErrorSource::Utf8, (2 + len) as u32);
            }
        }
    }

    #[test]
    fn every_raw_byte_is_classified_strictly() {
        for byte in 0u8..=255 {
            let input = [b'x', b'a', byte, b'z'];
            let span = Span32::new(1, 3);
            match byte {
                0..=0x1f => assert_content_error(&input, span,
                    ErrorCode::ControlCharInString, ErrorSource::Decode, 2),
                QUOTE => assert_content_error(&input, span,
                    ErrorCode::InvalidSyntax, ErrorSource::Decode, 2),
                BACKSLASH => assert_content_error(&input, span,
                    ErrorCode::InvalidEscape, ErrorSource::Decode, 3),
                0x80..=0xff => assert_content_error(&input, span,
                    ErrorCode::InvalidUtf8, ErrorSource::Utf8, 2),
                _ => {
                    let expected = core::str::from_utf8(&input[1..]).unwrap();
                    assert_measurement(&input, span, expected);
                }
            }
        }
    }

    #[test]
    fn malformed_utf8_reports_the_first_leading_byte() {
        let malformed: &[&[u8]] = &[
            &[0x80], &[0xbf], &[0xc0, 0x80], &[0xc1, 0xbf], &[0xc2],
            &[0xc2, b'a'], &[0xe0, 0x80, 0x80], &[0xe2, 0x82],
            &[0xe2, b'a', 0xa1], &[0xed, 0xa0, 0x80], &[0xed, 0xbf, 0xbf],
            &[0xf0, 0x80, 0x80, 0x80], &[0xf0, 0x9f, 0x99],
            &[0xf4, 0x90, 0x80, 0x80], &[0xf5, 0x80, 0x80, 0x80], &[0xff],
        ];
        for &bad in malformed {
            let mut input = [b'a'; 16];
            input[3..3 + bad.len()].copy_from_slice(bad);
            assert_content_error(&input, Span32::new(1, (2 + bad.len()) as u32),
                ErrorCode::InvalidUtf8, ErrorSource::Utf8, 3);
        }
    }

    #[test]
    fn unknown_escape_letters_and_bad_hex_keep_absolute_offsets() {
        for letter in 0u8..=255 {
            if !b"\"\\/bfnrtu".contains(&letter) {
                let input = [0xff, BACKSLASH, letter, 0xff];
                assert_content_error(&input, Span32::new(1, 2),
                    ErrorCode::InvalidEscape, ErrorSource::Decode, 2);
            }
        }
        for start in [0, 6] {
            for digit in 2..6 {
                for invalid in [b'g', b'G', b'/', b':', b' ', 0, 0xff] {
                    let mut input = *b"xx\\uD800\\uDC00";
                    let index = 2 + start + digit;
                    input[index] = invalid;
                    for len in [12, index - 1] {
                        assert_content_error(&input, Span32::new(2, len as u32),
                            ErrorCode::InvalidEscape, ErrorSource::Decode, index as u32);
                    }
                }
            }
        }
    }

    #[test]
    fn wrong_or_missing_surrogate_partners_are_rejected_precisely() {
        for unit in [0, 0x41, 0xd7ff, 0xd800, 0xdbff, 0xe000, 0xffff] {
            let mut input = *b"xx\\uD800\\uDC00";
            input[8..].copy_from_slice(&unicode_escape(unit));
            assert_content_error(&input, Span32::new(2, 12),
                ErrorCode::LoneSurrogate, ErrorSource::Decode, 8);
        }
        for suffix in [b"".as_slice(), b"\\", b"x", b"\\n", b"\\Udc00", b" \\uDC00"] {
            let mut input = [b'x'; 24];
            input[2..8].copy_from_slice(b"\\uD800");
            input[8..8 + suffix.len()].copy_from_slice(suffix);
            assert_content_error(&input, Span32::new(2, (6 + suffix.len()) as u32),
                ErrorCode::LoneSurrogate, ErrorSource::Decode, 2);
        }
        assert_content_error(b"xx\\uDC00\\uD800", Span32::new(2, 12),
            ErrorCode::LoneSurrogate, ErrorSource::Decode, 2);
    }

    #[test]
    fn first_error_matches_unescape_after_valid_decoded_prefixes() {
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
            (b"a\\n\"\\q", ErrorCode::InvalidSyntax, ErrorSource::Decode, 3),
        ] {
            assert_content_error(input, full_span(input), code, source, offset);
        }
    }

    #[test]
    fn deterministic_short_inputs_match_unescape_for_every_subspan() {
        let alphabet = [b'a', QUOTE, BACKSLASH, b'n', b'u', 0, 0xff, 0xc3, 0xa9];
        for a in alphabet {
            for b in alphabet {
                for c in alphabet {
                    let input = [a, b, c];
                    for start in 0..=3 {
                        for end in start..=3 {
                            let span = Span32::new(start as u32, (end - start) as u32);
                            let measured = decoded_length(&input, span);
                            let mut sink = RecordingSink::new();
                            let emitted = unescape(&input, span, &mut sink);
                            assert_eq!(measured, emitted, "input: {input:?}, span: {span:?}");
                            if let Ok(lengths) = measured {
                                let text = core::str::from_utf8(&sink.bytes[6..sink.used]).unwrap();
                                assert_eq!(utf8_length(text).unwrap(), lengths);
                            } else {
                                assert_eq!((sink.reserve_calls, sink.write_calls), (0, 0));
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn scanner_spans_exclude_quotes_and_discovery_still_needs_validation() {
        let input = b"xx\"a\\\"\\\\\\uD800\\uDC00\"tail";
        let scan = crate::scan::scan_string(input, 2).unwrap();
        assert_measurement(input, scan.content, "a\"\\\u{10000}");
        assert_eq!(decoded_length(input, scan.content).unwrap(), StringLengths::new(7, 4));
        assert_content_error(input, scan.raw, ErrorCode::InvalidSyntax, ErrorSource::Decode, 2);
        for input in [b"\"\\q\"".as_slice(), b"\"\\uD800\"", b"\"\\uDC00\""] {
            let scan = crate::scan::scan_string(input, 0).unwrap();
            let mut sink = RecordingSink::new();
            assert_eq!(decoded_length(input, scan.content), unescape(input, scan.content, &mut sink));
            assert!(decoded_length(input, scan.content).is_err());
        }
    }

    #[test]
    fn text_size_boundary_is_checked_without_large_allocations() {
        assert_eq!(checked_text_len(0).unwrap(), 0);
        assert_eq!(checked_text_len(1).unwrap(), 1);
        let maximum = u32::MAX as usize;
        assert_eq!(checked_text_len(maximum - 1).unwrap(), u32::MAX - 1);
        assert_eq!(checked_text_len(maximum).unwrap(), u32::MAX);
        if let Some(oversized) = maximum.checked_add(1) {
            for len in [oversized, usize::MAX] {
                let error = checked_text_len(len).unwrap_err();
                assert_error_metadata(&error, ErrorCode::PayloadTooLarge, ErrorSource::Decode, u32::MAX);
            }
        }
    }

    #[test]
    fn counter_boundaries_update_atomically_and_preserve_source_offsets() {
        for scalar in ['\0', '\u{7f}', '\u{80}', '\u{800}', '🙂'] {
            let width = scalar.len_utf8() as u32;
            let mut lengths = StringLengths::new(u32::MAX - width, u32::MAX - 1);
            add_scalar(&mut lengths, scalar, 17).unwrap();
            assert_eq!(lengths, StringLengths::new(u32::MAX, u32::MAX));
            for initial in [
                StringLengths::new(u32::MAX, 0),
                StringLengths::new(u32::MAX - width + 1, 0),
                StringLengths::new(0, u32::MAX),
                StringLengths::new(u32::MAX, u32::MAX),
            ] {
                for offset in [0, 17, u32::MAX - 1] {
                    let mut lengths = initial;
                    let error = add_scalar(&mut lengths, scalar, offset).unwrap_err();
                    assert_error_metadata(&error, ErrorCode::DecodedArenaOverflow,
                        ErrorSource::Decode, offset);
                    assert_eq!(lengths, initial);
                }
            }
        }
    }
}
