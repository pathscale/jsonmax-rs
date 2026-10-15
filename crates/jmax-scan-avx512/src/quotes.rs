//! Quote and escape carry analysis over a complete 64-bit predicate mask.
//!
//! Parallel prefix operations cross every byte boundary in the mask. They use
//! only portable integer operations, so this module requires no CPU features,
//! allocations, byte loads, or unsafe code.

use super::*;

/// Analyze the live quote and backslash bits and commit the outgoing carry.
///
/// An odd consecutive backslash run escapes the following quote, including
/// outside strings and across block boundaries. Unescaped quotes toggle the
/// string state but are excluded from string content. Escaped quotes belong
/// to string content only when the current state is inside a string.
///
/// Bits at or above `valid_len` are ignored. Empty input preserves all carry;
/// nonempty input changes only the two quote state fields. UTF-8 state remains
/// opaque, and unfinished strings and escape runs are valid outgoing states.
///
/// # Errors
///
/// Lengths above 64 produce `InvalidToken`. Noncanonical quote carry or live
/// quote/backslash overlap produces `InternalError`. All errors have source
/// `Scanner` and offset zero, and leave every carry field unchanged. Length
/// validation takes precedence over carry and mask validation.
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

    let mut odd_after = backslash_run_parity(backslashes);
    if carry.prev_escaped != 0 {
        // Incoming parity affects precisely the initial uninterrupted run.
        // Adding one clears its low one bits. Wrapping handles 64 backslashes.
        let initial_run = backslashes & !backslashes.wrapping_add(1);
        odd_after ^= initial_run;
    }
    let escaped = quotes & ((odd_after << 1) | carry.prev_escaped);
    let delimiters = quotes & !escaped;

    // Inclusive XOR prefix: bit i is the parity of delimiters through byte i.
    let mut string_parity = delimiters;
    string_parity ^= string_parity << 1;
    string_parity ^= string_parity << 2;
    string_parity ^= string_parity << 4;
    string_parity ^= string_parity << 8;
    string_parity ^= string_parity << 16;
    string_parity ^= string_parity << 32;
    let inside = if carry.prev_in_string != 0 {
        !string_parity
    } else {
        string_parity
    };
    let result = QuoteMasks {
        escaped_quote_mask: escaped,
        delimiter_quote_mask: delimiters,
        in_string_mask: inside & !delimiters & valid,
    };

    // Validation and all fallible operations precede these writes. The final
    // live bit is in 0..=63, even for a full block; padding cannot affect carry.
    let last = u32::from(valid_len) - 1;
    carry.prev_in_string ^= (string_parity >> last) & 1;
    carry.prev_escaped = (odd_after >> last) & 1;
    Ok(result)
}

/// Bit i is the parity of the backslash run ending at i, with zero input carry.
#[inline]
fn backslash_run_parity(backslashes: u64) -> u64 {
    // Per-byte state obeys p[i] = b[i] XOR (b[i] AND p[i - 1]).
    // Compose these affine Boolean transitions in spans of 1, 2, 4, ... bytes.
    // A span propagates earlier state only when every byte is a backslash.
    // Left shifts supply zero state below byte zero, never a lane-local reset.
    let mut parity = backslashes;
    let mut propagate = backslashes;
    for shift in [1u32, 2, 4, 8, 16, 32] {
        parity ^= propagate & (parity << shift);
        propagate &= propagate << shift;
    }
    parity
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initial_carry(inside: u64, escaped: u64) -> ScanCarry {
        ScanCarry {
            prev_in_string: inside,
            prev_escaped: escaped,
            utf8_tail: [0xf0, 0x80, 0xff, 0x00],
            // Intentionally not validated by this quote-only layer.
            utf8_tail_len: 0xff,
        }
    }

    fn raw_masks(bytes: &[u8]) -> (u64, u64) {
        assert!(bytes.len() <= BLOCK_BYTES);
        let mut quotes = 0;
        let mut backslashes = 0;
        for (index, &byte) in bytes.iter().enumerate() {
            if byte == b'"' {
                quotes |= 1u64 << index;
            } else if byte == b'\\' {
                backslashes |= 1u64 << index;
            }
        }
        (quotes, backslashes)
    }

    // Independent sequential oracle, without prefix or root mask helpers.
    fn reference(quotes: u64, backslashes: u64, len: u8, carry: &mut ScanCarry) -> QuoteMasks {
        let mut result = QuoteMasks::default();
        let mut inside = carry.prev_in_string != 0;
        let mut odd = carry.prev_escaped != 0;
        for index in 0..len {
            let bit = 1u64 << index;
            let quote = quotes & bit != 0;
            let slash = backslashes & bit != 0;
            assert!(!(quote && slash));
            if quote && !odd {
                result.delimiter_quote_mask |= bit;
                inside = !inside;
            } else if inside {
                result.in_string_mask |= bit;
            }
            if quote && odd {
                result.escaped_quote_mask |= bit;
            }
            odd = slash && !odd;
        }
        carry.prev_in_string = u64::from(inside);
        carry.prev_escaped = u64::from(odd);
        result
    }

    fn compare(quotes: u64, backslashes: u64, len: u8, initial: ScanCarry) -> QuoteMasks {
        let mut actual_carry = initial;
        let mut expected_carry = initial;
        let actual = quote_masks(quotes, backslashes, len, &mut actual_carry).unwrap();
        let expected = reference(quotes, backslashes, len, &mut expected_carry);
        assert_eq!(actual, expected,
            "quotes={quotes:#018x}, backslashes={backslashes:#018x}, len={len}, carry={initial:?}");
        assert_eq!(actual_carry, expected_carry);
        assert_eq!(actual.escaped_quote_mask & actual.delimiter_quote_mask, 0);
        assert_eq!(actual.in_string_mask & actual.delimiter_quote_mask, 0);
        let live = valid_mask(usize::from(len)).unwrap();
        assert_eq!(actual.escaped_quote_mask | actual.delimiter_quote_mask, quotes & live);
        assert_eq!(actual.in_string_mask & !live, 0);
        actual
    }

    #[test]
    fn signature_and_empty_masks_preserve_all_carry() {
        let entry: fn(u64, u64, u8, &mut ScanCarry) -> Result<QuoteMasks> = quote_masks;
        for inside in 0..=1 {
            for escaped in 0..=1 {
                let mut carry = initial_carry(inside, escaped);
                let before = carry;
                // Even overlapping bits are irrelevant when no bytes are live.
                assert_eq!(entry(u64::MAX, u64::MAX, 0, &mut carry).unwrap(),
                    QuoteMasks::default());
                assert_eq!(carry, before);
            }
        }
    }

    #[test]
    fn literal_masks_exclude_both_delimiters_and_include_escaped_content() {
        let bytes = [b'{', b'"', b':', b'\\', b'"', b',', b'"', b':', b'9', b'}'];
        let (quotes, backslashes) = raw_masks(&bytes);
        let mut carry = initial_carry(0, 0);
        let actual = quote_masks(quotes, backslashes, 10, &mut carry).unwrap();
        assert_eq!(actual, QuoteMasks {
            escaped_quote_mask: 0x10,
            delimiter_quote_mask: 0x42,
            in_string_mask: 0x3c,
        });
        assert_eq!(carry, initial_carry(0, 0));

        let mut carry = initial_carry(0, 0);
        assert_eq!(quote_masks(0b10, 0b01, 2, &mut carry).unwrap(), QuoteMasks {
            escaped_quote_mask: 0b10,
            delimiter_quote_mask: 0,
            in_string_mask: 0,
        });
        assert_eq!(carry, initial_carry(0, 0));

        let mut carry = initial_carry(1, 1);
        assert_eq!(quote_masks(0b1, 0, 1, &mut carry).unwrap(), QuoteMasks {
            escaped_quote_mask: 1,
            delimiter_quote_mask: 0,
            in_string_mask: 1,
        });
        assert_eq!(carry, initial_carry(1, 0));
    }

    #[test]
    fn exhaustive_short_ternary_blocks_and_all_incoming_states() {
        // Enumerate every ordinary-byte / quote / backslash sequence up to 8.
        let mut combinations = 1u32;
        for len in 0u8..=8 {
            for pattern in 0..combinations {
                let mut remaining = pattern;
                let mut quotes = 0;
                let mut backslashes = 0;
                for index in 0..len {
                    match remaining % 3 {
                        1 => quotes |= 1u64 << index,
                        2 => backslashes |= 1u64 << index,
                        _ => {}
                    }
                    remaining /= 3;
                }
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        compare(quotes, backslashes, len, initial_carry(inside, escaped));
                    }
                }
            }
            combinations *= 3;
        }
    }

    #[test]
    fn every_length_clips_padding_and_ignores_overlap_in_padding() {
        for len in 0u8..=64 {
            let live = valid_mask(usize::from(len)).unwrap();
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let initial = initial_carry(inside, escaped);
                    for (quotes, backslashes) in [
                        (0, 0), (live, 0), (0, live),
                        (0x5555_5555_5555_5555 & live, 0xaaaa_aaaa_aaaa_aaaa & live),
                        (0xaaaa_aaaa_aaaa_aaaa & live, 0x5555_5555_5555_5555 & live),
                    ] {
                        let clean = compare(quotes, backslashes, len, initial);
                        let padded = compare(quotes | !live, backslashes | !live, len, initial);
                        assert_eq!(clean, padded);
                    }
                }
            }
        }
    }

    #[test]
    fn runs_at_every_start_and_end_cross_all_internal_boundaries() {
        for start in 0..BLOCK_BYTES {
            let mut backslashes = 0;
            for end in start..BLOCK_BYTES {
                // The quote immediately follows a run of end - start bytes.
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        let initial = initial_carry(inside, escaped);
                        compare(1u64 << end, backslashes, (end + 1) as u8, initial);
                        compare(1u64 << end, backslashes, 64, initial);
                    }
                }
                backslashes |= 1u64 << end;
            }
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    compare(0, backslashes, 64, initial_carry(inside, escaped));
                }
            }
        }
    }

    #[test]
    fn full_width_runs_and_high_bit_carry_are_preserved() {
        for inside in 0..=1 {
            for escaped in 0..=1 {
                let mut carry = initial_carry(inside, escaped);
                for _ in 0..3 {
                    let masks = quote_masks(0, u64::MAX, 64, &mut carry).unwrap();
                    assert_eq!(masks.in_string_mask, if inside == 1 { u64::MAX } else { 0 });
                    assert_eq!(masks.escaped_quote_mask | masks.delimiter_quote_mask, 0);
                    assert_eq!(carry, initial_carry(inside, escaped));
                }
                let masks = quote_masks(1, 0, 1, &mut carry).unwrap();
                assert_eq!(masks.escaped_quote_mask, escaped);
                assert_eq!(masks.delimiter_quote_mask, escaped ^ 1);
                assert_eq!(masks.in_string_mask, inside & escaped);
                assert_eq!(carry, initial_carry(inside ^ (escaped ^ 1), 0));
            }
        }
        let mut carry = initial_carry(0, 0);
        let masks = quote_masks(1u64 << 63, 0, 64, &mut carry).unwrap();
        assert_eq!(masks.delimiter_quote_mask, 1u64 << 63);
        assert_eq!(masks.in_string_mask, 0);
        assert_eq!(carry, initial_carry(1, 0));
        let masks = quote_masks(0, 1u64 << 63, 64, &mut carry).unwrap();
        assert_eq!(masks.in_string_mask, u64::MAX);
        assert_eq!(carry, initial_carry(1, 1));
    }

    #[test]
    fn non_backslash_resets_incoming_escape_and_strings_can_remain_open() {
        for inside in 0..=1 {
            let mut carry = initial_carry(inside, 1);
            // An ordinary byte resets the incoming run before the quote.
            let masks = quote_masks(0b10, 0, 2, &mut carry).unwrap();
            assert_eq!(masks.escaped_quote_mask, 0);
            assert_eq!(masks.delimiter_quote_mask, 0b10);
            assert_eq!(masks.in_string_mask, inside);
            assert_eq!(carry, initial_carry(inside ^ 1, 0));
        }
        let mut carry = initial_carry(0, 0);
        let masks = quote_masks(1, 0b1000, 4, &mut carry).unwrap();
        assert_eq!(masks.in_string_mask, 0b1110);
        assert_eq!(carry, initial_carry(1, 1));
    }

    #[test]
    fn deterministic_mixed_masks_match_byte_oracle_at_every_length() {
        let mut seed = 0x6a09_e667_f3bc_c909u64;
        for _ in 0..128 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let quotes = seed;
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let backslashes = seed & !quotes;
            for len in 0..=64 {
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        compare(quotes, backslashes, len, initial_carry(inside, escaped));
                    }
                }
            }
        }
    }

    #[test]
    fn every_stream_split_matches_continuous_state_and_byte_masks() {
        let pattern = [b'"', b'\\', b'\\', b'"', b'x', b'\\', b'"', b'\\', b'\\', b'\\', b'"'];
        let mut stream = [0u8; 192];
        for (index, byte) in stream.iter_mut().enumerate() {
            *byte = pattern[index % pattern.len()];
        }
        stream[60..68].fill(b'\\');
        stream[68] = b'"';
        stream[125..130].fill(b'\\');
        stream[130] = b'"';

        for split in 0..=BLOCK_BYTES {
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let mut carry = initial_carry(inside, escaped);
                    let mut expected_carry = carry;
                    let mut start = 0;
                    let mut end = split;
                    loop {
                        let part = &stream[start..end];
                        let (quotes, backslashes) = raw_masks(part);
                        let actual = quote_masks(quotes, backslashes, part.len() as u8,
                            &mut carry).unwrap();
                        let mut expected = QuoteMasks::default();
                        // Advance the oracle one byte at a time across the entire stream.
                        for (index, byte) in part.iter().enumerate() {
                            let single = reference(u64::from(*byte == b'"'),
                                u64::from(*byte == b'\\'), 1, &mut expected_carry);
                            expected.escaped_quote_mask |= single.escaped_quote_mask << index;
                            expected.delimiter_quote_mask |= single.delimiter_quote_mask << index;
                            expected.in_string_mask |= single.in_string_mask << index;
                        }
                        assert_eq!(actual, expected, "split={split}, start={start}");
                        assert_eq!(carry, expected_carry);
                        let before_empty = carry;
                        assert_eq!(quote_masks(u64::MAX, u64::MAX, 0, &mut carry).unwrap(),
                            QuoteMasks::default());
                        assert_eq!(carry, before_empty);
                        if end == stream.len() {
                            break;
                        }
                        start = end;
                        end = core::cmp::min(start + BLOCK_BYTES, stream.len());
                    }
                }
            }
        }
    }

    fn assert_error_unchanged(quotes: u64, backslashes: u64, len: u8,
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
    fn every_oversized_length_is_rejected_before_other_errors() {
        for len in 65u8..=u8::MAX {
            for initial in [initial_carry(0, 0), initial_carry(1, 1),
                initial_carry(u64::MAX, u64::MAX)] {
                assert_error_unchanged(u64::MAX, u64::MAX, len, initial,
                    ErrorCode::InvalidToken);
            }
        }
    }

    #[test]
    fn invalid_quote_carry_is_rejected_including_on_empty_input() {
        for invalid in [2, 3, 255, 256, 1u64 << 32, 1u64 << 63, u64::MAX] {
            for valid in 0..=1 {
                for initial in [initial_carry(invalid, valid), initial_carry(valid, invalid),
                    initial_carry(invalid, invalid)] {
                    for len in 0..=64 {
                        assert_error_unchanged(1, 2, len, initial, ErrorCode::InternalError);
                    }
                }
            }
        }
    }

    #[test]
    fn overlap_at_every_live_position_is_transactional() {
        for len in 1u8..=64 {
            for index in 0..len {
                let bit = 1u64 << index;
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        assert_error_unchanged(bit, bit, len, initial_carry(inside, escaped),
                            ErrorCode::InternalError);
                    }
                }
            }
        }
    }

    #[test]
    fn every_utf8_tail_length_remains_opaque() {
        for tail_len in 0u8..=u8::MAX {
            let initial = ScanCarry {
                utf8_tail: [tail_len, !tail_len, 0x80, 0xff],
                utf8_tail_len: tail_len,
                ..initial_carry(1, 1)
            };
            for len in [0, 1, 31, 32, 33, 63, 64] {
                compare(0x8888_8888_8888_8888, 0x7777_7777_7777_7777, len, initial);
                assert_error_unchanged(1, 1, 64, initial, ErrorCode::InternalError);
            }
        }
    }
}
