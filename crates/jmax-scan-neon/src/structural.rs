//! Raw lexical classes for four consecutive 16-byte lanes.
//!
//! Every bit follows byte-address order. Quoting, tail clipping, UTF-8
//! validation, and grammar validation belong to the surrounding pipeline.

use super::*;

/// Classify all 64 bytes, independently of string state.
///
/// Punctuation matches `: , { } [ ]`. JSON whitespace is exactly space,
/// tab, CR, and LF; control bytes are below 0x20, digits are ASCII 0 through
/// 9, and non-ASCII bytes are at least 0x80. Tab, CR, and LF belong to both
/// whitespace and control classes. Quotes and backslashes are not punctuation.
///
/// This infallible function accepts arbitrary byte values, allocates nothing,
/// and requires neither alignment nor padding beyond the supplied array.
/// It checks NEON support independently and otherwise uses a local scalar
/// implementation, including on non-AArch64 and no_std targets.
pub fn classify_structural(input: &[u8; BLOCK_BYTES]) -> StructuralMasks {
    #[cfg(target_arch = "aarch64")]
    if is_supported() {
        // SAFETY: the support check establishes NEON availability. The array
        // contains the complete 64-byte object required by the native loads.
        return unsafe { classify_neon(input) };
    }

    classify_scalar(input)
}

fn classify_scalar(input: &[u8; BLOCK_BYTES]) -> StructuralMasks {
    let mut masks = StructuralMasks::default();
    for (index, &byte) in input.iter().enumerate() {
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
        if byte >= b'0' && byte <= b'9' {
            masks.digit_mask |= bit;
        }
        if byte >= 0x80 {
            masks.non_ascii_mask |= bit;
        }
    }
    masks
}

/// Reduce comparison bytes (0x00 or 0xff) in the same order as classify_raw.
/// Each eight-byte half sums distinct powers of two, never exceeding 255.
/// No integer reinterpretation or native-endian load participates in reduction.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn comparison_mask(value: core::arch::aarch64::uint8x16_t) -> u16 {
    use core::arch::aarch64::*;

    let weights = [1u8, 2, 4, 8, 16, 32, 64, 128];
    // SAFETY: the caller establishes NEON availability. The unaligned load
    // reads exactly the eight initialized bytes in weights.
    unsafe {
        let weights = vld1_u8(weights.as_ptr());
        let low = vaddv_u8(vand_u8(vget_low_u8(value), weights));
        let high = vaddv_u8(vand_u8(vget_high_u8(value), weights));
        u16::from(low) | (u16::from(high) << 8)
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn classify_neon(input: &[u8; BLOCK_BYTES]) -> StructuralMasks {
    use core::arch::aarch64::*;

    let mut masks = StructuralMasks::default();
    // SAFETY: the only production caller checks NEON support. Offsets
    // 0, 16, 32, and 48 each leave 16 initialized bytes in input. vld1q_u8
    // accepts unaligned addresses. The private reduction helper inherits
    // this feature guarantee and receives only 0x00/0xff comparison bytes.
    unsafe {
        let colon = vdupq_n_u8(b':');
        let comma = vdupq_n_u8(b',');
        let lbrace = vdupq_n_u8(b'{');
        let rbrace = vdupq_n_u8(b'}');
        let lbracket = vdupq_n_u8(b'[');
        let rbracket = vdupq_n_u8(b']');
        let space = vdupq_n_u8(b' ');
        let tab = vdupq_n_u8(b'\t');
        let cr = vdupq_n_u8(b'\r');
        let lf = vdupq_n_u8(b'\n');
        let digit_low = vdupq_n_u8(b'0');
        let digit_high = vdupq_n_u8(b'9');
        let non_ascii_low = vdupq_n_u8(0x80);

        for lane in 0..VECTOR_LANES {
            let offset = lane * LANE_BYTES;
            let bytes = vld1q_u8(input.as_ptr().add(offset));
            let whitespace = vorrq_u8(
                vorrq_u8(vceqq_u8(bytes, space), vceqq_u8(bytes, tab)),
                vorrq_u8(vceqq_u8(bytes, cr), vceqq_u8(bytes, lf)),
            );
            let digits = vandq_u8(
                vcgeq_u8(bytes, digit_low),
                vcleq_u8(bytes, digit_high),
            );

            // Each mask is widened before shifting, including lane 3.
            masks.colon_mask |= u64::from(comparison_mask(vceqq_u8(bytes, colon))) << offset;
            masks.comma_mask |= u64::from(comparison_mask(vceqq_u8(bytes, comma))) << offset;
            masks.lbrace_mask |= u64::from(comparison_mask(vceqq_u8(bytes, lbrace))) << offset;
            masks.rbrace_mask |= u64::from(comparison_mask(vceqq_u8(bytes, rbrace))) << offset;
            masks.lbracket_mask |= u64::from(comparison_mask(vceqq_u8(bytes, lbracket))) << offset;
            masks.rbracket_mask |= u64::from(comparison_mask(vceqq_u8(bytes, rbracket))) << offset;
            masks.whitespace_mask |= u64::from(comparison_mask(whitespace)) << offset;
            masks.control_mask |= u64::from(comparison_mask(vcltq_u8(bytes, space))) << offset;
            masks.digit_mask |= u64::from(comparison_mask(digits)) << offset;
            masks.non_ascii_mask |=
                u64::from(comparison_mask(vcgeq_u8(bytes, non_ascii_low))) << offset;
        }
    }
    masks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn components(masks: StructuralMasks) -> [u64; 10] {
        [
            masks.colon_mask, masks.comma_mask, masks.lbrace_mask,
            masks.rbrace_mask, masks.lbracket_mask, masks.rbracket_mask,
            masks.whitespace_mask, masks.control_mask, masks.digit_mask,
            masks.non_ascii_mask,
        ]
    }

    // Independent membership oracle, using byte sets rather than production
    // range comparisons. It does not call any scanner helper.
    fn membership(byte: u8) -> [bool; 10] {
        [
            byte == b':', byte == b',', byte == b'{', byte == b'}',
            byte == b'[', byte == b']', b" \t\r\n".contains(&byte),
            (0u8..32).contains(&byte), b"0123456789".contains(&byte),
            byte & 0x80 != 0,
        ]
    }

    fn reference(input: &[u8; BLOCK_BYTES]) -> [u64; 10] {
        let mut expected = [0u64; 10];
        for (position, &byte) in input.iter().enumerate() {
            for (mask, matches) in expected.iter_mut().zip(membership(byte)) {
                if matches {
                    *mask |= 1u64 << position;
                }
            }
        }
        expected
    }

    fn assert_paths(input: &[u8; BLOCK_BYTES]) {
        let expected = reference(input);
        assert_eq!(components(classify_scalar(input)), expected, "scalar {:?}", input);
        let actual = classify_structural(input);
        assert_eq!(components(actual), expected, "selected {:?}", input);
        let punctuation = expected[..6].iter().fold(0, |all, mask| all | mask);
        assert_eq!(actual.punctuation_mask(), punctuation);
        assert_eq!(punctuation & (actual.whitespace_mask | actual.control_mask
            | actual.digit_mask | actual.non_ascii_mask), 0);
    }

    #[test]
    fn exact_mixed_class_vector() {
        let mut input = [b'x'; BLOCK_BYTES];
        input[..24].copy_from_slice(b":,{}[] \t\r\n\x00\x1f09/\x3a\x7f\x80\xff\x0b\x0c\"\\\xa0");
        assert_eq!(classify_structural(&input), StructuralMasks {
            colon_mask: (1 << 0) | (1 << 15),
            comma_mask: 1 << 1,
            lbrace_mask: 1 << 2,
            rbrace_mask: 1 << 3,
            lbracket_mask: 1 << 4,
            rbracket_mask: 1 << 5,
            whitespace_mask: 0x3c0,
            control_mask: 0xf80 | (1 << 19) | (1 << 20),
            digit_mask: (1 << 12) | (1 << 13),
            non_ascii_mask: (1 << 17) | (1 << 18) | (1 << 23),
        });
        assert_paths(&input);
    }

    #[test]
    fn every_byte_value_at_every_position() {
        for byte in 0u8..=u8::MAX {
            let classes = membership(byte);
            for position in 0..BLOCK_BYTES {
                let mut input = [b'x'; BLOCK_BYTES];
                input[position] = byte;
                let mut expected = [0u64; 10];
                for (mask, matches) in expected.iter_mut().zip(classes) {
                    if matches {
                        *mask = 1u64 << position;
                    }
                }
                assert_eq!(components(classify_scalar(&input)), expected);
                assert_eq!(components(classify_structural(&input)), expected,
                    "byte {}, position {}", byte, position);
            }
        }
    }

    #[test]
    fn all_uniform_blocks_include_full_width_masks() {
        for byte in 0u8..=u8::MAX {
            let input = [byte; BLOCK_BYTES];
            let mut expected = [0u64; 10];
            for (mask, matches) in expected.iter_mut().zip(membership(byte)) {
                if matches {
                    *mask = u64::MAX;
                }
            }
            assert_eq!(components(classify_structural(&input)), expected);
            assert_paths(&input);
        }
    }

    #[test]
    fn lane_boundaries_and_high_bit_keep_address_order() {
        let positions = [0, 15, 16, 31, 32, 47, 48, 63];
        for byte in [b':', b',', b'{', b'}', b'[', b']', b'\t', 0, b'9', 0xff] {
            let mut input = [b'x'; BLOCK_BYTES];
            for position in positions {
                input[position] = byte;
            }
            for (mask, matches) in components(classify_structural(&input))
                .into_iter().zip(membership(byte))
            {
                assert_eq!(mask, if matches { 0x8001_8001_8001_8001 } else { 0 });
            }
            assert_paths(&input);
        }
        let mut input = [b'0'; BLOCK_BYTES];
        for position in (1..BLOCK_BYTES).step_by(2) {
            input[position] = 0x80;
        }
        let masks = classify_structural(&input);
        assert_eq!(masks.digit_mask, 0x5555_5555_5555_5555);
        assert_eq!(masks.non_ascii_mask, 0xaaaa_aaaa_aaaa_aaaa);
        assert_paths(&input);
    }

    #[test]
    fn unsigned_boundaries_and_non_json_whitespace() {
        let alphabet = b"\x00\x08\t\n\x0b\x0c\r\x1f !/019:;\x7f\x80\x85\xa0\xff";
        for rotation in 0..alphabet.len() {
            let mut input = [0u8; BLOCK_BYTES];
            for (position, byte) in input.iter_mut().enumerate() {
                *byte = alphabet[(position + rotation) % alphabet.len()];
            }
            assert_paths(&input);
        }
    }

    #[test]
    fn deterministic_permutations_and_mixed_blocks() {
        for seed in 0u8..=u8::MAX {
            let mut input = [0u8; BLOCK_BYTES];
            for (position, byte) in input.iter_mut().enumerate() {
                *byte = seed.wrapping_add((position as u8).wrapping_mul(73));
            }
            assert_paths(&input);
        }
        let mut seed = 0x243f_6a88_85a3_08d3u64;
        for _ in 0..512 {
            let mut input = [0u8; BLOCK_BYTES];
            for byte in &mut input {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *byte = (seed >> 32) as u8;
            }
            assert_paths(&input);
        }
    }

    #[test]
    fn arrays_at_every_alignment_are_read_without_mutation() {
        let mut storage = [0u8; BLOCK_BYTES + LANE_BYTES - 1];
        let alphabet = b":,{}[] \t\r\n\x00\x1f09\x7f\x80\xff";
        for (position, byte) in storage.iter_mut().enumerate() {
            *byte = alphabet[position % alphabet.len()];
        }
        let before = storage;
        for offset in 0..LANE_BYTES {
            let input: &[u8; BLOCK_BYTES] =
                storage[offset..offset + BLOCK_BYTES].try_into().unwrap();
            assert_paths(input);
        }
        assert_eq!(storage, before);
    }

    #[test]
    fn raw_classes_survive_quoting_and_invalid_utf8() {
        let mut input = [b'x'; BLOCK_BYTES];
        input[0] = b'"';
        input[1..15].copy_from_slice(b":,{}[] \t\r\n0\x00\x80\xff");
        input[63] = b'"';
        let classes = classify_structural(&input);
        assert_paths(&input);
        let mut carry = ScanCarry {
            utf8_tail: [0xff, 0x80, 0xc0, 0],
            utf8_tail_len: 255,
            ..ScanCarry::default()
        };
        let before = carry;
        let quote_mask = 1 | (1u64 << 63);
        let quotes = quotes::quote_masks(quote_mask, 0, 64, &mut carry).unwrap();
        let block = RawMasks {
            quote_mask, classes, ..RawMasks::default()
        }.finish(quotes, BLOCK_BYTES).unwrap();
        assert_eq!(classes.punctuation_mask(), 0x7e);
        assert_eq!(block.structural_mask, 0);
        assert_eq!(block.colon_mask, classes.colon_mask);
        assert_eq!(block.control_mask, classes.control_mask);
        assert_eq!(block.whitespace_mask, classes.whitespace_mask);
        assert_eq!(block.digit_mask, classes.digit_mask);
        assert_eq!(block.non_ascii_mask, classes.non_ascii_mask);
        assert_eq!(carry, before);
    }

    #[test]
    fn assembly_clips_raw_classes_at_every_valid_length() {
        for byte in [b':', b',', b'{', b'}', b'[', b']', b'\t', 0, b'9', 0xff] {
            let raw = RawMasks {
                classes: classify_structural(&[byte; BLOCK_BYTES]),
                ..RawMasks::default()
            };
            for len in 0..=BLOCK_BYTES {
                let valid = if len == BLOCK_BYTES { u64::MAX } else { (1u64 << len) - 1 };
                let block = raw.finish(QuoteMasks::default(), len).unwrap();
                let actual = [
                    block.colon_mask, block.comma_mask, block.lbrace_mask,
                    block.rbrace_mask, block.lbracket_mask, block.rbracket_mask,
                    block.whitespace_mask, block.control_mask, block.digit_mask,
                    block.non_ascii_mask,
                ];
                for (mask, full) in actual.into_iter().zip(components(raw.classes)) {
                    assert_eq!(mask, full & valid);
                }
                assert_eq!(block.structural_mask, raw.classes.punctuation_mask() & valid);
                assert_eq!(block.valid_mask, valid);
                assert_eq!(usize::from(block.valid_len), len);
            }
        }
    }

    // Structural classification itself is infallible. Error verification
    // therefore exercises its frozen assembly and quote integration boundary.
    #[test]
    fn integration_errors_preserve_raw_masks_and_carry() {
        let raw = RawMasks {
            classes: classify_structural(&[b':'; BLOCK_BYTES]),
            ..RawMasks::default()
        };
        let before = raw;
        for len in [65, 255, 256, usize::MAX] {
            let error = raw.finish(QuoteMasks::default(), len).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidToken);
            assert_eq!(error.source, ErrorSource::Scanner);
            assert_eq!(error.byte_offset, 0);
            assert_eq!(raw, before);
        }
        for initial in [
            ScanCarry { prev_in_string: 2, ..ScanCarry::default() },
            ScanCarry { prev_escaped: u64::MAX, ..ScanCarry::default() },
        ] {
            let mut carry = initial;
            let error = quotes::quote_masks(0, 0, 64, &mut carry).unwrap_err();
            assert_eq!(error.code, ErrorCode::InternalError);
            assert_eq!(error.source, ErrorSource::Scanner);
            assert_eq!(error.byte_offset, 0);
            assert_eq!(carry, initial);
            assert_eq!(classify_structural(&[b':'; BLOCK_BYTES]), raw.classes);
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn native_reduction_covers_every_sixteen_bit_pattern() {
        if !is_supported() {
            return;
        }
        for expected in 0u16..=u16::MAX {
            let mut bytes = [0u8; LANE_BYTES];
            for (position, byte) in bytes.iter_mut().enumerate() {
                if expected & (1u16 << position) != 0 {
                    *byte = 0xff;
                }
            }
            // SAFETY: support was checked above and the load reads exactly
            // 16 initialized bytes. Every byte satisfies the reducer contract.
            let actual = unsafe {
                comparison_mask(core::arch::aarch64::vld1q_u8(bytes.as_ptr()))
            };
            assert_eq!(actual, expected);
        }
    }
}
