//! Raw byte classes for one complete 64-byte block.
//!
//! Every mask uses increasing byte order, with byte 63 in the highest bit.
//! Classification is independent of strings, JSON syntax, and UTF-8 validity.

use super::*;

/// Classify all bytes in `input`, without requiring alignment or padding.
///
/// Punctuation matches its ASCII spelling. Whitespace is exactly space, tab,
/// carriage return, and line feed; control bytes are all bytes below `0x20`.
/// Digits are ASCII `0` through `9`, and non-ASCII bytes are `0x80..=0xff`.
/// Tab, carriage return, and line feed belong to both whitespace and control.
///
/// This function independently detects AVX-512F and AVX-512BW support before
/// using vector instructions. Other configurations use a scalar implementation.
/// Every possible input is accepted: no allocation, carry update, validation,
/// or error path is involved. Callers handling a partial block must clip the
/// resulting masks to its true length, including the control bits from padding.
#[inline]
pub fn classify_structural(input: &[u8; BLOCK_BYTES]) -> StructuralMasks {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    if is_supported() {
        // SAFETY: is_supported establishes AVX-512F and AVX-512BW support,
        // including OS-enabled register state with std, or statically enabled
        // features without std. The input reference covers a full 64 bytes.
        return unsafe { classify_structural_avx512(input) };
    }

    classify_structural_scalar(input)
}

fn classify_structural_scalar(input: &[u8; BLOCK_BYTES]) -> StructuralMasks {
    let mut masks = StructuralMasks::default();
    for (index, &byte) in input.iter().enumerate() {
        // The array bounds index to 0..64, so the shift is always below 64.
        let bit = 1u64 << index;
        match byte {
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
    }
    masks
}

#[cfg(target_arch = "x86")]
use core::arch::x86::{
    __m512i, _mm512_cmpeq_epi8_mask, _mm512_cmplt_epu8_mask, _mm512_loadu_si512,
    _mm512_set1_epi8,
};
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::{
    __m512i, _mm512_cmpeq_epi8_mask, _mm512_cmplt_epu8_mask, _mm512_loadu_si512,
    _mm512_set1_epi8,
};

/// Caller must establish AVX-512F and AVX-512BW CPU and OS support.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx512f,avx512bw")]
unsafe fn classify_structural_avx512(input: &[u8; BLOCK_BYTES]) -> StructuralMasks {
    // SAFETY: the reference guarantees all 64 bytes read belong to a live
    // array. This load is unaligned and requires no bytes beyond that array.
    let bytes = unsafe { _mm512_loadu_si512(input.as_ptr().cast::<__m512i>()) };

    // Byte comparisons produce native 64-bit predicates in address order.
    // Unsigned ranges keep high-bit bytes out of controls and ASCII digits.
    let below_zero = _mm512_cmplt_epu8_mask(bytes, _mm512_set1_epi8(b'0' as i8));
    let below_colon = _mm512_cmplt_epu8_mask(bytes, _mm512_set1_epi8(b':' as i8));
    let ascii = _mm512_cmplt_epu8_mask(bytes, _mm512_set1_epi8(i8::MIN));

    StructuralMasks {
        colon_mask: _mm512_cmpeq_epi8_mask(bytes, _mm512_set1_epi8(b':' as i8)),
        comma_mask: _mm512_cmpeq_epi8_mask(bytes, _mm512_set1_epi8(b',' as i8)),
        lbrace_mask: _mm512_cmpeq_epi8_mask(bytes, _mm512_set1_epi8(b'{' as i8)),
        rbrace_mask: _mm512_cmpeq_epi8_mask(bytes, _mm512_set1_epi8(b'}' as i8)),
        lbracket_mask: _mm512_cmpeq_epi8_mask(bytes, _mm512_set1_epi8(b'[' as i8)),
        rbracket_mask: _mm512_cmpeq_epi8_mask(bytes, _mm512_set1_epi8(b']' as i8)),
        whitespace_mask: _mm512_cmpeq_epi8_mask(bytes, _mm512_set1_epi8(b' ' as i8))
            | _mm512_cmpeq_epi8_mask(bytes, _mm512_set1_epi8(b'\t' as i8))
            | _mm512_cmpeq_epi8_mask(bytes, _mm512_set1_epi8(b'\r' as i8))
            | _mm512_cmpeq_epi8_mask(bytes, _mm512_set1_epi8(b'\n' as i8)),
        control_mask: _mm512_cmplt_epu8_mask(bytes, _mm512_set1_epi8(0x20)),
        digit_mask: below_colon & !below_zero,
        non_ascii_mask: !ascii,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build one expected mask at a time, independently of either production
    // implementation, including its combined punctuation dispatch.
    fn mask_where(input: &[u8; BLOCK_BYTES], predicate: impl Fn(u8) -> bool) -> u64 {
        input.iter().enumerate().fold(0, |mask, (index, &byte)| {
            mask | (u64::from(predicate(byte)) << index)
        })
    }

    fn reference(input: &[u8; BLOCK_BYTES]) -> StructuralMasks {
        StructuralMasks {
            colon_mask: mask_where(input, |b| b == 0x3a),
            comma_mask: mask_where(input, |b| b == 0x2c),
            lbrace_mask: mask_where(input, |b| b == 0x7b),
            rbrace_mask: mask_where(input, |b| b == 0x7d),
            lbracket_mask: mask_where(input, |b| b == 0x5b),
            rbracket_mask: mask_where(input, |b| b == 0x5d),
            whitespace_mask: mask_where(input, |b| [0x20, 0x09, 0x0d, 0x0a].contains(&b)),
            control_mask: mask_where(input, |b| (0..=31).contains(&b)),
            digit_mask: mask_where(input, |b| b"0123456789".contains(&b)),
            non_ascii_mask: mask_where(input, |b| b & 0x80 != 0),
        }
    }

    fn assert_paths(input: &[u8; BLOCK_BYTES]) -> StructuralMasks {
        let before = *input;
        let expected = reference(input);
        assert_eq!(classify_structural_scalar(input), expected, "scalar: {input:?}");
        let actual = classify_structural(input);
        assert_eq!(actual, expected, "dispatch: {input:?}");
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        if is_supported() {
            // SAFETY: the feature check establishes the private helper's CPU
            // and OS requirements, and input is a complete 64-byte array.
            let vector = unsafe { classify_structural_avx512(input) };
            assert_eq!(vector, expected, "AVX-512: {input:?}");
        }
        assert_eq!(*input, before);
        actual
    }

    #[test]
    fn signature_and_literal_classes() {
        let entry: fn(&[u8; BLOCK_BYTES]) -> StructuralMasks = classify_structural;
        let mut block = [b'x'; BLOCK_BYTES];
        block[..20].copy_from_slice(&[
            b':', b',', b'{', b'}', b'[', b']', b' ', b'\t', b'\r', b'\n',
            0, 0x1f, b'0', b'9', b'/', b';', 0x7f, 0x80, 0xff, b'"',
        ]);
        let expected = StructuralMasks {
            colon_mask: 0x1,
            comma_mask: 0x2,
            lbrace_mask: 0x4,
            rbrace_mask: 0x8,
            lbracket_mask: 0x10,
            rbracket_mask: 0x20,
            whitespace_mask: 0x3c0,
            control_mask: 0xf80,
            digit_mask: 0x3000,
            non_ascii_mask: 0x60000,
        };
        assert_eq!(entry(&block), expected);
        assert_eq!(assert_paths(&block), expected);
        assert_eq!(expected.punctuation_mask(), 0x3f);
    }

    #[test]
    fn every_uniform_byte_covers_zero_and_full_width_masks() {
        for byte in 0u8..=u8::MAX {
            assert_paths(&[byte; BLOCK_BYTES]);
        }
        assert_eq!(classify_structural(&[0; BLOCK_BYTES]), StructuralMasks {
            control_mask: u64::MAX,
            ..StructuralMasks::default()
        });
        assert_eq!(classify_structural(&[0xff; BLOCK_BYTES]), StructuralMasks {
            non_ascii_mask: u64::MAX,
            ..StructuralMasks::default()
        });
        assert_eq!(classify_structural(&[b'x'; BLOCK_BYTES]), StructuralMasks::default());
    }

    #[test]
    fn every_byte_in_every_position_has_exact_bit_order() {
        for byte in 0u8..=u8::MAX {
            for index in 0..BLOCK_BYTES {
                let mut block = [b'x'; BLOCK_BYTES];
                block[index] = byte;
                assert_paths(&block);
            }
        }
    }

    #[test]
    fn adjacent_lanes_across_all_vector_boundaries_are_independent() {
        for boundary in [1, 8, 16, 32, 48, 63] {
            let mut block = [b'x'; BLOCK_BYTES];
            block[boundary - 1] = b':';
            block[boundary] = b',';
            let masks = assert_paths(&block);
            assert_eq!(masks.colon_mask, 1u64 << (boundary - 1));
            assert_eq!(masks.comma_mask, 1u64 << boundary);
        }
        let mut block = [b'0'; BLOCK_BYTES];
        for index in (1..BLOCK_BYTES).step_by(2) {
            block[index] = 0xff;
        }
        let masks = assert_paths(&block);
        assert_eq!(masks.digit_mask, 0x5555_5555_5555_5555);
        assert_eq!(masks.non_ascii_mask, 0xaaaa_aaaa_aaaa_aaaa);
        assert_eq!(masks.control_mask | masks.whitespace_mask | masks.punctuation_mask(), 0);
    }

    #[test]
    fn deterministic_permutations_and_mixed_blocks_match_reference() {
        for seed in 0..256usize {
            let mut block = [0u8; BLOCK_BYTES];
            for (index, byte) in block.iter_mut().enumerate() {
                *byte = (seed + index * 73) as u8;
            }
            assert_paths(&block);
        }
        let mut state = 0x243f_6a88_85a3_08d3u64;
        for _ in 0..512 {
            let mut block = [0u8; BLOCK_BYTES];
            for byte in &mut block {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = (state >> 56) as u8;
            }
            assert_paths(&block);
        }
    }

    #[test]
    fn whitespace_controls_and_unsigned_ranges_have_exact_boundaries() {
        let values = [
            0x00, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x1f, 0x20, 0x21, 0x2f, 0x30, 0x39, 0x3a, 0x7f,
            0x80, 0x85, 0xa0, 0xb0, 0xb9, 0xff,
        ];
        let mut block = [b'x'; BLOCK_BYTES];
        block[..values.len()].copy_from_slice(&values);
        let masks = assert_paths(&block);
        assert_eq!(masks.whitespace_mask, (1 << 2) | (1 << 3) | (1 << 6) | (1 << 9));
        assert_eq!(masks.control_mask, 0x1ff);
        assert_eq!(masks.whitespace_mask & masks.control_mask, (1 << 2) | (1 << 3) | (1 << 6));
        assert_eq!(masks.digit_mask, (1 << 12) | (1 << 13));
        assert_eq!(masks.non_ascii_mask, 0x3f0000);
    }

    #[test]
    fn syntax_strings_and_unicode_do_not_change_raw_classification() {
        let mut block = [b'x'; BLOCK_BYTES];
        let content = b"\":,{}[]\"\\q\x00\xc0\xaf\xed\xa0\x80\xff\xc2\xa0\xe2\x80\xa8\xd9\xa0\xef\xbc\x90";
        block[..content.len()].copy_from_slice(content);
        let masks = assert_paths(&block);
        assert_eq!(masks.punctuation_mask(), 0x7e);
        assert_eq!(masks.whitespace_mask, 0);
        assert_eq!(masks.digit_mask, 0);
        assert_eq!(masks.control_mask, 1 << 10);
        // Quotes and backslashes are not among these ten raw classes. Invalid
        // UTF-8 and Unicode whitespace/digits are reported only as high bytes.
        assert_eq!(masks.non_ascii_mask, ((1u64 << content.len()) - 1) & !0x7ff);
    }

    #[test]
    fn all_address_offsets_accept_exact_arrays_without_alignment() {
        #[repr(align(64))]
        struct Storage([u8; 128]);

        for offset in 0..64 {
            let mut storage = Storage([0xff; 128]);
            let end = offset + BLOCK_BYTES;
            storage.0[offset..end].fill(b'x');
            storage.0[offset] = b'{';
            storage.0[end - 1] = b'}';
            let before = storage.0;
            let input: &[u8; BLOCK_BYTES] = (&storage.0[offset..end]).try_into().unwrap();
            assert_eq!(input.as_ptr() as usize % 64, offset);
            let masks = assert_paths(input);
            assert_eq!(masks.lbrace_mask, 1);
            assert_eq!(masks.rbrace_mask, 1u64 << 63);
            assert_eq!(masks.non_ascii_mask, 0);
            assert_eq!(storage.0, before);
        }
    }

    #[test]
    fn root_assembly_clips_raw_masks_at_every_valid_length() {
        for byte in [b':', b',', b'{', b'}', b'[', b']', b' ', b'\t', 0, b'9', 0xff] {
            let raw = RawMasks {
                classes: classify_structural(&[byte; BLOCK_BYTES]),
                ..RawMasks::default()
            };
            for len in 0..=BLOCK_BYTES {
                let live = (0..len).fold(0u64, |mask, index| mask | (1u64 << index));
                let block = raw.finish(QuoteMasks::default(), len).unwrap();
                let clipped = StructuralMasks {
                    colon_mask: block.colon_mask,
                    comma_mask: block.comma_mask,
                    lbrace_mask: block.lbrace_mask,
                    rbrace_mask: block.rbrace_mask,
                    lbracket_mask: block.lbracket_mask,
                    rbracket_mask: block.rbracket_mask,
                    whitespace_mask: block.whitespace_mask,
                    control_mask: block.control_mask,
                    digit_mask: block.digit_mask,
                    non_ascii_mask: block.non_ascii_mask,
                };
                let mut expected_input = [b'x'; BLOCK_BYTES];
                expected_input[..len].fill(byte);
                assert_eq!(clipped, reference(&expected_input));
                assert_eq!(block.valid_mask, live);
                assert_eq!(usize::from(block.valid_len), len);
                assert_eq!(block.structural_mask, raw.classes.punctuation_mask() & live);

                let inside = raw.finish(QuoteMasks {
                    in_string_mask: u64::MAX,
                    ..QuoteMasks::default()
                }, len).unwrap();
                assert_eq!(inside.structural_mask, 0);
                assert_eq!(inside.in_string_mask, live);
                // String filtering belongs to assembly; all raw fields survive.
                assert_eq!(inside, BlockMasks {
                    in_string_mask: live,
                    structural_mask: 0,
                    ..block
                });
            }
        }
    }

    #[test]
    fn root_assembly_rejects_invalid_lengths_without_changing_raw_classes() {
        // classify_structural is infallible and has no length or carry input.
        // Exercise the actual error boundary where its masks become ABI blocks.
        let raw = RawMasks {
            classes: classify_structural(&[b':'; BLOCK_BYTES]),
            ..RawMasks::default()
        };
        let before = raw;
        for len in [65, 127, 128, 255, 256, usize::MAX] {
            let error = raw.finish(QuoteMasks::default(), len).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidToken);
            assert_eq!(error.source, ErrorSource::Scanner);
            assert_eq!(error.byte_offset, 0);
            assert_eq!(error.schema_id, 0);
            assert_eq!(error.field_id, 0);
            assert!(!error.detail.is_empty());
            assert_eq!(raw, before);
        }
        assert_eq!(raw.finish(QuoteMasks::default(), 64).unwrap().colon_mask, u64::MAX);
    }
}
