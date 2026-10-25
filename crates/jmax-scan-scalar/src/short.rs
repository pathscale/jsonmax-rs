//! Bounded short scans used by syntax tokenization.

/// Return the first quote, backslash, or JSON control byte in `input`.
///
/// The returned index is relative to the slice, or `input.len()` when no
/// special byte occurs. A quote and a backslash are both boundaries because
/// callers must handle delimiters and escapes; bytes below `0x20` are reported
/// so callers can reject unescaped string controls. No bytes beyond the slice
/// are read.
#[inline]
pub fn string_special(input: &[u8]) -> usize {
    input
        .iter()
        .position(|&byte| byte == b'"' || byte == b'\\' || byte < 0x20)
        .unwrap_or(input.len())
}

/// Return the first string boundary and whether its preceding bytes are ASCII.
/// The second value covers bytes strictly before the returned boundary, or
/// the entire slice when no boundary occurs. This fuses key ASCII detection
/// with boundary discovery in one bounded pass.
#[inline]
pub fn string_special_ascii(input: &[u8]) -> (usize, bool) {
    const HIGH_BITS: u64 = 0x8080_8080_8080_8080;
    const LOW_BITS: u64 = 0x0101_0101_0101_0101;
    let mut ascii = true;
    let mut chunks = input.chunks_exact(8);
    let mut offset = 0;
    for chunk in &mut chunks {
        let word = u64::from_ne_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3],
            chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        let candidates = chunk.iter().any(|&byte| byte < 0x20)
            || has_zero_byte(word ^ u64::from_ne_bytes([b'"'; 8]), LOW_BITS, HIGH_BITS)
            || has_zero_byte(word ^ u64::from_ne_bytes([b'\\'; 8]), LOW_BITS, HIGH_BITS);
        if candidates {
            for (within, &byte) in chunk.iter().enumerate() {
                if byte == b'"' || byte == b'\\' || byte < 0x20 {
                    return (offset + within, ascii);
                }
                ascii &= byte.is_ascii();
            }
        } else {
            ascii &= (word & HIGH_BITS) == 0;
        }
        offset += 8;
    }
    for (within, &byte) in chunks.remainder().iter().enumerate() {
        if byte == b'"' || byte == b'\\' || byte < 0x20 {
            return (offset + within, ascii);
        }
        ascii &= byte.is_ascii();
    }
    (input.len(), ascii)
}

#[inline]
fn has_zero_byte(value: u64, low_bits: u64, high_bits: u64) -> bool {
    value.wrapping_sub(low_bits) & !value & high_bits != 0
}

/// Return the length of the leading JSON whitespace prefix in `input`.
///
/// JSON whitespace is exactly space, tab, carriage return, and line feed.
/// Returns `input.len()` when the entire slice is whitespace. No bytes beyond
/// the slice are read.
#[inline]
pub fn whitespace_prefix(input: &[u8]) -> usize {
    let spaces = u64::from_ne_bytes([b' '; 8]);
    let tabs = u64::from_ne_bytes([b'\t'; 8]);
    let returns = u64::from_ne_bytes([b'\r'; 8]);
    let lines = u64::from_ne_bytes([b'\n'; 8]);
    let mut chunks = input.chunks_exact(8);
    let mut offset = 0;
    for chunk in &mut chunks {
        let word = u64::from_ne_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3],
            chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        if word != spaces && word != tabs && word != returns && word != lines {
            if let Some(within) = chunk.iter().position(|&byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n')) {
                return offset + within;
            }
        }
        offset += 8;
    }
    offset + chunks.remainder().iter()
        .position(|&byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        .unwrap_or(chunks.remainder().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_special_boundaries() {
        for input in [b"".as_slice(), b"plain", b"0123456789abcdef"] {
            assert_eq!(string_special(input), input.len());
        }
        assert_eq!(string_special(b"abc\"tail"), 3);
        assert_eq!(string_special(b"abc\\tail"), 3);
        assert_eq!(string_special(b"abc\0tail"), 3);
        assert_eq!(string_special(b"\x1f"), 0);
    }

    #[test]
    fn whitespace_prefix_boundaries_and_alphabet() {
        assert_eq!(whitespace_prefix(b""), 0);
        assert_eq!(whitespace_prefix(b" \t\r\n"), 4);
        assert_eq!(whitespace_prefix(b" \tX\r"), 2);
        assert_eq!(whitespace_prefix(b"\x0b"), 0);
        assert_eq!(whitespace_prefix(b"\x0c"), 0);
    }
}
