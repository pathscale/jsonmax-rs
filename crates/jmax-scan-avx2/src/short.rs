//! Bounded short scans with x86 SSE2 and AVX2 implementations.

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use crate::is_supported;

#[cfg(target_arch = "x86")]
use core::arch::x86 as arch;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64 as arch;

/// Return the first quote, backslash, or JSON control byte in `input`.
///
/// Returns `input.len()` when no special byte occurs. A quote or backslash is
/// a string decoding boundary, and controls are bytes below `0x20`. The native
/// paths load only complete 16-byte or 32-byte vectors; the final tail is
/// scanned bytewise without reading beyond the slice.
#[inline]
pub fn string_special(input: &[u8]) -> usize {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if is_avx2_supported() {
            // SAFETY: runtime support is checked and AVX2 loads occur only
            // inside 32-byte chunks yielded from the slice.
            return unsafe { string_special_avx2(input) };
        }
        if is_sse2_supported() {
            // SAFETY: SSE2 is checked (or is baseline on x86_64), and loads
            // occur only inside complete 16-byte chunks.
            return unsafe { string_special_sse2(input) };
        }
    }
    scalar_string_special(input)
}

/// Return the first string boundary and whether its preceding bytes are ASCII.
/// The ASCII result covers bytes strictly before the boundary, or all bytes
/// when no boundary occurs. This is useful for key scans that need both facts.
#[inline]
pub fn string_special_ascii(input: &[u8]) -> (usize, bool) {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if is_avx2_supported() {
            // SAFETY: runtime support is checked and loads use complete chunks.
            return unsafe { string_special_ascii_avx2(input) };
        }
        if is_sse2_supported() {
            // SAFETY: SSE2 is supported and loads use complete chunks.
            return unsafe { string_special_ascii_sse2(input) };
        }
    }
    scalar_string_special_ascii(input)
}

fn scalar_string_special_ascii(input: &[u8]) -> (usize, bool) {
    let mut ascii = true;
    for (index, &byte) in input.iter().enumerate() {
        if byte == b'"' || byte == b'\\' || byte < 0x20 { return (index, ascii); }
        ascii &= byte.is_ascii();
    }
    (input.len(), ascii)
}

/// Return the length of the leading JSON whitespace prefix in `input`.
///
/// JSON whitespace is exactly space, tab, carriage return, and line feed.
/// Native loads are limited to complete vectors within the slice; the tail is
/// checked bytewise.
#[inline]
pub fn whitespace_prefix(input: &[u8]) -> usize {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if is_avx2_supported() {
            // SAFETY: runtime support is checked and AVX2 loads occur only
            // inside 32-byte chunks yielded from the slice.
            return unsafe { whitespace_prefix_avx2(input) };
        }
        if is_sse2_supported() {
            // SAFETY: SSE2 is checked (or is baseline on x86_64), and loads
            // occur only inside complete 16-byte chunks.
            return unsafe { whitespace_prefix_sse2(input) };
        }
    }
    scalar_whitespace_prefix(input)
}

fn scalar_string_special(input: &[u8]) -> usize {
    input.iter().position(|&b| b == b'"' || b == b'\\' || b < 0x20).unwrap_or(input.len())
}

fn scalar_whitespace_prefix(input: &[u8]) -> usize {
    input.iter().position(|&b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n')).unwrap_or(input.len())
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn is_sse2_supported() -> bool {
    #[cfg(target_arch = "x86_64")]
    { true }
    #[cfg(all(target_arch = "x86", feature = "std"))]
    { std::is_x86_feature_detected!("sse2") }
    #[cfg(all(target_arch = "x86", not(feature = "std")))]
    { cfg!(target_feature = "sse2") }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn is_avx2_supported() -> bool {
    is_supported()
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "sse2")]
unsafe fn string_special_sse2(input: &[u8]) -> usize {
    let quote = unsafe { arch::_mm_set1_epi8(b'"' as i8) };
    let slash = unsafe { arch::_mm_set1_epi8(b'\\' as i8) };
    let limit = unsafe { arch::_mm_set1_epi8(0x1f) };
    let mut chunks = input.chunks_exact(16);
    let mut offset = 0;
    for chunk in &mut chunks {
        // SAFETY: each chunk has 16 readable bytes; loadu has no alignment need.
        let bytes = unsafe { arch::_mm_loadu_si128(chunk.as_ptr().cast()) };
        let quoted = unsafe { arch::_mm_cmpeq_epi8(bytes, quote) };
        let escaped = unsafe { arch::_mm_cmpeq_epi8(bytes, slash) };
        let controls = unsafe {
            arch::_mm_cmpeq_epi8(arch::_mm_min_epu8(bytes, limit), bytes)
        };
        let special = unsafe { arch::_mm_or_si128(arch::_mm_or_si128(quoted, escaped), controls) };
        let mask = unsafe { arch::_mm_movemask_epi8(special) as u32 };
        if mask != 0 {
            return offset + mask.trailing_zeros() as usize;
        }
        offset += 16;
    }
    offset + scalar_string_special(chunks.remainder())
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "sse2")]
unsafe fn whitespace_prefix_sse2(input: &[u8]) -> usize {
    let space = unsafe { arch::_mm_set1_epi8(b' ' as i8) };
    let tab = unsafe { arch::_mm_set1_epi8(b'\t' as i8) };
    let cr = unsafe { arch::_mm_set1_epi8(b'\r' as i8) };
    let lf = unsafe { arch::_mm_set1_epi8(b'\n' as i8) };
    let mut chunks = input.chunks_exact(16);
    let mut offset = 0;
    for chunk in &mut chunks {
        // SAFETY: each chunk has 16 readable bytes; loadu has no alignment need.
        let bytes = unsafe { arch::_mm_loadu_si128(chunk.as_ptr().cast()) };
        let whitespace = unsafe {
            arch::_mm_or_si128(
                arch::_mm_or_si128(arch::_mm_cmpeq_epi8(bytes, space), arch::_mm_cmpeq_epi8(bytes, tab)),
                arch::_mm_or_si128(arch::_mm_cmpeq_epi8(bytes, cr), arch::_mm_cmpeq_epi8(bytes, lf)),
            )
        };
        let mask = !(unsafe { arch::_mm_movemask_epi8(whitespace) as u32 }) & 0xffff;
        if mask != 0 {
            return offset + mask.trailing_zeros() as usize;
        }
        offset += 16;
    }
    offset + scalar_whitespace_prefix(chunks.remainder())
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "sse2")]
unsafe fn string_special_ascii_sse2(input: &[u8]) -> (usize, bool) {
    let quote = unsafe { arch::_mm_set1_epi8(b'"' as i8) };
    let slash = unsafe { arch::_mm_set1_epi8(b'\\' as i8) };
    let limit = unsafe { arch::_mm_set1_epi8(0x1f) };
    let mut chunks = input.chunks_exact(16);
    let mut offset = 0;
    let mut ascii = true;
    for chunk in &mut chunks {
        let bytes = unsafe { arch::_mm_loadu_si128(chunk.as_ptr().cast()) };
        let special = unsafe { arch::_mm_or_si128(
            arch::_mm_or_si128(arch::_mm_cmpeq_epi8(bytes, quote), arch::_mm_cmpeq_epi8(bytes, slash)),
            arch::_mm_cmpeq_epi8(arch::_mm_min_epu8(bytes, limit), bytes),
        ) };
        let mask = unsafe { arch::_mm_movemask_epi8(special) as u32 };
        let high = unsafe { arch::_mm_movemask_epi8(bytes) as u32 };
        if mask != 0 {
            let first = mask.trailing_zeros() as usize;
            let prefix = (1u32 << first).wrapping_sub(1);
            return (offset + first, ascii && (high & prefix) == 0);
        }
        ascii &= high == 0;
        offset += 16;
    }
    let (tail, tail_ascii) = scalar_string_special_ascii(chunks.remainder());
    (offset + tail, ascii && tail_ascii)
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn string_special_avx2(input: &[u8]) -> usize {
    let quote = unsafe { arch::_mm256_set1_epi8(b'"' as i8) };
    let slash = unsafe { arch::_mm256_set1_epi8(b'\\' as i8) };
    let limit = unsafe { arch::_mm256_set1_epi8(0x1f) };
    let mut chunks = input.chunks_exact(32);
    let mut offset = 0;
    for chunk in &mut chunks {
        // SAFETY: each chunk has 32 readable bytes; loadu has no alignment need.
        let bytes = unsafe { arch::_mm256_loadu_si256(chunk.as_ptr().cast()) };
        let quoted = unsafe { arch::_mm256_cmpeq_epi8(bytes, quote) };
        let escaped = unsafe { arch::_mm256_cmpeq_epi8(bytes, slash) };
        let controls = unsafe {
            arch::_mm256_cmpeq_epi8(arch::_mm256_min_epu8(bytes, limit), bytes)
        };
        let special = unsafe { arch::_mm256_or_si256(arch::_mm256_or_si256(quoted, escaped), controls) };
        let mask = unsafe { arch::_mm256_movemask_epi8(special) as u32 };
        if mask != 0 {
            return offset + mask.trailing_zeros() as usize;
        }
        offset += 32;
    }
    offset + scalar_string_special(chunks.remainder())
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn whitespace_prefix_avx2(input: &[u8]) -> usize {
    let space = unsafe { arch::_mm256_set1_epi8(b' ' as i8) };
    let tab = unsafe { arch::_mm256_set1_epi8(b'\t' as i8) };
    let cr = unsafe { arch::_mm256_set1_epi8(b'\r' as i8) };
    let lf = unsafe { arch::_mm256_set1_epi8(b'\n' as i8) };
    let mut chunks = input.chunks_exact(32);
    let mut offset = 0;
    for chunk in &mut chunks {
        // SAFETY: each chunk has 32 readable bytes; loadu has no alignment need.
        let bytes = unsafe { arch::_mm256_loadu_si256(chunk.as_ptr().cast()) };
        let whitespace = unsafe {
            arch::_mm256_or_si256(
                arch::_mm256_or_si256(arch::_mm256_cmpeq_epi8(bytes, space), arch::_mm256_cmpeq_epi8(bytes, tab)),
                arch::_mm256_or_si256(arch::_mm256_cmpeq_epi8(bytes, cr), arch::_mm256_cmpeq_epi8(bytes, lf)),
            )
        };
        let mask = !(unsafe { arch::_mm256_movemask_epi8(whitespace) as u32 });
        if mask != 0 {
            return offset + mask.trailing_zeros() as usize;
        }
        offset += 32;
    }
    offset + scalar_whitespace_prefix(chunks.remainder())
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn string_special_ascii_avx2(input: &[u8]) -> (usize, bool) {
    let quote = unsafe { arch::_mm256_set1_epi8(b'"' as i8) };
    let slash = unsafe { arch::_mm256_set1_epi8(b'\\' as i8) };
    let limit = unsafe { arch::_mm256_set1_epi8(0x1f) };
    let mut chunks = input.chunks_exact(32);
    let mut offset = 0;
    let mut ascii = true;
    for chunk in &mut chunks {
        let bytes = unsafe { arch::_mm256_loadu_si256(chunk.as_ptr().cast()) };
        let special = unsafe { arch::_mm256_or_si256(
            arch::_mm256_or_si256(arch::_mm256_cmpeq_epi8(bytes, quote), arch::_mm256_cmpeq_epi8(bytes, slash)),
            arch::_mm256_cmpeq_epi8(arch::_mm256_min_epu8(bytes, limit), bytes),
        ) };
        let mask = unsafe { arch::_mm256_movemask_epi8(special) as u32 };
        let high = unsafe { arch::_mm256_movemask_epi8(bytes) as u32 };
        if mask != 0 {
            let first = mask.trailing_zeros() as usize;
            let prefix = (1u32 << first).wrapping_sub(1);
            return (offset + first, ascii && (high & prefix) == 0);
        }
        ascii &= high == 0;
        offset += 32;
    }
    let (tail, tail_ascii) = scalar_string_special_ascii(chunks.remainder());
    (offset + tail, ascii && tail_ascii)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_boundaries_include_each_vector_edge() {
        let text = [b'a'; 65];
        for len in [0, 1, 15, 16, 17, 31, 32, 33, 63, 64, 65] {
            assert_eq!(string_special(&text[..len]), len);
            if len != 0 {
                let mut with_quote = text;
                with_quote[len - 1] = b'"';
                assert_eq!(string_special(&with_quote[..len]), len - 1);
            }
        }
        assert_eq!(string_special(b"abcdefghijklmnop\\tail"), 16);
        assert_eq!(string_special(b"abcdefghijklmnop\0tail"), 16);
    }

    #[test]
    fn whitespace_edges_and_json_alphabet() {
        let text = [b' '; 33];
        for len in [0, 1, 15, 16, 17, 31, 32, 33] {
            assert_eq!(whitespace_prefix(&text[..len]), len);
        }
        assert_eq!(whitespace_prefix(b" \t\r\nabcdefghijkl"), 4);
        assert_eq!(whitespace_prefix(b"               X"), 15);
        assert_eq!(whitespace_prefix(b"\x0b"), 0);
        assert_eq!(whitespace_prefix(b"\x0c"), 0);
    }
}
