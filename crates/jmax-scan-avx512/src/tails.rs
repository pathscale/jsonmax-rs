//! Bounded tail storage and the complete portable raw classifier.
//!
//! These functions are available on every target, including without `std`.
//! They require no CPU features, allocation, pointer operations, or alignment.
//! The fallback is a leaf: it never calls the other scanner modules.

use super::*;

/// Copy at most 64 live bytes into an owned, zero-padded block.
///
/// Only the input slice is read. Empty slices need no readable backing bytes,
/// and full blocks need neither extra padding nor vector alignment. Consumers
/// must retain the original length when analyzing quotes and assembling masks:
/// padding is storage, not input, and its zeros otherwise classify as controls.
///
/// # Errors
///
/// Input longer than 64 bytes returns `InvalidToken`, source `Scanner`, at
/// offset zero. The length is validated before copying or narrowing it.
#[inline]
pub fn load_block(input: &[u8]) -> Result<[u8; BLOCK_BYTES]> {
    valid_mask(input.len())?;
    let mut block = [0u8; BLOCK_BYTES];
    block[..input.len()].copy_from_slice(input);
    Ok(block)
}

/// Classify all 64 bytes locally, always reporting `BackendId::Scalar`.
///
/// Bit `i` describes byte `i`, independently of host endianness. Quotes,
/// backslashes, and punctuation are raw, without string-state filtering.
/// Whitespace is exactly space, tab, CR, and LF; controls are bytes below
/// `0x20`; digits are ASCII `0` through `9`; non-ASCII is `0x80..=0xff`.
/// Tab, CR, and LF intentionally belong to both whitespace and controls.
///
/// All byte patterns are accepted, including invalid JSON and invalid UTF-8.
/// A padded block's masks must subsequently be clipped to its live length.
#[inline]
pub fn classify_scalar_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    let mut raw = RawMasks {
        backend: BackendId::Scalar,
        ..RawMasks::default()
    };
    for (index, &byte) in input.iter().enumerate() {
        // An actual 64-byte array bounds index to 0..64, including bit 63.
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
            _ => {}
        }
        if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
            raw.classes.whitespace_mask |= bit;
        }
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
            utf8_tail: [0xf0, 0x00, 0x80, 0xff],
            // UTF-8 state is opaque even when its length is noncanonical.
            utf8_tail_len: 0xff,
        }
    }

    // Independent per-class membership oracle, restricted to real bytes.
    fn member_mask(input: &[u8], members: &[u8]) -> u64 {
        assert!(input.len() <= BLOCK_BYTES);
        input.iter().enumerate().fold(0, |mask, (index, byte)| {
            mask | (u64::from(members.contains(byte)) << index)
        })
    }

    fn reference_raw(input: &[u8]) -> RawMasks {
        assert!(input.len() <= BLOCK_BYTES);
        let control_mask = input.iter().enumerate().fold(0, |mask, (index, &byte)| {
            mask | (u64::from(byte & 0xe0 == 0) << index)
        });
        let non_ascii_mask = input.iter().enumerate().fold(0, |mask, (index, &byte)| {
            mask | (u64::from(byte & 0x80 != 0) << index)
        });
        RawMasks {
            quote_mask: member_mask(input, b"\""),
            backslash_mask: member_mask(input, b"\\"),
            classes: StructuralMasks {
                colon_mask: member_mask(input, b":"),
                comma_mask: member_mask(input, b","),
                lbrace_mask: member_mask(input, b"{"),
                rbrace_mask: member_mask(input, b"}"),
                lbracket_mask: member_mask(input, b"["),
                rbracket_mask: member_mask(input, b"]"),
                whitespace_mask: member_mask(input, b" \t\r\n"),
                control_mask,
                digit_mask: member_mask(input, b"0123456789"),
                non_ascii_mask,
            },
            backend: BackendId::Scalar,
        }
    }

    // Derive ABI masks without calling load_block, scalar classification,
    // quote analysis, finish, or valid_mask. Count consecutive backslashes
    // explicitly so this oracle does not share the prefix implementation.
    fn reference_block(input: &[u8], carry: &mut ScanCarry) -> BlockMasks {
        let raw = reference_raw(input);
        let c = raw.classes;
        let mut inside = carry.prev_in_string != 0;
        let mut run = carry.prev_escaped as usize;
        let mut in_string_mask = 0;
        let mut live = 0;
        for (index, &byte) in input.iter().enumerate() {
            let bit = 1u64 << index;
            live |= bit;
            if byte == b'"' && run % 2 == 0 {
                inside = !inside;
            } else if inside {
                in_string_mask |= bit;
            }
            if byte == b'\\' {
                run += 1;
            } else {
                run = 0;
            }
        }
        carry.prev_in_string = u64::from(inside);
        carry.prev_escaped = (run % 2) as u64;
        BlockMasks {
            quote_mask: raw.quote_mask,
            backslash_mask: raw.backslash_mask,
            in_string_mask,
            colon_mask: c.colon_mask,
            comma_mask: c.comma_mask,
            lbrace_mask: c.lbrace_mask,
            rbrace_mask: c.rbrace_mask,
            lbracket_mask: c.lbracket_mask,
            rbracket_mask: c.rbracket_mask,
            structural_mask: (c.colon_mask | c.comma_mask | c.lbrace_mask
                | c.rbrace_mask | c.lbracket_mask | c.rbracket_mask) & !in_string_mask,
            whitespace_mask: c.whitespace_mask,
            control_mask: c.control_mask,
            digit_mask: c.digit_mask,
            non_ascii_mask: c.non_ascii_mask,
            valid_mask: live,
            valid_len: input.len() as u8,
            backend: 0,
        }
    }

    fn assert_raw_paths(input: &[u8; BLOCK_BYTES]) {
        let before = *input;
        let expected = reference_raw(input);
        assert_eq!(classify_scalar_raw(input), expected);
        assert_eq!(structural::classify_structural(input), expected.classes);
        let dispatched = classify::classify_raw(input);
        assert_eq!(dispatched, RawMasks {
            backend: if is_supported() { BackendId::Avx512 } else { BackendId::Scalar },
            ..expected
        });
        assert_eq!(*input, before);
    }

    fn assert_pipeline(input: &[u8], initial: ScanCarry) -> ScanCarry {
        let mut expected_carry = initial;
        let expected = reference_block(input, &mut expected_carry);
        let block = load_block(input).unwrap();
        let raw = classify_scalar_raw(&block);
        let mut scalar_carry = initial;
        let quote = quotes::quote_masks(
            raw.quote_mask, raw.backslash_mask, input.len() as u8, &mut scalar_carry,
        ).unwrap();
        assert_eq!(raw.finish(quote, input.len()).unwrap(), expected);
        assert_eq!(scalar_carry, expected_carry);

        let entry: ClassifyFn = classify::classify;
        let mut dispatched_carry = initial;
        let mut dispatched_expected = expected;
        if !input.is_empty() && is_supported() {
            dispatched_expected.backend = BACKEND_TAG;
        }
        assert_eq!(entry(input, &mut dispatched_carry).unwrap(), dispatched_expected);
        assert_eq!(dispatched_carry, expected_carry);
        if input.len() == BLOCK_BYTES {
            let mut fixed_carry = initial;
            assert_eq!(classify::classify_block(&block, &mut fixed_carry).unwrap(),
                dispatched_expected);
            assert_eq!(fixed_carry, expected_carry);
        }
        expected_carry
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
    fn signatures_empty_storage_and_literal_raw_masks() {
        let load: fn(&[u8]) -> Result<[u8; BLOCK_BYTES]> = load_block;
        let scalar: fn(&[u8; BLOCK_BYTES]) -> RawMasks = classify_scalar_raw;
        assert_eq!(load(&[]).unwrap(), [0; BLOCK_BYTES]);
        assert_eq!(scalar(&[0; BLOCK_BYTES]), RawMasks {
            classes: StructuralMasks {
                control_mask: u64::MAX,
                ..StructuralMasks::default()
            },
            ..RawMasks::default()
        });
        let mut input = [b'x'; BLOCK_BYTES];
        input[..20].copy_from_slice(&[
            b'"', b'\\', b':', b',', b'{', b'}', b'[', b']',
            b' ', b'\t', b'\r', b'\n', 0, 0x1f, b'0', b'9',
            b'/', 0x7f, 0x80, 0xff,
        ]);
        assert_eq!(scalar(&input), RawMasks {
            quote_mask: 0x1,
            backslash_mask: 0x2,
            classes: StructuralMasks {
                colon_mask: 0x4,
                comma_mask: 0x8,
                lbrace_mask: 0x10,
                rbrace_mask: 0x20,
                lbracket_mask: 0x40,
                rbracket_mask: 0x80,
                whitespace_mask: 0xf00,
                control_mask: 0x3e00,
                digit_mask: 0xc000,
                non_ascii_mask: 0xc0000,
            },
            backend: BackendId::Scalar,
        });
        assert_raw_paths(&input);
    }

    #[test]
    fn every_length_and_address_offset_copies_only_the_slice() {
        #[repr(align(64))]
        struct Storage([u8; 128]);

        for offset in 0..64 {
            for len in 0..=BLOCK_BYTES {
                let mut storage = Storage([0xa5; 128]);
                for index in 0..len {
                    storage.0[offset + index] = (index * 37 + len) as u8;
                }
                let before = storage.0;
                let input = &storage.0[offset..offset + len];
                assert_eq!(input.as_ptr() as usize % 64, offset);
                let mut loaded = load_block(input).unwrap();
                assert_eq!(&loaded[..len], input);
                assert_eq!(&loaded[len..], &[0; BLOCK_BYTES][len..]);
                // The output owns its storage and cannot alias the source.
                loaded.fill(0x5a);
                assert_eq!(storage.0, before);
            }
        }
    }

    fn exact_object<const N: usize>() {
        let input = [0xff; N];
        let loaded = load_block(&input).unwrap();
        assert_eq!(&loaded[..N], &input);
        assert!(loaded[N..].iter().all(|&byte| byte == 0));
        // A zero-length slice at the end of an object has no readable tail.
        assert_eq!(load_block(&input[N..]).unwrap(), [0; BLOCK_BYTES]);
    }

    #[test]
    fn exact_sized_objects_need_no_readable_padding() {
        exact_object::<0>();
        exact_object::<1>();
        exact_object::<15>();
        exact_object::<16>();
        exact_object::<17>();
        exact_object::<31>();
        exact_object::<32>();
        exact_object::<33>();
        exact_object::<63>();
        exact_object::<64>();
    }

    #[test]
    fn all_byte_values_at_all_positions_match_scalar_and_vector_paths() {
        for byte in 0u8..=u8::MAX {
            assert_raw_paths(&[byte; BLOCK_BYTES]);
            for index in 0..BLOCK_BYTES {
                let mut input = [b'x'; BLOCK_BYTES];
                input[index] = byte;
                assert_raw_paths(&input);
            }
        }
    }

    #[test]
    fn lane_boundaries_and_deterministic_mixed_vectors() {
        for boundary in [1, 8, 16, 32, 48, 63] {
            let mut input = [b'x'; BLOCK_BYTES];
            input[boundary - 1] = b'"';
            input[boundary] = b'\\';
            let raw = classify_scalar_raw(&input);
            assert_eq!(raw.quote_mask, 1u64 << (boundary - 1));
            assert_eq!(raw.backslash_mask, 1u64 << boundary);
            assert_raw_paths(&input);
        }
        let mut alternating = [b'"'; BLOCK_BYTES];
        for index in (1..BLOCK_BYTES).step_by(2) {
            alternating[index] = b'\\';
        }
        let raw = classify_scalar_raw(&alternating);
        assert_eq!(raw.quote_mask, 0x5555_5555_5555_5555);
        assert_eq!(raw.backslash_mask, 0xaaaa_aaaa_aaaa_aaaa);
        assert_raw_paths(&alternating);
        for seed in 0..256usize {
            let mut input = [0; BLOCK_BYTES];
            for (index, byte) in input.iter_mut().enumerate() {
                *byte = (seed + index * 73) as u8;
            }
            assert_raw_paths(&input);
        }
    }

    #[test]
    fn every_tail_length_and_carry_state_matches_independent_oracle() {
        let pattern = b"\"\\\\\":,{}[] \t\r\n\x00\x1f09/;\x7f\x80\xff\\";
        let mut input = [0; BLOCK_BYTES];
        for (index, byte) in input.iter_mut().enumerate() {
            *byte = pattern[index % pattern.len()];
        }
        for len in 0..=BLOCK_BYTES {
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    assert_pipeline(&input[..len], initial_carry(inside, escaped));
                }
            }
        }
    }

    #[test]
    fn zero_padding_is_raw_control_but_never_live_input() {
        for len in 0..=BLOCK_BYTES {
            let live = (0..len).fold(0u64, |mask, index| mask | (1u64 << index));
            let input = [b' '; BLOCK_BYTES];
            let raw = classify_scalar_raw(&load_block(&input[..len]).unwrap());
            assert_eq!(raw.classes.control_mask, !live);
            assert_eq!(raw.classes.whitespace_mask, live);
            let assembled = raw.finish(QuoteMasks::default(), len).unwrap();
            assert_eq!(assembled.control_mask, 0);
            assert_eq!(assembled.whitespace_mask, live);
            assert_eq!(assembled.valid_mask, live);
            assert_eq!(usize::from(assembled.valid_len), len);
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    assert_pipeline(&input[..len], initial_carry(inside, escaped));
                    assert_pipeline(&[0; BLOCK_BYTES][..len], initial_carry(inside, escaped));
                }
            }
        }
    }

    #[test]
    fn trailing_backslash_parity_survives_every_tail_length() {
        let input = [b'\\'; BLOCK_BYTES];
        for len in 0..=BLOCK_BYTES {
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let outgoing = assert_pipeline(&input[..len], initial_carry(inside, escaped));
                    assert_eq!(outgoing.prev_in_string, inside);
                    assert_eq!(outgoing.prev_escaped, escaped ^ (len as u64 & 1));
                    assert_pipeline(b"\":", outgoing);
                }
            }
        }
    }

    #[test]
    fn every_stream_split_preserves_masks_and_continuous_state() {
        let pattern = b"\\\\\\\":\",{}[]\t\x00\xff9";
        let mut stream = [0; 128];
        for (index, byte) in stream.iter_mut().enumerate() {
            *byte = pattern[index % pattern.len()];
        }
        stream[62..65].fill(b'\\');
        stream[65] = b'"';
        for split in 0..=BLOCK_BYTES {
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let initial = initial_carry(inside, escaped);
                    let mut carry = initial;
                    for part in [&stream[..split], &stream[split..split + BLOCK_BYTES],
                        &stream[split + BLOCK_BYTES..]] {
                        carry = assert_pipeline(part, carry);
                        assert_eq!(assert_pipeline(&[], carry), carry);
                    }
                    let mut bytewise = initial;
                    for byte in &stream {
                        reference_block(core::slice::from_ref(byte), &mut bytewise);
                    }
                    assert_eq!(carry, bytewise);
                }
            }
        }
    }

    #[test]
    fn syntax_and_utf8_errors_remain_raw_byte_classes() {
        for input in [
            &b"\":,{}[]\"\\q\x00\xc0\xaf\xed\xa0\x80\xff"[..],
            &b"\xc2\xa0\xe2\x80\xa8\xd9\xa0\xef\xbc\x90\x0b\x0c"[..],
            &b"}{][,,::\"unfinished\\"[..],
        ] {
            assert_raw_paths(&load_block(input).unwrap());
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    assert_pipeline(input, initial_carry(inside, escaped));
                }
            }
        }
    }

    #[test]
    fn oversized_slices_fail_before_state_validation_without_side_effects() {
        let input = [b'"'; 257];
        for len in BLOCK_BYTES + 1..=input.len() {
            assert_error(load_block(&input[..len]).unwrap_err(), ErrorCode::InvalidToken);
            for initial in [initial_carry(0, 0), initial_carry(1, 1),
                initial_carry(u64::MAX, u64::MAX)] {
                let mut carry = initial;
                assert_error(classify::classify(&input[..len], &mut carry).unwrap_err(),
                    ErrorCode::InvalidToken);
                assert_eq!(carry, initial);
            }
        }
        assert_eq!(input, [b'"'; 257]);
        // A rejected load does not affect the next valid operation.
        assert_eq!(load_block(&input[..BLOCK_BYTES]).unwrap(), [b'"'; BLOCK_BYTES]);
    }

    #[test]
    fn invalid_quote_state_is_transactional_at_every_tail_length() {
        let input = [b'"'; BLOCK_BYTES];
        for invalid in [2, 255, 256, 1u64 << 63, u64::MAX] {
            for initial in [initial_carry(invalid, 0), initial_carry(1, invalid),
                initial_carry(invalid, invalid)] {
                for len in 0..=BLOCK_BYTES {
                    let mut carry = initial;
                    assert_error(classify::classify(&input[..len], &mut carry).unwrap_err(),
                        ErrorCode::InternalError);
                    assert_eq!(carry, initial);
                    // Storage and raw classification have no carry precondition.
                    assert_raw_paths(&load_block(&input[..len]).unwrap());
                }
                let mut carry = initial;
                assert_error(classify::classify_block(&input, &mut carry).unwrap_err(),
                    ErrorCode::InternalError);
                assert_eq!(carry, initial);
            }
        }
    }

    #[test]
    fn quote_and_assembly_error_boundaries_preserve_carry_and_raw_masks() {
        let raw = classify_scalar_raw(&[b'"'; BLOCK_BYTES]);
        let before = raw;
        for len in [65, 127, 128, 255, 256, usize::MAX] {
            assert_error(raw.finish(QuoteMasks::default(), len).unwrap_err(),
                ErrorCode::InvalidToken);
            assert_eq!(raw, before);
        }
        for index in 0..BLOCK_BYTES {
            let bit = 1u64 << index;
            let initial = initial_carry(1, 1);
            let mut carry = initial;
            assert_error(quotes::quote_masks(bit, bit, (index + 1) as u8, &mut carry)
                .unwrap_err(), ErrorCode::InternalError);
            assert_eq!(carry, initial);
            // The same overlap is ignored when it lies in padding.
            let quote = quotes::quote_masks(bit, bit, index as u8, &mut carry).unwrap();
            let mut expected_carry = initial;
            let expected = reference_block(&[b'x'; BLOCK_BYTES][..index], &mut expected_carry);
            assert_eq!(quote.in_string_mask, expected.in_string_mask);
            assert_eq!(quote.escaped_quote_mask | quote.delimiter_quote_mask, 0);
            assert_eq!(carry, expected_carry);
        }
    }
}
