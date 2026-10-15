//! Bounded block loading and an independent, portable raw classifier.

use super::*;

/// Copy zero through 64 available bytes into a zero-padded block.
///
/// The input needs no alignment or readable padding. Only bytes within the
/// slice are read, and the returned array owns its storage. Consumers must
/// retain the original length when analyzing quotes and assembling ABI masks:
/// the zero padding is storage, not input.
///
/// # Errors
///
/// An input longer than 64 bytes returns `InvalidToken`, source `Scanner`, at
/// offset zero, before any bytes are copied or any length is narrowed.
pub fn load_block(input: &[u8]) -> Result<[u8; BLOCK_BYTES]> {
    if input.len() > BLOCK_BYTES {
        return Err(Error::new(
            ErrorCode::InvalidToken,
            ErrorSource::Scanner,
            0,
            "scanner block exceeds 64 bytes",
        ));
    }

    let mut block = [0; BLOCK_BYTES];
    block[..input.len()].copy_from_slice(input);
    Ok(block)
}

/// Classify every byte of a full block using only unsigned scalar operations.
///
/// Bit `i` describes byte `i` independently of alignment and host endianness.
/// All masks are raw: quotes include escaped quotes, punctuation includes
/// string contents, and zero padding contributes control bits. Final assembly
/// must clip these masks to the actual input length.
///
/// This reference fallback always reports `BackendId::Scalar`. It performs no
/// allocation, quote-state processing, JSON or UTF-8 validation, or calls into
/// the other scanner modules, so it remains usable on every target.
pub fn classify_scalar_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    let mut masks = RawMasks::default();
    for (index, &byte) in input.iter().enumerate() {
        // The array length bounds every shift to 0..=63.
        let bit = 1u64 << index;
        match byte {
            b'"' => masks.quote_mask |= bit,
            b'\\' => masks.backslash_mask |= bit,
            b':' => masks.classes.colon_mask |= bit,
            b',' => masks.classes.comma_mask |= bit,
            b'{' => masks.classes.lbrace_mask |= bit,
            b'}' => masks.classes.rbrace_mask |= bit,
            b'[' => masks.classes.lbracket_mask |= bit,
            b']' => masks.classes.rbracket_mask |= bit,
            b' ' | b'\t' | b'\r' | b'\n' => masks.classes.whitespace_mask |= bit,
            _ => {}
        }
        // Tab, CR, and LF belong to both whitespace and control classes.
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

    fn initial_carry(inside: u64, escaped: u64) -> ScanCarry {
        ScanCarry {
            prev_in_string: inside,
            prev_escaped: escaped,
            // UTF-8 metadata is opaque to this scanner, including its length.
            utf8_tail: [0xf0, 0x9f, 0xff, 0x80],
            utf8_tail_len: 255,
        }
    }

    fn raw_fields(raw: RawMasks) -> [u64; 12] {
        let c = raw.classes;
        [
            raw.quote_mask, raw.backslash_mask, c.colon_mask, c.comma_mask,
            c.lbrace_mask, c.rbrace_mask, c.lbracket_mask, c.rbracket_mask,
            c.whitespace_mask, c.control_mask, c.digit_mask, c.non_ascii_mask,
        ]
    }

    fn block_fields(block: BlockMasks) -> [u64; 15] {
        [
            block.quote_mask, block.backslash_mask, block.colon_mask,
            block.comma_mask, block.lbrace_mask, block.rbrace_mask,
            block.lbracket_mask, block.rbracket_mask, block.whitespace_mask,
            block.control_mask, block.digit_mask, block.non_ascii_mask,
            block.in_string_mask, block.structural_mask, block.valid_mask,
        ]
    }

    // Independent set-membership oracle, shared only by tests.
    fn membership(byte: u8) -> [bool; 12] {
        [
            b"\"".contains(&byte), b"\\".contains(&byte),
            b":".contains(&byte), b",".contains(&byte),
            b"{".contains(&byte), b"}".contains(&byte),
            b"[".contains(&byte), b"]".contains(&byte),
            b" \t\r\n".contains(&byte), (0u8..=31).contains(&byte),
            b"0123456789".contains(&byte), (128u8..=255).contains(&byte),
        ]
    }

    // Count preceding backslashes directly rather than using mask parity or
    // any production helper. This oracle never examines padded bytes.
    fn reference(input: &[u8], initial: ScanCarry) -> ([u64; 15], ScanCarry) {
        assert!(input.len() <= BLOCK_BYTES);
        let mut fields = [0; 15];
        let mut inside = initial.prev_in_string == 1;
        for (index, &byte) in input.iter().enumerate() {
            let bit = 1u64 << index;
            for (field, member) in fields[..12].iter_mut().zip(membership(byte)) {
                if member {
                    *field |= bit;
                }
            }
            let mut start = index;
            while start > 0 && input[start - 1] == b'\\' {
                start -= 1;
            }
            let run = index - start
                + if start == 0 { initial.prev_escaped as usize } else { 0 };
            if byte == b'"' && run % 2 == 0 {
                inside = !inside;
            } else if inside {
                fields[12] |= bit;
            }
            if b":,{}[]".contains(&byte) && !inside {
                fields[13] |= bit;
            }
            fields[14] |= bit;
        }
        let mut next = initial;
        if !input.is_empty() {
            let trailing = input.iter().rev().take_while(|&&byte| byte == b'\\').count();
            let run = trailing
                + if trailing == input.len() { initial.prev_escaped as usize } else { 0 };
            next.prev_escaped = (run % 2) as u64;
            next.prev_in_string = u64::from(inside);
        }
        (fields, next)
    }

    fn fallback(input: &[u8], carry: &mut ScanCarry) -> Result<BlockMasks> {
        let loaded = load_block(input)?;
        let raw = classify_scalar_raw(&loaded);
        let mut next = *carry;
        let quoted = quotes::quote_masks(
            raw.quote_mask, raw.backslash_mask, input.len() as u8, &mut next,
        )?;
        let result = raw.finish(quoted, input.len())?;
        *carry = next;
        Ok(result)
    }

    fn assert_pipeline(input: &[u8], initial: ScanCarry) -> BlockMasks {
        let (expected, expected_carry) = reference(input, initial);
        let mut fallback_carry = initial;
        let actual = fallback(input, &mut fallback_carry).unwrap();
        assert_eq!(block_fields(actual), expected);
        assert_eq!(usize::from(actual.valid_len), input.len());
        assert_eq!(actual.backend, 0);
        assert_eq!(fallback_carry, expected_carry);
        let mut canonical_carry = initial;
        let entry: ClassifyFn = crate::classify;
        assert_eq!(entry(input, &mut canonical_carry).unwrap(), actual);
        assert_eq!(canonical_carry, expected_carry);
        if input.len() == BLOCK_BYTES {
            let block: &[u8; BLOCK_BYTES] = input.try_into().unwrap();
            let mut full_carry = initial;
            assert_eq!(classify::classify_block(block, &mut full_carry).unwrap(), actual);
            assert_eq!(full_carry, expected_carry);
        }
        actual
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
    fn exact_size_inputs_load_without_caller_padding() {
        assert_eq!(load_block(&[]).unwrap(), [0; BLOCK_BYTES]);
        let one = [0xff];
        let loaded = load_block(&one).unwrap();
        assert_eq!(loaded[0], 0xff);
        assert_eq!(&loaded[1..], &[0; BLOCK_BYTES - 1]);
        let short = [0xa5; BLOCK_BYTES - 1];
        let loaded = load_block(&short).unwrap();
        assert_eq!(&loaded[..BLOCK_BYTES - 1], &short);
        assert_eq!(loaded[BLOCK_BYTES - 1], 0);
        let full = [0x81; BLOCK_BYTES];
        assert_eq!(load_block(&full).unwrap(), full);
    }

    #[test]
    fn every_length_and_slice_offset_copy_only_live_bytes() {
        let mut storage = [0; BLOCK_BYTES * 2];
        for (index, byte) in storage.iter_mut().enumerate() {
            *byte = (index + 1) as u8;
        }
        let before = storage;
        for offset in 0..=BLOCK_BYTES {
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
    fn returned_storage_is_independent_and_zero_bytes_are_preserved() {
        let mut input = [0, 0xff, 0, b'"', b'\\', 0];
        let original = input;
        let mut first = load_block(&input).unwrap();
        input.fill(0x7f);
        assert_eq!(&first[..original.len()], &original);
        first.fill(b':');
        assert_eq!(input, [0x7f; 6]);
        assert_eq!(load_block(&[]).unwrap(), [0; BLOCK_BYTES]);
    }

    #[test]
    fn oversized_inputs_are_rejected_without_narrowing_or_mutation() {
        let input = [0xa5; 4096];
        for len in [65, 66, 127, 128, 255, 256, 257, 4096] {
            assert_error(load_block(&input[..len]).unwrap_err(), ErrorCode::InvalidToken);
        }
        assert_eq!(input, [0xa5; 4096]);
    }

    #[test]
    fn explicit_vector_sets_exact_raw_masks() {
        let prefix = b"\"\\:,{}[] \t\r\n09/\x1f\x7f\x80\xff\x0b\x0c";
        let mut input = [b'a'; BLOCK_BYTES];
        input[..prefix.len()].copy_from_slice(prefix);
        let actual = classify_scalar_raw(&input);
        assert_eq!(actual, RawMasks {
            quote_mask: 1,
            backslash_mask: 2,
            classes: StructuralMasks {
                colon_mask: 1 << 2,
                comma_mask: 1 << 3,
                lbrace_mask: 1 << 4,
                rbrace_mask: 1 << 5,
                lbracket_mask: 1 << 6,
                rbracket_mask: 1 << 7,
                whitespace_mask: 0x0f00,
                control_mask: (1 << 9) | (1 << 10) | (1 << 11)
                    | (1 << 15) | (1 << 19) | (1 << 20),
                digit_mask: (1 << 12) | (1 << 13),
                non_ascii_mask: (1 << 17) | (1 << 18),
            },
            backend: BackendId::Scalar,
        });
    }

    #[test]
    fn every_byte_value_at_every_position_has_exact_membership() {
        for value in 0u16..=255 {
            let byte = value as u8;
            let members = membership(byte);
            let full = classify_scalar_raw(&[byte; BLOCK_BYTES]);
            assert_eq!(raw_fields(full), members.map(|yes| if yes { u64::MAX } else { 0 }));
            assert_eq!(full.backend, BackendId::Scalar);
            for position in 0..BLOCK_BYTES {
                let mut input = [b'a'; BLOCK_BYTES];
                input[position] = byte;
                let actual = classify_scalar_raw(&input);
                let bit = 1u64 << position;
                assert_eq!(raw_fields(actual), members.map(|yes| if yes { bit } else { 0 }),
                    "byte {byte:#04x}, position {position}");
                assert_eq!(actual.backend, BackendId::Scalar);
            }
        }
    }

    #[test]
    fn byte_ramps_match_prior_wave_raw_classifiers() {
        for start in 0u16..=255 {
            let mut input = [0; BLOCK_BYTES];
            for (index, byte) in input.iter_mut().enumerate() {
                *byte = (start as u8).wrapping_add(index as u8);
            }
            let actual = classify_scalar_raw(&input);
            assert_eq!(actual, classify::classify_raw(&input));
            assert_eq!(actual.classes, structural::classify_structural(&input));
            for (index, &byte) in input.iter().enumerate() {
                for (mask, member) in raw_fields(actual).iter().zip(membership(byte)) {
                    assert_eq!((mask >> index) & 1, u64::from(member));
                }
            }
        }
    }

    #[test]
    fn padding_control_bits_are_removed_at_every_tail_length() {
        let input = [b'a'; BLOCK_BYTES];
        for len in 0..=BLOCK_BYTES {
            let raw = classify_scalar_raw(&load_block(&input[..len]).unwrap());
            let valid = if len == BLOCK_BYTES { u64::MAX } else { (1u64 << len) - 1 };
            assert_eq!(raw.classes.control_mask, !valid);
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let actual = assert_pipeline(&input[..len], initial_carry(inside, escaped));
                    assert_eq!(actual.control_mask, 0);
                    assert_eq!(actual.valid_mask, valid);
                    for mask in block_fields(actual) {
                        assert_eq!(mask & !valid, 0);
                    }
                }
            }
        }
    }

    #[test]
    fn mixed_prefixes_match_independent_oracle_for_all_incoming_states() {
        let pattern = b"{\"a\\\"b\": [09,\t\xff\x00]}\\\\\"\r\n\x7f";
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
    fn explicit_string_vector_preserves_raw_classes_and_filters_punctuation() {
        let input = b"[\"a\\\",\",0]\n";
        let block = assert_pipeline(input, initial_carry(0, 0));
        assert_eq!(block.quote_mask, (1 << 1) | (1 << 4) | (1 << 6));
        assert_eq!(block.backslash_mask, 1 << 3);
        assert_eq!(block.in_string_mask, 0x3c);
        assert_eq!(block.comma_mask, (1 << 5) | (1 << 7));
        assert_eq!(block.structural_mask, (1 << 0) | (1 << 7) | (1 << 9));
        assert_eq!(block.digit_mask, 1 << 8);
        assert_eq!(block.whitespace_mask, 1 << 10);
        assert_eq!(block.control_mask, 1 << 10);
        assert_eq!(block.valid_mask, 0x7ff);
    }

    #[test]
    fn splitting_at_every_boundary_preserves_masks_and_carry() {
        let pattern = b"\\\"a,\\\\\":{}[]\t\x00\xff0";
        let mut input = [0; BLOCK_BYTES];
        for (index, byte) in input.iter_mut().enumerate() {
            *byte = pattern[index % pattern.len()];
        }
        for inside in 0..=1 {
            for escaped in 0..=1 {
                let initial = initial_carry(inside, escaped);
                let (expected, final_carry) = reference(&input, initial);
                for split in 0..=BLOCK_BYTES {
                    let mut carry = initial;
                    let left = block_fields(fallback(&input[..split], &mut carry).unwrap());
                    let before_empty = carry;
                    assert_eq!(fallback(&[], &mut carry).unwrap(), BlockMasks::default());
                    assert_eq!(carry, before_empty);
                    let right = block_fields(fallback(&input[split..], &mut carry).unwrap());
                    for index in 0..expected.len() {
                        let shifted = if split == BLOCK_BYTES { 0 } else { right[index] << split };
                        assert_eq!(left[index] | shifted, expected[index], "split {split}");
                    }
                    assert_eq!(carry, final_carry);
                }
            }
        }
    }

    #[test]
    fn trailing_backslashes_and_high_bit_quotes_survive_block_edges() {
        for run in 1..=BLOCK_BYTES {
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let initial = initial_carry(inside, escaped);
                    let mut carry = initial;
                    let input = [b'\\'; BLOCK_BYTES];
                    assert_pipeline(&input[..run], initial);
                    fallback(&input[..run], &mut carry).unwrap();
                    let parity = (run as u64 + escaped) % 2;
                    assert_eq!(carry.prev_escaped, parity);
                    assert_eq!(carry.prev_in_string, inside);
                    assert_pipeline(b"\":", carry);
                    let next = fallback(b"\":", &mut carry).unwrap();
                    assert_eq!(next.in_string_mask & 1, inside & parity);
                    assert_eq!(carry.prev_escaped, 0);
                    assert_eq!(carry.prev_in_string, inside ^ (1 - parity));
                    assert_eq!(carry.utf8_tail, initial.utf8_tail);
                    assert_eq!(carry.utf8_tail_len, initial.utf8_tail_len);
                }
            }
        }
        let mut input = [b'a'; BLOCK_BYTES];
        input[62] = b'\\';
        input[63] = b'"';
        let initial = initial_carry(1, 0);
        let actual = assert_pipeline(&input, initial);
        assert_eq!(actual.quote_mask, 1u64 << 63);
        assert_eq!(actual.backslash_mask, 1u64 << 62);
        assert_eq!(actual.in_string_mask, u64::MAX);
    }

    #[test]
    fn invalid_carry_and_oversized_input_leave_state_unchanged() {
        let input = [b'"'; 257];
        for initial in [
            initial_carry(0, 0), initial_carry(1, 1),
            initial_carry(2, 0), initial_carry(0, 2),
            initial_carry(u64::MAX, 1), initial_carry(1, u64::MAX),
        ] {
            for len in [65, 256, 257] {
                let mut carry = initial;
                assert_error(fallback(&input[..len], &mut carry).unwrap_err(), ErrorCode::InvalidToken);
                assert_eq!(carry, initial);
                assert_error(crate::classify(&input[..len], &mut carry).unwrap_err(), ErrorCode::InvalidToken);
                assert_eq!(carry, initial);
            }
            if initial.prev_in_string > 1 || initial.prev_escaped > 1 {
                for len in [0, 1, 63, 64] {
                    let mut carry = initial;
                    assert_error(fallback(&input[..len], &mut carry).unwrap_err(), ErrorCode::InternalError);
                    assert_eq!(carry, initial);
                    assert_error(crate::classify(&input[..len], &mut carry).unwrap_err(), ErrorCode::InternalError);
                    assert_eq!(carry, initial);
                }
            }
        }
    }
}
