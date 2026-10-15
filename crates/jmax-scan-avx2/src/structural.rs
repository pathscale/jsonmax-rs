//! Raw byte classes for a complete block, independent of JSON string state.
//!
//! The AVX2 implementation consumes two bounded, unaligned 32-byte vectors.
//! Every safe call checks ISA support and otherwise uses the local scalar path.

use super::*;

#[cfg(target_arch = "x86")]
use core::arch::x86 as arch;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64 as arch;

/// Classify all 64 bytes, with bit `i` describing `input[i]`.
///
/// Punctuation matches the six ASCII JSON delimiters. Whitespace is exactly
/// space, tab, CR, and LF; control bytes are 0x00 through 0x1f. These classes
/// overlap at tab, CR, and LF. Digits are ASCII 0 through 9, and non-ASCII
/// bytes have their high bit set. Quotes and backslashes are not punctuation.
///
/// This operation is infallible for every possible block. It neither filters
/// string content nor validates UTF-8 or JSON grammar, and requires no padding,
/// vector alignment, allocation, or caller-side CPU feature check.
pub fn classify_structural(input: &[u8; BLOCK_BYTES]) -> StructuralMasks {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    if is_supported() {
        // SAFETY: the root detector establishes AVX2 CPU and OS support, or
        // statically enabled AVX2 without std. The reference covers all 64 bytes.
        return unsafe { classify_avx2(input) };
    }

    classify_scalar(input)
}

/// Kept local so the portable path does not depend on another wave's fallback.
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
            b' ' | b'\t' | b'\r' | b'\n' => masks.whitespace_mask |= bit,
            _ => {}
        }
        // Control must be independent of the whitespace match above.
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

/// The caller must establish AVX2 CPU and OS support.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn classify_avx2(input: &[u8; BLOCK_BYTES]) -> StructuralMasks {
    // SAFETY: the array is readable for exactly BLOCK_BYTES bytes. Each loadu
    // reads 32 bytes, at offsets zero and 32, without alignment requirements.
    // Both lane helpers inherit this function's AVX2 precondition.
    let (low, high) = unsafe {
        let low = arch::_mm256_loadu_si256(input.as_ptr().cast::<arch::__m256i>());
        let high = arch::_mm256_loadu_si256(
            input.as_ptr().add(LANE_BYTES).cast::<arch::__m256i>(),
        );
        (classify_lane(low), classify_lane(high))
    };

    // Each lane mask is already zero-extended from u32. Only the second lane
    // can contribute bits 32..63, including when the first lane's bit 31 is set.
    StructuralMasks {
        colon_mask: low.colon_mask | (high.colon_mask << LANE_BYTES),
        comma_mask: low.comma_mask | (high.comma_mask << LANE_BYTES),
        lbrace_mask: low.lbrace_mask | (high.lbrace_mask << LANE_BYTES),
        rbrace_mask: low.rbrace_mask | (high.rbrace_mask << LANE_BYTES),
        lbracket_mask: low.lbracket_mask | (high.lbracket_mask << LANE_BYTES),
        rbracket_mask: low.rbracket_mask | (high.rbracket_mask << LANE_BYTES),
        whitespace_mask: low.whitespace_mask | (high.whitespace_mask << LANE_BYTES),
        control_mask: low.control_mask | (high.control_mask << LANE_BYTES),
        digit_mask: low.digit_mask | (high.digit_mask << LANE_BYTES),
        non_ascii_mask: low.non_ascii_mask | (high.non_ascii_mask << LANE_BYTES),
    }
}

/// Return zero-extended masks for one vector. Requires AVX2 CPU and OS support.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn classify_lane(bytes: arch::__m256i) -> StructuralMasks {
    // SAFETY: all intrinsics require only AVX2, established by the caller.
    // This helper operates solely on values and performs no memory access.
    unsafe {
        let space = arch::_mm256_cmpeq_epi8(bytes, arch::_mm256_set1_epi8(b' ' as i8));
        let tab = arch::_mm256_cmpeq_epi8(bytes, arch::_mm256_set1_epi8(b'\t' as i8));
        let cr = arch::_mm256_cmpeq_epi8(bytes, arch::_mm256_set1_epi8(b'\r' as i8));
        let lf = arch::_mm256_cmpeq_epi8(bytes, arch::_mm256_set1_epi8(b'\n' as i8));
        let whitespace = arch::_mm256_or_si256(
            arch::_mm256_or_si256(space, tab),
            arch::_mm256_or_si256(cr, lf),
        );

        // Unsigned min(x, 31) == x precisely when x < 0x20. A signed
        // comparison would incorrectly include bytes 0x80 through 0xff.
        let control = arch::_mm256_cmpeq_epi8(
            arch::_mm256_min_epu8(bytes, arch::_mm256_set1_epi8(0x1f)),
            bytes,
        );
        // Subtraction wraps per byte. Values below '0' wrap above 9; values
        // above '9' remain above 9. Unsigned comparison excludes both ranges.
        let digit_offset = arch::_mm256_sub_epi8(bytes, arch::_mm256_set1_epi8(b'0' as i8));
        let digit = arch::_mm256_cmpeq_epi8(
            arch::_mm256_min_epu8(digit_offset, arch::_mm256_set1_epi8(9)),
            digit_offset,
        );

        // Movemask returns i32: every conversion must pass through u32 to
        // prevent a matching byte 31 from sign-extending into the high lane.
        StructuralMasks {
            colon_mask: arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(
                bytes, arch::_mm256_set1_epi8(b':' as i8),
            )) as u32 as u64,
            comma_mask: arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(
                bytes, arch::_mm256_set1_epi8(b',' as i8),
            )) as u32 as u64,
            lbrace_mask: arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(
                bytes, arch::_mm256_set1_epi8(b'{' as i8),
            )) as u32 as u64,
            rbrace_mask: arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(
                bytes, arch::_mm256_set1_epi8(b'}' as i8),
            )) as u32 as u64,
            lbracket_mask: arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(
                bytes, arch::_mm256_set1_epi8(b'[' as i8),
            )) as u32 as u64,
            rbracket_mask: arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(
                bytes, arch::_mm256_set1_epi8(b']' as i8),
            )) as u32 as u64,
            whitespace_mask: arch::_mm256_movemask_epi8(whitespace) as u32 as u64,
            control_mask: arch::_mm256_movemask_epi8(control) as u32 as u64,
            digit_mask: arch::_mm256_movemask_epi8(digit) as u32 as u64,
            // The input sign bits are exactly the unsigned >= 0x80 predicate.
            non_ascii_mask: arch::_mm256_movemask_epi8(bytes) as u32 as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matching(input: &[u8; BLOCK_BYTES], predicate: impl Fn(u8) -> bool) -> u64 {
        input.iter().enumerate().fold(0, |mask, (index, &byte)| {
            mask | (u64::from(predicate(byte)) << index)
        })
    }

    /// Independent predicate oracle, without production classification helpers.
    fn reference(input: &[u8; BLOCK_BYTES]) -> StructuralMasks {
        StructuralMasks {
            colon_mask: matching(input, |b| b == b':'),
            comma_mask: matching(input, |b| b == b','),
            lbrace_mask: matching(input, |b| b == b'{'),
            rbrace_mask: matching(input, |b| b == b'}'),
            lbracket_mask: matching(input, |b| b == b'['),
            rbracket_mask: matching(input, |b| b == b']'),
            whitespace_mask: matching(input, |b| [0x20, 0x09, 0x0d, 0x0a].contains(&b)),
            control_mask: matching(input, |b| (0..=31).contains(&b)),
            digit_mask: matching(input, |b| b"0123456789".contains(&b)),
            non_ascii_mask: matching(input, |b| b & 0x80 != 0),
        }
    }

    fn fields(masks: StructuralMasks) -> [u64; 10] {
        [
            masks.colon_mask, masks.comma_mask, masks.lbrace_mask,
            masks.rbrace_mask, masks.lbracket_mask, masks.rbracket_mask,
            masks.whitespace_mask, masks.control_mask, masks.digit_mask,
            masks.non_ascii_mask,
        ]
    }

    fn block_classes(masks: BlockMasks) -> StructuralMasks {
        StructuralMasks {
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
        }
    }

    fn assert_paths(input: &[u8; BLOCK_BYTES]) {
        let expected = reference(input);
        assert_eq!(classify_scalar(input), expected, "scalar: {input:?}");
        assert_eq!(classify_structural(input), expected, "dispatch: {input:?}");
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        if is_supported() {
            // SAFETY: feature support is checked here, independently of the
            // public dispatcher; the input is a complete, readable array.
            assert_eq!(unsafe { classify_avx2(input) }, expected, "AVX2: {input:?}");
        }
    }

    fn assert_scanner_error(error: Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Scanner);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn exact_signature_and_known_masks() {
        let entry: fn(&[u8; BLOCK_BYTES]) -> StructuralMasks = classify_structural;
        let sample = b":,{}[] \t\r\n\x00\x1f09/\x7f\x80\xff";
        let mut block = [b'x'; BLOCK_BYTES];
        block[..sample.len()].copy_from_slice(sample);
        let expected = StructuralMasks {
            colon_mask: 1,
            comma_mask: 2,
            lbrace_mask: 4,
            rbrace_mask: 8,
            lbracket_mask: 16,
            rbracket_mask: 32,
            whitespace_mask: 0x03c0,
            control_mask: 0x0f80,
            digit_mask: 0x3000,
            non_ascii_mask: 0x0003_0000,
        };
        assert_eq!(entry(&block), expected);
        assert_eq!(expected.punctuation_mask(), 0x3f);
        assert_paths(&block);
    }

    #[test]
    fn every_uniform_byte_value_matches_all_paths() {
        for byte in 0..=u8::MAX {
            let block = [byte; BLOCK_BYTES];
            assert_paths(&block);
            for mask in fields(classify_structural(&block)) {
                assert!(mask == 0 || mask == u64::MAX);
            }
        }
        assert_eq!(classify_structural(&[b'x'; BLOCK_BYTES]), StructuralMasks::default());
        assert_eq!(classify_structural(&[0; BLOCK_BYTES]), StructuralMasks {
            control_mask: u64::MAX,
            ..StructuralMasks::default()
        });
        assert_eq!(classify_structural(&[0xff; BLOCK_BYTES]), StructuralMasks {
            non_ascii_mask: u64::MAX,
            ..StructuralMasks::default()
        });
    }

    #[test]
    fn every_byte_at_every_position_is_isolated() {
        for byte in 0..=u8::MAX {
            for position in 0..BLOCK_BYTES {
                let mut block = [b'x'; BLOCK_BYTES];
                block[position] = byte;
                assert_paths(&block);
                for mask in fields(classify_structural(&block)) {
                    assert!(mask == 0 || mask == 1u64 << position);
                }
            }
        }
    }

    #[test]
    fn lane_sign_bits_and_endpoints_do_not_contaminate_other_positions() {
        for &byte in b":,{}[] \t\r\n\x00\x1f09\x80\xff" {
            for positions in [
                [0usize, 31], [31, 32], [32, 63], [0, 63], [31, 63],
            ] {
                let mut block = [b'x'; BLOCK_BYTES];
                for position in positions {
                    block[position] = byte;
                }
                let expected_bits = (1u64 << positions[0]) | (1u64 << positions[1]);
                assert_paths(&block);
                for mask in fields(classify_structural(&block)) {
                    assert!(mask == 0 || mask == expected_bits);
                }
            }
            for (start, end, expected_bits) in [
                (0, 32, 0x0000_0000_ffff_ffffu64),
                (32, 64, 0xffff_ffff_0000_0000),
            ] {
                let mut block = [b'x'; BLOCK_BYTES];
                block[start..end].fill(byte);
                assert_paths(&block);
                for mask in fields(classify_structural(&block)) {
                    assert!(mask == 0 || mask == expected_bits);
                }
            }
        }
    }

    #[test]
    fn unsigned_range_boundaries_and_whitespace_overlap_are_exact() {
        let sample = b"\x00\x08\t\n\x0b\x0c\r\x1f !/09:~\x7f\x80\x85\xa0\xaf\xb0\xb9\xba\xff";
        let mut block = [b'x'; BLOCK_BYTES];
        block[..sample.len()].copy_from_slice(sample);
        assert_paths(&block);
        let masks = classify_structural(&block);
        assert_eq!(masks.control_mask, 0xff);
        assert_eq!(masks.whitespace_mask, (1 << 2) | (1 << 3) | (1 << 6) | (1 << 8));
        assert_eq!(masks.control_mask & masks.whitespace_mask, (1 << 2) | (1 << 3) | (1 << 6));
        assert_eq!(masks.digit_mask, (1 << 11) | (1 << 12));
        assert_eq!(masks.non_ascii_mask, 0x00ff_0000);
        assert_eq!(masks.non_ascii_mask & masks.control_mask, 0);
        assert_eq!(masks.non_ascii_mask & masks.digit_mask, 0);
        assert_eq!(masks.non_ascii_mask & masks.whitespace_mask, 0);
    }

    #[test]
    fn rotated_byte_ramps_and_deterministic_mixed_blocks_match() {
        for base in 0..=u8::MAX {
            let mut block = [0; BLOCK_BYTES];
            for (index, byte) in block.iter_mut().enumerate() {
                *byte = base.wrapping_add(index as u8);
            }
            assert_paths(&block);
        }
        let alphabet = b":,{}[] \t\r\n019/\"\\az\x00\x1f\x7f\x80\xff";
        let mut state = 0x8d12_4f6a_39c7_b5e1u64;
        for case in 0..512 {
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
            assert_paths(&block);
        }
    }

    #[test]
    fn all_vector_alignments_use_only_the_borrowed_block() {
        #[repr(C, align(32))]
        struct Aligned([u8; BLOCK_BYTES + LANE_BYTES]);

        for offset in 0..LANE_BYTES {
            let mut storage = Aligned([b':'; BLOCK_BYTES + LANE_BYTES]);
            storage.0[offset..offset + BLOCK_BYTES].fill(b'x');
            storage.0[offset] = b',';
            storage.0[offset + 31] = b'0';
            storage.0[offset + 32] = 0x1f;
            storage.0[offset + 63] = 0xff;
            let before = storage.0;
            let input: &[u8; BLOCK_BYTES] =
                (&storage.0[offset..offset + BLOCK_BYTES]).try_into().unwrap();
            assert_eq!(input.as_ptr() as usize % LANE_BYTES, offset);
            assert_paths(input);
            assert_eq!(classify_structural(input), StructuralMasks {
                comma_mask: 1,
                digit_mask: 1u64 << 31,
                control_mask: 1u64 << 32,
                non_ascii_mask: 1u64 << 63,
                ..StructuralMasks::default()
            });
            assert_eq!(storage.0, before);
        }
    }

    #[test]
    fn quotes_backslashes_and_unicode_classes_are_not_structural_classes() {
        for byte in [b'"', b'\\', b'/', b'+', b'-', b'.', b'\x7f'] {
            assert_eq!(classify_structural(&[byte; BLOCK_BYTES]), StructuralMasks::default());
        }
        // NBSP, em space, Arabic-Indic zero, and fullwidth nine in UTF-8.
        let sample = b"\xc2\xa0\xe2\x80\x83\xd9\xa0\xef\xbc\x99";
        let mut block = [b'x'; BLOCK_BYTES];
        block[..sample.len()].copy_from_slice(sample);
        assert_paths(&block);
        assert_eq!(classify_structural(&block), StructuralMasks {
            non_ascii_mask: (1u64 << sample.len()) - 1,
            ..StructuralMasks::default()
        });
    }

    #[test]
    fn malformed_json_and_utf8_remain_infallible_raw_classification() {
        let samples: [&[u8]; 4] = [
            b"}{][,,::000+NaN",
            b"\"unfinished\\",
            b"\"\x00\x1f\xff\\q\"",
            b"\xc0\xaf\xed\xa0\x80\xf5\xff",
        ];
        for sample in samples {
            let mut block = [b'x'; BLOCK_BYTES];
            block[..sample.len()].copy_from_slice(sample);
            assert_paths(&block);
        }
    }

    #[test]
    fn raw_classes_inside_strings_survive_pipeline_filtering() {
        let sample = b"\":,{}[] 09\t\xff\"";
        let mut block = [b'x'; BLOCK_BYTES];
        block[..sample.len()].copy_from_slice(sample);
        let classes = classify_structural(&block);
        assert_eq!(classes.punctuation_mask(), 0x7e);
        let mut carry = ScanCarry {
            utf8_tail: [0xff, 0x00, 0x80, 0xf0],
            utf8_tail_len: 0xff,
            ..ScanCarry::default()
        };
        let before = carry;
        let masks = classify::classify_block(&block, &mut carry).unwrap();
        assert_eq!(block_classes(masks), classes);
        assert_eq!(masks.structural_mask, 0);
        assert_eq!(masks.in_string_mask & classes.punctuation_mask(), 0x7e);
        assert_eq!(carry, before);
    }

    #[test]
    fn every_pipeline_length_clips_all_raw_classes_and_zero_padding() {
        let pattern = b":,{}[] \t\r\n09\x00\x1f\x80\xff";
        let mut block = [0; BLOCK_BYTES];
        for (index, byte) in block.iter_mut().enumerate() {
            *byte = pattern[index % pattern.len()];
        }
        for input in [block, [b'x'; BLOCK_BYTES], [0; BLOCK_BYTES]] {
            let full = fields(reference(&input));
            for len in 0..=BLOCK_BYTES {
                let valid = ((1u128 << len) - 1) as u64;
                let mut carry = ScanCarry::default();
                let masks = classify::classify(&input[..len], &mut carry).unwrap();
                assert_eq!(masks.valid_mask, valid);
                assert_eq!(masks.valid_len as usize, len);
                let actual = fields(block_classes(masks));
                for index in 0..full.len() {
                    assert_eq!(actual[index], full[index] & valid, "len {len}, field {index}");
                }
                assert_eq!(carry, ScanCarry::default());
            }
        }
    }

    #[test]
    fn enclosing_scanner_errors_preserve_carry_and_error_metadata() {
        // classify_structural itself has no error path: its array type fixes
        // the length and every byte is valid input. Verify errors at the
        // enclosing fallible API instead of inventing lexical rejections.
        let storage = [b':'; BLOCK_BYTES + 1];
        let initial = ScanCarry {
            prev_in_string: 1,
            prev_escaped: 1,
            utf8_tail: [0xff, 0, 0x80, 0xf0],
            utf8_tail_len: 0xff,
        };
        let mut carry = initial;
        assert_scanner_error(
            classify::classify(&storage, &mut carry).unwrap_err(),
            ErrorCode::InvalidToken,
        );
        assert_eq!(carry, initial);
        for invalid in [2u64, 1u64 << 63, u64::MAX] {
            for before in [
                ScanCarry { prev_in_string: invalid, ..initial },
                ScanCarry { prev_escaped: invalid, ..initial },
            ] {
                for len in [0usize, 1, 31, 32, 33, 63, 64] {
                    let mut carry = before;
                    assert_scanner_error(
                        classify::classify(&storage[..len], &mut carry).unwrap_err(),
                        ErrorCode::InternalError,
                    );
                    assert_eq!(carry, before);
                }
                let mut carry = before;
                assert_scanner_error(
                    classify::classify(&storage, &mut carry).unwrap_err(),
                    ErrorCode::InvalidToken,
                );
                assert_eq!(carry, before);
            }
        }
    }
}
