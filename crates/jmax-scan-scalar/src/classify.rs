//! Portable scalar classification with transactional quote carry updates.

use super::*;

/// Classify a single block containing zero through 64 bytes.
///
/// Every mask is restricted to the actual input length. Quote state flows into
/// and out of the block; UTF-8 state is preserved without validation. Incomplete
/// strings, escapes, and otherwise invalid JSON are classified, not rejected.
///
/// Returns a scanner `InvalidToken` error for oversized input or `InternalError`
/// for noncanonical quote carry. Any error leaves the caller's carry unchanged.
pub fn classify(input: &[u8], carry: &mut ScanCarry) -> Result<BlockMasks> {
    valid_mask(input.len())?;
    validate_carry(carry)?;
    let block = tails::load_block(input)?;
    // The length check above makes this conversion lossless.
    classify_loaded(&block, input.len() as u8, carry)
}

/// Classify all 64 bytes of an array without copying or padding the input.
///
/// This has the same masks, state transitions, and transactional error behavior
/// as [`classify`] on the array's full slice.
pub fn classify_block(
    input: &[u8; BLOCK_BYTES],
    carry: &mut ScanCarry,
) -> Result<BlockMasks> {
    validate_carry(carry)?;
    classify_loaded(input, BLOCK_BYTES as u8, carry)
}

/// Return full-width lexical masks without applying quote state or validation.
///
/// Bit `i` describes byte `i` regardless of host endianness. Punctuation and
/// other byte classes are raw, including occurrences within strings.
pub fn classify_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    let mut quote_mask = 0;
    let mut backslash_mask = 0;
    for (index, &byte) in input.iter().enumerate() {
        let bit = 1u64 << index;
        match byte {
            b'"' => quote_mask |= bit,
            b'\\' => backslash_mask |= bit,
            _ => {}
        }
    }
    RawMasks {
        quote_mask,
        backslash_mask,
        classes: structural::classify_structural(input),
        backend: BackendId::Scalar,
    }
}

// Both callers validate quote carry and supply a length in 0..=64. Keep the
// pending state private until quote analysis and ABI mask assembly succeed.
fn classify_loaded(
    input: &[u8; BLOCK_BYTES],
    valid_len: u8,
    carry: &mut ScanCarry,
) -> Result<BlockMasks> {
    let raw = classify_raw(input);
    let mut next_carry = *carry;
    let quotes = quotes::quote_masks(
        raw.quote_mask,
        raw.backslash_mask,
        valid_len,
        &mut next_carry,
    )?;
    let masks = raw.finish(quotes, usize::from(valid_len))?;
    *carry = next_carry;
    Ok(masks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initial_carry(in_string: u64, escaped: u64) -> ScanCarry {
        ScanCarry {
            prev_in_string: in_string,
            prev_escaped: escaped,
            // Deliberately opaque, including an out-of-range UTF-8 tail length.
            utf8_tail: [0xf0, 0x9f, 0xff, 0x80],
            utf8_tail_len: 255,
        }
    }

    fn mask_fields(masks: &BlockMasks) -> [u64; 15] {
        [
            masks.quote_mask,
            masks.backslash_mask,
            masks.in_string_mask,
            masks.colon_mask,
            masks.comma_mask,
            masks.lbrace_mask,
            masks.rbrace_mask,
            masks.lbracket_mask,
            masks.rbracket_mask,
            masks.structural_mask,
            masks.whitespace_mask,
            masks.control_mask,
            masks.digit_mask,
            masks.non_ascii_mask,
            masks.valid_mask,
        ]
    }

    // A byte-at-a-time oracle deliberately avoids production helpers, padding,
    // and quote bit algorithms. Count complete backslash runs before quotes.
    fn reference(input: &[u8], carry: &mut ScanCarry) -> BlockMasks {
        assert!(input.len() <= BLOCK_BYTES);
        let mut result = BlockMasks {
            valid_len: input.len() as u8,
            backend: 0,
            ..BlockMasks::default()
        };
        let mut inside = carry.prev_in_string == 1;
        let mut run = carry.prev_escaped as usize;
        for (index, &byte) in input.iter().enumerate() {
            let bit = 1u64 << index;
            result.valid_mask |= bit;
            match byte {
                b'"' => result.quote_mask |= bit,
                b'\\' => result.backslash_mask |= bit,
                b':' => result.colon_mask |= bit,
                b',' => result.comma_mask |= bit,
                b'{' => result.lbrace_mask |= bit,
                b'}' => result.rbrace_mask |= bit,
                b'[' => result.lbracket_mask |= bit,
                b']' => result.rbracket_mask |= bit,
                _ => {}
            }
            if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
                result.whitespace_mask |= bit;
            }
            if byte < 32 {
                result.control_mask |= bit;
            }
            if (b'0'..=b'9').contains(&byte) {
                result.digit_mask |= bit;
            }
            if byte >= 128 {
                result.non_ascii_mask |= bit;
            }
            if byte == b'"' && run % 2 == 0 {
                inside = !inside;
            } else if inside {
                result.in_string_mask |= bit;
            }
            if !inside && matches!(byte, b':' | b',' | b'{' | b'}' | b'[' | b']') {
                result.structural_mask |= bit;
            }
            if byte == b'\\' {
                run += 1;
            } else {
                run = 0;
            }
        }
        carry.prev_in_string = u64::from(inside);
        carry.prev_escaped = (run % 2) as u64;
        result
    }

    fn assert_reference(input: &[u8], initial: ScanCarry) {
        let mut expected_carry = initial;
        let expected = reference(input, &mut expected_carry);
        let mut actual_carry = initial;
        let actual = classify(input, &mut actual_carry).unwrap();
        assert_eq!(actual, expected, "input: {input:?}, initial: {initial:?}");
        assert_eq!(actual_carry, expected_carry);
        for mask in mask_fields(&actual) {
            assert_eq!(mask & !actual.valid_mask, 0);
        }
    }

    #[test]
    fn canonical_entry_point_and_explicit_json_masks() {
        let entry: ClassifyFn = classify;
        let mut carry = ScanCarry::default();
        let masks = entry(b"{\"a:b\",0}\n", &mut carry).unwrap();
        assert_eq!(masks, BlockMasks {
            quote_mask: 0x022,
            in_string_mask: 0x01c,
            colon_mask: 0x008,
            comma_mask: 0x040,
            lbrace_mask: 0x001,
            rbrace_mask: 0x100,
            structural_mask: 0x141,
            whitespace_mask: 0x200,
            control_mask: 0x200,
            digit_mask: 0x080,
            valid_mask: 0x3ff,
            valid_len: 10,
            backend: 0,
            ..BlockMasks::default()
        });
        assert_eq!(carry, ScanCarry::default());
    }

    #[test]
    fn empty_input_preserves_every_canonical_carry_combination() {
        for inside in 0..=1 {
            for escaped in 0..=1 {
                let initial = initial_carry(inside, escaped);
                let mut carry = initial;
                assert_eq!(classify(&[], &mut carry).unwrap(), BlockMasks::default());
                assert_eq!(carry, initial);
            }
        }
    }

    #[test]
    fn raw_masks_cover_every_byte_value_in_every_lane() {
        for value in 0u16..=255 {
            let byte = value as u8;
            let input = [byte; BLOCK_BYTES];
            let full_if = |condition: bool| if condition { u64::MAX } else { 0 };
            let expected = RawMasks {
                quote_mask: full_if(byte == b'"'),
                backslash_mask: full_if(byte == b'\\'),
                classes: StructuralMasks {
                    colon_mask: full_if(byte == b':'),
                    comma_mask: full_if(byte == b','),
                    lbrace_mask: full_if(byte == b'{'),
                    rbrace_mask: full_if(byte == b'}'),
                    lbracket_mask: full_if(byte == b'['),
                    rbracket_mask: full_if(byte == b']'),
                    whitespace_mask: full_if(matches!(byte, b' ' | b'\t' | b'\r' | b'\n')),
                    control_mask: full_if(byte < 0x20),
                    digit_mask: full_if((b'0'..=b'9').contains(&byte)),
                    non_ascii_mask: full_if(byte >= 0x80),
                },
                backend: BackendId::Scalar,
            };
            assert_eq!(classify_raw(&input), expected, "byte: {byte}");
        }
    }

    #[test]
    fn each_quote_and_backslash_position_has_the_correct_bit() {
        for position in 0..BLOCK_BYTES {
            for byte in [b'"', b'\\'] {
                let mut input = [b'x'; BLOCK_BYTES];
                input[position] = byte;
                let raw = classify_raw(&input);
                let expected = 1u64 << position;
                assert_eq!(raw.quote_mask, if byte == b'"' { expected } else { 0 });
                assert_eq!(raw.backslash_mask, if byte == b'\\' { expected } else { 0 });
                assert_eq!(raw.classes, StructuralMasks::default());
                assert_eq!(raw.backend, BackendId::Scalar);
            }
        }
    }

    #[test]
    fn all_lengths_clip_padding_and_match_the_byte_oracle() {
        let pattern = b"{\"a\\\"b\": [0,9]}\t\r\n\x00\x1f\x7f\x80\xff\\";
        let mut input = [0; BLOCK_BYTES];
        for (index, byte) in input.iter_mut().enumerate() {
            *byte = pattern[index % pattern.len()];
        }
        for len in 0..=BLOCK_BYTES {
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    assert_reference(&input[..len], initial_carry(inside, escaped));
                }
            }
            let mut carry = ScanCarry::default();
            let zeros = [0; BLOCK_BYTES];
            let masks = classify(&zeros[..len], &mut carry).unwrap();
            assert_eq!(masks.control_mask, masks.valid_mask);
            let spaces = [b' '; BLOCK_BYTES];
            let masks = classify(&spaces[..len], &mut carry).unwrap();
            assert_eq!(masks.whitespace_mask, masks.valid_mask);
            assert_eq!(masks.control_mask, 0);
        }
    }

    #[test]
    fn fixed_blocks_match_slices_and_reference_for_all_bytes() {
        for value in 0u16..=255 {
            let input = [value as u8; BLOCK_BYTES];
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let initial = initial_carry(inside, escaped);
                    let mut slice_carry = initial;
                    let mut array_carry = initial;
                    let mut expected_carry = initial;
                    let expected = reference(&input, &mut expected_carry);
                    let slice = classify(&input, &mut slice_carry).unwrap();
                    let array = classify_block(&input, &mut array_carry).unwrap();
                    assert_eq!(array, expected);
                    assert_eq!(slice, array);
                    assert_eq!(array_carry, expected_carry);
                    assert_eq!(slice_carry, array_carry);
                    assert_eq!(array.valid_mask, u64::MAX);
                    assert_eq!(array.valid_len, 64);
                    assert_eq!(array.backend, 0);
                }
            }
        }
    }

    #[test]
    fn quote_escape_runs_and_every_split_preserve_masks() {
        // Include every possible run length that still leaves room for a quote.
        for run_len in 0..BLOCK_BYTES {
            let mut input = [b'\\'; BLOCK_BYTES];
            input[run_len] = b'"';
            let input = &input[..run_len + 1];
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let initial = initial_carry(inside, escaped);
                    let mut expected_carry = initial;
                    let expected = reference(input, &mut expected_carry);
                    assert_reference(input, initial);
                    for split in 0..=input.len() {
                        let mut carry = initial;
                        let left = classify(&input[..split], &mut carry).unwrap();
                        let right = classify(&input[split..], &mut carry).unwrap();
                        for ((low, high), whole) in mask_fields(&left)
                            .iter()
                            .zip(mask_fields(&right).iter())
                            .zip(mask_fields(&expected).iter())
                        {
                            // A split at 64 leaves an empty right block.
                            let shifted = if split == 64 { 0 } else { *high << split };
                            assert_eq!(*low | shifted, *whole);
                        }
                        assert_eq!(carry, expected_carry);
                    }
                }
            }
        }
    }

    #[test]
    fn string_and_escape_state_cross_full_block_edges() {
        let mut first = [b'x'; BLOCK_BYTES];
        first[0] = b'"';
        first[63] = b'\\';
        let mut carry = initial_carry(0, 0);
        let masks = classify_block(&first, &mut carry).unwrap();
        assert_eq!(masks.quote_mask, 1);
        assert_eq!(masks.backslash_mask, 1u64 << 63);
        assert_eq!(masks.in_string_mask, u64::MAX ^ 1);
        assert_eq!(carry, initial_carry(1, 1));

        let masks = classify(b"\":\",", &mut carry).unwrap();
        assert_eq!(masks.quote_mask, 0b0101);
        assert_eq!(masks.in_string_mask, 0b0011);
        assert_eq!(masks.colon_mask, 0b0010);
        assert_eq!(masks.structural_mask, 0b1000);
        assert_eq!(carry, initial_carry(0, 0));
    }

    #[test]
    fn scanner_accepts_invalid_syntax_escapes_controls_and_utf8() {
        let vectors: &[&[u8]] = &[
            b"\"unterminated\\",
            b"\\\":",
            b"\"\\q\x00\x01\x1f\xff\xc0\xaf\"",
            b"][}{,:+01NaN",
            b"\x0b\x0c\x7f\x85\xa0\xc2\xa0",
        ];
        for &input in vectors {
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    assert_reference(input, initial_carry(inside, escaped));
                }
            }
        }
    }

    fn assert_scanner_error(error: Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Scanner);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
    }

    #[test]
    fn oversized_input_is_rejected_before_narrowing_without_mutation() {
        let input = [b'"'; 1024];
        for len in [65, 255, 256, 257, 1024] {
            for initial in [initial_carry(1, 1), initial_carry(u64::MAX, u64::MAX)] {
                let mut carry = initial;
                let error = classify(&input[..len], &mut carry).unwrap_err();
                assert_scanner_error(error, ErrorCode::InvalidToken);
                assert_eq!(carry, initial);
            }
        }
    }

    #[test]
    fn noncanonical_quote_carry_is_rejected_by_both_entry_points() {
        let input = [b'"'; BLOCK_BYTES];
        for bad in [2, 3, 255, 256, 1u64 << 63, u64::MAX] {
            for initial in [
                initial_carry(bad, 0),
                initial_carry(0, bad),
                initial_carry(bad, 1),
                initial_carry(1, bad),
                initial_carry(bad, bad),
            ] {
                for len in [0, 1, 63, 64] {
                    let mut carry = initial;
                    let error = classify(&input[..len], &mut carry).unwrap_err();
                    assert_scanner_error(error, ErrorCode::InternalError);
                    assert_eq!(carry, initial);
                }
                let mut carry = initial;
                let error = classify_block(&input, &mut carry).unwrap_err();
                assert_scanner_error(error, ErrorCode::InternalError);
                assert_eq!(carry, initial);
            }
        }
    }
}
