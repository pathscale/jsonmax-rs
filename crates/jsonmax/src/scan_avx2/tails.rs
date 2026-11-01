//! Bounded tail storage and the architecture-independent raw fallback.
//!
//! These functions require neither CPU feature detection nor caller padding.
//! The production fallback depends only on root-owned types and helpers, so
//! dispatching here cannot recurse into the vector or quote implementations.

use super::*;

/// Copy at most 64 bytes into a complete, zero-padded block.
///
/// Only the supplied slice is read. Empty input returns an all-zero array,
/// and a full block is copied unchanged. No alignment is required. Consumers
/// must retain the original length when resolving quotes and clipping masks:
/// the padding is storage, not additional input.
///
/// # Errors
///
/// Lengths above 64 return `InvalidToken`, source `Scanner`, at offset zero.
/// Length validation precedes slicing, copying, or any narrowing conversion.
pub fn load_block(input: &[u8]) -> Result<[u8; BLOCK_BYTES]> {
    valid_mask(input.len())?;
    let mut block = [0; BLOCK_BYTES];
    block[..input.len()].copy_from_slice(input);
    Ok(block)
}

/// Classify every byte of a complete block using portable scalar operations.
///
/// Bit `i` describes byte `i`, including positions 31, 32, and 63, independently
/// of host endianness. All masks are raw: quotes, backslashes, and punctuation
/// inside strings remain present. Whitespace is exactly space, tab, CR, and LF;
/// tab, CR, and LF also belong to the control class. Digits are ASCII only.
///
/// This function always reports `BackendId::Scalar`, allocates nothing, and
/// works without `std` on every architecture. It performs no quote analysis,
/// UTF-8 validation, or JSON grammar validation. Every possible block is valid
/// input, including an all-zero block whose control mask is all ones.
pub fn classify_scalar_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    let mut raw = RawMasks {
        backend: BackendId::Scalar,
        ..RawMasks::default()
    };
    for (index, &byte) in input.iter().enumerate() {
        // The array bounds index to 0..64, so this never shifts by 64.
        let bit = 1u64 << index;
        match byte {
            b'"' => raw.quote_mask |= bit,
            b'\\' => raw.backslash_mask |= bit,
            b':' => raw.classes.colon_mask |= bit,
            b',' => raw.classes.comma_mask |= bit,
            b'{' => raw.classes.lbrace_mask |= bit,
            b'}' => raw.classes.rbrace_mask |= bit,
            b'[' => raw.classes.lbracket_mask |= bit,
            b']' => raw.classes.rbracket_mask |= bit,
            b' ' | b'\t' | b'\r' | b'\n' => raw.classes.whitespace_mask |= bit,
            _ => {}
        }
        // These predicates are independent: control and whitespace overlap.
        if byte < 0x20 {
            raw.classes.control_mask |= bit;
        }
        if byte.is_ascii_digit() {
            raw.classes.digit_mask |= bit;
        }
        if byte >= 0x80 {
            raw.classes.non_ascii_mask |= bit;
        }
    }
    raw
}
