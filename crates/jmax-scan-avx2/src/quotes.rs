//! Quote and escape carry for the combined two-lane AVX2 masks.
//!
//! Whole-word prefix operations propagate state across byte 31/32 without
//! requiring extra CPU features. This stage is allocation-free, works on every
//! target, and also handles masks produced by the portable classifier.

use super::*;

/// Resolve quote delimiters and string content for at most 64 real bytes.
///
/// Backslash parity applies inside and outside strings. Only unescaped quotes
/// toggle string state; neither opening nor closing delimiters are content.
/// Bits above `valid_len` are ignored, including overlapping padding bits.
/// Empty input preserves canonical carry. UTF-8 fields are always untouched.
/// Unclosed strings and trailing backslashes are valid intermediate states.
///
/// # Errors
///
/// Returns `InvalidToken` if `valid_len` exceeds 64, or `InternalError` if either
/// quote carry field is not zero or one or live quote and backslash bits
/// overlap. Errors have source `Scanner`, offset zero, and leave carry intact.
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
    if valid_len == 0 {
        return Ok(QuoteMasks::default());
    }

    // Segmented inclusive XOR: bit i becomes the parity of the consecutive
    // backslashes ending at i. Before each step, connected marks positions
    // ending in at least `distance` consecutive backslashes. It gates the
    // contribution from the preceding segment so parity never crosses a gap.
    let mut odd_runs = backslashes;
    let mut connected = backslashes;
    for distance in [1, 2, 4, 8, 16, 32] {
        odd_runs ^= (odd_runs << distance) & connected;
        connected &= connected << distance;
    }

    // Adding one clears precisely the low consecutive set bits. Their mask
    // also works for an all-backslash block because addition is wrapping.
    // Only this initial run can inherit parity from the preceding block.
    let initial_run = backslashes & !backslashes.wrapping_add(1);
    odd_runs ^= initial_run & 0u64.wrapping_sub(carry.prev_escaped);
    let escaped_quotes = quotes & ((odd_runs << 1) | carry.prev_escaped);
    let delimiters = quotes & !escaped_quotes;

    // Inclusive delimiter parity is the state after each byte, relative to
    // the incoming string state. Clear both kinds of delimiter from content.
    // These shifts deliberately span the boundary between the two AVX2 lanes.
    let mut prefix = delimiters;
    prefix ^= prefix << 1;
    prefix ^= prefix << 2;
    prefix ^= prefix << 4;
    prefix ^= prefix << 8;
    prefix ^= prefix << 16;
    prefix ^= prefix << 32;
    let states = prefix ^ 0u64.wrapping_sub(carry.prev_in_string);
    let masks = QuoteMasks {
        escaped_quote_mask: escaped_quotes,
        delimiter_quote_mask: delimiters,
        in_string_mask: states & !delimiters & valid,
    };

    // All validation has succeeded. The nonempty length bounds last to 0..63;
    // reading the final live bit avoids treating padding as an escape reset.
    let last = u32::from(valid_len) - 1;
    carry.prev_in_string = (states >> last) & 1;
    carry.prev_escaped = (odd_runs >> last) & 1;
    Ok(masks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn carry_with(inside: u64, escaped: u64) -> ScanCarry {
        ScanCarry {
            prev_in_string: inside,
            prev_escaped: escaped,
            utf8_tail: [0xf0, 0x00, 0xff, 0x92],
            // Intentionally noncanonical: this module must treat it as opaque.
            utf8_tail_len: 0xff,
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

    /// Independent bytewise oracle, without production helpers or prefixes.
    fn reference(input: &[u8], carry: &mut ScanCarry) -> QuoteMasks {
        assert!(input.len() <= BLOCK_BYTES);
        let mut result = QuoteMasks::default();
        let mut inside = carry.prev_in_string == 1;
        let mut escaped = carry.prev_escaped == 1;
        for (index, &byte) in input.iter().enumerate() {
            let bit = 1u64 << index;
            if byte == b'"' {
                if escaped {
                    result.escaped_quote_mask |= bit;
                    if inside {
                        result.in_string_mask |= bit;
                    }
                } else {
                    result.delimiter_quote_mask |= bit;
                    inside = !inside;
                }
            } else if inside {
                result.in_string_mask |= bit;
            }
            escaped = byte == b'\\' && !escaped;
        }
        carry.prev_in_string = u64::from(inside);
        carry.prev_escaped = u64::from(escaped);
        result
    }

    fn analyze(input: &[u8], carry: &mut ScanCarry) -> QuoteMasks {
        let (quotes, backslashes) = raw_masks(input);
        quote_masks(quotes, backslashes, input.len() as u8, carry).unwrap()
    }

    fn assert_matches(input: &[u8], initial: ScanCarry) {
        let mut expected_carry = initial;
        let expected = reference(input, &mut expected_carry);
        let mut actual_carry = initial;
        let actual = analyze(input, &mut actual_carry);
        assert_eq!(actual, expected, "input {input:?}, carry {initial:?}");
        assert_eq!(actual_carry, expected_carry);
        let (quotes, _) = raw_masks(input);
        let valid = ((1u128 << input.len()) - 1) as u64;
        assert_eq!(actual.escaped_quote_mask | actual.delimiter_quote_mask, quotes);
        assert_eq!(actual.escaped_quote_mask & actual.delimiter_quote_mask, 0);
        assert_eq!(actual.in_string_mask & actual.delimiter_quote_mask, 0);
        assert_eq!(actual.in_string_mask & !valid, 0);
    }

    fn assert_error(
        quotes: u64,
        backslashes: u64,
        len: u8,
        initial: ScanCarry,
        code: ErrorCode,
    ) {
        let mut carry = initial;
        let error = quote_masks(quotes, backslashes, len, &mut carry).unwrap_err();
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Scanner);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
        assert_eq!(carry, initial);
    }

    #[test]
    fn exact_signature_and_known_content_masks() {
        let entry: fn(u64, u64, u8, &mut ScanCarry) -> Result<QuoteMasks> = quote_masks;
        // Bytes: {, opening quote, colon, slash, escaped quote, comma,
        // closing quote, }. Both delimiters are excluded from content.
        let mut carry = carry_with(0, 0);
        let initial = carry;
        let actual = entry(0b0101_0010, 0b0000_1000, 8, &mut carry).unwrap();
        assert_eq!(actual, QuoteMasks {
            escaped_quote_mask: 0b0001_0000,
            delimiter_quote_mask: 0b0100_0010,
            in_string_mask: 0b0011_1100,
        });
        assert_eq!(carry, initial);

        let actual = analyze(b"\"\"\"x\"", &mut carry);
        assert_eq!(actual.escaped_quote_mask, 0);
        assert_eq!(actual.delimiter_quote_mask, 0b1_0111);
        assert_eq!(actual.in_string_mask, 0b0_1000);
        assert_eq!(carry, initial);
    }

    #[test]
    fn empty_blocks_ignore_all_masks_and_preserve_carry() {
        for inside in 0..=1 {
            for escaped in 0..=1 {
                let initial = carry_with(inside, escaped);
                let mut carry = initial;
                let result = quote_masks(u64::MAX, u64::MAX, 0, &mut carry).unwrap();
                assert_eq!(result, QuoteMasks::default());
                assert_eq!(carry, initial);
            }
        }
    }

    #[test]
    fn incoming_escape_applies_outside_strings_and_resets_on_any_other_byte() {
        for inside in 0..=1 {
            let mut carry = carry_with(inside, 1);
            let result = analyze(b"\"x\"", &mut carry);
            assert_eq!(result.escaped_quote_mask, 1);
            assert_eq!(result.delimiter_quote_mask, 4);
            assert_eq!(result.in_string_mask, if inside == 1 { 3 } else { 0 });
            assert_eq!(carry.prev_in_string, inside ^ 1);
            assert_eq!(carry.prev_escaped, 0);
            for byte in [0, b'x', b' ', b'\n', 0x80, 0xff] {
                let mut carry = carry_with(inside, 1);
                let result = analyze(&[byte, b'"'], &mut carry);
                assert_eq!(result.escaped_quote_mask, 0);
                assert_eq!(result.delimiter_quote_mask, 2);
                assert_eq!(result.in_string_mask, inside);
                assert_eq!(carry.prev_in_string, inside ^ 1);
                assert_eq!(carry.prev_escaped, 0);
            }
        }
    }

    #[test]
    fn every_length_handles_uniform_blocks_and_ignores_overlapping_padding() {
        for byte in [b'x', b'"', b'\\', 0, 0xff] {
            let block = [byte; BLOCK_BYTES];
            for len in 0..=BLOCK_BYTES {
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        let initial = carry_with(inside, escaped);
                        assert_matches(&block[..len], initial);
                        let (quotes, backslashes) = raw_masks(&block[..len]);
                        let valid = ((1u128 << len) - 1) as u64;
                        let mut carry = initial;
                        let actual = quote_masks(
                            quotes | !valid,
                            backslashes | !valid,
                            len as u8,
                            &mut carry,
                        ).unwrap();
                        let mut expected_carry = initial;
                        assert_eq!(actual, reference(&block[..len], &mut expected_carry));
                        assert_eq!(carry, expected_carry);
                    }
                }
            }
        }
    }

    #[test]
    fn every_backslash_run_start_and_length_has_the_expected_parity() {
        for start in 0..BLOCK_BYTES {
            for end in start..=BLOCK_BYTES {
                let mut block = [b'x'; BLOCK_BYTES];
                block[start..end].fill(b'\\');
                if end < BLOCK_BYTES {
                    block[end] = b'"';
                }
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        let initial = carry_with(inside, escaped);
                        assert_matches(&block, initial);
                        // Stop exactly at the end of the run: padding cannot
                        // clear trailing parity before the next block's quote.
                        let mut carry = initial;
                        analyze(&block[..end], &mut carry);
                        let incoming = if start == 0 { escaped } else { 0 };
                        let odd = ((end - start) as u64 & 1) ^ incoming;
                        assert_eq!(carry.prev_escaped, odd);
                        let next = analyze(b"\"", &mut carry);
                        assert_eq!(next.escaped_quote_mask, odd);
                        assert_eq!(next.delimiter_quote_mask, odd ^ 1);
                        assert_eq!(next.in_string_mask, odd & inside);
                        assert_eq!(carry.prev_in_string, inside ^ (odd ^ 1));
                        assert_eq!(carry.prev_escaped, 0);
                    }
                }
            }
        }
    }

    #[test]
    fn isolated_and_boundary_bits_propagate_between_both_lanes() {
        for position in 0..BLOCK_BYTES {
            for byte in [b'"', b'\\'] {
                let mut block = [b'x'; BLOCK_BYTES];
                block[position] = byte;
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        assert_matches(&block, carry_with(inside, escaped));
                    }
                }
            }
        }
        let mut carry = carry_with(0, 0);
        let result = quote_masks((1 << 31) | (1 << 63), 0, 64, &mut carry).unwrap();
        assert_eq!(result.delimiter_quote_mask, (1 << 31) | (1 << 63));
        assert_eq!(result.in_string_mask, 0x7fff_ffff_0000_0000);
        assert_eq!(carry.prev_in_string, 0);
        let result = quote_masks(1 << 32, 1 << 31, 64, &mut carry).unwrap();
        assert_eq!(result.escaped_quote_mask, 1 << 32);
        assert_eq!(result.delimiter_quote_mask, 0);
        assert_eq!(result.in_string_mask, 0);
    }

    #[test]
    fn exhaustive_short_sequences_match_the_bytewise_oracle() {
        let alphabet = [b'x', b'"', b'\\'];
        for len in 0..=8usize {
            for encoding in 0..3usize.pow(len as u32) {
                // Offset 28 moves the exhaustive sequence across the lane
                // boundary; offset 56 exercises the top bit of the word.
                for offset in [0usize, 28, 56] {
                    let mut block = [b'x'; BLOCK_BYTES];
                    let mut digits = encoding;
                    for byte in &mut block[offset..offset + len] {
                        *byte = alphabet[digits % alphabet.len()];
                        digits /= alphabet.len();
                    }
                    for inside in 0..=1 {
                        for escaped in 0..=1 {
                            assert_matches(&block[..offset + len], carry_with(inside, escaped));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn deterministic_mixed_blocks_and_all_prefix_lengths_match_the_oracle() {
        let alphabet = b"\"\\\\\"x{}[]:,\x00\x1f\x80\xff";
        let mut state = 0x4d59_5df4_d0f3_3173u64;
        for _ in 0..128 {
            let mut block = [0; BLOCK_BYTES];
            for byte in &mut block {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = alphabet[(state % alphabet.len() as u64) as usize];
            }
            for len in 0..=BLOCK_BYTES {
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        assert_matches(&block[..len], carry_with(inside, escaped));
                    }
                }
            }
        }
    }

    #[test]
    fn every_split_preserves_masks_and_final_carry() {
        let pattern = b"\"a\\\"b\\\\\"{}\\\\\\\"\xff";
        let mut mixed = [0; BLOCK_BYTES];
        for (index, byte) in mixed.iter_mut().enumerate() {
            *byte = pattern[index % pattern.len()];
        }
        for block in [mixed, [b'"'; BLOCK_BYTES], [b'\\'; BLOCK_BYTES]] {
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let initial = carry_with(inside, escaped);
                    let mut expected_carry = initial;
                    let expected = reference(&block, &mut expected_carry);
                    for split in 0..=BLOCK_BYTES {
                        let mut carry = initial;
                        let left = analyze(&block[..split], &mut carry);
                        let right = analyze(&block[split..], &mut carry);
                        for (low, high, whole) in [
                            (left.escaped_quote_mask, right.escaped_quote_mask,
                                expected.escaped_quote_mask),
                            (left.delimiter_quote_mask, right.delimiter_quote_mask,
                                expected.delimiter_quote_mask),
                            (left.in_string_mask, right.in_string_mask,
                                expected.in_string_mask),
                        ] {
                            // Widen first to permit a split at exactly 64.
                            assert_eq!(
                                u128::from(low) | (u128::from(high) << split),
                                u128::from(whole),
                                "split {split}, carry {initial:?}",
                            );
                        }
                        assert_eq!(carry, expected_carry);
                    }
                }
            }
        }
    }

    #[test]
    fn backslash_runs_cross_multiple_blocks_and_empty_blocks() {
        for inside in 0..=1 {
            for escaped in 0..=1 {
                for tail_len in 0..=BLOCK_BYTES {
                    let initial = carry_with(inside, escaped);
                    let mut carry = initial;
                    for _ in 0..3 {
                        let result = analyze(&[b'\\'; BLOCK_BYTES], &mut carry);
                        assert_eq!(result.in_string_mask, if inside == 1 { u64::MAX } else { 0 });
                        assert_eq!(result.escaped_quote_mask, 0);
                        assert_eq!(result.delimiter_quote_mask, 0);
                        assert_eq!(carry, initial);
                        assert_eq!(analyze(&[], &mut carry), QuoteMasks::default());
                        assert_eq!(carry, initial);
                    }
                    analyze(&[b'\\'; BLOCK_BYTES][..tail_len], &mut carry);
                    let odd = escaped ^ (tail_len as u64 & 1);
                    assert_eq!(carry.prev_escaped, odd);
                    let result = analyze(b"\"", &mut carry);
                    assert_eq!(result.escaped_quote_mask, odd);
                    assert_eq!(result.delimiter_quote_mask, odd ^ 1);
                    assert_eq!(result.in_string_mask, inside & odd);
                    assert_eq!(carry.prev_in_string, inside ^ (odd ^ 1));
                    assert_eq!(carry.prev_escaped, 0);
                    assert_eq!(carry.utf8_tail, initial.utf8_tail);
                    assert_eq!(carry.utf8_tail_len, initial.utf8_tail_len);
                }
            }
        }
    }

    #[test]
    fn unfinished_strings_and_invalid_json_bytes_are_lexical_successes() {
        for input in [
            &b"\"unfinished\\"[..],
            &b"\"\x00\x1f\xff\\q\""[..],
            &b"\\\"}{][,NaN"[..],
            &b"\xc0\xaf\xed\xa0\x80\xf5\xff"[..],
        ] {
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    assert_matches(input, carry_with(inside, escaped));
                }
            }
        }
        let mut carry = carry_with(0, 0);
        analyze(b"\"unfinished\\", &mut carry);
        assert_eq!(carry.prev_in_string, 1);
        assert_eq!(carry.prev_escaped, 1);
    }

    #[test]
    fn all_oversized_lengths_fail_without_mutating_carry() {
        for len in 65..=u8::MAX {
            for initial in [carry_with(0, 0), carry_with(1, 1), carry_with(2, u64::MAX)] {
                assert_error(u64::MAX, u64::MAX, len, initial, ErrorCode::InvalidToken);
            }
        }
    }

    #[test]
    fn noncanonical_quote_carry_is_rejected_even_for_empty_input() {
        for invalid in [2, 3, 255, 1u64 << 32, 1u64 << 63, u64::MAX] {
            for initial in [
                carry_with(invalid, 0), carry_with(invalid, 1),
                carry_with(0, invalid), carry_with(1, invalid),
                carry_with(invalid, invalid),
            ] {
                for len in 0..=BLOCK_BYTES {
                    assert_error(0, 0, len as u8, initial, ErrorCode::InternalError);
                }
            }
        }
    }

    #[test]
    fn every_live_overlap_is_rejected_atomically_but_padding_overlap_is_ignored() {
        for position in 0..BLOCK_BYTES {
            let bit = 1u64 << position;
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let initial = carry_with(inside, escaped);
                    for len in position + 1..=BLOCK_BYTES {
                        assert_error(bit, bit, len as u8, initial, ErrorCode::InternalError);
                    }
                    let mut carry = initial;
                    let actual = quote_masks(bit, bit, position as u8, &mut carry).unwrap();
                    let mut expected_carry = initial;
                    let expected = reference(&[b'x'; BLOCK_BYTES][..position], &mut expected_carry);
                    assert_eq!(actual, expected);
                    assert_eq!(carry, expected_carry);
                }
            }
        }
    }

    #[test]
    fn every_utf8_tail_length_is_preserved_on_success_and_failure() {
        for tail_len in 0..=u8::MAX {
            let mut carry = carry_with(1, 1);
            carry.utf8_tail_len = tail_len;
            let initial = carry;
            let result = analyze(b"\\\"x", &mut carry);
            assert_eq!(result.delimiter_quote_mask, 2);
            assert_eq!(carry.prev_in_string, 0);
            assert_eq!(carry.prev_escaped, 0);
            assert_eq!(carry.utf8_tail, initial.utf8_tail);
            assert_eq!(carry.utf8_tail_len, initial.utf8_tail_len);
            assert_error(1, 1, 1, initial, ErrorCode::InternalError);
            assert_error(0, 0, 65, initial, ErrorCode::InvalidToken);
        }
    }
}
