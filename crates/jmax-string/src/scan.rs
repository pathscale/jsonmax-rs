//! Allocation-free discovery of JSON string boundaries and escape positions.
//!
//! Discovery does not validate escape spellings. A successful scan establishes
//! token boundaries and raw UTF-8 validity, but callers must still decode or
//! measure the content before accepting its JSON escape syntax.

use super::{Error, ErrorCode, ErrorSource, Result, Span32, StringScan, BACKSLASH, QUOTE};

/// Find the first quote, backslash, or raw control byte below `0x20`.
///
/// Arbitrary byte slices are accepted, including invalid UTF-8 and slices whose
/// lengths exceed the ABI's 32-bit limit. The returned index is slice-relative.
/// All reads are bounded by `bytes`; no padding or scanner carry is required.
#[inline]
pub fn first_special(bytes: &[u8]) -> Option<usize> {
    bytes.iter().position(|&byte| byte == QUOTE || byte == BACKSLASH || byte < 0x20)
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

    while let Some(relative) = first_special(&input[cursor..]) {
        // relative is strictly below the remaining slice's length.
        cursor += relative;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_scan_error(
        input: &[u8],
        opening: u32,
        code: ErrorCode,
        source: ErrorSource,
        offset: u32,
    ) {
        let error = scan_string(input, opening).unwrap_err();
        assert_eq!(error.code, code, "input: {input:?}, opening: {opening}");
        assert_eq!(error.source, source, "input: {input:?}, opening: {opening}");
        assert_eq!(error.byte_offset, offset, "input: {input:?}, opening: {opening}");
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn discovery_accepts_empty_and_ordinary_bytes() {
        for bytes in [
            b"".as_slice(),
            b"hello / world",
            b" !#$%&'()*+,-./0123456789:;<=>?@[]^_`{|}~",
            &[0x7f, 0x80, 0xff],
        ] {
            assert_eq!(first_special(bytes), None, "bytes: {bytes:?}");
        }
    }

    #[test]
    fn discovery_classifies_every_byte_value() {
        for value in 0u16..=255 {
            let byte = value as u8;
            let bytes = [b'a', byte, b'z'];
            let expected = if byte == QUOTE || byte == BACKSLASH || byte < 0x20 {
                Some(1)
            } else {
                None
            };
            assert_eq!(first_special(&bytes), expected, "byte: {byte:#04x}");
        }
    }

    #[test]
    fn discovery_returns_the_earliest_special() {
        assert_eq!(first_special(b"a\"\\\n"), Some(1));
        assert_eq!(first_special(b"ab\\\"\0"), Some(2));
        assert_eq!(first_special(b"abc\x1f\\\""), Some(3));
    }

    #[test]
    fn discovery_respects_subslice_ends_and_alignment() {
        let mut storage = [b'a'; 260];
        for start in 0..=65 {
            for len in 0..=128 {
                storage[start + len] = QUOTE;
                assert_eq!(first_special(&storage[start..start + len]), None);
                assert_eq!(first_special(&storage[start..=start + len]), Some(len));
                storage[start + len] = b'a';
            }
        }
    }

    #[test]
    fn discovery_finds_each_special_at_every_short_tail_position() {
        let mut storage = [b'a'; 130];
        for special in [QUOTE, BACKSLASH, 0, 0x1f] {
            for len in 1..=storage.len() {
                for position in 0..len {
                    storage[position] = special;
                    assert_eq!(first_special(&storage[..len]), Some(position));
                    storage[position] = b'a';
                }
            }
        }
    }

    #[test]
    fn empty_string_has_empty_content_and_inclusive_raw_quotes() {
        assert_eq!(scan_string(b"\"\"", 0).unwrap(), StringScan {
            raw: Span32::new(0, 2),
            content: Span32::new(1, 0),
            next_offset: 2,
            first_escape: None,
        });
    }

    #[test]
    fn offsets_are_absolute_and_trailing_bytes_are_ignored() {
        let input = b"xx\"abc\"\xff\0\\\"";
        let scan = scan_string(input, 2).unwrap();
        assert_eq!(scan.raw, Span32::new(2, 5));
        assert_eq!(scan.content, Span32::new(3, 3));
        assert_eq!(scan.next_offset, 7);
        assert_eq!(scan.first_escape, None);
        assert_eq!(scan.raw.end(), Some(scan.next_offset));
        assert_eq!(scan.raw.slice(input), Some(b"\"abc\"".as_slice()));
        assert_eq!(scan.content.slice(input), Some(b"abc".as_slice()));
    }

    #[test]
    fn opening_validation_preserves_requested_offset() {
        for (input, opening) in [
            (b"".as_slice(), 0),
            (b"\"\"".as_slice(), 2),
            (b"\"\"".as_slice(), 3),
            (b"\"\"".as_slice(), u32::MAX),
        ] {
            assert_scan_error(input, opening, ErrorCode::UnexpectedEof, ErrorSource::Scanner, opening);
        }
        for input in [b"x".as_slice(), b"\\", b"\0", &[0xff]] {
            assert_scan_error(input, 0, ErrorCode::InvalidToken, ErrorSource::Scanner, 0);
        }
        assert_scan_error(b"\"x", 1, ErrorCode::InvalidToken, ErrorSource::Scanner, 1);
    }

    #[test]
    fn unclosed_strings_and_dangling_escapes_report_the_opening() {
        for input in [b"\"".as_slice(), b"\"abc", b"\"abc\\", b"\"\\\"", b"\"\\\\"] {
            assert_scan_error(input, 0, ErrorCode::UnclosedString, ErrorSource::Scanner, 0);
        }
        assert_scan_error(b"abc\"tail\\", 3, ErrorCode::UnclosedString, ErrorSource::Scanner, 3);
    }

    #[test]
    fn escaped_quotes_do_not_end_the_token() {
        let input = b"p\"a\\\"b\"tail";
        let scan = scan_string(input, 1).unwrap();
        assert_eq!(scan.raw, Span32::new(1, 6));
        assert_eq!(scan.content, Span32::new(2, 4));
        assert_eq!(scan.next_offset, 7);
        assert_eq!(scan.first_escape, Some(3));
        assert_eq!(scan.content.slice(input), Some(b"a\\\"b".as_slice()));
    }

    #[test]
    fn consecutive_backslash_parity_is_correct_across_boundaries() {
        let mut input = [b'a'; 140];
        for count in 0..=130 {
            input.fill(b'a');
            input[0] = QUOTE;
            input[1..1 + count].fill(BACKSLASH);
            input[1 + count] = QUOTE;
            input[2 + count] = b'x';
            input[3 + count] = QUOTE;
            let bytes = &input[..4 + count];
            let scan = scan_string(bytes, 0).unwrap();
            let expected_end = if count % 2 == 0 { count + 2 } else { count + 4 };
            assert_eq!(scan.next_offset, expected_end as u32, "backslashes: {count}");
            assert_eq!(scan.raw.len, expected_end as u32);
            assert_eq!(scan.content.len, (expected_end - 2) as u32);
            assert_eq!(scan.first_escape, if count == 0 { None } else { Some(1) });
        }
    }

    #[test]
    fn escaped_terminal_quotes_do_not_supply_a_delimiter() {
        let mut input = [BACKSLASH; 132];
        input[0] = QUOTE;
        for count in 0..=129 {
            input[1..1 + count].fill(BACKSLASH);
            input[1 + count] = QUOTE;
            let bytes = &input[..count + 2];
            if count % 2 == 0 {
                assert_eq!(scan_string(bytes, 0).unwrap().next_offset, bytes.len() as u32);
            } else {
                assert_scan_error(bytes, 0, ErrorCode::UnclosedString, ErrorSource::Scanner, 0);
            }
        }
    }

    #[test]
    fn escape_state_resets_after_each_escaped_byte() {
        let input = [QUOTE, BACKSLASH, b'x', QUOTE, b'a', QUOTE];
        assert_eq!(scan_string(&input, 0).unwrap().next_offset, 4);
        let input = [QUOTE, BACKSLASH, BACKSLASH, b'x', QUOTE];
        assert_eq!(scan_string(&input, 0).unwrap().next_offset, 5);
    }

    #[test]
    fn first_escape_is_the_earliest_absolute_backslash() {
        let input = b"xx\"ab\\ncd\\t\"";
        let scan = scan_string(input, 2).unwrap();
        assert_eq!(scan.first_escape, Some(5));
        assert_eq!(scan.next_offset, input.len() as u32);
    }

    #[test]
    fn all_unescaped_controls_are_rejected_at_their_absolute_offset() {
        for control in 0u8..0x20 {
            let input = [b'p', QUOTE, b'a', control, QUOTE];
            assert_scan_error(&input, 1, ErrorCode::ControlCharInString, ErrorSource::Scanner, 3);
            let input = [QUOTE, BACKSLASH, BACKSLASH, control, QUOTE];
            assert_scan_error(&input, 0, ErrorCode::ControlCharInString, ErrorSource::Scanner, 3);
        }
    }

    #[test]
    fn discovery_defers_escape_spelling_and_surrogate_validation() {
        for input in [
            b"\"\\q\"".as_slice(),
            b"\"\\u\"",
            b"\"\\uZZZZ\"",
            b"\"\\uD800\"",
            b"\"\\uDC00\"",
            b"\"\\uD800\\uDC00\"",
            b"\"\\/\\b\\f\\n\\r\\t\"",
        ] {
            let scan = scan_string(input, 0).unwrap();
            assert_eq!(scan.next_offset, input.len() as u32);
            assert_eq!(scan.first_escape, Some(1));
        }
        // An escaped raw control is an invalid escape spelling for the decoder,
        // rather than an unescaped control for this discovery stage.
        for control in 0u8..0x20 {
            let input = [QUOTE, BACKSLASH, control, QUOTE];
            assert_eq!(scan_string(&input, 0).unwrap().next_offset, 4);
        }
    }

    #[test]
    fn valid_utf8_is_preserved_without_normalization() {
        for text in [
            "", "ASCII /\u{7f}", "é", "e\u{301}", "中文", "🙂", "\u{feff}", "\u{10ffff}",
        ] {
            let mut input = [0u8; 64];
            input[0] = QUOTE;
            input[1..1 + text.len()].copy_from_slice(text.as_bytes());
            input[1 + text.len()] = QUOTE;
            let input = &input[..text.len() + 2];
            let scan = scan_string(input, 0).unwrap();
            assert_eq!(scan.content.slice(input), Some(text.as_bytes()));
            assert_eq!(scan.content.len, text.len() as u32);
            assert_eq!(scan.first_escape, None);
        }
    }

    #[test]
    fn malformed_utf8_reports_the_first_sequence_leader() {
        let invalid: &[&[u8]] = &[
            &[0x80], &[0xbf], &[0xc0, 0x80], &[0xc1, 0xbf],
            &[0xc2], &[0xc2, b'a'], &[0xe0, 0x80, 0x80],
            &[0xe2, 0x82], &[0xe2, b'a', 0xa1], &[0xed, 0xa0, 0x80],
            &[0xf0, 0x80, 0x80, 0x80], &[0xf0, 0x9f, 0x99],
            &[0xf4, 0x90, 0x80, 0x80], &[0xf5, 0x80, 0x80, 0x80], &[0xff],
        ];
        for &malformed in invalid {
            let mut input = [b'a'; 16];
            input[2] = QUOTE;
            input[4..4 + malformed.len()].copy_from_slice(malformed);
            input[4 + malformed.len()] = QUOTE;
            assert_scan_error(
                &input[..5 + malformed.len()], 2,
                ErrorCode::InvalidUtf8, ErrorSource::Utf8, 4,
            );
        }
        assert_scan_error(
            &[QUOTE, 0xc3, 0xa9, 0xff, 0x80, QUOTE], 0,
            ErrorCode::InvalidUtf8, ErrorSource::Utf8, 3,
        );
    }

    #[test]
    fn escaped_bytes_still_participate_in_raw_utf8_validation() {
        assert_scan_error(
            &[QUOTE, BACKSLASH, 0xff, QUOTE], 0,
            ErrorCode::InvalidUtf8, ErrorSource::Utf8, 2,
        );
        let input = [QUOTE, BACKSLASH, 0xc3, 0xa9, QUOTE];
        assert_eq!(scan_string(&input, 0).unwrap().first_escape, Some(1));
    }

    #[test]
    fn scanning_errors_precede_post_scan_utf8_errors() {
        assert_scan_error(
            &[QUOTE, 0xff, b'a'], 0,
            ErrorCode::UnclosedString, ErrorSource::Scanner, 0,
        );
        assert_scan_error(
            &[QUOTE, 0xff, BACKSLASH], 0,
            ErrorCode::UnclosedString, ErrorSource::Scanner, 0,
        );
        assert_scan_error(
            &[QUOTE, 0xff, 0x1f, QUOTE], 0,
            ErrorCode::ControlCharInString, ErrorSource::Scanner, 2,
        );
        assert_scan_error(
            &[QUOTE, 0xff, BACKSLASH, QUOTE], 0,
            ErrorCode::UnclosedString, ErrorSource::Scanner, 0,
        );
    }

    #[test]
    fn bytes_outside_the_token_do_not_affect_utf8_or_escape_discovery() {
        let input = [0xff, BACKSLASH, QUOTE, b'a', QUOTE, 0xff, BACKSLASH, 0];
        let scan = scan_string(&input, 2).unwrap();
        assert_eq!(scan.raw, Span32::new(2, 3));
        assert_eq!(scan.content, Span32::new(3, 1));
        assert_eq!(scan.next_offset, 5);
        assert_eq!(scan.first_escape, None);
    }

    #[test]
    fn scanning_never_uses_bytes_beyond_the_provided_subslice() {
        let input = [QUOTE, b'a', BACKSLASH, QUOTE, b'b', QUOTE];
        for len in 1..input.len() {
            assert_scan_error(&input[..len], 0, ErrorCode::UnclosedString, ErrorSource::Scanner, 0);
        }
        assert_eq!(scan_string(&input, 0).unwrap().next_offset, 6);
    }

    #[test]
    fn sequential_scans_have_no_shared_escape_state() {
        let input = [QUOTE, BACKSLASH, b'n', QUOTE, QUOTE, b'x', QUOTE];
        let before = input;
        let first = scan_string(&input, 0).unwrap();
        let second = scan_string(&input, first.next_offset).unwrap();
        assert_eq!(first.first_escape, Some(1));
        assert_eq!(second.first_escape, None);
        assert_eq!(second.raw, Span32::new(4, 3));
        assert_eq!(input, before);
        assert_eq!(scan_string(&input, 0).unwrap(), first);
    }

    #[test]
    fn scanner_length_guard_handles_host_and_abi_boundaries() {
        assert_eq!(checked_scan_len(0).unwrap(), 0);
        assert_eq!(checked_scan_len(1).unwrap(), 1);
        assert_eq!(checked_scan_len(u32::MAX as usize).unwrap(), u32::MAX);
        // No fabricated oversized slice is needed to exercise the exact guard
        // used by scan_string. This branch is unreachable on 32-bit hosts.
        if let Some(oversized) = (u32::MAX as usize).checked_add(1) {
            for len in [oversized, usize::MAX] {
                let error = checked_scan_len(len).unwrap_err();
                assert_eq!(error.code, ErrorCode::PayloadTooLarge);
                assert_eq!(error.source, ErrorSource::Scanner);
                assert_eq!(error.byte_offset, u32::MAX);
                assert_eq!(error.schema_id, 0);
                assert_eq!(error.field_id, 0);
            }
        }
    }
}
