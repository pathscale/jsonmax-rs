//! Bounded block classification with guarded AVX-512 byte comparisons.
//!
//! Carry is committed only after quote analysis and ABI mask assembly succeed.
//! Partial blocks use owned, zero-padded storage; only live bytes affect masks
//! or outgoing quote state.

use super::*;

/// Classify at most 64 bytes, preserving carry on every error.
///
/// Oversized input produces `InvalidToken`; noncanonical quote carry produces
/// `InternalError`. Both errors have source `Scanner` and offset zero. Empty
/// input still validates carry, then returns zero masks with a scalar tag.
pub fn classify(input: &[u8], carry: &mut ScanCarry) -> Result<BlockMasks> {
    valid_mask(input.len())?;
    validate_carry(carry)?;
    if input.is_empty() {
        return Ok(BlockMasks::default());
    }

    let block = tails::load_block(input)?;
    classify_validated(&block, input.len(), carry)
}

/// Classify an entire array, detecting CPU support independently of callers.
///
/// The array need not have vector alignment. Invalid quote carry is rejected
/// before classification and remains unchanged.
pub fn classify_block(input: &[u8; BLOCK_BYTES], carry: &mut ScanCarry) -> Result<BlockMasks> {
    validate_carry(carry)?;
    classify_validated(input, BLOCK_BYTES, carry)
}

// Both callers validate the length and carry before entering this pipeline.
fn classify_validated(
    input: &[u8; BLOCK_BYTES],
    len: usize,
    carry: &mut ScanCarry,
) -> Result<BlockMasks> {
    let raw = classify_raw(input);
    let mut next_carry = *carry;
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

/// Classify all 64 bytes without applying string state or modifying carry.
///
/// Bit `i` describes byte `i`, including the highest bit for byte 63. The
/// backend identifies the path actually executed. Unsupported architectures,
/// CPUs, and OS configurations use the complete scalar fallback.
pub fn classify_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    if is_supported() {
        // SAFETY: is_supported checks both required features and, with std,
        // OS-enabled vector state. Without std it requires static features.
        // The reference supplies a complete readable 64-byte object.
        let (quote_mask, backslash_mask) = unsafe { quote_backslash_avx512(input) };
        return RawMasks {
            quote_mask,
            backslash_mask,
            classes: structural::classify_structural(input),
            backend: BACKEND,
        };
    }

    tails::classify_scalar_raw(input)
}

#[cfg(target_arch = "x86")]
use core::arch::x86::{
    __m512i, _mm512_cmpeq_epi8_mask, _mm512_loadu_si512, _mm512_set1_epi8,
};
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::{
    __m512i, _mm512_cmpeq_epi8_mask, _mm512_loadu_si512, _mm512_set1_epi8,
};

/// Caller must establish AVX-512F and AVX-512BW CPU and OS support.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx512f,avx512bw")]
unsafe fn quote_backslash_avx512(input: &[u8; BLOCK_BYTES]) -> (u64, u64) {
    // SAFETY: input covers exactly the 64 bytes read. The unaligned load
    // requires no extra alignment or padding and never reads past the array.
    let bytes = unsafe { _mm512_loadu_si512(input.as_ptr().cast::<__m512i>()) };
    let quotes = _mm512_cmpeq_epi8_mask(bytes, _mm512_set1_epi8(b'"' as i8));
    let backslashes = _mm512_cmpeq_epi8_mask(bytes, _mm512_set1_epi8(b'\\' as i8));
    (quotes, backslashes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn carry_with(in_string: u64, escaped: u64) -> ScanCarry {
        ScanCarry {
            prev_in_string: in_string,
            prev_escaped: escaped,
            utf8_tail: [0xf0, 0x9f, 0x92, 0xa9],
            // Deliberately opaque to the scanner, even if not a UTF-8 length.
            utf8_tail_len: 0xff,
        }
    }

    // An independent byte oracle: no tails, structural, quotes, finish, or
    // valid_mask helpers are used to derive the expected results.
    fn reference(input: &[u8], carry: &mut ScanCarry) -> BlockMasks {
        assert!(input.len() <= BLOCK_BYTES);
        let mut masks = BlockMasks::default();
        let mut inside = carry.prev_in_string != 0;
        let mut odd_run = carry.prev_escaped != 0;
        for (index, &byte) in input.iter().enumerate() {
            let bit = 1u64 << index;
            masks.valid_mask |= bit;
            let delimiter = byte == b'"' && !odd_run;
            if delimiter {
                inside = !inside;
            } else if inside {
                masks.in_string_mask |= bit;
            }
            match byte {
                b'"' => masks.quote_mask |= bit,
                b'\\' => masks.backslash_mask |= bit,
                b':' => masks.colon_mask |= bit,
                b',' => masks.comma_mask |= bit,
                b'{' => masks.lbrace_mask |= bit,
                b'}' => masks.rbrace_mask |= bit,
                b'[' => masks.lbracket_mask |= bit,
                b']' => masks.rbracket_mask |= bit,
                _ => {}
            }
            if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
                masks.whitespace_mask |= bit;
            }
            if byte < 0x20 {
                masks.control_mask |= bit;
            }
            if byte.is_ascii_digit() {
                masks.digit_mask |= bit;
            }
            if byte >= 0x80 {
                masks.non_ascii_mask |= bit;
            }
            odd_run = byte == b'\\' && !odd_run;
        }
        masks.structural_mask = (masks.colon_mask
            | masks.comma_mask
            | masks.lbrace_mask
            | masks.rbrace_mask
            | masks.lbracket_mask
            | masks.rbracket_mask)
            & !masks.in_string_mask;
        masks.valid_len = input.len() as u8;
        carry.prev_in_string = u64::from(inside);
        carry.prev_escaped = u64::from(odd_run);
        masks
    }

    fn expected_backend() -> u8 {
        if is_supported() { BACKEND_TAG } else { 0 }
    }

    fn compare_block(input: &[u8], initial: ScanCarry) -> BlockMasks {
        let mut actual_carry = initial;
        let mut expected_carry = initial;
        let actual = classify(input, &mut actual_carry).unwrap();
        let mut expected = reference(input, &mut expected_carry);
        if !input.is_empty() {
            expected.backend = expected_backend();
        }
        assert_eq!(actual, expected, "input: {:?}, carry: {:?}", input, initial);
        assert_eq!(actual_carry, expected_carry);
        actual
    }

    fn assert_raw(input: &[u8; BLOCK_BYTES]) {
        let raw = classify_raw(input);
        let expected = reference(input, &mut ScanCarry::default());
        assert_eq!(raw.quote_mask, expected.quote_mask);
        assert_eq!(raw.backslash_mask, expected.backslash_mask);
        assert_eq!(raw.classes, StructuralMasks {
            colon_mask: expected.colon_mask,
            comma_mask: expected.comma_mask,
            lbrace_mask: expected.lbrace_mask,
            rbrace_mask: expected.rbrace_mask,
            lbracket_mask: expected.lbracket_mask,
            rbracket_mask: expected.rbracket_mask,
            whitespace_mask: expected.whitespace_mask,
            control_mask: expected.control_mask,
            digit_mask: expected.digit_mask,
            non_ascii_mask: expected.non_ascii_mask,
        });
        assert_eq!(raw.backend, if is_supported() {
            BackendId::Avx512
        } else {
            BackendId::Scalar
        });
    }

    #[test]
    fn canonical_signature_and_empty_input_preserve_all_carry() {
        let entry: ClassifyFn = classify;
        for inside in 0..=1 {
            for escaped in 0..=1 {
                let mut carry = carry_with(inside, escaped);
                let before = carry;
                assert_eq!(entry(&[], &mut carry).unwrap(), BlockMasks::default());
                assert_eq!(carry, before);
            }
        }
    }

    #[test]
    fn every_byte_value_in_every_raw_lane() {
        for byte in 0u16..=255 {
            assert_raw(&[byte as u8; BLOCK_BYTES]);
        }
        // Odd stride permutes all byte values, breaking homogeneous patterns.
        for seed in 0..256usize {
            let mut block = [0u8; BLOCK_BYTES];
            for (lane, byte) in block.iter_mut().enumerate() {
                *byte = (seed + lane * 73) as u8;
            }
            assert_raw(&block);
        }
    }

    #[test]
    fn quote_and_backslash_masks_use_increasing_byte_order() {
        for lane in 0..BLOCK_BYTES {
            let mut block = [b'x'; BLOCK_BYTES];
            block[lane] = b'"';
            let raw = classify_raw(&block);
            assert_eq!(raw.quote_mask, 1u64 << lane);
            assert_eq!(raw.backslash_mask, 0);
            block[lane] = b'\\';
            let raw = classify_raw(&block);
            assert_eq!(raw.quote_mask, 0);
            assert_eq!(raw.backslash_mask, 1u64 << lane);
        }
        let mut block = [b'"'; BLOCK_BYTES];
        for lane in (1..BLOCK_BYTES).step_by(2) {
            block[lane] = b'\\';
        }
        let raw = classify_raw(&block);
        assert_eq!(raw.quote_mask, 0x5555_5555_5555_5555);
        assert_eq!(raw.backslash_mask, 0xaaaa_aaaa_aaaa_aaaa);
    }

    #[test]
    fn literal_masks_keep_raw_punctuation_inside_strings() {
        let input = [b'{', b'"', b':', b'\\', b'"', b',', b'"', b':',
            b'9', b'}', b'\t', 0, 0x80];
        let masks = compare_block(&input, carry_with(0, 0));
        assert_eq!(masks.quote_mask, 0x52);
        assert_eq!(masks.backslash_mask, 0x08);
        assert_eq!(masks.in_string_mask, 0x3c);
        assert_eq!(masks.colon_mask, 0x84);
        assert_eq!(masks.comma_mask, 0x20);
        assert_eq!(masks.structural_mask, 0x281);
        assert_eq!(masks.digit_mask, 0x100);
        assert_eq!(masks.whitespace_mask, 0x400);
        assert_eq!(masks.control_mask, 0xc00);
        assert_eq!(masks.non_ascii_mask, 0x1000);
        assert_eq!(masks.valid_mask, 0x1fff);
    }

    #[test]
    fn every_tail_length_clips_padding_and_preserves_opaque_utf8() {
        let pattern = [b'"', b'\\', b'\\', b'"', b':', b',', b'{', b'}',
            b'[', b']', b' ', b'\t', b'\r', b'\n', 0, 0x1f, b'0', b'9',
            b'/', b':', 0x7f, 0x80, 0xff, b'\\'];
        let mut block = [0u8; BLOCK_BYTES];
        for (index, byte) in block.iter_mut().enumerate() {
            *byte = pattern[index % pattern.len()];
        }
        for len in 0..=BLOCK_BYTES {
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    compare_block(&block[..len], carry_with(inside, escaped));
                }
            }
            let zeros = compare_block(&[0; BLOCK_BYTES][..len], carry_with(1, 1));
            assert_eq!(zeros.control_mask, zeros.valid_mask);
            assert_eq!(zeros.in_string_mask, zeros.valid_mask);
            let spaces = compare_block(&[b' '; BLOCK_BYTES][..len], carry_with(0, 0));
            assert_eq!(spaces.control_mask, 0);
            assert_eq!(spaces.whitespace_mask, spaces.valid_mask);
        }
    }

    #[test]
    fn fixed_array_and_slice_entry_points_agree() {
        let mut block = [b'x'; BLOCK_BYTES];
        for lane in [0, 15, 16, 31, 32, 47, 48, 62, 63] {
            block[lane] = if lane % 2 == 0 { b'\\' } else { b'"' };
        }
        for inside in 0..=1 {
            for escaped in 0..=1 {
                let initial = carry_with(inside, escaped);
                let expected = compare_block(&block, initial);
                let mut array_carry = initial;
                let mut slice_carry = initial;
                assert_eq!(classify_block(&block, &mut array_carry).unwrap(), expected);
                assert_eq!(classify(&block, &mut slice_carry).unwrap(), expected);
                assert_eq!(array_carry, slice_carry);
                assert_eq!(expected.valid_len, 64);
                assert_eq!(expected.valid_mask, u64::MAX);
            }
        }
    }

    #[test]
    fn backslash_run_parity_at_every_possible_length() {
        for run in 0..BLOCK_BYTES {
            let mut input = [b'\\'; BLOCK_BYTES];
            input[run] = b'"';
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    compare_block(&input[..run + 1], carry_with(inside, escaped));
                }
            }
        }
        for inside in 0..=1 {
            for escaped in 0..=1 {
                let mut carry = carry_with(inside, escaped);
                let masks = classify_block(&[b'\\'; BLOCK_BYTES], &mut carry).unwrap();
                assert_eq!(masks.backslash_mask, u64::MAX);
                assert_eq!(carry, carry_with(inside, escaped));
                let next = classify(b"\"", &mut carry).unwrap();
                assert_eq!(next.quote_mask, 1);
                assert_eq!(next.in_string_mask, inside & escaped);
                assert_eq!(carry.prev_in_string, inside ^ (escaped ^ 1));
                assert_eq!(carry.prev_escaped, 0);
            }
        }
    }

    #[test]
    fn arbitrary_stream_splits_match_continuous_byte_state() {
        let mut stream = [b'x'; 128];
        for (index, byte) in stream.iter_mut().enumerate() {
            *byte = match index % 13 {
                0 | 1 | 2 => b'\\',
                3 | 7 => b'"',
                4 => b':',
                5 => b',',
                6 => b'\t',
                8 => 0xff,
                9 => 0,
                10 => b'9',
                _ => b'x',
            };
        }
        // An escape run spans both byte 63 and byte 64.
        stream[62..65].copy_from_slice(b"\\\\\\");
        stream[65] = b'"';
        for split in 0..=BLOCK_BYTES {
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let initial = carry_with(inside, escaped);
                    let mut actual_carry = initial;
                    let mut expected_carry = initial;
                    for part in [&stream[..split], &stream[split..split + BLOCK_BYTES],
                        &stream[split + BLOCK_BYTES..]] {
                        let actual = classify(part, &mut actual_carry).unwrap();
                        let mut expected = reference(part, &mut expected_carry);
                        if !part.is_empty() {
                            expected.backend = expected_backend();
                        }
                        assert_eq!(actual, expected);
                        assert_eq!(actual_carry, expected_carry);
                    }
                    let mut bytewise_carry = initial;
                    for byte in &stream {
                        reference(core::slice::from_ref(byte), &mut bytewise_carry);
                    }
                    assert_eq!(actual_carry, bytewise_carry);
                }
            }
        }
    }

    #[test]
    fn unaligned_full_arrays_and_bounded_subslices() {
        #[repr(align(64))]
        struct Aligned([u8; 128]);
        let mut storage = Aligned([0xff; 128]);
        storage.0[1..65].fill(b'x');
        storage.0[1] = b'"';
        storage.0[64] = b'\\';
        let block: &[u8; BLOCK_BYTES] = (&storage.0[1..65]).try_into().unwrap();
        assert_raw(block);
        let mut carry = carry_with(0, 0);
        let masks = classify_block(block, &mut carry).unwrap();
        assert_eq!(masks.quote_mask, 1);
        assert_eq!(masks.backslash_mask, 1u64 << 63);
        assert_eq!(masks.non_ascii_mask, 0);
        assert_eq!(carry, carry_with(1, 1));
        for len in 0..=BLOCK_BYTES {
            compare_block(&storage.0[1..1 + len], carry_with(0, 0));
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
    fn oversized_input_has_no_carry_side_effects() {
        let input = [b'"'; 257];
        for len in [65, 127, 128, 255, 256, 257] {
            for initial in [carry_with(0, 0), carry_with(1, 1), carry_with(2, 2)] {
                let mut carry = initial;
                assert_error(classify(&input[..len], &mut carry).unwrap_err(),
                    ErrorCode::InvalidToken);
                assert_eq!(carry, initial);
            }
        }
    }

    #[test]
    fn noncanonical_carry_is_rejected_even_for_empty_input() {
        for invalid in [2, 255, 256, 1u64 << 63, u64::MAX] {
            for initial in [carry_with(invalid, 0), carry_with(0, invalid),
                carry_with(invalid, invalid)] {
                for input in [&b""[..], &b"\"\\"[..], &[b'"'; BLOCK_BYTES][..]] {
                    let mut carry = initial;
                    assert_error(classify(input, &mut carry).unwrap_err(),
                        ErrorCode::InternalError);
                    assert_eq!(carry, initial);
                }
                let mut carry = initial;
                assert_error(classify_block(&[b'"'; BLOCK_BYTES], &mut carry).unwrap_err(),
                    ErrorCode::InternalError);
                assert_eq!(carry, initial);
            }
        }
    }

    #[test]
    fn syntax_utf8_and_unclosed_strings_are_not_scanner_errors() {
        for input in [&b"\"\\q\x00\xff\\"[..], &b"}{][,,::"[..],
            &b"\\\"\x80\xc0\xaf\xed\xa0\x80"[..]] {
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    compare_block(input, carry_with(inside, escaped));
                }
            }
        }
    }
}
