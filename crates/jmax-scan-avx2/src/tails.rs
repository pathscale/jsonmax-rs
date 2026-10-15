//! Bounded tail storage and the architecture-independent raw fallback.
//!
//! These functions require neither CPU feature detection nor caller padding.
//! The production fallback depends only on root-owned types and helpers, so
//! dispatching here cannot recurse into the vector or quote implementations.

use super::*;

/// Copy at most 64 bytes into a complete, zero-padded block.
///
/// Only the supplied slice is read. Empty input returns an all-zero array,
/// and a full block is copied unchanged. No alignment is required. Consumers
/// must retain the original length when resolving quotes and clipping masks:
/// the padding is storage, not additional input.
///
/// # Errors
///
/// Lengths above 64 return `InvalidToken`, source `Scanner`, at offset zero.
/// Length validation precedes slicing, copying, or any narrowing conversion.
pub fn load_block(input: &[u8]) -> Result<[u8; BLOCK_BYTES]> {
    valid_mask(input.len())?;
    let mut block = [0; BLOCK_BYTES];
    block[..input.len()].copy_from_slice(input);
    Ok(block)
}

/// Classify every byte of a complete block using portable scalar operations.
///
/// Bit `i` describes byte `i`, including positions 31, 32, and 63, independently
/// of host endianness. All masks are raw: quotes, backslashes, and punctuation
/// inside strings remain present. Whitespace is exactly space, tab, CR, and LF;
/// tab, CR, and LF also belong to the control class. Digits are ASCII only.
///
/// This function always reports `BackendId::Scalar`, allocates nothing, and
/// works without `std` on every architecture. It performs no quote analysis,
/// UTF-8 validation, or JSON grammar validation. Every possible block is valid
/// input, including an all-zero block whose control mask is all ones.
pub fn classify_scalar_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    let mut raw = RawMasks {
        backend: BackendId::Scalar,
        ..RawMasks::default()
    };
    for (index, &byte) in input.iter().enumerate() {
        // The array bounds index to 0..64, so this never shifts by 64.
        let bit = 1u64 << index;
        match byte {
            b'"' => raw.quote_mask |= bit,
            b'\\' => raw.backslash_mask |= bit,
            b':' => raw.classes.colon_mask |= bit,
            b',' => raw.classes.comma_mask |= bit,
            b'{' => raw.classes.lbrace_mask |= bit,
            b'}' => raw.classes.rbrace_mask |= bit,
            b'[' => raw.classes.lbracket_mask |= bit,
            b']' => raw.classes.rbracket_mask |= bit,
            b' ' | b'\t' | b'\r' | b'\n' => raw.classes.whitespace_mask |= bit,
            _ => {}
        }
        // These predicates are independent: control and whitespace overlap.
        if byte < 0x20 {
            raw.classes.control_mask |= bit;
        }
        if byte.is_ascii_digit() {
            raw.classes.digit_mask |= bit;
        }
        if byte >= 0x80 {
            raw.classes.non_ascii_mask |= bit;
        }
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initial_carry(inside: u64, escaped: u64) -> ScanCarry {
        ScanCarry {
            prev_in_string: inside,
            prev_escaped: escaped,
            utf8_tail: [0xff, 0x00, 0xf0, 0x80],
            // Opaque to the scanner, even when not a valid UTF-8 tail length.
            utf8_tail_len: 0xff,
        }
    }

    fn matching(input: &[u8], predicate: impl Fn(u8) -> bool) -> u64 {
        assert!(input.len() <= BLOCK_BYTES);
        input.iter().enumerate().fold(0, |mask, (index, &byte)| {
            mask | (u64::from(predicate(byte)) << index)
        })
    }

    // Independent per-class predicates, with no production classification or
    // padding helper. A short input produces only live bits in this oracle.
    fn reference_raw(input: &[u8]) -> RawMasks {
        RawMasks {
            quote_mask: matching(input, |b| b == 0x22),
            backslash_mask: matching(input, |b| b == 0x5c),
            classes: StructuralMasks {
                colon_mask: matching(input, |b| b == 0x3a),
                comma_mask: matching(input, |b| b == 0x2c),
                lbrace_mask: matching(input, |b| b == 0x7b),
                rbrace_mask: matching(input, |b| b == 0x7d),
                lbracket_mask: matching(input, |b| b == 0x5b),
                rbracket_mask: matching(input, |b| b == 0x5d),
                whitespace_mask: matching(input, |b| [0x20, 0x09, 0x0d, 0x0a].contains(&b)),
                control_mask: matching(input, |b| (0x00..=0x1f).contains(&b)),
                digit_mask: matching(input, |b| (0x30..=0x39).contains(&b)),
                non_ascii_mask: matching(input, |b| b & 0x80 != 0),
            },
            backend: BackendId::Scalar,
        }
    }

    // Sequential bytewise state machine, independent of quote_masks and finish.
    fn reference_block(input: &[u8], carry: &mut ScanCarry) -> BlockMasks {
        let raw = reference_raw(input);
        let c = raw.classes;
        let mut inside = carry.prev_in_string == 1;
        let mut escaped = carry.prev_escaped == 1;
        let mut content = 0;
        for (index, &byte) in input.iter().enumerate() {
            if byte == b'"' && !escaped {
                inside = !inside;
            } else if inside {
                content |= 1u64 << index;
            }
            escaped = byte == b'\\' && !escaped;
        }
        carry.prev_in_string = u64::from(inside);
        carry.prev_escaped = u64::from(escaped);
        let punctuation = c.colon_mask | c.comma_mask | c.lbrace_mask
            | c.rbrace_mask | c.lbracket_mask | c.rbracket_mask;
        BlockMasks {
            quote_mask: raw.quote_mask,
            backslash_mask: raw.backslash_mask,
            in_string_mask: content,
            colon_mask: c.colon_mask,
            comma_mask: c.comma_mask,
            lbrace_mask: c.lbrace_mask,
            rbrace_mask: c.rbrace_mask,
            lbracket_mask: c.lbracket_mask,
            rbracket_mask: c.rbracket_mask,
            structural_mask: punctuation & !content,
            whitespace_mask: c.whitespace_mask,
            control_mask: c.control_mask,
            digit_mask: c.digit_mask,
            non_ascii_mask: c.non_ascii_mask,
            valid_mask: ((1u128 << input.len()) - 1) as u64,
            valid_len: input.len() as u8,
            backend: 0,
        }
    }

    fn fields(m: BlockMasks) -> [u64; 15] {
        [
            m.quote_mask, m.backslash_mask, m.in_string_mask, m.colon_mask,
            m.comma_mask, m.lbrace_mask, m.rbrace_mask, m.lbracket_mask,
            m.rbracket_mask, m.structural_mask, m.whitespace_mask,
            m.control_mask, m.digit_mask, m.non_ascii_mask, m.valid_mask,
        ]
    }

    fn assert_error(error: Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Scanner);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
    }

    fn assert_raw_paths(input: &[u8; BLOCK_BYTES]) {
        let expected = reference_raw(input);
        assert_eq!(classify_scalar_raw(input), expected);
        assert_eq!(structural::classify_structural(input), expected.classes);
        let mut dispatched = expected;
        if is_supported() {
            dispatched.backend = BackendId::Avx2;
        }
        assert_eq!(classify::classify_raw(input), dispatched);
    }

    fn assert_pipeline(input: &[u8], initial: ScanCarry) {
        let mut expected_carry = initial;
        let mut expected = reference_block(input, &mut expected_carry);
        let loaded = load_block(input).unwrap();
        let raw = classify_scalar_raw(&loaded);
        let mut scalar_carry = initial;
        let quotes = quotes::quote_masks(
            raw.quote_mask, raw.backslash_mask, input.len() as u8, &mut scalar_carry,
        ).unwrap();
        assert_eq!(raw.finish(quotes, input.len()).unwrap(), expected);
        assert_eq!(scalar_carry, expected_carry);

        if !input.is_empty() && is_supported() {
            expected.backend = BACKEND_TAG;
        }
        let entry: ClassifyFn = classify::classify;
        let mut actual_carry = initial;
        let actual = entry(input, &mut actual_carry).unwrap();
        assert_eq!(actual, expected, "input {input:?}, carry {initial:?}");
        assert_eq!(actual_carry, expected_carry);
        for mask in fields(actual) {
            assert_eq!(mask & !actual.valid_mask, 0);
        }
        if input.len() == BLOCK_BYTES {
            let mut block_carry = initial;
            assert_eq!(classify::classify_block(&loaded, &mut block_carry).unwrap(), actual);
            assert_eq!(block_carry, actual_carry);
        }
    }

    #[test]
    fn frozen_signatures_and_literal_raw_masks() {
        let load: fn(&[u8]) -> Result<[u8; BLOCK_BYTES]> = load_block;
        let scalar: fn(&[u8; BLOCK_BYTES]) -> RawMasks = classify_scalar_raw;
        let sample = b"\"\\:,{}[] \t\r\n\x00\x1f09/\x7f\x80\xff";
        let mut block = [b'x'; BLOCK_BYTES];
        block[..sample.len()].copy_from_slice(sample);
        assert_eq!(load(&block).unwrap(), block);
        assert_eq!(scalar(&block), RawMasks {
            quote_mask: 0x1,
            backslash_mask: 0x2,
            classes: StructuralMasks {
                colon_mask: 0x4,
                comma_mask: 0x8,
                lbrace_mask: 0x10,
                rbrace_mask: 0x20,
                lbracket_mask: 0x40,
                rbracket_mask: 0x80,
                whitespace_mask: 0x0f00,
                control_mask: 0x3e00,
                digit_mask: 0xc000,
                non_ascii_mask: 0x000c_0000,
            },
            backend: BackendId::Scalar,
        });
    }

    #[test]
    fn every_length_and_alignment_copies_only_the_slice_and_zeros_the_tail() {
        #[repr(C, align(32))]
        struct Aligned([u8; BLOCK_BYTES + LANE_BYTES]);

        for offset in 0..LANE_BYTES {
            for len in 0..=BLOCK_BYTES {
                let mut storage = Aligned([0xa5; BLOCK_BYTES + LANE_BYTES]);
                for (index, byte) in storage.0[offset..offset + len].iter_mut().enumerate() {
                    *byte = (index as u8).wrapping_mul(37).wrapping_add(11);
                }
                let before = storage.0;
                let input = &storage.0[offset..offset + len];
                assert_eq!(input.as_ptr() as usize % LANE_BYTES, offset);
                let actual = load_block(input).unwrap();
                assert_eq!(&actual[..len], input);
                assert_eq!(&actual[len..], &[0; BLOCK_BYTES][len..]);
                assert_eq!(storage.0, before);
            }
        }
    }

    #[test]
    fn exact_small_objects_and_end_slices_need_no_readable_padding() {
        let empty: [u8; 0] = [];
        assert_eq!(load_block(&empty).unwrap(), [0; BLOCK_BYTES]);
        for input in [&[b'"'][..], &[b'\\', 0xff][..], &[0, b':', b'9'][..]] {
            let block = load_block(input).unwrap();
            assert_eq!(&block[..input.len()], input);
            assert!(block[input.len()..].iter().all(|&byte| byte == 0));
            assert_pipeline(input, initial_carry(1, 1));
        }
        let storage = [0xff; BLOCK_BYTES];
        for len in 0..=BLOCK_BYTES {
            let input = &storage[BLOCK_BYTES - len..];
            let block = load_block(input).unwrap();
            assert_eq!(&block[..len], input);
            assert!(block[len..].iter().all(|&byte| byte == 0));
        }
    }

    #[test]
    fn oversized_inputs_report_exact_errors_without_mutating_input_or_carry() {
        let storage = [b'\\'; 1024];
        let before = storage;
        for len in [65, 66, 127, 128, 255, 256, 1024] {
            assert_error(load_block(&storage[..len]).unwrap_err(), ErrorCode::InvalidToken);
            for initial in [initial_carry(1, 1), initial_carry(u64::MAX, 2)] {
                let mut carry = initial;
                assert_error(
                    classify::classify(&storage[..len], &mut carry).unwrap_err(),
                    ErrorCode::InvalidToken,
                );
                assert_eq!(carry, initial);
            }
        }
        assert_eq!(storage, before);
    }

    #[test]
    fn every_byte_value_at_every_position_matches_all_safe_raw_paths() {
        for value in 0..=u8::MAX {
            assert_raw_paths(&[value; BLOCK_BYTES]);
            for index in 0..BLOCK_BYTES {
                let mut block = [b'x'; BLOCK_BYTES];
                block[index] = value;
                assert_raw_paths(&block);
            }
        }
        assert_eq!(classify_scalar_raw(&[0; BLOCK_BYTES]).classes.control_mask, u64::MAX);
        assert_eq!(classify_scalar_raw(&[0xff; BLOCK_BYTES]).classes.non_ascii_mask, u64::MAX);
        assert_eq!(classify_scalar_raw(&[b'x'; BLOCK_BYTES]), RawMasks::default());
    }

    #[test]
    fn vector_sign_bits_and_lane_boundaries_have_literal_expectations() {
        let mut block = [b'x'; BLOCK_BYTES];
        block[0] = b':';
        block[31] = b'"';
        block[32] = b'\\';
        block[63] = b'"';
        assert_eq!(classify_scalar_raw(&block), RawMasks {
            quote_mask: 0x8000_0000_8000_0000,
            backslash_mask: 0x0000_0001_0000_0000,
            classes: StructuralMasks { colon_mask: 1, ..StructuralMasks::default() },
            backend: BackendId::Scalar,
        });
        assert_raw_paths(&block);
        let mut carry = initial_carry(0, 0);
        let masks = classify::classify(&block, &mut carry).unwrap();
        assert_eq!(masks.in_string_mask, 0x7fff_ffff_0000_0000);
        assert_eq!(masks.structural_mask, 1);
        assert_eq!(carry, initial_carry(0, 0));

        block[31] = b'\\';
        block[32] = b'"';
        block[63] = b'\\';
        let raw = classify_scalar_raw(&block);
        assert_eq!(raw.quote_mask, 0x0000_0001_0000_0000);
        assert_eq!(raw.backslash_mask, 0x8000_0000_8000_0000);
        assert_raw_paths(&block);
        for inside in 0..=1 {
            for escaped in 0..=1 {
                assert_pipeline(&block, initial_carry(inside, escaped));
            }
        }
    }

    #[test]
    fn every_tail_length_clips_padding_and_preserves_all_carry_fields() {
        let alphabet = b"\"\\:,{}[] \t\r\n09\x00\x1f\x7f\x80\xff";
        let mut mixed = [0; BLOCK_BYTES];
        for (index, byte) in mixed.iter_mut().enumerate() {
            *byte = alphabet[index % alphabet.len()];
        }
        for block in [mixed, [0; BLOCK_BYTES], [b'x'; BLOCK_BYTES],
            [b'"'; BLOCK_BYTES], [b'\\'; BLOCK_BYTES], [0xff; BLOCK_BYTES]]
        {
            for len in 0..=BLOCK_BYTES {
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        assert_pipeline(&block[..len], initial_carry(inside, escaped));
                    }
                }
            }
        }
    }

    #[test]
    fn zero_padding_does_not_contribute_control_bits_or_reset_escape_carry() {
        for len in 0..=BLOCK_BYTES {
            let slashes = [b'\\'; BLOCK_BYTES];
            let loaded = load_block(&slashes[..len]).unwrap();
            let valid = ((1u128 << len) - 1) as u64;
            let raw = classify_scalar_raw(&loaded);
            assert_eq!(raw.backslash_mask, valid);
            assert_eq!(raw.classes.control_mask, !valid);
            for escaped in 0..=1 {
                let mut carry = initial_carry(1, escaped);
                let masks = classify::classify(&slashes[..len], &mut carry).unwrap();
                assert_eq!(masks.control_mask, 0);
                assert_eq!(masks.in_string_mask, valid);
                assert_eq!(carry, initial_carry(1, escaped ^ (len as u64 & 1)));
            }
        }
    }

    #[test]
    fn deterministic_mixed_vectors_match_scalar_and_dispatched_pipelines() {
        let alphabet = b"\"\\{}[]:, \t\r\n019az\x00\x1f\x7f\x80\xff";
        let mut state = 0x6470_2ac9_ef81_3b5du64;
        for case in 0..128 {
            let mut block = [0; BLOCK_BYTES];
            for byte in &mut block {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = if case % 2 == 0 {
                    state as u8
                } else {
                    alphabet[(state % alphabet.len() as u64) as usize]
                };
            }
            assert_raw_paths(&block);
            for len in [0, 1, 2, 30, 31, 32, 33, 62, 63, 64] {
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        assert_pipeline(&block[..len], initial_carry(inside, escaped));
                    }
                }
            }
        }
    }

    #[test]
    fn every_split_reassembles_masks_and_preserves_final_state() {
        let pattern = b"\"a\\\"{}\"\\\\\"[,:]\t\xff";
        let mut mixed = [0; BLOCK_BYTES];
        for (index, byte) in mixed.iter_mut().enumerate() {
            *byte = pattern[index % pattern.len()];
        }
        for block in [mixed, [b'\\'; BLOCK_BYTES], [b'"'; BLOCK_BYTES]] {
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let initial = initial_carry(inside, escaped);
                    let mut expected_carry = initial;
                    let expected = fields(reference_block(&block, &mut expected_carry));
                    for split in 0..=BLOCK_BYTES {
                        let mut carry = initial;
                        let low = fields(classify::classify(&block[..split], &mut carry).unwrap());
                        let before_empty = carry;
                        assert_eq!(classify::classify(&[], &mut carry).unwrap(), BlockMasks::default());
                        assert_eq!(carry, before_empty);
                        let high = fields(classify::classify(&block[split..], &mut carry).unwrap());
                        for index in 0..expected.len() {
                            // u128 also handles the split-at-64 empty suffix.
                            let combined = u128::from(low[index]) | (u128::from(high[index]) << split);
                            assert_eq!(combined, u128::from(expected[index]), "split {split}, field {index}");
                        }
                        assert_eq!(carry, expected_carry);
                    }
                }
            }
        }
    }

    #[test]
    fn long_backslash_runs_cross_full_blocks_and_every_partial_tail() {
        let slashes = [b'\\'; BLOCK_BYTES];
        for tail_len in 0..=BLOCK_BYTES {
            for incoming in 0..=1 {
                let mut carry = initial_carry(1, incoming);
                for _ in 0..3 {
                    assert_pipeline(&slashes, carry);
                    classify::classify(&slashes, &mut carry).unwrap();
                    assert_eq!(carry, initial_carry(1, incoming));
                }
                assert_pipeline(&slashes[..tail_len], carry);
                classify::classify(&slashes[..tail_len], &mut carry).unwrap();
                let odd = incoming ^ (tail_len as u64 & 1);
                assert_eq!(carry, initial_carry(1, odd));
                assert_pipeline(b"\":", carry);
                let final_masks = classify::classify(b"\":", &mut carry).unwrap();
                assert_eq!(final_masks.quote_mask, 1);
                assert_eq!(final_masks.in_string_mask, if odd == 1 { 3 } else { 0 });
                assert_eq!(final_masks.structural_mask, if odd == 1 { 0 } else { 2 });
                assert_eq!(carry, initial_carry(odd, 0));
            }
        }
    }

    #[test]
    fn lexical_errors_are_reported_as_raw_classes_without_grammar_rejection() {
        let samples: [&[u8]; 6] = [
            b"}{][,,::000+NaN", b"\"unfinished\\", b"\"\x00\x1f\xff\\q\"",
            b"\xc0\xaf\xed\xa0\x80\xf5\xff", b"\xc2\xa0\xe2\x80\x83\xd9\xa0",
            b"{\":\\\",\"}",
        ];
        for sample in samples {
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    assert_pipeline(sample, initial_carry(inside, escaped));
                }
            }
        }
        let mut carry = initial_carry(0, 0);
        let masks = classify::classify(samples[5], &mut carry).unwrap();
        assert_eq!(masks.quote_mask, 0x52);
        assert_eq!(masks.backslash_mask, 0x08);
        assert_eq!(masks.in_string_mask, 0x3c);
        assert_eq!(masks.colon_mask, 0x04);
        assert_eq!(masks.comma_mask, 0x20);
        assert_eq!(masks.structural_mask, 0x81);
        assert_eq!(carry, initial_carry(0, 0));
    }

    #[test]
    fn invalid_quote_carry_is_rejected_atomically_at_every_length() {
        let block = [b'"'; BLOCK_BYTES];
        for invalid in [2, 3, 255, 1u64 << 32, 1u64 << 63, u64::MAX] {
            for initial in [initial_carry(invalid, 1), initial_carry(1, invalid)] {
                for len in 0..=BLOCK_BYTES {
                    let mut carry = initial;
                    assert_error(
                        classify::classify(&block[..len], &mut carry).unwrap_err(),
                        ErrorCode::InternalError,
                    );
                    assert_eq!(carry, initial);
                }
                let mut carry = initial;
                assert_error(
                    classify::classify_block(&block, &mut carry).unwrap_err(),
                    ErrorCode::InternalError,
                );
                assert_eq!(carry, initial);
            }
        }
    }

    #[test]
    fn quote_integration_rejects_live_overlap_but_ignores_padding_overlap() {
        for len in 0..=BLOCK_BYTES {
            let valid = ((1u128 << len) - 1) as u64;
            let initial = initial_carry(1, 1);
            let mut carry = initial;
            let result = quotes::quote_masks(!valid, !valid, len as u8, &mut carry).unwrap();
            assert_eq!(result, QuoteMasks { in_string_mask: valid, ..QuoteMasks::default() });
            assert_eq!(carry, initial_carry(1, u64::from(len == 0)));
            for index in 0..len {
                let mut carry = initial;
                let bit = 1u64 << index;
                assert_error(
                    quotes::quote_masks(bit, bit, len as u8, &mut carry).unwrap_err(),
                    ErrorCode::InternalError,
                );
                assert_eq!(carry, initial);
            }
        }
        for len in 65..=u8::MAX {
            let initial = initial_carry(1, 1);
            let mut carry = initial;
            assert_error(
                quotes::quote_masks(0, 0, len, &mut carry).unwrap_err(),
                ErrorCode::InvalidToken,
            );
            assert_eq!(carry, initial);
        }
    }

    #[test]
    fn all_utf8_tail_lengths_are_preserved_on_success_and_failure() {
        for tail_len in 0..=u8::MAX {
            let initial = ScanCarry { utf8_tail_len: tail_len, ..initial_carry(1, 1) };
            for sample in [&b""[..], &b"\\"[..], &b"\"\xff\x00"[..]] {
                assert_pipeline(sample, initial);
            }
            let mut carry = initial;
            assert_error(
                classify::classify(&[0; BLOCK_BYTES + 1], &mut carry).unwrap_err(),
                ErrorCode::InvalidToken,
            );
            assert_eq!(carry, initial);
        }
    }

    #[test]
    #[cfg(any(
        not(any(target_arch = "x86", target_arch = "x86_64")),
        all(not(feature = "std"), not(target_feature = "avx2")),
    ))]
    fn cfg_without_avx2_support_uses_the_scalar_backend() {
        assert!(!is_supported());
        let block = [b'"'; BLOCK_BYTES];
        assert_eq!(classify::classify_raw(&block), classify_scalar_raw(&block));
        for len in 0..=BLOCK_BYTES {
            let mut carry = initial_carry(0, 0);
            let masks = classify::classify(&block[..len], &mut carry).unwrap();
            assert_eq!(masks.backend, 0);
            assert_pipeline(&block[..len], initial_carry(0, 0));
        }
    }
}
