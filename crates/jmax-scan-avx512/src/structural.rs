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
