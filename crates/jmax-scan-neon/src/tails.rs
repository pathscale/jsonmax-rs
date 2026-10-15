//! Bounded tail storage and the architecture-independent raw fallback.
//!
//! These functions use only safe, allocation-free operations and remain
//! available on every target, including no_std and non-AArch64 builds.

use super::*;

/// Copy at most 64 bytes into a completely initialized, zero-padded block.
///
/// Only the supplied slice is read. No alignment, readable suffix, or caller
/// padding is required. Consumers must retain the original input length to
/// exclude padding from observable masks and quote-carry transitions.
///
/// # Errors
///
/// Inputs longer than 64 bytes return `InvalidToken` from `Scanner` at offset
/// zero, before any copy or length narrowing occurs.
pub fn load_block(input: &[u8]) -> Result<[u8; BLOCK_BYTES]> {
    if input.len() > BLOCK_BYTES {
        return Err(Error::new(
            ErrorCode::InvalidToken,
            ErrorSource::Scanner,
            0,
            "scanner block exceeds 64 bytes",
        ));
    }

    let mut block = [0u8; BLOCK_BYTES];
    block[..input.len()].copy_from_slice(input);
    Ok(block)
}

/// Classify every byte in a complete block with the portable scalar path.
///
/// Bit `i` describes byte `i`, independently of alignment and host endianness.
/// Quotes, backslashes, and lexical classes are raw: no quoting, grammar, or
/// UTF-8 validation occurs. JSON whitespace is space, tab, CR, and LF; control
/// bytes are below 0x20, digits are ASCII, and non-ASCII bytes are at least 0x80.
/// Tab, CR, and LF therefore appear in both whitespace and control masks.
///
/// The backend is always `Scalar`, even on a NEON-capable machine. This local
/// implementation does not invoke the selected classifier, structural module,
/// or quote resolver, so fallback selection cannot recurse.
pub fn classify_scalar_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    let mut masks = RawMasks {
        backend: BackendId::Scalar,
        ..RawMasks::default()
    };
    for (position, &byte) in input.iter().enumerate() {
        // The fixed array bounds position to 0..64, including bit 63.
        let bit = 1u64 << position;
        match byte {
            b'"' => masks.quote_mask |= bit,
            b'\\' => masks.backslash_mask |= bit,
            b':' => masks.classes.colon_mask |= bit,
            b',' => masks.classes.comma_mask |= bit,
            b'{' => masks.classes.lbrace_mask |= bit,
            b'}' => masks.classes.rbrace_mask |= bit,
            b'[' => masks.classes.lbracket_mask |= bit,
            b']' => masks.classes.rbracket_mask |= bit,
            _ => {}
        }
        if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
            masks.classes.whitespace_mask |= bit;
        }
        if byte < 0x20 {
            masks.classes.control_mask |= bit;
        }
        if byte.is_ascii_digit() {
            masks.classes.digit_mask |= bit;
        }
        if byte >= 0x80 {
            masks.classes.non_ascii_mask |= bit;
        }
    }
    masks
}

#[cfg(test)]
mod tests {
    use super::*;

    // Independent set-membership oracle, with no production scanner helpers.
    fn raw_reference(input: &[u8]) -> RawMasks {
        assert!(input.len() <= BLOCK_BYTES);
        let mut masks = [0u64; 12];
        for (position, &byte) in input.iter().enumerate() {
            let membership = [
                byte == b'"', byte == b'\\', byte == b':', byte == b',',
                byte == b'{', byte == b'}', byte == b'[', byte == b']',
                b" \t\r\n".contains(&byte), (0u8..32).contains(&byte),
                b"0123456789".contains(&byte), byte & 0x80 != 0,
            ];
            for (mask, member) in masks.iter_mut().zip(membership) {
                if member {
                    *mask |= 1u64 << position;
                }
            }
        }
        RawMasks {
            quote_mask: masks[0],
            backslash_mask: masks[1],
            classes: StructuralMasks {
                colon_mask: masks[2], comma_mask: masks[3],
                lbrace_mask: masks[4], rbrace_mask: masks[5],
                lbracket_mask: masks[6], rbracket_mask: masks[7],
                whitespace_mask: masks[8], control_mask: masks[9],
                digit_mask: masks[10], non_ascii_mask: masks[11],
            },
            backend: BackendId::Scalar,
        }
    }

    fn state(inside: u64, escaped: u64) -> ScanCarry {
        ScanCarry {
            prev_in_string: inside,
            prev_escaped: escaped,
            utf8_tail: [0xf0, 0x80, 0xff, 0xc0],
            // UTF-8 state is opaque to this scanner, even when noncanonical.
            utf8_tail_len: 255,
        }
    }

    fn reference(input: &[u8], carry: &mut ScanCarry) -> BlockMasks {
        let raw = raw_reference(input);
        let c = raw.classes;
        let mut inside = carry.prev_in_string != 0;
        let mut odd_run = carry.prev_escaped != 0;
        let mut in_string_mask = 0;
        let mut valid = 0;
        for (position, &byte) in input.iter().enumerate() {
            let bit = 1u64 << position;
            valid |= bit;
            if byte == b'"' && !odd_run {
                inside = !inside;
            } else if inside {
                in_string_mask |= bit;
            }
            odd_run = byte == b'\\' && !odd_run;
        }
        carry.prev_in_string = u64::from(inside);
        carry.prev_escaped = u64::from(odd_run);
        BlockMasks {
            quote_mask: raw.quote_mask,
            backslash_mask: raw.backslash_mask,
            in_string_mask,
            colon_mask: c.colon_mask, comma_mask: c.comma_mask,
            lbrace_mask: c.lbrace_mask, rbrace_mask: c.rbrace_mask,
            lbracket_mask: c.lbracket_mask, rbracket_mask: c.rbracket_mask,
            structural_mask: (c.colon_mask | c.comma_mask | c.lbrace_mask
                | c.rbrace_mask | c.lbracket_mask | c.rbracket_mask) & !in_string_mask,
            whitespace_mask: c.whitespace_mask,
            control_mask: c.control_mask,
            digit_mask: c.digit_mask,
            non_ascii_mask: c.non_ascii_mask,
            valid_mask: valid,
            valid_len: input.len() as u8,
            backend: if !input.is_empty() && is_supported() { BACKEND_TAG } else { 0 },
        }
    }

    fn components(m: BlockMasks) -> [u64; 15] {
        [
            m.quote_mask, m.backslash_mask, m.in_string_mask,
            m.colon_mask, m.comma_mask, m.lbrace_mask, m.rbrace_mask,
            m.lbracket_mask, m.rbracket_mask, m.structural_mask,
            m.whitespace_mask, m.control_mask, m.digit_mask,
            m.non_ascii_mask, m.valid_mask,
        ]
    }

    fn assert_pipeline(input: &[u8], initial: ScanCarry) {
        let mut expected_carry = initial;
        let expected = reference(input, &mut expected_carry);
        let entry: ClassifyFn = crate::classify;
        let mut carry = initial;
        let actual = entry(input, &mut carry).unwrap();
        assert_eq!(actual, expected, "input {:?}, carry {:?}", input, initial);
        assert_eq!(carry, expected_carry);
        for mask in components(actual) {
            assert_eq!(mask & !actual.valid_mask, 0);
        }

        // Exercise the fallback on every host, even when native NEON is selected.
        let block = load_block(input).unwrap();
        let raw = classify_scalar_raw(&block);
        let mut scalar_carry = initial;
        let quotes = quotes::quote_masks(
            raw.quote_mask, raw.backslash_mask, input.len() as u8, &mut scalar_carry,
        ).unwrap();
        let scalar = raw.finish(quotes, input.len()).unwrap();
        let mut scalar_expected = expected;
        scalar_expected.backend = 0;
        assert_eq!(scalar, scalar_expected);
        assert_eq!(scalar_carry, expected_carry);

        if input.len() == BLOCK_BYTES {
            let mut full_carry = initial;
            assert_eq!(classify::classify_block(&block, &mut full_carry).unwrap(), expected);
            assert_eq!(full_carry, expected_carry);
        }
    }

    fn assert_error(error: Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Scanner);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn load_every_length_at_every_lane_alignment() {
        let mut storage = [0u8; BLOCK_BYTES + LANE_BYTES];
        for (position, byte) in storage.iter_mut().enumerate() {
            *byte = (position as u8).wrapping_mul(73).wrapping_add(1);
        }
        let before = storage;
        for offset in 0..LANE_BYTES {
            for len in 0..=BLOCK_BYTES {
                let input = &storage[offset..offset + len];
                let loaded = load_block(input).unwrap();
                assert_eq!(&loaded[..len], input);
                assert!(loaded[len..].iter().all(|&byte| byte == 0));
            }
        }
        assert_eq!(storage, before);
    }

    #[test]
    fn exact_sized_objects_need_no_readable_suffix() {
        fn exercise<const N: usize>() {
            let input = [0xff; N];
            let block = load_block(&input).unwrap();
            assert_eq!(&block[..N], &input);
            assert_eq!(block[N..], [0u8; BLOCK_BYTES][N..]);
        }
        exercise::<0>();
        exercise::<1>();
        exercise::<15>();
        exercise::<16>();
        exercise::<17>();
        exercise::<31>();
        exercise::<32>();
        exercise::<33>();
        exercise::<47>();
        exercise::<48>();
        exercise::<49>();
        exercise::<63>();
        exercise::<64>();
    }

    #[test]
    fn loaded_block_owns_its_bytes_and_zero_padding() {
        let mut input = [b'"', b'\\', 0xff];
        let block = load_block(&input).unwrap();
        input.fill(0);
        assert_eq!(&block[..3], &[b'"', b'\\', 0xff]);
        assert!(block[3..].iter().all(|&byte| byte == 0));
        assert_eq!(input, [0; 3]);
    }

    #[test]
    fn oversized_inputs_are_rejected_without_length_truncation() {
        let storage = [0xa5; 1024];
        for len in 65..=storage.len() {
            assert_error(load_block(&storage[..len]).unwrap_err(), ErrorCode::InvalidToken);
        }
        assert_eq!(storage, [0xa5; 1024]);
    }

    #[test]
    fn exact_mixed_vector_includes_every_raw_class() {
        let mut input = [b'x'; BLOCK_BYTES];
        input[..20].copy_from_slice(b"\"\\:,{}[] \t\r\n\x00\x1f09\x7f\x80\xff\x0b");
        assert_eq!(classify_scalar_raw(&input), RawMasks {
            quote_mask: 0x1,
            backslash_mask: 0x2,
            classes: StructuralMasks {
                colon_mask: 0x4, comma_mask: 0x8,
                lbrace_mask: 0x10, rbrace_mask: 0x20,
                lbracket_mask: 0x40, rbracket_mask: 0x80,
                whitespace_mask: 0xf00,
                control_mask: 0x83e00,
                digit_mask: 0xc000,
                non_ascii_mask: 0x60000,
            },
            backend: BackendId::Scalar,
        });
    }

    #[test]
    fn every_byte_at_every_position_matches_membership_oracle() {
        for byte in 0u8..=u8::MAX {
            for position in 0..BLOCK_BYTES {
                let mut input = [b'x'; BLOCK_BYTES];
                input[position] = byte;
                assert_eq!(classify_scalar_raw(&input), raw_reference(&input),
                    "byte {}, position {}", byte, position);
            }
            let input = [byte; BLOCK_BYTES];
            assert_eq!(classify_scalar_raw(&input), raw_reference(&input));
        }
    }

    #[test]
    fn lane_edges_and_bit_63_keep_byte_order() {
        let mut input = [b'x'; BLOCK_BYTES];
        for position in [0, 15, 16, 31, 32, 47, 48, 63] {
            input[position] = b'"';
        }
        assert_eq!(classify_scalar_raw(&input).quote_mask, 0x8001_8001_8001_8001);
        for byte in &mut input {
            if *byte == b'"' {
                *byte = b'\\';
            }
        }
        assert_eq!(classify_scalar_raw(&input).backslash_mask, 0x8001_8001_8001_8001);
        for (position, byte) in input.iter_mut().enumerate() {
            *byte = if position % 2 == 0 { b'9' } else { 0xff };
        }
        let raw = classify_scalar_raw(&input);
        assert_eq!(raw.classes.digit_mask, 0x5555_5555_5555_5555);
        assert_eq!(raw.classes.non_ascii_mask, 0xaaaa_aaaa_aaaa_aaaa);
    }

    #[test]
    fn selected_raw_and_structural_paths_match_scalar_on_all_hosts() {
        for seed in 0u8..=u8::MAX {
            let mut storage = [0u8; BLOCK_BYTES + LANE_BYTES - 1];
            for (position, byte) in storage.iter_mut().enumerate() {
                *byte = seed.wrapping_add((position as u8).wrapping_mul(73));
            }
            for offset in 0..LANE_BYTES {
                let input: &[u8; BLOCK_BYTES] =
                    storage[offset..offset + BLOCK_BYTES].try_into().unwrap();
                let scalar = classify_scalar_raw(input);
                assert_eq!(scalar, raw_reference(input));
                let mut expected = scalar;
                expected.backend = if is_supported() { BackendId::Neon } else { BackendId::Scalar };
                assert_eq!(classify::classify_raw(input), expected);
                assert_eq!(structural::classify_structural(input), scalar.classes);
            }
        }
    }

    #[cfg(not(target_arch = "aarch64"))]
    #[test]
    fn non_aarch64_always_selects_scalar() {
        assert!(!is_supported());
        let input = [b'"'; BLOCK_BYTES];
        assert_eq!(classify::classify_raw(&input).backend, BackendId::Scalar);
        assert_eq!(crate::classify(&input, &mut state(0, 0)).unwrap().backend, 0);
    }

    #[test]
    fn every_tail_length_and_initial_quote_state_matches_oracle() {
        let alphabet = b"\"\\:,{}[] \t\r\n\x00\x1f09/\x7f\x80\xff\x0b\x0c\xa0";
        for rotation in 0..alphabet.len() {
            let mut input = [0u8; BLOCK_BYTES];
            for (position, byte) in input.iter_mut().enumerate() {
                *byte = alphabet[(position + rotation) % alphabet.len()];
            }
            for len in 0..=BLOCK_BYTES {
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        assert_pipeline(&input[..len], state(inside, escaped));
                    }
                }
            }
        }
    }

    #[test]
    fn exact_partial_vectors_preserve_raw_classes_and_filter_string_punctuation() {
        let backend = if is_supported() { BACKEND_TAG } else { 0 };
        let mut carry = state(0, 0);
        assert_eq!(crate::classify(b"{\"a:b\":9}", &mut carry).unwrap(), BlockMasks {
            quote_mask: 0x22,
            in_string_mask: 0x1c,
            colon_mask: 0x48,
            lbrace_mask: 0x1,
            rbrace_mask: 0x100,
            structural_mask: 0x141,
            digit_mask: 0x80,
            valid_mask: 0x1ff,
            valid_len: 9,
            backend,
            ..BlockMasks::default()
        });
        assert_eq!(carry, state(0, 0));

        let input = [b'"', b'\\', b'"', b':', b'"'];
        assert_eq!(crate::classify(&input, &mut carry).unwrap(), BlockMasks {
            quote_mask: 0x15,
            backslash_mask: 0x2,
            in_string_mask: 0xe,
            colon_mask: 0x8,
            valid_mask: 0x1f,
            valid_len: 5,
            backend,
            ..BlockMasks::default()
        });
        assert_eq!(carry, state(0, 0));
    }

    #[test]
    fn padding_neither_emits_controls_nor_consumes_trailing_escape() {
        for len in 0..=BLOCK_BYTES {
            let input = [b'\\'; BLOCK_BYTES];
            let raw = classify_scalar_raw(&load_block(&input[..len]).unwrap());
            let valid = if len == BLOCK_BYTES { u64::MAX } else { (1u64 << len) - 1 };
            assert_eq!(raw.backslash_mask, valid);
            assert_eq!(raw.classes.control_mask, !valid);
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let initial = state(inside, escaped);
                    let mut carry = initial;
                    let actual = crate::classify(&input[..len], &mut carry).unwrap();
                    assert_eq!(actual.control_mask, 0);
                    assert_eq!(actual.backslash_mask, valid);
                    assert_eq!(actual.in_string_mask, if inside == 1 { valid } else { 0 });
                    assert_eq!(carry.prev_in_string, inside);
                    assert_eq!(carry.prev_escaped, escaped ^ ((len as u64) & 1));
                    assert_eq!(carry.utf8_tail, initial.utf8_tail);
                    assert_eq!(carry.utf8_tail_len, initial.utf8_tail_len);
                    assert_pipeline(&input[..len], initial);
                }
            }
        }
    }

    #[test]
    fn every_split_preserves_masks_and_carry_across_empty_and_partial_blocks() {
        for run_start in [0, 14, 15, 16, 30, 31, 32, 46, 47, 48, 61] {
            let mut input = [b':'; BLOCK_BYTES];
            input[0] = b'"';
            input[run_start..run_start + 2].fill(b'\\');
            input[run_start + 2] = b'"';
            input[63] = b'\\';
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let initial = state(inside, escaped);
                    let mut expected_carry = initial;
                    let expected = reference(&input, &mut expected_carry);
                    for split in 0..=BLOCK_BYTES {
                        let mut carry = initial;
                        let left = crate::classify(&input[..split], &mut carry).unwrap();
                        let right = crate::classify(&input[split..], &mut carry).unwrap();
                        for ((low, high), whole) in components(left).into_iter()
                            .zip(components(right)).zip(components(expected))
                        {
                            let joined = if split == BLOCK_BYTES {
                                assert_eq!(high, 0);
                                low
                            } else {
                                low | (high << split)
                            };
                            assert_eq!(joined, whole, "run {}, split {}", run_start, split);
                        }
                        assert_eq!(carry, expected_carry);
                    }
                }
            }
        }
    }

    #[test]
    fn long_backslash_runs_cross_multiple_full_blocks() {
        let input = [b'\\'; BLOCK_BYTES];
        for inside in 0..=1 {
            for escaped in 0..=1 {
                let mut carry = state(inside, escaped);
                for _ in 0..3 {
                    assert_pipeline(&input, carry);
                    crate::classify(&input, &mut carry).unwrap();
                    assert_eq!(carry, state(inside, escaped));
                }
                let tail = [b'\\', b'"', b':', b'"', b'\\'];
                assert_pipeline(&tail, carry);
                let mut expected_carry = carry;
                let expected = reference(&tail, &mut expected_carry);
                assert_eq!(crate::classify(&tail, &mut carry).unwrap(), expected);
                assert_eq!(carry, expected_carry);
            }
        }
    }

    #[test]
    fn empty_input_and_all_utf8_tail_lengths_preserve_opaque_state() {
        for tail_len in 0u8..=u8::MAX {
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let mut carry = state(inside, escaped);
                    carry.utf8_tail_len = tail_len;
                    carry.utf8_tail = [tail_len, !tail_len, 0xff, 0];
                    let before = carry;
                    assert_eq!(crate::classify(b"", &mut carry).unwrap(), BlockMasks::default());
                    assert_eq!(carry, before);
                    assert_pipeline(b"\"\x00\xff\\", before);
                }
            }
        }
    }

    #[test]
    fn integration_errors_are_transactional_and_length_takes_precedence() {
        let input = [b'"'; 257];
        for bad in [2, 255, 256, 1u64 << 32, 1u64 << 63, u64::MAX] {
            for initial in [state(bad, 0), state(1, bad), state(bad, bad)] {
                for len in 0..=BLOCK_BYTES {
                    let mut carry = initial;
                    assert_error(crate::classify(&input[..len], &mut carry).unwrap_err(),
                        ErrorCode::InternalError);
                    assert_eq!(carry, initial);
                }
                let mut carry = initial;
                assert_error(classify::classify_block(&[b'"'; BLOCK_BYTES], &mut carry)
                    .unwrap_err(), ErrorCode::InternalError);
                assert_eq!(carry, initial);
                for len in [65, 128, 255, 256, 257] {
                    assert_error(crate::classify(&input[..len], &mut carry).unwrap_err(),
                        ErrorCode::InvalidToken);
                    assert_eq!(carry, initial);
                }
            }
        }
        for initial in [state(0, 0), state(0, 1), state(1, 0), state(1, 1)] {
            let mut carry = initial;
            assert_error(crate::classify(&input, &mut carry).unwrap_err(), ErrorCode::InvalidToken);
            assert_eq!(carry, initial);
        }
    }
}
