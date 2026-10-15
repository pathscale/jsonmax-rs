//! Four-lane byte classification and transactional quote-carry integration.

use super::*;

/// Classify one block of at most 64 bytes, retaining quote state for the next.
///
/// Errors leave every field of `carry` unchanged. Empty input still validates
/// the quote carry, then returns empty Scalar masks without advancing state.
pub fn classify(input: &[u8], carry: &mut ScanCarry) -> Result<BlockMasks> {
    valid_mask(input.len())?;
    validate_carry(carry)?;
    if input.is_empty() {
        return Ok(BlockMasks::default());
    }

    let block = tails::load_block(input)?;
    classify_loaded(&block, input.len(), carry)
}

/// Classify a complete block without copying it into padded storage.
pub fn classify_block(
    input: &[u8; BLOCK_BYTES],
    carry: &mut ScanCarry,
) -> Result<BlockMasks> {
    validate_carry(carry)?;
    classify_loaded(input, BLOCK_BYTES, carry)
}

// Both callers validate carry and guarantee len <= BLOCK_BYTES before entry.
// Keeping the pending state private also covers any error from quote analysis
// or final assembly, rather than relying on those callees to roll state back.
fn classify_loaded(
    input: &[u8; BLOCK_BYTES],
    len: usize,
    carry: &mut ScanCarry,
) -> Result<BlockMasks> {
    let raw = classify_raw(input);
    let mut pending = *carry;
    let quotes = quotes::quote_masks(
        raw.quote_mask,
        raw.backslash_mask,
        len as u8,
        &mut pending,
    )?;
    let masks = raw.finish(quotes, len)?;
    *carry = pending;
    Ok(masks)
}

/// Return raw full-block masks with a tag identifying the executed backend.
///
/// Bit `i` always denotes input byte `i`. CPU support is checked here even
/// when the caller has already selected this crate through a dispatcher.
pub fn classify_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    #[cfg(target_arch = "aarch64")]
    if is_supported() {
        // SAFETY: is_supported establishes NEON availability for this build
        // and CPU. The array contains all 64 bytes read by the four loads.
        let (quote_mask, backslash_mask) = unsafe { classify_quotes_neon(input) };
        // This safe entry point independently applies the same support check,
        // so the structural and quote classifiers select the same backend.
        return RawMasks {
            quote_mask,
            backslash_mask,
            classes: structural::classify_structural(input),
            backend: BackendId::Neon,
        };
    }

    tails::classify_scalar_raw(input)
}

/// Reduce 0x00/0xff comparison lanes in byte-address order.
///
/// Each half sums distinct powers of two, with a maximum sum of 255. No
/// vector-to-integer reinterpretation or native-endian integer load is used.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn comparison_mask(value: core::arch::aarch64::uint8x16_t) -> u16 {
    use core::arch::aarch64::*;

    let weights = [1u8, 2, 4, 8, 16, 32, 64, 128];
    // SAFETY: callers establish NEON availability. The weight load reads
    // exactly eight initialized bytes and has no alignment requirement.
    unsafe {
        let weights = vld1_u8(weights.as_ptr());
        let low = vaddv_u8(vand_u8(vget_low_u8(value), weights));
        let high = vaddv_u8(vand_u8(vget_high_u8(value), weights));
        u16::from(low) | (u16::from(high) << 8)
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn classify_quotes_neon(input: &[u8; BLOCK_BYTES]) -> (u64, u64) {
    use core::arch::aarch64::*;

    let mut quote_mask = 0u64;
    let mut backslash_mask = 0u64;
    // SAFETY: the only caller checks NEON support. For lane 0..4, offsets
    // 0, 16, 32, 48 each leave 16 readable bytes in the fixed-size array.
    // vld1q_u8 accepts unaligned pointers. All helper calls inherit NEON.
    unsafe {
        let quote = vdupq_n_u8(b'"');
        let backslash = vdupq_n_u8(b'\\');
        for lane in 0..VECTOR_LANES {
            let offset = lane * LANE_BYTES;
            let bytes = vld1q_u8(input.as_ptr().add(offset));
            let quotes = comparison_mask(vceqq_u8(bytes, quote));
            let backslashes = comparison_mask(vceqq_u8(bytes, backslash));
            // Widen before shifting, including the highest lane and bit 63.
            quote_mask |= u64::from(quotes) << offset;
            backslash_mask |= u64::from(backslashes) << offset;
        }
    }
    (quote_mask, backslash_mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected_backend() -> u8 {
        if is_supported() { BACKEND_TAG } else { 0 }
    }

    fn carry(in_string: u64, escaped: u64) -> ScanCarry {
        ScanCarry {
            prev_in_string: in_string,
            prev_escaped: escaped,
            utf8_tail: [0xf0, 0x9f, 0x92, 0xa9],
            // Deliberately opaque to the scanner, including an invalid UTF-8
            // tail length: classification must preserve it without validation.
            utf8_tail_len: 255,
        }
    }

    // Independent byte-by-byte oracle: no production classification, quote,
    // tail, valid-mask, or assembly helpers participate in expected masks.
    fn reference(input: &[u8], state: &mut ScanCarry) -> BlockMasks {
        assert!(input.len() <= BLOCK_BYTES);
        let mut out = BlockMasks::default();
        out.valid_len = input.len() as u8;
        if !input.is_empty() {
            out.backend = expected_backend();
        }
        for (index, &byte) in input.iter().enumerate() {
            let bit = 1u64 << index;
            out.valid_mask |= bit;
            match byte {
                b'"' => out.quote_mask |= bit,
                b'\\' => out.backslash_mask |= bit,
                b':' => out.colon_mask |= bit,
                b',' => out.comma_mask |= bit,
                b'{' => out.lbrace_mask |= bit,
                b'}' => out.rbrace_mask |= bit,
                b'[' => out.lbracket_mask |= bit,
                b']' => out.rbracket_mask |= bit,
                _ => {}
            }
            if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
                out.whitespace_mask |= bit;
            }
            if byte < 0x20 {
                out.control_mask |= bit;
            }
            if byte >= 0x80 {
                out.non_ascii_mask |= bit;
            }
            if byte >= b'0' && byte <= b'9' {
                out.digit_mask |= bit;
            }

            let delimiter = byte == b'"' && state.prev_escaped == 0;
            if delimiter {
                state.prev_in_string ^= 1;
            } else if state.prev_in_string != 0 {
                out.in_string_mask |= bit;
            }
            if byte == b'\\' {
                state.prev_escaped ^= 1;
            } else {
                state.prev_escaped = 0;
            }
        }
        out.structural_mask = (out.colon_mask | out.comma_mask
            | out.lbrace_mask | out.rbrace_mask
            | out.lbracket_mask | out.rbracket_mask) & !out.in_string_mask;
        out
    }

    fn reference_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks {
        let masks = reference(input, &mut ScanCarry::default());
        RawMasks {
            quote_mask: masks.quote_mask,
            backslash_mask: masks.backslash_mask,
            classes: StructuralMasks {
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
            },
            backend: if is_supported() { BackendId::Neon } else { BackendId::Scalar },
        }
    }

    fn all_masks(m: BlockMasks) -> [u64; 15] {
        [m.quote_mask, m.backslash_mask, m.in_string_mask, m.colon_mask,
            m.comma_mask, m.lbrace_mask, m.rbrace_mask, m.lbracket_mask,
            m.rbracket_mask, m.structural_mask, m.whitespace_mask,
            m.control_mask, m.digit_mask, m.non_ascii_mask, m.valid_mask]
    }

    fn assert_pipeline(input: &[u8], initial: ScanCarry) {
        let mut expected_state = initial;
        let expected = reference(input, &mut expected_state);
        let mut actual_state = initial;
        let actual = classify(input, &mut actual_state).unwrap();
        assert_eq!(actual, expected, "input: {:?}, carry: {:?}", input, initial);
        assert_eq!(actual_state, expected_state);
        for mask in all_masks(actual) {
            assert_eq!(mask & !actual.valid_mask, 0);
        }
    }

    #[test]
    fn abi_entry_points_and_empty_input() {
        let entry: ClassifyFn = classify;
        let root_entry: ClassifyFn = super::super::classify;
        for inside in 0..=1 {
            for escaped in 0..=1 {
                let initial = carry(inside, escaped);
                for classifier in [entry, root_entry] {
                    let mut state = initial;
                    assert_eq!(classifier(&[], &mut state).unwrap(), BlockMasks::default());
                    assert_eq!(state, initial);
                }
            }
        }
    }

    #[test]
    fn every_byte_value_in_every_lane() {
        for byte in 0u16..=255 {
            let input = [byte as u8; BLOCK_BYTES];
            assert_eq!(classify_raw(&input), reference_raw(&input));
        }
        // Odd multiplication permutes all 256 byte values; each position sees
        // every value, including unsigned boundaries 0x7f, 0x80, and 0xff.
        for seed in 0u16..=255 {
            let mut input = [0u8; BLOCK_BYTES];
            for (index, byte) in input.iter_mut().enumerate() {
                *byte = (seed as u8).wrapping_add((index as u8).wrapping_mul(73));
            }
            assert_eq!(classify_raw(&input), reference_raw(&input));
        }
    }

    #[test]
    fn quote_and_backslash_bits_have_exact_address_order() {
        for position in 0..BLOCK_BYTES {
            for byte in [b'"', b'\\'] {
                let mut input = [b'x'; BLOCK_BYTES];
                input[position] = byte;
                let raw = classify_raw(&input);
                assert_eq!(raw, reference_raw(&input));
                assert_eq!(raw.quote_mask | raw.backslash_mask, 1u64 << position);
                assert_eq!(raw.quote_mask & raw.backslash_mask, 0);
            }
        }
        let mut alternating = [0u8; BLOCK_BYTES];
        for (index, byte) in alternating.iter_mut().enumerate() {
            *byte = if index % 2 == 0 { b'"' } else { b'\\' };
        }
        let raw = classify_raw(&alternating);
        assert_eq!(raw.quote_mask, 0x5555_5555_5555_5555);
        assert_eq!(raw.backslash_mask, 0xaaaa_aaaa_aaaa_aaaa);
    }

    #[test]
    fn every_tail_length_and_initial_quote_state() {
        let alphabet = b"\"\\{}[]:, \t\r\n\x00\x1f/09:\x7f\x80\xffx";
        let mut input = [0u8; BLOCK_BYTES];
        for rotation in 0..alphabet.len() {
            for (index, byte) in input.iter_mut().enumerate() {
                *byte = alphabet[(index + rotation) % alphabet.len()];
            }
            for len in 0..=BLOCK_BYTES {
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        assert_pipeline(&input[..len], carry(inside, escaped));
                    }
                }
            }
        }
    }

    #[test]
    fn full_array_and_slice_entry_points_agree() {
        let mut input = [b'\\'; BLOCK_BYTES];
        for position in [0, 15, 16, 31, 32, 47, 48, 63] {
            input[position] = b'"';
        }
        for inside in 0..=1 {
            for escaped in 0..=1 {
                let initial = carry(inside, escaped);
                let mut array_state = initial;
                let mut slice_state = initial;
                let fixed = classify_block(&input, &mut array_state).unwrap();
                let sliced = classify(&input, &mut slice_state).unwrap();
                assert_eq!(fixed, sliced);
                assert_eq!(array_state, slice_state);
                let mut expected_state = initial;
                assert_eq!(fixed, reference(&input, &mut expected_state));
                assert_eq!(array_state, expected_state);
            }
        }
    }

    #[test]
    fn punctuation_inside_strings_remains_raw() {
        let input = b"{\"a\":\"[,]\",\"b\":9}";
        let masks = classify(input, &mut ScanCarry::default()).unwrap();
        assert_eq!(masks.quote_mask, (1 << 1) | (1 << 3) | (1 << 5)
            | (1 << 9) | (1 << 11) | (1 << 13));
        assert_eq!(masks.in_string_mask, (1 << 2) | (1 << 6)
            | (1 << 7) | (1 << 8) | (1 << 12));
        assert_eq!(masks.comma_mask, (1 << 7) | (1 << 10));
        assert_eq!(masks.lbracket_mask, 1 << 6);
        assert_eq!(masks.rbracket_mask, 1 << 8);
        assert_eq!(masks.structural_mask, (1 << 0) | (1 << 4)
            | (1 << 10) | (1 << 14) | (1 << 16));
        assert_eq!(masks.digit_mask, 1 << 15);
    }

    #[test]
    fn only_json_whitespace_and_ascii_digits_match() {
        let input = b" \t\r\n\x0b\x0c\x1f\x00/09:\x7f\x80\xa0\xff";
        let masks = classify(input, &mut ScanCarry::default()).unwrap();
        assert_eq!(masks.whitespace_mask, 0x0f);
        assert_eq!(masks.control_mask, 0xfe);
        assert_eq!(masks.digit_mask, (1 << 9) | (1 << 10));
        assert_eq!(masks.non_ascii_mask, (1 << 13) | (1 << 14) | (1 << 15));
        assert_eq!(masks.colon_mask, 1 << 11);
    }

    #[test]
    fn padding_neither_contributes_controls_nor_resets_escape_parity() {
        for len in 1..BLOCK_BYTES {
            let mut input = [b'x'; BLOCK_BYTES];
            input[len - 1] = b'\\';
            let mut state = carry(1, 0);
            let masks = classify(&input[..len], &mut state).unwrap();
            assert_eq!(masks.control_mask, 0);
            assert_eq!(masks.backslash_mask, 1u64 << (len - 1));
            assert_eq!(state, carry(1, 1));
            assert_eq!(masks.in_string_mask, (1u64 << len) - 1);
        }
    }

    #[test]
    fn all_backslash_run_lengths_cross_lane_boundaries() {
        for quote_position in 0..BLOCK_BYTES {
            for run_len in 0..=quote_position {
                let mut input = [b'x'; BLOCK_BYTES];
                input[quote_position - run_len..quote_position].fill(b'\\');
                input[quote_position] = b'"';
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        assert_pipeline(&input, carry(inside, escaped));
                    }
                }
            }
        }
    }

    #[test]
    fn splitting_at_every_byte_preserves_masks_and_carry() {
        let mut input = [b'\\'; BLOCK_BYTES];
        for position in [0, 3, 15, 16, 31, 32, 47, 48, 63] {
            input[position] = b'"';
        }
        input[7] = b':';
        input[22] = b',';
        input[54] = 0x80;
        for inside in 0..=1 {
            for escaped in 0..=1 {
                let initial = carry(inside, escaped);
                let mut whole_state = initial;
                let whole = reference(&input, &mut whole_state);
                for split in 0..=BLOCK_BYTES {
                    let mut state = initial;
                    let left = classify(&input[..split], &mut state).unwrap();
                    let right = classify(&input[split..], &mut state).unwrap();
                    assert_eq!(state, whole_state);
                    let left = all_masks(left);
                    let right = all_masks(right);
                    let whole = all_masks(whole);
                    for index in 0..whole.len() {
                        let joined = if split == BLOCK_BYTES {
                            assert_eq!(right[index], 0);
                            left[index]
                        } else {
                            left[index] | (right[index] << split)
                        };
                        assert_eq!(joined, whole[index], "split {}, mask {}", split, index);
                    }
                }
            }
        }
    }

    #[test]
    fn backslash_parity_survives_full_block_edges() {
        for trailing_run in 0..=BLOCK_BYTES {
            let mut first = [b'x'; BLOCK_BYTES];
            first[BLOCK_BYTES - trailing_run..].fill(b'\\');
            let mut second = [b'\\'; BLOCK_BYTES];
            for quote_position in [0, 1, 15, 16, 31, 32, 47, 48, 63] {
                second.fill(b'\\');
                second[quote_position] = b'"';
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        let mut expected = carry(inside, escaped);
                        let mut actual = expected;
                        assert_eq!(classify_block(&first, &mut actual).unwrap(),
                            reference(&first, &mut expected));
                        assert_eq!(actual, expected);
                        assert_eq!(classify_block(&second, &mut actual).unwrap(),
                            reference(&second, &mut expected));
                        assert_eq!(actual, expected);
                    }
                }
            }
        }
    }

    #[test]
    fn unaligned_arrays_and_exact_tail_slices_are_supported() {
        let mut storage = [0u8; BLOCK_BYTES + LANE_BYTES];
        for (index, byte) in storage.iter_mut().enumerate() {
            *byte = match index % 5 {
                0 => b'"', 1 => b'\\', 2 => b':', 3 => 0xff, _ => 0,
            };
        }
        for offset in 0..LANE_BYTES {
            let block: &[u8; BLOCK_BYTES] =
                storage[offset..offset + BLOCK_BYTES].try_into().unwrap();
            assert_eq!(classify_raw(block), reference_raw(block));
            let mut state = carry(1, 1);
            let mut expected_state = state;
            assert_eq!(classify_block(block, &mut state).unwrap(),
                reference(block, &mut expected_state));
            assert_eq!(state, expected_state);
            for len in 0..=BLOCK_BYTES {
                // Each slice ends at the end of the backing storage.
                assert_pipeline(&storage[storage.len() - len..], carry(1, 1));
            }
        }
    }

    fn assert_error(error: Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Scanner);
        assert_eq!(error.byte_offset, 0);
    }

    #[test]
    fn oversized_input_is_rejected_before_length_narrowing_or_state_changes() {
        let input = [b'"'; 1024];
        for len in [65, 127, 255, 256, 257, 1024] {
            for initial in [carry(0, 0), carry(1, 1), carry(u64::MAX, 2)] {
                let mut state = initial;
                assert_error(classify(&input[..len], &mut state).unwrap_err(),
                    ErrorCode::InvalidToken);
                assert_eq!(state, initial);
            }
        }
    }

    #[test]
    fn invalid_quote_carry_is_rejected_transactionally_even_on_empty_input() {
        let input = [b'"'; BLOCK_BYTES];
        for invalid in [2, 255, 256, 1u64 << 63, u64::MAX] {
            for initial in [carry(invalid, 0), carry(0, invalid), carry(invalid, invalid)] {
                for len in [0, 1, 16, 63, 64] {
                    let mut state = initial;
                    assert_error(classify(&input[..len], &mut state).unwrap_err(),
                        ErrorCode::InternalError);
                    assert_eq!(state, initial);
                }
                let mut state = initial;
                assert_error(classify_block(&input, &mut state).unwrap_err(),
                    ErrorCode::InternalError);
                assert_eq!(state, initial);
            }
        }
    }

    #[test]
    fn unsupported_targets_report_the_scalar_path() {
        if !is_supported() {
            let block = [b'"'; BLOCK_BYTES];
            assert_eq!(classify_raw(&block).backend, BackendId::Scalar);
            let masks = classify_block(&block, &mut ScanCarry::default()).unwrap();
            assert_eq!(masks.backend, 0);
            assert_eq!(masks.quote_mask, u64::MAX);
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn native_reduction_covers_every_sixteen_bit_pattern() {
        if !is_supported() {
            return;
        }
        for pattern in 0u32..=u16::MAX as u32 {
            let mut bytes = [0u8; LANE_BYTES];
            for (index, byte) in bytes.iter_mut().enumerate() {
                *byte = if pattern & (1 << index) != 0 { 0xff } else { 0 };
            }
            // SAFETY: support was checked above, and the load reads the
            // complete initialized 16-byte array without requiring alignment.
            let actual = unsafe {
                comparison_mask(core::arch::aarch64::vld1q_u8(bytes.as_ptr()))
            };
            assert_eq!(actual, pattern as u16);
        }
    }
}
