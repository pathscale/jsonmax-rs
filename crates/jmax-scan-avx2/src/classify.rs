//! Two-lane AVX2 quote classification and the bounded scanner pipeline.
//!
//! Safe entry points select the portable implementation when AVX2 is not
//! available. Quote state is committed only after the complete block succeeds.

use super::*;

#[cfg(target_arch = "x86")]
use core::arch::x86 as arch;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64 as arch;

/// Classify at most 64 bytes, preserving the caller's UTF-8 state verbatim.
///
/// An empty block produces zero masks with the scalar backend tag and leaves
/// carry unchanged. Partial blocks are copied into bounded, padded storage;
/// neither the returned masks nor quote carry include padding bytes.
///
/// # Errors
///
/// Returns `InvalidToken` for more than 64 bytes or `InternalError` for
/// noncanonical quote carry. Errors leave all caller state unchanged.
pub fn classify(input: &[u8], carry: &mut ScanCarry) -> Result<BlockMasks> {
    valid_mask(input.len())?;
    validate_carry(carry)?;
    if input.is_empty() {
        return Ok(BlockMasks::default());
    }

    let block = tails::load_block(input)?;
    classify_loaded(&block, input.len(), carry)
}

/// Classify a complete block without copying it into tail storage.
///
/// # Errors
///
/// Returns `InternalError` for noncanonical quote carry. Errors leave all
/// caller state unchanged, including the opaque UTF-8 fields.
pub fn classify_block(
    input: &[u8; BLOCK_BYTES],
    carry: &mut ScanCarry,
) -> Result<BlockMasks> {
    validate_carry(carry)?;
    classify_loaded(input, BLOCK_BYTES, carry)
}

/// Run the shared pipeline after the public caller validates length and carry.
fn classify_loaded(
    input: &[u8; BLOCK_BYTES],
    len: usize,
    carry: &mut ScanCarry,
) -> Result<BlockMasks> {
    let raw = classify_raw(input);
    let mut next_carry = *carry;
    // Both callers establish len <= BLOCK_BYTES before this narrowing cast.
    let quotes = quotes::quote_masks(
        raw.quote_mask,
        raw.backslash_mask,
        len as u8,
        &mut next_carry,
    )?;
    let masks = raw.finish(quotes, len)?;
    *carry = next_carry;
    Ok(masks)
}

/// Return raw lexical classes for all 64 bytes, independent of quote state.
///
/// The backend identifies the path actually executed. This safe function
/// checks AVX2 support itself and is also available on non-x86 targets.
pub fn classify_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    if is_supported() {
        // SAFETY: is_supported checks AVX2 and OS register-state support, or
        // requires statically enabled AVX2 in no-std builds. The array reference
        // guarantees that both unaligned 32-byte reads remain within bounds.
        let (quote_mask, backslash_mask) = unsafe { quote_backslash_masks_avx2(input) };
        return RawMasks {
            quote_mask,
            backslash_mask,
            classes: structural::classify_structural(input),
            backend: BACKEND,
        };
    }

    tails::classify_scalar_raw(input)
}

/// The caller must establish AVX2 CPU and OS support before entering here.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn quote_backslash_masks_avx2(input: &[u8; BLOCK_BYTES]) -> (u64, u64) {
    // SAFETY: input is a readable 64-byte object. The two loads cover exactly
    // bytes 0..32 and 32..64 and loadu imposes no vector alignment requirement.
    // The function's target-feature precondition permits these instructions.
    unsafe {
        let low = arch::_mm256_loadu_si256(input.as_ptr().cast::<arch::__m256i>());
        let high = arch::_mm256_loadu_si256(
            input.as_ptr().add(LANE_BYTES).cast::<arch::__m256i>(),
        );
        let quote = arch::_mm256_set1_epi8(b'"' as i8);
        let backslash = arch::_mm256_set1_epi8(b'\\' as i8);

        // Movemask returns i32. Convert through u32 before widening so a match at
        // byte 31 cannot sign-extend into the independent upper 32-byte lane.
        let low_quotes = arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(low, quote))
            as u32 as u64;
        let high_quotes = arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(high, quote))
            as u32 as u64;
        let low_backslashes =
            arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(low, backslash)) as u32 as u64;
        let high_backslashes =
            arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(high, backslash)) as u32 as u64;

        (
            low_quotes | (high_quotes << LANE_BYTES),
            low_backslashes | (high_backslashes << LANE_BYTES),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected_backend() -> BackendId {
        if is_supported() { BackendId::Avx2 } else { BackendId::Scalar }
    }

    fn expected_tag(len: usize) -> u8 {
        if len != 0 && is_supported() { 2 } else { 0 }
    }

    fn carry_with(in_string: u64, escaped: u64) -> ScanCarry {
        ScanCarry {
            prev_in_string: in_string,
            prev_escaped: escaped,
            utf8_tail: [0xf0, 0x9f, 0x92, 0xa9],
            // Deliberately opaque, even when not a valid UTF-8 tail length.
            utf8_tail_len: 0xff,
        }
    }

    /// Independent bytewise oracle; does not call any production classifier.
    fn reference_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks {
        let mut raw = RawMasks::default();
        for (index, &byte) in input.iter().enumerate() {
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
            if byte >= b'0' && byte <= b'9' {
                raw.classes.digit_mask |= bit;
            }
            if byte >= 0x80 {
                raw.classes.non_ascii_mask |= bit;
            }
        }
        raw
    }

    /// Reference quote state advances only over real bytes and never invokes
    /// quote_masks or RawMasks::finish, so integration errors remain visible.
    fn reference_block(input: &[u8], carry: &mut ScanCarry) -> BlockMasks {
        assert!(input.len() <= BLOCK_BYTES);
        let mut storage = [0; BLOCK_BYTES];
        storage[..input.len()].copy_from_slice(input);
        let raw = reference_raw(&storage);
        let mut valid = 0u64;
        let mut content = 0u64;
        let mut inside = carry.prev_in_string != 0;
        let mut escaped = carry.prev_escaped != 0;
        for (index, &byte) in input.iter().enumerate() {
            let bit = 1u64 << index;
            valid |= bit;
            if byte == b'"' && !escaped {
                inside = !inside;
            } else if inside {
                content |= bit;
            }
            escaped = byte == b'\\' && !escaped;
        }
        carry.prev_in_string = u64::from(inside);
        carry.prev_escaped = u64::from(escaped);
        let c = raw.classes;
        let punctuation = c.colon_mask | c.comma_mask | c.lbrace_mask
            | c.rbrace_mask | c.lbracket_mask | c.rbracket_mask;
        BlockMasks {
            quote_mask: raw.quote_mask & valid,
            backslash_mask: raw.backslash_mask & valid,
            in_string_mask: content,
            colon_mask: c.colon_mask & valid,
            comma_mask: c.comma_mask & valid,
            lbrace_mask: c.lbrace_mask & valid,
            rbrace_mask: c.rbrace_mask & valid,
            lbracket_mask: c.lbracket_mask & valid,
            rbracket_mask: c.rbracket_mask & valid,
            structural_mask: punctuation & !content & valid,
            whitespace_mask: c.whitespace_mask & valid,
            control_mask: c.control_mask & valid,
            digit_mask: c.digit_mask & valid,
            non_ascii_mask: c.non_ascii_mask & valid,
            valid_mask: valid,
            valid_len: input.len() as u8,
            backend: expected_tag(input.len()),
        }
    }

    fn mask_fields(masks: &BlockMasks) -> [u64; 15] {
        [
            masks.quote_mask, masks.backslash_mask, masks.in_string_mask,
            masks.colon_mask, masks.comma_mask, masks.lbrace_mask, masks.rbrace_mask,
            masks.lbracket_mask, masks.rbracket_mask, masks.structural_mask,
            masks.whitespace_mask, masks.control_mask, masks.digit_mask,
            masks.non_ascii_mask, masks.valid_mask,
        ]
    }

    fn assert_raw_matches(input: &[u8; BLOCK_BYTES]) {
        let reference = reference_raw(input);
        assert_eq!(tails::classify_scalar_raw(input), reference);
        let mut expected = reference;
        expected.backend = expected_backend();
        assert_eq!(classify_raw(input), expected);
    }

    fn assert_block_matches(input: &[u8], initial: ScanCarry) {
        let mut expected_carry = initial;
        let expected = reference_block(input, &mut expected_carry);
        let mut actual_carry = initial;
        let actual = classify(input, &mut actual_carry).unwrap();
        assert_eq!(actual, expected, "length {}", input.len());
        assert_eq!(actual_carry, expected_carry, "length {}", input.len());
        for mask in mask_fields(&actual) {
            assert_eq!(mask & !actual.valid_mask, 0);
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
    fn public_entry_point_has_the_canonical_function_type() {
        let entry: ClassifyFn = classify;
        let mut carry = ScanCarry::default();
        let masks = entry(b"[]", &mut carry).unwrap();
        assert_eq!(masks.lbracket_mask, 1);
        assert_eq!(masks.rbracket_mask, 2);
        assert_eq!(masks.structural_mask, 3);
        assert_eq!(masks.valid_mask, 3);
        assert_eq!(masks.valid_len, 2);
        assert_eq!(masks.backend, expected_tag(2));
    }

    #[test]
    fn empty_blocks_preserve_every_carry_field_and_use_scalar_tag() {
        for inside in 0..=1 {
            for escaped in 0..=1 {
                let mut carry = carry_with(inside, escaped);
                let before = carry;
                assert_eq!(classify(&[], &mut carry).unwrap(), BlockMasks::default());
                assert_eq!(carry, before);
            }
        }
    }

    #[test]
    fn every_byte_value_matches_the_independent_raw_oracle() {
        for byte in 0..=255u16 {
            assert_raw_matches(&[byte as u8; BLOCK_BYTES]);
        }
        for base in [0u8, 64, 128, 192] {
            let mut block = [0; BLOCK_BYTES];
            for (index, byte) in block.iter_mut().enumerate() {
                *byte = base + index as u8;
            }
            assert_raw_matches(&block);
            assert_block_matches(&block, carry_with(0, 0));
        }
    }

    #[test]
    fn each_quote_and_backslash_bit_is_isolated_across_both_lanes() {
        for index in 0..BLOCK_BYTES {
            let mut block = [b'x'; BLOCK_BYTES];
            block[index] = b'"';
            let raw = classify_raw(&block);
            assert_eq!(raw.quote_mask, 1u64 << index);
            assert_eq!(raw.backslash_mask, 0);
            block[index] = b'\\';
            let raw = classify_raw(&block);
            assert_eq!(raw.quote_mask, 0);
            assert_eq!(raw.backslash_mask, 1u64 << index);
        }
    }

    #[test]
    fn lane_sign_bits_do_not_contaminate_the_other_lane() {
        let mut block = [b'x'; BLOCK_BYTES];
        block[31] = b'"';
        block[32] = b'\\';
        block[63] = b'"';
        let raw = classify_raw(&block);
        assert_eq!(raw.quote_mask, (1u64 << 31) | (1u64 << 63));
        assert_eq!(raw.backslash_mask, 1u64 << 32);
        block[31] = b'\\';
        block[32] = b'"';
        block[63] = b'\\';
        let raw = classify_raw(&block);
        assert_eq!(raw.quote_mask, 1u64 << 32);
        assert_eq!(raw.backslash_mask, (1u64 << 31) | (1u64 << 63));
    }

    #[test]
    fn deterministic_mixed_blocks_match_scalar_and_bytewise_oracles() {
        let alphabet = b"\"\\{}[]:, \t\r\n019az\x00\x1f\x7f\x80\xff";
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
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
            assert_raw_matches(&block);
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    assert_block_matches(&block, carry_with(inside, escaped));
                }
            }
        }
    }

    #[test]
    fn all_tail_lengths_clip_padding_and_preserve_real_quote_carry() {
        let pattern = b"\"\\\\\"{}[]:, \t\r\n09\x00\x1f\x7f\x80\xff";
        let mut mixed = [0; BLOCK_BYTES];
        for (index, byte) in mixed.iter_mut().enumerate() {
            *byte = pattern[index % pattern.len()];
        }
        for block in [mixed, [0; BLOCK_BYTES], [b'\\'; BLOCK_BYTES], [b'"'; BLOCK_BYTES]] {
            for len in 0..=BLOCK_BYTES {
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        assert_block_matches(&block[..len], carry_with(inside, escaped));
                    }
                }
            }
        }
    }

    #[test]
    fn full_block_and_slice_entry_points_agree() {
        let pattern = b"{\"key\": [10,\"a\\\"b\"],\"raw\":\xff}\n";
        let mut block = [b' '; BLOCK_BYTES];
        block[..pattern.len()].copy_from_slice(pattern);
        for inside in 0..=1 {
            for escaped in 0..=1 {
                let initial = carry_with(inside, escaped);
                let mut slice_carry = initial;
                let mut block_carry = initial;
                let mut expected_carry = initial;
                let expected = reference_block(&block, &mut expected_carry);
                assert_eq!(classify_block(&block, &mut block_carry).unwrap(), expected);
                assert_eq!(classify(&block, &mut slice_carry).unwrap(), expected);
                assert_eq!(block_carry, expected_carry);
                assert_eq!(slice_carry, expected_carry);
            }
        }
    }

    #[test]
    fn full_blocks_need_no_vector_alignment() {
        #[repr(C, align(32))]
        struct Aligned([u8; BLOCK_BYTES + 1]);

        let mut storage = Aligned([b'x'; BLOCK_BYTES + 1]);
        storage.0[1] = b'"';
        storage.0[32] = b'\\';
        storage.0[33] = b'"';
        storage.0[64] = b'"';
        let input: &[u8; BLOCK_BYTES] = (&storage.0[1..]).try_into().unwrap();
        assert_eq!(input.as_ptr() as usize % LANE_BYTES, 1);
        assert_raw_matches(input);
        let mut carry = carry_with(0, 0);
        let mut expected_carry = carry;
        let expected = reference_block(input, &mut expected_carry);
        assert_eq!(classify_block(input, &mut carry).unwrap(), expected);
        assert_eq!(carry, expected_carry);
    }

    #[test]
    fn subslices_do_not_classify_bytes_before_or_after_their_bounds() {
        let mut storage = [b'"'; BLOCK_BYTES + 2];
        storage[1..=BLOCK_BYTES].fill(b' ');
        for len in 0..=BLOCK_BYTES {
            let input = &storage[1..1 + len];
            let mut carry = carry_with(1, 1);
            let masks = classify(input, &mut carry).unwrap();
            assert_eq!(masks.quote_mask, 0);
            assert_eq!(masks.backslash_mask, 0);
            assert_eq!(masks.control_mask, 0);
            assert_eq!(masks.whitespace_mask, masks.valid_mask);
            assert_eq!(masks.in_string_mask, masks.valid_mask);
            assert_eq!(carry.prev_in_string, 1);
            assert_eq!(carry.prev_escaped, u64::from(len == 0));
            assert_eq!(carry.utf8_tail, [0xf0, 0x9f, 0x92, 0xa9]);
            assert_eq!(carry.utf8_tail_len, 0xff);
        }
    }

    #[test]
    fn string_filtering_does_not_remove_raw_punctuation_or_escaped_quotes() {
        // Positions: opening brace 0, opening quote 1, colon 2, slash 3,
        // escaped quote 4, comma 5, closing quote 6, closing brace 7.
        let mut carry = ScanCarry::default();
        let masks = classify(b"{\":\\\",\"}", &mut carry).unwrap();
        assert_eq!(masks.quote_mask, (1 << 1) | (1 << 4) | (1 << 6));
        assert_eq!(masks.backslash_mask, 1 << 3);
        assert_eq!(masks.in_string_mask, 0b0011_1100);
        assert_eq!(masks.colon_mask, 1 << 2);
        assert_eq!(masks.comma_mask, 1 << 5);
        assert_eq!(masks.structural_mask, (1 << 0) | (1 << 7));
        assert_eq!(carry, ScanCarry::default());
    }

    #[test]
    fn quote_escape_runs_cross_lane_and_block_boundaries() {
        for run_len in 0..=BLOCK_BYTES {
            for incoming in 0..=1 {
                let mut carry = carry_with(1, incoming);
                let mut expected_carry = carry;
                let slashes = [b'\\'; BLOCK_BYTES];
                let expected = reference_block(&slashes[..run_len], &mut expected_carry);
                assert_eq!(classify(&slashes[..run_len], &mut carry).unwrap(), expected);
                assert_eq!(carry, expected_carry);
                let expected = reference_block(b"\":", &mut expected_carry);
                let masks = classify(b"\":", &mut carry).unwrap();
                assert_eq!(masks, expected);
                assert_eq!(carry, expected_carry);
                let odd = (run_len as u64 + incoming) % 2 != 0;
                assert_eq!(masks.in_string_mask, if odd { 3 } else { 0 });
                assert_eq!(masks.structural_mask, if odd { 0 } else { 2 });
            }
        }

        for run_len in [1usize, 2, 3, 30, 31, 32, 33, 62] {
            let mut block = [b'x'; BLOCK_BYTES];
            block[..run_len].fill(b'\\');
            block[run_len] = b'"';
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    assert_block_matches(&block, carry_with(inside, escaped));
                }
            }
        }
    }

    #[test]
    fn every_split_produces_the_same_masks_and_final_state() {
        let pattern = b"\"a\\\"{}\"\\\\\"[,:]\t\xff";
        let mut mixed = [0; BLOCK_BYTES];
        for (index, byte) in mixed.iter_mut().enumerate() {
            *byte = pattern[index % pattern.len()];
        }
        for block in [mixed, [b'\\'; BLOCK_BYTES], [b'"'; BLOCK_BYTES]] {
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let initial = carry_with(inside, escaped);
                    let mut expected_carry = initial;
                    let expected = reference_block(&block, &mut expected_carry);
                    let expected_fields = mask_fields(&expected);
                    for split in 0..=BLOCK_BYTES {
                        let mut carry = initial;
                        let left = classify(&block[..split], &mut carry).unwrap();
                        let right = classify(&block[split..], &mut carry).unwrap();
                        let left_fields = mask_fields(&left);
                        let right_fields = mask_fields(&right);
                        for index in 0..expected_fields.len() {
                            // u128 also makes split == 64 a defined shift.
                            let joined = u128::from(left_fields[index])
                                | (u128::from(right_fields[index]) << split);
                            assert_eq!(joined, u128::from(expected_fields[index]));
                        }
                        assert_eq!(carry, expected_carry, "split {split}");
                    }
                }
            }
        }
    }

    #[test]
    fn scanner_reports_raw_classes_without_validating_json_or_utf8() {
        for input in [
            &b"\"unterminated\\"[..],
            &b"\"\x00\x1f\xff\\q\""[..],
            &b"}{][:,000+NaN"[..],
            &b"\xc0\xaf\xed\xa0\x80\xf5\xff"[..],
        ] {
            assert_block_matches(input, carry_with(0, 0));
        }
        let mut carry = ScanCarry::default();
        let masks = classify(b" \t\r\n\x0b\x0c\x1f\x7f\x80\xa009", &mut carry).unwrap();
        assert_eq!(masks.whitespace_mask, 0b1111);
        assert_eq!(masks.control_mask, 0b111_1110);
        assert_eq!(masks.non_ascii_mask, (1 << 8) | (1 << 9));
        assert_eq!(masks.digit_mask, (1 << 10) | (1 << 11));
    }

    #[test]
    fn oversized_inputs_fail_before_mutating_or_narrowing_state() {
        let storage = [b'"'; 1024];
        for len in [65usize, 66, 127, 128, 255, 256, 1024] {
            for initial in [carry_with(0, 0), carry_with(1, 1), carry_with(2, u64::MAX)] {
                let mut carry = initial;
                let error = classify(&storage[..len], &mut carry).unwrap_err();
                assert_scanner_error(error, ErrorCode::InvalidToken);
                assert_eq!(carry, initial);
            }
        }
    }

    #[test]
    fn noncanonical_quote_carry_is_rejected_even_for_empty_input() {
        let block = [b'"'; BLOCK_BYTES];
        for invalid in [2u64, 3, 1 << 32, u64::MAX] {
            for initial in [
                carry_with(invalid, 0), carry_with(0, invalid),
                carry_with(invalid, 1), carry_with(1, invalid),
                carry_with(invalid, invalid),
            ] {
                for len in [0usize, 1, 31, 32, 33, 63, 64] {
                    let mut carry = initial;
                    let error = classify(&block[..len], &mut carry).unwrap_err();
                    assert_scanner_error(error, ErrorCode::InternalError);
                    assert_eq!(carry, initial);
                }
                let mut carry = initial;
                let error = classify_block(&block, &mut carry).unwrap_err();
                assert_scanner_error(error, ErrorCode::InternalError);
                assert_eq!(carry, initial);
            }
        }
    }
}
