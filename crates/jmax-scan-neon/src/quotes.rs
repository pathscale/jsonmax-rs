//! Quote and escape resolution over the combined four-lane classification.
//!
//! Portable integer operations preserve byte order across lane boundaries and
//! work on every target, without allocation or additional CPU requirements.

use super::*;

/// Resolve quotes in the low `valid_len` positions and advance quote carry.
///
/// Odd consecutive backslash runs escape the following byte, even outside a
/// string. Unescaped quotes toggle string state and are excluded from string
/// content. Escaped quotes are content only when already inside a string.
/// Incoming escape parity extends only a run starting at position zero.
///
/// Bits beyond the valid length are ignored. Empty blocks preserve carry;
/// UTF-8 state is always opaque and unchanged. Unclosed strings and trailing
/// escapes are valid intermediate states.
///
/// # Errors
///
/// Lengths above 64 return `InvalidToken`. Noncanonical quote carry or live
/// quote/backslash overlap returns `InternalError`. Errors originate from
/// `Scanner` at offset zero and leave every carry field unchanged.
pub fn quote_masks(
    quote_mask: u64,
    backslash_mask: u64,
    valid_len: u8,
    carry: &mut ScanCarry,
) -> Result<QuoteMasks> {
    let valid = valid_mask(usize::from(valid_len))?;
    validate_carry(carry)?;
    let quotes = quote_mask & valid;
    let mut backslashes = backslash_mask & valid;
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

    // Bit zero may follow an odd run from the preceding block. For every
    // local run, only its first following byte can be an escaped quote.
    let mut escaped_positions = carry.prev_escaped;
    let mut trailing_parity = 0u64;
    while backslashes != 0 {
        let start = backslashes.trailing_zeros();
        let run_len = (backslashes >> start).trailing_ones();
        let end = start + run_len;
        let mut parity = u64::from(run_len & 1);
        if start == 0 {
            parity ^= carry.prev_escaped;
        }
        if end == u32::from(valid_len) {
            trailing_parity = parity;
        } else if parity != 0 {
            // end < valid_len <= 64, so this never shifts by the word width.
            escaped_positions |= 1u64 << end;
        }

        // Remove this entire run. A run reaching bit 63 needs a separate
        // branch to avoid a shift by 64, including an all-backslash block.
        backslashes = if end < 64 {
            backslashes & (u64::MAX << end)
        } else {
            0
        };
    }

    let escaped_quote_mask = quotes & escaped_positions;
    let delimiter_quote_mask = quotes & !escaped_positions;

    // Inclusive prefix XOR: bit i is the parity of delimiters at 0..=i.
    // All four NEON lanes participate in one prefix, with no lane reset.
    let mut prefix = delimiter_quote_mask;
    prefix ^= prefix << 1;
    prefix ^= prefix << 2;
    prefix ^= prefix << 4;
    prefix ^= prefix << 8;
    prefix ^= prefix << 16;
    prefix ^= prefix << 32;
    if carry.prev_in_string != 0 {
        prefix = !prefix;
    }
    let result = QuoteMasks {
        escaped_quote_mask,
        delimiter_quote_mask,
        in_string_mask: prefix & !delimiter_quote_mask & valid,
    };

    // Commit only after validation and calculation. Inspect the last actual
    // byte, never padding, and modify only the two fields owned by quoting.
    carry.prev_in_string = (prefix >> (valid_len - 1)) & 1;
    carry.prev_escaped = trailing_parity;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(inside: u64, escaped: u64) -> ScanCarry {
        ScanCarry {
            prev_in_string: inside,
            prev_escaped: escaped,
            utf8_tail: [0xf0, 0x80, 0xff, 0x00],
            // Deliberately invalid for a UTF-8 consumer, but opaque here.
            utf8_tail_len: 255,
        }
    }

    fn raw(input: &[u8]) -> (u64, u64) {
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

    // Independent byte-state oracle: no mask-prefix or run-scanning logic.
    fn reference(input: &[u8], carry: &mut ScanCarry) -> QuoteMasks {
        let mut result = QuoteMasks::default();
        for (index, &byte) in input.iter().enumerate() {
            let bit = 1u64 << index;
            let delimiter = byte == b'"' && carry.prev_escaped == 0;
            if byte == b'"' {
                if delimiter {
                    result.delimiter_quote_mask |= bit;
                } else {
                    result.escaped_quote_mask |= bit;
                }
            }
            if delimiter {
                carry.prev_in_string ^= 1;
            } else if carry.prev_in_string != 0 {
                result.in_string_mask |= bit;
            }
            carry.prev_escaped = if byte == b'\\' {
                carry.prev_escaped ^ 1
            } else {
                0
            };
        }
        result
    }

    fn resolve(input: &[u8], carry: &mut ScanCarry) -> QuoteMasks {
        let (quotes, backslashes) = raw(input);
        quote_masks(quotes, backslashes, input.len() as u8, carry).unwrap()
    }

    fn assert_reference(input: &[u8], initial: ScanCarry) {
        let mut expected_state = initial;
        let expected = reference(input, &mut expected_state);
        let mut actual_state = initial;
        let actual = resolve(input, &mut actual_state);
        assert_eq!(actual, expected, "input {:?}, carry {:?}", input, initial);
        assert_eq!(actual_state, expected_state);
        let (quotes, _) = raw(input);
        assert_eq!(actual.escaped_quote_mask | actual.delimiter_quote_mask, quotes);
        assert_eq!(actual.escaped_quote_mask & actual.delimiter_quote_mask, 0);
        assert_eq!(actual.in_string_mask & actual.delimiter_quote_mask, 0);
    }

    #[test]
    fn exact_quote_and_escape_vectors() {
        let vectors: &[(&[u8], QuoteMasks, u64, u64)] = &[
            (b"", QuoteMasks {
                escaped_quote_mask: 0, delimiter_quote_mask: 0, in_string_mask: 0,
            }, 0, 0),
            (b"\"\"", QuoteMasks {
                escaped_quote_mask: 0, delimiter_quote_mask: 0b11, in_string_mask: 0,
            }, 0, 0),
            (b"\"ab\"", QuoteMasks {
                escaped_quote_mask: 0, delimiter_quote_mask: 0b1001,
                in_string_mask: 0b0110,
            }, 0, 0),
            (b"\"a\\\"b\"", QuoteMasks {
                escaped_quote_mask: 0b001000, delimiter_quote_mask: 0b100001,
                in_string_mask: 0b011110,
            }, 0, 0),
            (b"\\\"", QuoteMasks {
                escaped_quote_mask: 0b10, delimiter_quote_mask: 0, in_string_mask: 0,
            }, 0, 0),
            (b"\"x\\", QuoteMasks {
                escaped_quote_mask: 0, delimiter_quote_mask: 1, in_string_mask: 0b110,
            }, 1, 1),
            (b"\\\\\"", QuoteMasks {
                escaped_quote_mask: 0, delimiter_quote_mask: 0b100, in_string_mask: 0,
            }, 1, 0),
        ];
        for &(input, expected, inside, escaped) in vectors {
            let mut carry = state(0, 0);
            assert_eq!(resolve(input, &mut carry), expected, "input {:?}", input);
            assert_eq!(carry, state(inside, escaped));
        }
    }

    #[test]
    fn empty_masks_preserve_all_four_states_even_with_dirty_padding() {
        for inside in 0..=1 {
            for escaped in 0..=1 {
                let initial = state(inside, escaped);
                let mut carry = initial;
                assert_eq!(quote_masks(u64::MAX, u64::MAX, 0, &mut carry).unwrap(),
                    QuoteMasks::default());
                assert_eq!(carry, initial);
            }
        }
    }

    #[test]
    fn exhaustive_short_sequences_and_incoming_states() {
        let alphabet = [b'x', b'"', b'\\'];
        for len in 0..=8usize {
            for mut code in 0..3usize.pow(len as u32) {
                let mut input = [0u8; 8];
                for byte in &mut input[..len] {
                    *byte = alphabet[code % 3];
                    code /= 3;
                }
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        assert_reference(&input[..len], state(inside, escaped));
                    }
                }
            }
        }
    }

    #[test]
    fn every_run_length_before_every_quote_position() {
        for position in 0..BLOCK_BYTES {
            for run_len in 0..=position {
                let mut input = [b'x'; BLOCK_BYTES];
                input[position - run_len..position].fill(b'\\');
                input[position] = b'"';
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        let initial = state(inside, escaped);
                        assert_reference(&input, initial);
                        assert_reference(&input[..position + 1], initial);
                    }
                }
            }
        }
    }

    #[test]
    fn uniform_blocks_and_all_tail_lengths() {
        for len in 0..=BLOCK_BYTES {
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    for byte in [b'x', b'"', b'\\', 0, 0x1f, 0x80, 0xff] {
                        assert_reference(&[byte; BLOCK_BYTES][..len], state(inside, escaped));
                    }
                    let mut carry = state(inside, escaped);
                    let masks = resolve(&[b'\\'; BLOCK_BYTES][..len], &mut carry);
                    assert_eq!(carry.prev_in_string, inside);
                    assert_eq!(carry.prev_escaped, escaped ^ (len as u64 & 1));
                    assert_eq!(masks.delimiter_quote_mask, 0);
                    assert_eq!(masks.escaped_quote_mask, 0);
                }
            }
        }
    }

    #[test]
    fn incoming_escape_is_consumed_by_exactly_the_first_non_backslash() {
        for inside in 0..=1 {
            let mut carry = state(inside, 1);
            let masks = resolve(b"\"\"", &mut carry);
            assert_eq!(masks.escaped_quote_mask, 1);
            assert_eq!(masks.delimiter_quote_mask, 2);
            assert_eq!(masks.in_string_mask, inside);
            assert_eq!(carry, state(inside ^ 1, 0));
            for input in [b"x\"".as_slice(), b"\0\"", b"\xff\"", b"\\\""] {
                let mut carry = state(inside, 1);
                let masks = resolve(input, &mut carry);
                assert_eq!(masks.escaped_quote_mask, 0);
                assert_eq!(masks.delimiter_quote_mask, 2);
                assert_eq!(carry, state(inside ^ 1, 0));
            }
        }
    }

    #[test]
    fn padded_bits_are_ignored_and_do_not_advance_carry() {
        let mut input = [b'\\'; BLOCK_BYTES];
        for index in (0..BLOCK_BYTES).step_by(3) {
            input[index] = b'"';
        }
        for len in 0..=BLOCK_BYTES {
            let (quotes, backslashes) = raw(&input[..len]);
            let live = if len == BLOCK_BYTES { u64::MAX } else { (1u64 << len) - 1 };
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let initial = state(inside, escaped);
                    let mut expected_state = initial;
                    let expected = reference(&input[..len], &mut expected_state);
                    let mut carry = initial;
                    // All padded bits overlap intentionally and must be ignored.
                    let actual = quote_masks(quotes | !live, backslashes | !live,
                        len as u8, &mut carry).unwrap();
                    assert_eq!(actual, expected);
                    assert_eq!(carry, expected_state);
                    assert_eq!((actual.escaped_quote_mask | actual.delimiter_quote_mask
                        | actual.in_string_mask) & !live, 0);
                }
            }
        }
        for len in 1..=BLOCK_BYTES {
            let mut carry = state(1, 0);
            quote_masks(0, 1u64 << (len - 1), len as u8, &mut carry).unwrap();
            assert_eq!(carry, state(1, 1));
        }
    }

    #[test]
    fn high_bit_and_full_width_prefix_have_exact_masks() {
        let mut carry = state(0, 0);
        let masks = quote_masks(1 | (1u64 << 63), 0, 64, &mut carry).unwrap();
        assert_eq!(masks, QuoteMasks {
            escaped_quote_mask: 0,
            delimiter_quote_mask: 0x8000_0000_0000_0001,
            in_string_mask: 0x7fff_ffff_ffff_fffe,
        });
        assert_eq!(carry, state(0, 0));
        let masks = quote_masks(1u64 << 63, 1u64 << 62, 64, &mut carry).unwrap();
        assert_eq!(masks.escaped_quote_mask, 1u64 << 63);
        assert_eq!(masks.delimiter_quote_mask, 0);
        assert_eq!(masks.in_string_mask, 0);
        assert_eq!(carry, state(0, 0));
        for inside in 0..=1 {
            let mut carry = state(inside, 0);
            let masks = quote_masks(u64::MAX, 0, 64, &mut carry).unwrap();
            assert_eq!(masks.delimiter_quote_mask, u64::MAX);
            assert_eq!(masks.in_string_mask, 0);
            assert_eq!(carry, state(inside, 0));
        }
    }

    fn components(masks: QuoteMasks) -> [u64; 3] {
        [masks.escaped_quote_mask, masks.delimiter_quote_mask, masks.in_string_mask]
    }

    #[test]
    fn every_split_matches_continuous_processing() {
        let mut input = [b'\\'; BLOCK_BYTES];
        for position in [0, 3, 15, 16, 31, 32, 47, 48, 63] {
            input[position] = b'"';
        }
        input[7] = b'x';
        input[25] = 0xff;
        for inside in 0..=1 {
            for escaped in 0..=1 {
                let initial = state(inside, escaped);
                let mut expected_state = initial;
                let expected = components(reference(&input, &mut expected_state));
                for split in 0..=BLOCK_BYTES {
                    let mut carry = initial;
                    let left = components(resolve(&input[..split], &mut carry));
                    let before_empty = carry;
                    assert_eq!(resolve(b"", &mut carry), QuoteMasks::default());
                    assert_eq!(carry, before_empty);
                    let right = components(resolve(&input[split..], &mut carry));
                    for index in 0..3 {
                        let joined = if split == BLOCK_BYTES {
                            assert_eq!(right[index], 0);
                            left[index]
                        } else {
                            left[index] | (right[index] << split)
                        };
                        assert_eq!(joined, expected[index], "split {}, mask {}", split, index);
                    }
                    assert_eq!(carry, expected_state);
                }
            }
        }
    }

    #[test]
    fn backslash_runs_continue_across_full_blocks() {
        for trailing in 0..=BLOCK_BYTES {
            let mut first = [b'x'; BLOCK_BYTES];
            first[BLOCK_BYTES - trailing..].fill(b'\\');
            for leading in 0..BLOCK_BYTES {
                let mut second = [b'x'; BLOCK_BYTES];
                second[..leading].fill(b'\\');
                second[leading] = b'"';
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        let mut carry = state(inside, escaped);
                        let mut expected_state = carry;
                        assert_eq!(resolve(&first, &mut carry),
                            reference(&first, &mut expected_state));
                        assert_eq!(carry, expected_state);
                        assert_eq!(resolve(&second, &mut carry),
                            reference(&second, &mut expected_state));
                        assert_eq!(carry, expected_state);
                    }
                }
            }
        }
    }

    #[test]
    fn deterministic_mixed_blocks_at_every_length() {
        let alphabet = [b'"', b'\\', b'\\', b'x', b':', 0, 0x1f, 0x80, 0xff];
        let mut seed = 0x6a09_e667_f3bc_c909u64;
        for _ in 0..128 {
            let mut input = [0u8; BLOCK_BYTES];
            for byte in &mut input {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *byte = alphabet[(seed % alphabet.len() as u64) as usize];
            }
            for len in 0..=BLOCK_BYTES {
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        assert_reference(&input[..len], state(inside, escaped));
                    }
                }
            }
        }
    }

    fn assert_error(quotes: u64, backslashes: u64, len: u8,
        initial: ScanCarry, code: ErrorCode)
    {
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
    fn every_oversized_length_is_rejected_before_state_validation() {
        for len in 65u8..=u8::MAX {
            for initial in [state(0, 0), state(1, 1), state(u64::MAX, 2)] {
                assert_error(u64::MAX, u64::MAX, len, initial, ErrorCode::InvalidToken);
            }
        }
    }

    #[test]
    fn noncanonical_carry_is_rejected_without_mutation() {
        for bad in [2, 3, 255, 256, 1u64 << 32, 1u64 << 63, u64::MAX] {
            for good in 0..=1 {
                for initial in [state(bad, good), state(good, bad), state(bad, bad)] {
                    for len in 0..=BLOCK_BYTES {
                        assert_error(0, 0, len as u8, initial, ErrorCode::InternalError);
                    }
                }
            }
        }
    }

    #[test]
    fn overlap_at_every_live_position_is_rejected_transactionally() {
        for len in 1..=BLOCK_BYTES {
            for position in 0..len {
                let bit = 1u64 << position;
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        assert_error(bit, bit, len as u8, state(inside, escaped),
                            ErrorCode::InternalError);
                    }
                }
            }
        }
    }

    #[test]
    fn every_utf8_tail_length_is_preserved_without_interpretation() {
        for tail_len in 0u8..=u8::MAX {
            let mut initial = state(1, 1);
            initial.utf8_tail = [tail_len, !tail_len, 0x80, 0xff];
            initial.utf8_tail_len = tail_len;
            assert_reference(b"", initial);
            assert_reference(b"\\\"x\"\\", initial);
            assert_error(1, 1, 1, initial, ErrorCode::InternalError);
        }
    }
}
