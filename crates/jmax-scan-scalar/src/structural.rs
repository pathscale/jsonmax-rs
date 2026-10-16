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
    // Same classes as the byte loop in tails::classify_scalar_raw.
    classify_swar(input).classes
}

const LOW7: u64 = 0x7f7f_7f7f_7f7f_7f7f;
const HIGH: u64 = 0x8080_8080_8080_8080;
const ONES: u64 = 0x0101_0101_0101_0101;

/// High bit of each byte lane set exactly where that byte equals `byte`
/// (`byte < 0x80`). `low` is `word & LOW7` and `ascii` is `!word & HIGH`.
/// Lane sums stay below 0x100, so no carry crosses a byte boundary.
#[inline(always)]
const fn lanes_equal(low: u64, ascii: u64, byte: u8) -> u64 {
    !((low ^ (byte as u64 * ONES)) + LOW7) & ascii
}

/// High bit of each byte lane set exactly where that byte is below `bound`
/// (`bound <= 0x80`), again without inter-lane carries.
#[inline(always)]
const fn lanes_below(low: u64, ascii: u64, bound: u8) -> u64 {
    !(low + (0x80 - bound as u64) * ONES) & ascii
}

/// Gathers the eight lane high bits into one byte, lane 0 in bit 0.
#[inline(always)]
const fn lane_bits(lanes: u64) -> u64 {
    ((lanes >> 7) & ONES).wrapping_mul(0x0102_0408_1020_4080) >> 56
}

/// Word-at-a-time (SWAR) equivalent of the byte loop in `classify_structural`,
/// also producing the quote and backslash masks. Pure safe integer code:
/// byte lane `i` of each little-endian word is input byte `8 * word + i`.
#[inline]
pub(crate) fn classify_swar(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    let mut raw = RawMasks::default();
    let mut word_index = 0;
    while word_index < BLOCK_BYTES / 8 {
        let start = word_index * 8;
        let word = u64::from_le_bytes([
            input[start],
            input[start + 1],
            input[start + 2],
            input[start + 3],
            input[start + 4],
            input[start + 5],
            input[start + 6],
            input[start + 7],
        ]);
        let shift = start as u32;
        let low = word & LOW7;
        let ascii = !word & HIGH;
        let eq = |byte: u8| lane_bits(lanes_equal(low, ascii, byte)) << shift;
        raw.quote_mask |= eq(b'"');
        raw.backslash_mask |= eq(b'\\');
        let c = &mut raw.classes;
        c.colon_mask |= eq(b':');
        c.comma_mask |= eq(b',');
        c.lbrace_mask |= eq(b'{');
        c.rbrace_mask |= eq(b'}');
        c.lbracket_mask |= eq(b'[');
        c.rbracket_mask |= eq(b']');
        c.whitespace_mask |= lane_bits(
            lanes_equal(low, ascii, b' ')
                | lanes_equal(low, ascii, b'\t')
                | lanes_equal(low, ascii, b'\r')
                | lanes_equal(low, ascii, b'\n'),
        ) << shift;
        c.control_mask |= lane_bits(lanes_below(low, ascii, 0x20)) << shift;
        // '0'..='9' is at least '0' and below ':' (0x3a).
        let digits = !lanes_below(low, ascii, b'0') & lanes_below(low, ascii, b':');
        c.digit_mask |= lane_bits(digits) << shift;
        c.non_ascii_mask |= lane_bits(word & HIGH) << shift;
        word_index += 1;
    }
    raw
}
