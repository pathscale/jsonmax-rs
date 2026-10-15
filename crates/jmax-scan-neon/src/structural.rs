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
