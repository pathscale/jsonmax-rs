//! Bounded short scans with a 16-byte NEON fast path.

#[cfg(target_arch = "aarch64")]
use crate::is_supported;

/// Return the first quote, backslash, or JSON control byte in `input`.
///
/// Returns `input.len()` when no such byte occurs. A quote and backslash are
/// boundaries for string decoding; controls are bytes below `0x20`. Full
/// vectors are loaded only when 16 bytes remain, and the final tail is scalar.
#[inline]
pub fn string_special(input: &[u8]) -> usize {
    #[cfg(target_arch = "aarch64")]
    if is_supported() {
        // SAFETY: support is checked and each load is guarded by a 16-byte
        // chunks_exact iterator, so it remains within the supplied slice.
        return unsafe { string_special_neon(input) };
    }
    scalar_string_special(input)
}

/// Return the first string boundary and whether its preceding bytes are ASCII.
/// The ASCII result covers bytes strictly before the boundary, or all bytes
/// when no boundary occurs.
#[inline]
pub fn string_special_ascii(input: &[u8]) -> (usize, bool) {
    #[cfg(target_arch = "aarch64")]
    if is_supported() {
        // SAFETY: support is checked and the helper reads only complete vectors.
        return unsafe { string_special_ascii_neon(input) };
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
/// Full vectors are loaded only when 16 bytes remain, and the final tail is
/// checked bytewise. No alignment or readable suffix is required.
#[inline]
pub fn whitespace_prefix(input: &[u8]) -> usize {
    #[cfg(target_arch = "aarch64")]
    if is_supported() {
        // SAFETY: support is checked and each load is guarded by a 16-byte
        // chunks_exact iterator, so it remains within the supplied slice.
        return unsafe { whitespace_prefix_neon(input) };
    }
    scalar_whitespace_prefix(input)
}

fn scalar_string_special(input: &[u8]) -> usize {
    input.iter().position(|&b| b == b'"' || b == b'\\' || b < 0x20).unwrap_or(input.len())
}

fn scalar_whitespace_prefix(input: &[u8]) -> usize {
    input.iter().position(|&b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n')).unwrap_or(input.len())
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
unsafe fn string_special_neon(input: &[u8]) -> usize {
    use core::arch::aarch64::*;
    unsafe {
    let quote = vdupq_n_u8(b'"');
    let slash = vdupq_n_u8(b'\\');
    let below_space = vdupq_n_u8(b' ');
    let indexes = vld1q_u8([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15].as_ptr());
    let absent = vdupq_n_u8(16);
    let mut chunks = input.chunks_exact(16);
    let mut offset = 0;
    for chunk in &mut chunks {
        // SAFETY: chunks_exact yields exactly 16 readable bytes.
        let bytes = vld1q_u8(chunk.as_ptr());
        let special = vorrq_u8(
            vorrq_u8(vceqq_u8(bytes, quote), vceqq_u8(bytes, slash)),
            vcltq_u8(bytes, below_space),
        );
        let first = vminvq_u8(vbslq_u8(special, indexes, absent)) as usize;
        if first != 16 { return offset + first; }
        offset += 16;
    }
    offset + scalar_string_special(chunks.remainder())
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
unsafe fn string_special_ascii_neon(input: &[u8]) -> (usize, bool) {
    use core::arch::aarch64::*;
    unsafe {
        let quote = vdupq_n_u8(b'"');
        let slash = vdupq_n_u8(b'\\');
        let below_space = vdupq_n_u8(b' ');
        let indexes = vld1q_u8([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15].as_ptr());
        let absent = vdupq_n_u8(16);
        let mut chunks = input.chunks_exact(16);
        let mut offset = 0;
        let mut ascii = true;
        for chunk in &mut chunks {
            let bytes = vld1q_u8(chunk.as_ptr());
            let special = vorrq_u8(
                vorrq_u8(vceqq_u8(bytes, quote), vceqq_u8(bytes, slash)),
                vcltq_u8(bytes, below_space),
            );
            let first = vminvq_u8(vbslq_u8(special, indexes, absent)) as usize;
            if first != 16 {
                let prefix = vcltq_u8(indexes, vdupq_n_u8(first as u8));
                ascii &= vmaxvq_u8(vandq_u8(bytes, prefix)) < 0x80;
                return (offset + first, ascii);
            }
            ascii &= vmaxvq_u8(bytes) < 0x80;
            offset += 16;
        }
        let (tail, tail_ascii) = scalar_string_special_ascii(chunks.remainder());
        (offset + tail, ascii && tail_ascii)
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
unsafe fn whitespace_prefix_neon(input: &[u8]) -> usize {
    use core::arch::aarch64::*;
    unsafe {
    let space = vdupq_n_u8(b' ');
    let tab = vdupq_n_u8(b'\t');
    let cr = vdupq_n_u8(b'\r');
    let lf = vdupq_n_u8(b'\n');
    let indexes = vld1q_u8([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15].as_ptr());
    let absent = vdupq_n_u8(16);
    let mut chunks = input.chunks_exact(16);
    let mut offset = 0;
    for chunk in &mut chunks {
        // SAFETY: chunks_exact yields exactly 16 readable bytes.
        let bytes = vld1q_u8(chunk.as_ptr());
        let whitespace = vorrq_u8(
            vorrq_u8(vceqq_u8(bytes, space), vceqq_u8(bytes, tab)),
            vorrq_u8(vceqq_u8(bytes, cr), vceqq_u8(bytes, lf)),
        );
        let non_whitespace = vmvnq_u8(whitespace);
        let first = vminvq_u8(vbslq_u8(non_whitespace, indexes, absent)) as usize;
        if first != 16 { return offset + first; }
        offset += 16;
    }
    offset + scalar_whitespace_prefix(chunks.remainder())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_vector_and_tail_boundaries() {
        for len in [0, 1, 15, 16, 17, 31, 32, 33] {
            let mut text = [b'a'; 33];
            assert_eq!(string_special(&text[..len]), len);
            text[len.saturating_sub(1)] = b'\\';
            if len != 0 {
                assert_eq!(string_special(&text[..len]), len - 1);
            }
        }
        assert_eq!(string_special(b"abcdefghijklmnop\0"), 16);
        assert_eq!(string_special(b"abcdefghijklmnop\\"), 16);
    }

    #[test]
    fn whitespace_alphabet_and_edges() {
        assert_eq!(whitespace_prefix(b" \t\r\nabcdefghijkl"), 4);
        assert_eq!(whitespace_prefix(b"                X"), 16);
        assert_eq!(whitespace_prefix(b"\x0b"), 0);
        assert_eq!(whitespace_prefix(b"\x0c"), 0);
        assert_eq!(whitespace_prefix(b" \t\r\n"), 4);
    }
}
