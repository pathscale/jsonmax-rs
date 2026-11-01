//! Structural callers share the fused four-load raw classifier.
use super::*;

pub fn classify_structural(input: &[u8; BLOCK_BYTES]) -> StructuralMasks {
    classify::classify_raw(input).classes
}

/// Pack comparison bytes in address order, with one reduction tree for both
/// eight-byte halves. Weights are hoisted out of all per-class reductions.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
unsafe fn comparison_mask(
    value: core::arch::aarch64::uint8x16_t,
    weights: core::arch::aarch64::uint8x16_t,
) -> u16 {
    use core::arch::aarch64::*;
    let pairs = vpaddlq_u8(vandq_u8(value, weights));
    let quads = vpaddlq_u16(pairs);
    let halves = vpaddlq_u32(quads);
    (vgetq_lane_u64(halves, 0) | (vgetq_lane_u64(halves, 1) << 8)) as u16
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
pub(crate) unsafe fn classify_neon(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    use core::arch::aarch64::*;

    let mut masks = StructuralMasks::default();
    let mut quote_mask = 0u64;
    let mut backslash_mask = 0u64;
    // SAFETY: the only production caller checks NEON support. Offsets
    // 0, 16, 32, and 48 each leave 16 initialized bytes in input. vld1q_u8
    // accepts unaligned addresses. The private reduction helper inherits
    // this feature guarantee and receives only 0x00/0xff comparison bytes.
    unsafe {
        let weights = vcombine_u8(
            vcreate_u8(u64::from_ne_bytes([1, 2, 4, 8, 16, 32, 64, 128])),
            vcreate_u8(u64::from_ne_bytes([1, 2, 4, 8, 16, 32, 64, 128])),
        );
        let quote = vdupq_n_u8(b'"');
        let backslash = vdupq_n_u8(b'\\');
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
            quote_mask |= u64::from(comparison_mask(vceqq_u8(bytes, quote), weights)) << offset;
            backslash_mask |=
                u64::from(comparison_mask(vceqq_u8(bytes, backslash), weights)) << offset;
            let whitespace = vorrq_u8(
                vorrq_u8(vceqq_u8(bytes, space), vceqq_u8(bytes, tab)),
                vorrq_u8(vceqq_u8(bytes, cr), vceqq_u8(bytes, lf)),
            );
            let digits = vandq_u8(vcgeq_u8(bytes, digit_low), vcleq_u8(bytes, digit_high));

            // Each mask is widened before shifting, including lane 3.
            masks.colon_mask |=
                u64::from(comparison_mask(vceqq_u8(bytes, colon), weights)) << offset;
            masks.comma_mask |=
                u64::from(comparison_mask(vceqq_u8(bytes, comma), weights)) << offset;
            masks.lbrace_mask |=
                u64::from(comparison_mask(vceqq_u8(bytes, lbrace), weights)) << offset;
            masks.rbrace_mask |=
                u64::from(comparison_mask(vceqq_u8(bytes, rbrace), weights)) << offset;
            masks.lbracket_mask |=
                u64::from(comparison_mask(vceqq_u8(bytes, lbracket), weights)) << offset;
            masks.rbracket_mask |=
                u64::from(comparison_mask(vceqq_u8(bytes, rbracket), weights)) << offset;
            masks.whitespace_mask |= u64::from(comparison_mask(whitespace, weights)) << offset;
            masks.control_mask |=
                u64::from(comparison_mask(vcltq_u8(bytes, space), weights)) << offset;
            masks.digit_mask |= u64::from(comparison_mask(digits, weights)) << offset;
            masks.non_ascii_mask |=
                u64::from(comparison_mask(vcgeq_u8(bytes, non_ascii_low), weights)) << offset;
        }
    }
    RawMasks {
        quote_mask,
        backslash_mask,
        classes: masks,
        backend: BackendId::Neon,
    }
}
