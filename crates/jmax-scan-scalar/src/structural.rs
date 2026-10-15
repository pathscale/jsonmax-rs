//! Raw punctuation, whitespace, and byte-class masks for a full scalar block.

use super::*;

/// Classify all 64 bytes, with bit `i` corresponding to input byte `i`.
///
/// Punctuation is exactly `: , { } [ ]`. JSON whitespace is space, tab, CR,
/// and LF; the latter three also belong to the control class (`0x00..=0x1f`).
/// Digits are ASCII `0..=9`, and every byte at least `0x80` is non-ASCII.
///
/// These masks are raw, including bytes inside strings. This infallible
/// operation neither validates JSON or UTF-8 nor reads or updates carry.
/// Every array element is classified, including any caller-supplied padding;
/// the scanner's final assembly clips masks to the actual input length.
pub fn classify_structural(input: &[u8; BLOCK_BYTES]) -> StructuralMasks {
    let mut masks = StructuralMasks::default();
    for (index, &byte) in input.iter().enumerate() {
        // The fixed-size array bounds the shift to 0..=63 on every target.
        let bit = 1u64 << index;
        match byte {
            b':' => masks.colon_mask |= bit,
            b',' => masks.comma_mask |= bit,
            b'{' => masks.lbrace_mask |= bit,
            b'}' => masks.rbrace_mask |= bit,
            b'[' => masks.lbracket_mask |= bit,
            b']' => masks.rbracket_mask |= bit,
            b' ' | b'\t' | b'\r' | b'\n' => masks.whitespace_mask |= bit,
            _ => {}
        }
        // Control and whitespace intentionally overlap for tab, CR, and LF.
        if byte < 0x20 {
            masks.control_mask |= bit;
        }
        if byte >= b'0' && byte <= b'9' {
            masks.digit_mask |= bit;
        }
        if byte >= 0x80 {
            masks.non_ascii_mask |= bit;
        }
    }
    masks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(masks: StructuralMasks) -> [u64; 10] {
        [
            masks.colon_mask,
            masks.comma_mask,
            masks.lbrace_mask,
            masks.rbrace_mask,
            masks.lbracket_mask,
            masks.rbracket_mask,
            masks.whitespace_mask,
            masks.control_mask,
            masks.digit_mask,
            masks.non_ascii_mask,
        ]
    }

    fn block_classes(masks: BlockMasks) -> StructuralMasks {
        StructuralMasks {
            colon_mask: masks.colon_mask,
            comma_mask: masks.comma_mask,
            lbrace_mask: masks.lbrace_mask,
            rbrace_mask: masks.rbrace_mask,
            lbracket_mask: masks.lbracket_mask,
            rbracket_mask: masks.rbracket_mask,
            whitespace_mask: masks.whitespace_mask,
            control_mask: masks.control_mask,
            digit_mask: masks.digit_mask,
            non_ascii_mask: masks.non_ascii_mask,
        }
    }

    // Set membership supplies a separate oracle for each byte class without
    // sharing the production match arms, range comparisons, or mask builder.
    fn membership(byte: u8) -> [bool; 10] {
        [
            b":".contains(&byte),
            b",".contains(&byte),
            b"{".contains(&byte),
            b"}".contains(&byte),
            b"[".contains(&byte),
            b"]".contains(&byte),
            b" \t\r\n".contains(&byte),
            (0u8..=31).contains(&byte),
            b"0123456789".contains(&byte),
            (128u8..=255).contains(&byte),
        ]
    }

    #[test]
    fn explicit_mixed_vector_has_exact_raw_masks() {
        let mut input = [b'a'; BLOCK_BYTES];
        let prefix = b":,{}[] \t\r\n09/\x1f\x20\x7f\x80\xff\"\\a\x0b\x0c";
        input[..prefix.len()].copy_from_slice(prefix);
        let actual = classify_structural(&input);
        assert_eq!(actual, StructuralMasks {
            colon_mask: 1 << 0,
            comma_mask: 1 << 1,
            lbrace_mask: 1 << 2,
            rbrace_mask: 1 << 3,
            lbracket_mask: 1 << 4,
            rbracket_mask: 1 << 5,
            whitespace_mask: (1 << 6) | (1 << 7) | (1 << 8) | (1 << 9) | (1 << 14),
            control_mask: (1 << 7) | (1 << 8) | (1 << 9) | (1 << 13)
                | (1 << 21) | (1 << 22),
            digit_mask: (1 << 10) | (1 << 11),
            non_ascii_mask: (1 << 16) | (1 << 17),
        });
        assert_eq!(actual.punctuation_mask(), 0x3f);
        assert_eq!(actual.whitespace_mask & actual.control_mask, 0x380);
    }

    #[test]
    fn every_byte_value_fills_exactly_its_classes() {
        for value in 0u16..=255 {
            let byte = value as u8;
            let expected = membership(byte).map(|member| if member { u64::MAX } else { 0 });
            assert_eq!(fields(classify_structural(&[byte; BLOCK_BYTES])), expected,
                "byte {byte:#04x}");
        }
    }

    #[test]
    fn every_byte_value_at_every_position_sets_only_that_bit() {
        for value in 0u16..=255 {
            let byte = value as u8;
            for position in 0..BLOCK_BYTES {
                let mut input = [b'a'; BLOCK_BYTES];
                input[position] = byte;
                let bit = 1u64 << position;
                let expected = membership(byte).map(|member| if member { bit } else { 0 });
                assert_eq!(fields(classify_structural(&input)), expected,
                    "byte {byte:#04x}, position {position}");
            }
        }
    }

    #[test]
    fn mixed_byte_ramps_preserve_byte_order_and_class_independence() {
        for start in 0u16..=255 {
            let mut input = [0; BLOCK_BYTES];
            for (position, byte) in input.iter_mut().enumerate() {
                *byte = (start as u8).wrapping_add(position as u8);
            }
            let masks = classify_structural(&input);
            let actual = fields(masks);
            for (position, &byte) in input.iter().enumerate() {
                for (mask, member) in actual.iter().zip(membership(byte)) {
                    assert_eq!((mask >> position) & 1, u64::from(member),
                        "start {start}, position {position}");
                }
            }
            let punctuation = masks.punctuation_mask();
            for (index, mask) in actual[..6].iter().enumerate() {
                for other in &actual[index + 1..] {
                    assert_eq!(mask & other, 0);
                }
            }
            assert_eq!(punctuation, actual[..6].iter().fold(0, |bits, mask| bits | mask));
            assert_eq!(masks.non_ascii_mask & (punctuation | masks.whitespace_mask
                | masks.control_mask | masks.digit_mask), 0);
        }
    }

    #[test]
    fn first_and_last_bytes_are_independent_of_backing_slice_alignment() {
        let mut storage = [b'a'; BLOCK_BYTES + 2];
        storage[0] = b',';
        storage[1] = b':';
        storage[BLOCK_BYTES] = b'\n';
        storage[BLOCK_BYTES + 1] = b'9';
        let input: &[u8; BLOCK_BYTES] = storage[1..BLOCK_BYTES + 1].try_into().unwrap();
        assert_eq!(classify_structural(input), StructuralMasks {
            colon_mask: 1,
            whitespace_mask: 1u64 << 63,
            control_mask: 1u64 << 63,
            ..StructuralMasks::default()
        });
    }

    #[test]
    fn unicode_whitespace_digits_and_punctuation_are_only_non_ascii_bytes() {
        // NBSP, EM SPACE, LINE SEPARATOR, Arabic-Indic zero, fullwidth zero
        // and fullwidth colon are not their ASCII lexical counterparts.
        let prefix = "\u{00a0}\u{2003}\u{2028}\u{0660}\u{ff10}\u{ff1a}".as_bytes();
        let mut input = [b'a'; BLOCK_BYTES];
        input[..prefix.len()].copy_from_slice(prefix);
        assert_eq!(classify_structural(&input), StructuralMasks {
            non_ascii_mask: (1u64 << prefix.len()) - 1,
            ..StructuralMasks::default()
        });
    }

    #[test]
    fn raw_classes_remain_present_inside_strings_and_invalid_json() {
        let prefix = b"\":,{}[] \t\x00\xff09\",";
        let mut input = [b'a'; BLOCK_BYTES];
        input[..prefix.len()].copy_from_slice(prefix);
        let classes = classify_structural(&input);
        assert_eq!(classes.punctuation_mask(), 0x407e);
        let raw = classify::classify_raw(&input);
        assert_eq!(raw.classes, classes);
        let mut carry = ScanCarry::default();
        let quotes = quotes::quote_masks(raw.quote_mask, raw.backslash_mask,
            prefix.len() as u8, &mut carry).unwrap();
        let finished = raw.finish(quotes, prefix.len()).unwrap();
        assert_eq!(block_classes(finished), classes);
        assert_eq!(finished.structural_mask, 1 << 14);
        assert_eq!(finished.whitespace_mask, (1 << 7) | (1 << 8));
        assert_eq!(finished.control_mask, (1 << 8) | (1 << 9));
        assert_eq!(finished.non_ascii_mask, 1 << 10);
        assert_eq!(finished.digit_mask, (1 << 11) | (1 << 12));
        assert_eq!(carry, ScanCarry::default());
    }

    #[test]
    fn final_assembly_clips_all_classes_at_every_length() {
        let pattern = b":,{}[] \t\r\n\x00\x1f09\x80\xff";
        let mut input = [0; BLOCK_BYTES];
        for (position, byte) in input.iter_mut().enumerate() {
            *byte = pattern[position % pattern.len()];
        }
        let classes = classify_structural(&input);
        let raw = RawMasks { classes, ..RawMasks::default() };
        for len in 0..=BLOCK_BYTES {
            let valid = if len == BLOCK_BYTES { u64::MAX } else { (1u64 << len) - 1 };
            let finished = raw.finish(QuoteMasks::default(), len).unwrap();
            assert_eq!(fields(block_classes(finished)), fields(classes).map(|mask| mask & valid));
            assert_eq!(finished.structural_mask, classes.punctuation_mask() & valid);
            assert_eq!(finished.valid_mask, valid);
            assert_eq!(usize::from(finished.valid_len), len);
            assert_eq!(finished.backend, BACKEND_TAG);
        }
    }

    #[test]
    fn zero_padding_is_raw_control_but_not_live_control() {
        let mut input = [0; BLOCK_BYTES];
        input[0] = b':';
        let raw = RawMasks {
            classes: classify_structural(&input),
            ..RawMasks::default()
        };
        assert_eq!(raw.classes.control_mask, u64::MAX ^ 1);
        let empty = raw.finish(QuoteMasks::default(), 0).unwrap();
        assert_eq!(empty, BlockMasks::default());
        let single = raw.finish(QuoteMasks::default(), 1).unwrap();
        assert_eq!(single.control_mask, 0);
        assert_eq!(single.colon_mask, 1);
        assert_eq!(single.structural_mask, 1);
        let full = raw.finish(QuoteMasks::default(), BLOCK_BYTES).unwrap();
        assert_eq!(full.control_mask, u64::MAX ^ 1);
    }

    fn assert_scanner_error(error: Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Scanner);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
    }

    // Structural classification itself has no error path. Verify errors at
    // its integration boundaries without adding validation to the raw API.
    #[test]
    fn final_assembly_rejects_oversized_lengths() {
        let raw = RawMasks {
            classes: classify_structural(&[b':'; BLOCK_BYTES]),
            ..RawMasks::default()
        };
        for len in [65, 255, 256, usize::MAX] {
            assert_scanner_error(raw.finish(QuoteMasks::default(), len).unwrap_err(),
                ErrorCode::InvalidToken);
        }
        assert_eq!(raw.classes.colon_mask, u64::MAX);
    }

    #[test]
    fn scanner_errors_leave_carry_unchanged() {
        let input = [b':'; BLOCK_BYTES + 1];
        for (inside, escaped) in [(0, 0), (1, 1), (2, 0), (0, u64::MAX)] {
            let initial = ScanCarry {
                prev_in_string: inside,
                prev_escaped: escaped,
                utf8_tail: [0xf0, 0x9f, 0xff, 0x80],
                utf8_tail_len: 255,
            };
            let mut carry = initial;
            assert_scanner_error(crate::classify(&input, &mut carry).unwrap_err(),
                ErrorCode::InvalidToken);
            assert_eq!(carry, initial);
            if inside > 1 || escaped > 1 {
                for len in [0, 1, BLOCK_BYTES] {
                    assert_scanner_error(crate::classify(&input[..len], &mut carry).unwrap_err(),
                        ErrorCode::InternalError);
                    assert_eq!(carry, initial);
                }
            }
        }
    }
}
