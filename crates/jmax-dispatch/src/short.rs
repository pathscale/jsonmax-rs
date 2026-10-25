//! Architecture-selected bounded scans for syntax tokenization.

/// Return the first quote, backslash, or JSON control byte in a string body.
///
/// The result is the byte offset within `input`, or its length when no
/// delimiter, escape, or unescaped-control candidate occurs. The scan does
/// not interpret escapes or validate strings; callers handle the reported
/// boundary. Native implementations use only complete vectors inside the
/// slice and inspect any remaining bytes individually.
#[inline]
pub fn string_special(input: &[u8]) -> usize {
    let _t = jmax_trace::span!(SCAN_STRING_SPECIAL);
    #[cfg(target_arch = "aarch64")]
    { jmax_scan_neon::short::string_special(input) }
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    { jmax_scan_avx2::short::string_special(input) }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86", target_arch = "x86_64")))]
    { jmax_scan_scalar::short::string_special(input) }
}

/// Return the first string boundary and whether its preceding bytes are ASCII.
/// The ASCII result covers bytes strictly before the boundary, or the whole
/// slice when no boundary occurs. Key scanners can use this to find the close
/// quote and validate an unescaped ASCII body in the same byte pass.
#[inline]
pub fn string_special_ascii(input: &[u8]) -> (usize, bool) {
    let _t = jmax_trace::span!(SCAN_STRING_SPECIAL);
    #[cfg(target_arch = "aarch64")]
    { jmax_scan_neon::short::string_special_ascii(input) }
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    { jmax_scan_avx2::short::string_special_ascii(input) }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86", target_arch = "x86_64")))]
    { jmax_scan_scalar::short::string_special_ascii(input) }
}

/// Return the length of the leading JSON whitespace prefix in `input`.
///
/// JSON whitespace is exactly space, tab, carriage return, and line feed.
/// Returns the slice length when every byte is whitespace. This operation is
/// bounded to the provided slice and does not use sentinel or padding bytes.
#[inline]
pub fn whitespace_prefix(input: &[u8]) -> usize {
    let _t = jmax_trace::span!(SCAN_WHITESPACE);
    #[cfg(target_arch = "aarch64")]
    { jmax_scan_neon::short::whitespace_prefix(input) }
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    { jmax_scan_avx2::short::whitespace_prefix(input) }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86", target_arch = "x86_64")))]
    { jmax_scan_scalar::short::whitespace_prefix(input) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_helpers_are_bounded_and_exact() {
        let mut string = [b'x'; 34];
        assert_eq!(string_special(&string[..]), string.len());
        string[16] = b'\\';
        assert_eq!(string_special(&string), 16);
        string[16] = b'x';
        string[33] = 0;
        assert_eq!(string_special(&string), 33);

        assert_eq!(whitespace_prefix(b" \t\r\nabcdefghijkl"), 4);
        assert_eq!(whitespace_prefix(b"                "), 16);
        assert_eq!(whitespace_prefix(b"\x0b"), 0);
        assert_eq!(whitespace_prefix(b""), 0);
    }

    #[test]
    fn every_byte_at_every_vector_position_matches_scalar() {
        for byte in 0..=255u8 {
            for position in 0..65 {
                let mut text = [b'x'; 65];
                text[position] = byte;
                for length in [position, position + 1, 65] {
                    assert_eq!(string_special(&text[..length]),
                        jmax_scan_scalar::short::string_special(&text[..length]));
                }
                let mut spaces = [b' '; 65];
                spaces[position] = byte;
                assert_eq!(whitespace_prefix(&spaces),
                    jmax_scan_scalar::short::whitespace_prefix(&spaces));
            }
        }
    }
}

#[cfg(test)]
mod ascii_prefix_tests {
    use super::*;

    #[test]
    fn ascii_flag_excludes_every_byte_after_boundary() {
        for boundary in 0..65 {
            for position in 0..65 {
                for byte in [0, b'\\', b'"', 0x7f, 0x80, 0xff] {
                    let mut raw = [b'x'; 65];
                    raw[boundary] = b'"';
                    raw[position] = byte;
                    for len in [boundary, boundary + 1, 65] {
                        let slice = &raw[..len];
                        let end = string_special(slice);
                        assert_eq!(string_special_ascii(slice), (end, slice[..end].is_ascii()));
                    }
                }
            }
        }
    }
}
