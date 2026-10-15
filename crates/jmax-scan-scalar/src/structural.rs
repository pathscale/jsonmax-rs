//! Raw punctuation, whitespace, and byte-class masks for a full scalar block.

use super::*;

/// Classify all 64 bytes, with bit `i` corresponding to input byte `i`.
///
/// Punctuation is exactly `: , { } [ ]`. JSON whitespace is space, tab, CR,
/// and LF; the latter three also belong to the control class (`0x00..=0x1f`).
/// Digits are ASCII `0..=9`, and every byte at least `0x80` is non-ASCII.
///
/// These masks are raw, including bytes inside strings. This infallible
/// operation neither validates JSON or UTF-8 nor reads or updates carry.
/// Every array element is classified, including any caller-supplied padding;
/// the scanner's final assembly clips masks to the actual input length.
pub fn classify_structural(input: &[u8; BLOCK_BYTES]) -> StructuralMasks {
    let mut masks = StructuralMasks::default();
    for (index, &byte) in input.iter().enumerate() {
        // The fixed-size array bounds the shift to 0..=63 on every target.
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
        // Control and whitespace intentionally overlap for tab, CR, and LF.
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
