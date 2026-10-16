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
        return unsafe { classify_avx2(input) }.classes;
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
pub(crate) unsafe fn classify_avx2(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    // SAFETY: the array is readable for exactly BLOCK_BYTES bytes. Each loadu
    // reads 32 bytes, at offsets zero and 32, without alignment requirements.
    // Both lane helpers inherit this function's AVX2 precondition.
    let (low, high) = unsafe {
        let low = arch::_mm256_loadu_si256(input.as_ptr().cast::<arch::__m256i>());
        let high = arch::_mm256_loadu_si256(input.as_ptr().add(LANE_BYTES).cast::<arch::__m256i>());
        (classify_lane(low), classify_lane(high))
    };

    // Each lane mask is already zero-extended from u32. Only the second lane
    // can contribute bits 32..63, including when the first lane's bit 31 is set.
    RawMasks {
        quote_mask: low.quote_mask | (high.quote_mask << LANE_BYTES),
        backslash_mask: low.backslash_mask | (high.backslash_mask << LANE_BYTES),
        backend: BackendId::Avx2,
        classes: StructuralMasks {
            colon_mask: low.classes.colon_mask | (high.classes.colon_mask << LANE_BYTES),
            comma_mask: low.classes.comma_mask | (high.classes.comma_mask << LANE_BYTES),
            lbrace_mask: low.classes.lbrace_mask | (high.classes.lbrace_mask << LANE_BYTES),
            rbrace_mask: low.classes.rbrace_mask | (high.classes.rbrace_mask << LANE_BYTES),
            lbracket_mask: low.classes.lbracket_mask | (high.classes.lbracket_mask << LANE_BYTES),
            rbracket_mask: low.classes.rbracket_mask | (high.classes.rbracket_mask << LANE_BYTES),
            whitespace_mask: low.classes.whitespace_mask
                | (high.classes.whitespace_mask << LANE_BYTES),
            control_mask: low.classes.control_mask | (high.classes.control_mask << LANE_BYTES),
            digit_mask: low.classes.digit_mask | (high.classes.digit_mask << LANE_BYTES),
            non_ascii_mask: low.classes.non_ascii_mask
                | (high.classes.non_ascii_mask << LANE_BYTES),
        },
    }
}

/// Return zero-extended masks for one vector. Requires AVX2 CPU and OS support.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn classify_lane(bytes: arch::__m256i) -> RawMasks {
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
        RawMasks {
            quote_mask: arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(
                bytes,
                arch::_mm256_set1_epi8(b'"' as i8),
            )) as u32 as u64,
            backslash_mask: arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(
                bytes,
                arch::_mm256_set1_epi8(b'\\' as i8),
            )) as u32 as u64,
            backend: BackendId::Avx2,
            classes: StructuralMasks {
                colon_mask: arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(
                    bytes,
                    arch::_mm256_set1_epi8(b':' as i8),
                )) as u32 as u64,
                comma_mask: arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(
                    bytes,
                    arch::_mm256_set1_epi8(b',' as i8),
                )) as u32 as u64,
                lbrace_mask: arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(
                    bytes,
                    arch::_mm256_set1_epi8(b'{' as i8),
                )) as u32 as u64,
                rbrace_mask: arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(
                    bytes,
                    arch::_mm256_set1_epi8(b'}' as i8),
                )) as u32 as u64,
                lbracket_mask: arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(
                    bytes,
                    arch::_mm256_set1_epi8(b'[' as i8),
                )) as u32 as u64,
                rbracket_mask: arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(
                    bytes,
                    arch::_mm256_set1_epi8(b']' as i8),
                )) as u32 as u64,
                whitespace_mask: arch::_mm256_movemask_epi8(whitespace) as u32 as u64,
                control_mask: arch::_mm256_movemask_epi8(control) as u32 as u64,
                digit_mask: arch::_mm256_movemask_epi8(digit) as u32 as u64,
                // The input sign bits are exactly the unsigned >= 0x80 predicate.
                non_ascii_mask: arch::_mm256_movemask_epi8(bytes) as u32 as u64,
            },
        }
    }
}
