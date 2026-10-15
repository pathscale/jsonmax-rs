//! Bounded quote analysis with consecutive-backslash parity across blocks.

use super::*;

/// Resolve raw quote and backslash masks for zero through 64 live bytes.
///
/// Bit `i` describes byte `i`. Bits beyond `valid_len` are ignored, including
/// overlaps in padding. An odd preceding backslash run escapes a quote even
/// outside a string. Unescaped quotes toggle string state and are excluded
/// from string content; escaped quotes are content only when already inside.
///
/// The outgoing carry records string state and trailing backslash parity.
/// Empty input preserves carry, and UTF-8 fields are always left untouched.
/// Unclosed strings and trailing escapes are valid intermediate states.
///
/// # Errors
///
/// Returns `InvalidToken` for lengths above 64, or `InternalError` for
/// noncanonical quote carry or overlapping live quote and backslash bits.
/// All errors have source `Scanner`, offset zero, and leave carry unchanged.
pub fn quote_masks(
    quote_mask: u64,
    backslash_mask: u64,
    valid_len: u8,
    carry: &mut ScanCarry,
) -> Result<QuoteMasks> {
    let valid = valid_mask(usize::from(valid_len))?;
    validate_carry(carry)?;
    let quotes = quote_mask & valid;
    let backslashes = backslash_mask & valid;
    if quotes & backslashes != 0 {
        return Err(Error::new(
            ErrorCode::InternalError,
            ErrorSource::Scanner,
            0,
            "quote and backslash masks overlap at a live byte",
        ));
    }

    let mut result = QuoteMasks::default();
    let mut inside = carry.prev_in_string == 1;
    let mut odd_run = carry.prev_escaped == 1;
    for index in 0..usize::from(valid_len) {
        // Length validation bounds every shift to 0..=63.
        let bit = 1u64 << index;
        let is_quote = quotes & bit != 0;
        let is_delimiter = is_quote && !odd_run;
        if is_delimiter {
            result.delimiter_quote_mask |= bit;
            inside = !inside;
        } else {
            if is_quote {
                result.escaped_quote_mask |= bit;
            }
            if inside {
                result.in_string_mask |= bit;
            }
        }

        // Consume the preceding run before resetting it at a non-backslash.
        // This parity is a byte property, independent of the string state.
        odd_run = backslashes & bit != 0 && !odd_run;
    }

    // Every fallible operation precedes publication. With zero live bytes the
    // validated boolean fields retain their original values exactly.
    carry.prev_in_string = u64::from(inside);
    carry.prev_escaped = u64::from(odd_run);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initial_carry(inside: u64, escaped: u64) -> ScanCarry {
        ScanCarry {
            prev_in_string: inside,
            prev_escaped: escaped,
            // Opaque to this layer, including the deliberately invalid length.
            utf8_tail: [0xf0, 0x9f, 0xff, 0x80],
            utf8_tail_len: 255,
        }
    }

    fn raw_masks(input: &[u8]) -> (u64, u64) {
        assert!(input.len() <= BLOCK_BYTES);
        let mut quotes = 0;
        let mut backslashes = 0;
        for (index, &byte) in input.iter().enumerate() {
            if byte == b'"' {
                quotes |= 1u64 << index;
            } else if byte == b'\\' {
                backslashes |= 1u64 << index;
            }
        }
        (quotes, backslashes)
    }

    fn analyze(input: &[u8], carry: &mut ScanCarry) -> QuoteMasks {
        let (quotes, backslashes) = raw_masks(input);
        quote_masks(quotes, backslashes, input.len() as u8, carry).unwrap()
    }

    // Independently count preceding runs by walking backward from each quote.
    // This does not use the production parity transition or root helpers.
    fn reference(input: &[u8], initial: ScanCarry) -> (QuoteMasks, ScanCarry) {
        assert!(input.len() <= BLOCK_BYTES);
        let mut result = QuoteMasks::default();
        let mut inside = initial.prev_in_string == 1;
        for (index, &byte) in input.iter().enumerate() {
            let bit = 1u64 << index;
            let mut delimiter = false;
            if byte == b'"' {
                let mut start = index;
                while start > 0 && input[start - 1] == b'\\' {
                    start -= 1;
                }
                let run = index - start
                    + if start == 0 { initial.prev_escaped as usize } else { 0 };
                delimiter = run % 2 == 0;
                if delimiter {
                    result.delimiter_quote_mask |= bit;
                } else {
                    result.escaped_quote_mask |= bit;
                }
            }
            if delimiter {
                inside = !inside;
            } else if inside {
                result.in_string_mask |= bit;
            }
        }
        let mut start = input.len();
        while start > 0 && input[start - 1] == b'\\' {
            start -= 1;
        }
        let trailing = input.len() - start
            + if start == 0 { initial.prev_escaped as usize } else { 0 };
        let mut carry = initial;
        carry.prev_in_string = u64::from(inside);
        carry.prev_escaped = (trailing % 2) as u64;
        (result, carry)
    }

    fn assert_reference(input: &[u8], initial: ScanCarry) {
        let (expected, expected_carry) = reference(input, initial);
        let mut carry = initial;
        let actual = analyze(input, &mut carry);
        assert_eq!(actual, expected, "input: {input:?}, initial: {initial:?}");
        assert_eq!(carry, expected_carry);
        let (quotes, _) = raw_masks(input);
        assert_eq!(actual.escaped_quote_mask | actual.delimiter_quote_mask, quotes);
        assert_eq!(actual.escaped_quote_mask & actual.delimiter_quote_mask, 0);
        assert_eq!(actual.in_string_mask & actual.delimiter_quote_mask, 0);
        let valid = if input.len() == 64 { u64::MAX } else { (1u64 << input.len()) - 1 };
        assert_eq!(actual.in_string_mask & !valid, 0);
    }

    #[test]
    fn explicit_delimiter_content_and_escape_masks() {
        let mut carry = initial_carry(0, 0);
        let input = [b'"', b'a', b'\\', b'"', b'b', b'"', b','];
        assert_eq!(analyze(&input, &mut carry), QuoteMasks {
            escaped_quote_mask: 0b0001000,
            delimiter_quote_mask: 0b0100001,
            in_string_mask: 0b0011110,
        });
        assert_eq!(carry, initial_carry(0, 0));

        // An escaped quote outside a string does not open one.
        let input = [b'\\', b'"', b'x', b'"', b'y'];
        assert_eq!(analyze(&input, &mut carry), QuoteMasks {
            escaped_quote_mask: 0b00010,
            delimiter_quote_mask: 0b01000,
            in_string_mask: 0b10000,
        });
        assert_eq!(carry, initial_carry(1, 0));
    }

    #[test]
    fn empty_blocks_ignore_all_mask_bits_and_preserve_carry() {
        for inside in 0..=1 {
            for escaped in 0..=1 {
                let initial = initial_carry(inside, escaped);
                let mut carry = initial;
                assert_eq!(
                    quote_masks(u64::MAX, u64::MAX, 0, &mut carry).unwrap(),
                    QuoteMasks::default(),
                );
                assert_eq!(carry, initial);
            }
        }
    }

    #[test]
    fn exhaustive_short_sequences_match_independent_run_counts() {
        let alphabet = [b'x', b'"', b'\\'];
        let mut input = [0; 8];
        for len in 0..=input.len() {
            for mut encoding in 0..3usize.pow(len as u32) {
                for byte in &mut input[..len] {
                    *byte = alphabet[encoding % 3];
                    encoding /= 3;
                }
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        assert_reference(&input[..len], initial_carry(inside, escaped));
                    }
                }
            }
        }
    }

    #[test]
    fn every_live_length_clips_padding_before_overlap_validation() {
        let pattern = [b'"', b'x', b'\\', b'"', b'\\', b'\\', b'"', b'x'];
        let mut input = [0; BLOCK_BYTES];
        for (index, byte) in input.iter_mut().enumerate() {
            *byte = pattern[index % pattern.len()];
        }
        for len in 0..=BLOCK_BYTES {
            let valid = if len == 64 { u64::MAX } else { (1u64 << len) - 1 };
            let (quotes, backslashes) = raw_masks(&input[..len]);
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let initial = initial_carry(inside, escaped);
                    let (expected, expected_carry) = reference(&input[..len], initial);
                    let mut carry = initial;
                    let actual = quote_masks(
                        quotes | !valid,
                        backslashes | !valid,
                        len as u8,
                        &mut carry,
                    ).unwrap();
                    assert_eq!(actual, expected);
                    assert_eq!(carry, expected_carry);
                }
            }
        }
    }

    #[test]
    fn isolated_quotes_and_backslashes_cover_every_bit_position() {
        for position in 0..BLOCK_BYTES {
            for byte in [b'"', b'\\'] {
                let mut input = [b'x'; BLOCK_BYTES];
                input[position] = byte;
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        assert_reference(&input, initial_carry(inside, escaped));
                        assert_reference(&input[..position + 1], initial_carry(inside, escaped));
                    }
                }
            }
        }
    }

    fn append_masks(left: QuoteMasks, right: QuoteMasks, shift: usize) -> QuoteMasks {
        assert!(shift <= BLOCK_BYTES);
        if shift == BLOCK_BYTES {
            assert_eq!(right, QuoteMasks::default());
            left
        } else {
            QuoteMasks {
                escaped_quote_mask: left.escaped_quote_mask | (right.escaped_quote_mask << shift),
                delimiter_quote_mask: left.delimiter_quote_mask | (right.delimiter_quote_mask << shift),
                in_string_mask: left.in_string_mask | (right.in_string_mask << shift),
            }
        }
    }

    #[test]
    fn every_backslash_run_and_split_preserves_quote_masks_and_carry() {
        for run_len in 0..BLOCK_BYTES {
            let mut input = [b'\\'; BLOCK_BYTES];
            input[run_len] = b'"';
            let input = &input[..run_len + 1];
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let initial = initial_carry(inside, escaped);
                    let (expected, expected_carry) = reference(input, initial);
                    assert_reference(input, initial);
                    for split in 0..=input.len() {
                        let mut carry = initial;
                        let left = analyze(&input[..split], &mut carry);
                        let before_empty = carry;
                        assert_eq!(analyze(&[], &mut carry), QuoteMasks::default());
                        assert_eq!(carry, before_empty);
                        let right = analyze(&input[split..], &mut carry);
                        assert_eq!(append_masks(left, right, split), expected);
                        assert_eq!(carry, expected_carry);
                    }
                }
            }
        }
    }

    #[test]
    fn trailing_backslash_parity_covers_every_length_and_incoming_state() {
        let input = [b'\\'; BLOCK_BYTES];
        for len in 0..=BLOCK_BYTES {
            let valid = if len == 64 { u64::MAX } else { (1u64 << len) - 1 };
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let mut carry = initial_carry(inside, escaped);
                    assert_eq!(analyze(&input[..len], &mut carry), QuoteMasks {
                        in_string_mask: if inside == 1 { valid } else { 0 },
                        ..QuoteMasks::default()
                    });
                    assert_eq!(carry, initial_carry(inside, escaped ^ (len as u64 & 1)));
                }
            }
        }
    }

    #[test]
    fn full_width_quote_masks_exclude_every_delimiter() {
        for inside in 0..=1 {
            for escaped in 0..=1 {
                let mut carry = initial_carry(inside, escaped);
                assert_eq!(quote_masks(u64::MAX, 0, 64, &mut carry).unwrap(), QuoteMasks {
                    escaped_quote_mask: escaped,
                    delimiter_quote_mask: u64::MAX ^ escaped,
                    in_string_mask: inside & escaped,
                });
                assert_eq!(carry, initial_carry(inside ^ escaped, 0));
            }
        }
    }

    #[test]
    fn backslash_runs_and_strings_cross_multiple_full_blocks() {
        let mut first = [b'x'; BLOCK_BYTES];
        first[0] = b'"';
        first[63] = b'\\';
        let mut carry = initial_carry(0, 0);
        assert_eq!(analyze(&first, &mut carry), QuoteMasks {
            delimiter_quote_mask: 1,
            in_string_mask: u64::MAX ^ 1,
            ..QuoteMasks::default()
        });
        assert_eq!(carry, initial_carry(1, 1));
        for _ in 0..3 {
            assert_eq!(analyze(&[b'\\'; BLOCK_BYTES], &mut carry), QuoteMasks {
                in_string_mask: u64::MAX,
                ..QuoteMasks::default()
            });
            assert_eq!(carry, initial_carry(1, 1));
        }
        assert_eq!(analyze(&[b'"', b':', b'"', b','], &mut carry), QuoteMasks {
            escaped_quote_mask: 0b0001,
            delimiter_quote_mask: 0b0100,
            in_string_mask: 0b0011,
        });
        assert_eq!(carry, initial_carry(0, 0));
    }

    #[test]
    fn non_backslashes_reset_parity_without_validating_json_or_utf8() {
        for value in 0u16..=255 {
            let input = [b'\\', value as u8, b'"'];
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    assert_reference(&input, initial_carry(inside, escaped));
                }
            }
        }
        for input in [b'"', b'\\', 0xff, 0] {
            assert_reference(&[input], initial_carry(1, 1));
        }
    }

    fn assert_error(quote: u64, backslash: u64, len: u8, initial: ScanCarry, code: ErrorCode) {
        let mut carry = initial;
        let error = quote_masks(quote, backslash, len, &mut carry).unwrap_err();
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Scanner);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
        assert_eq!(carry, initial);
    }

    #[test]
    fn every_oversized_u8_length_is_rejected_before_other_errors() {
        for len in 65..=u8::MAX {
            for initial in [initial_carry(1, 1), initial_carry(u64::MAX, u64::MAX)] {
                assert_error(u64::MAX, u64::MAX, len, initial, ErrorCode::InvalidToken);
            }
        }
    }

    #[test]
    fn invalid_carry_is_rejected_even_for_empty_blocks() {
        for bad in [2, 3, 255, 256, 1u64 << 63, u64::MAX] {
            for initial in [
                initial_carry(bad, 0),
                initial_carry(0, bad),
                initial_carry(bad, 1),
                initial_carry(1, bad),
                initial_carry(bad, bad),
            ] {
                for len in [0, 1, 63, 64] {
                    assert_error(0, 0, len, initial, ErrorCode::InternalError);
                }
            }
        }
    }

    #[test]
    fn live_overlap_is_rejected_at_every_position_without_mutation() {
        for position in 0..BLOCK_BYTES {
            let bit = 1u64 << position;
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    for len in [position as u8 + 1, 64] {
                        // Earlier quotes would change state if carry were
                        // published before reaching the conflicting byte.
                        assert_error(
                            bit | 1,
                            bit,
                            len,
                            initial_carry(inside, escaped),
                            ErrorCode::InternalError,
                        );
                    }
                }
            }
        }
    }
}
