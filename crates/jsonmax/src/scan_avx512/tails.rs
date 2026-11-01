//! Bounded tail storage and the complete portable raw classifier.
//!
//! These functions are available on every target, including without `std`.
//! They require no CPU features, allocation, pointer operations, or alignment.
//! The fallback is a leaf: it never calls the other scanner modules.

use super::*;

/// Copy at most 64 live bytes into an owned, zero-padded block.
///
/// Only the input slice is read. Empty slices need no readable backing bytes,
/// and full blocks need neither extra padding nor vector alignment. Consumers
/// must retain the original length when analyzing quotes and assembling masks:
/// padding is storage, not input, and its zeros otherwise classify as controls.
///
/// # Errors
///
/// Input longer than 64 bytes returns `InvalidToken`, source `Scanner`, at
/// offset zero. The length is validated before copying or narrowing it.
#[inline]
pub fn load_block(input: &[u8]) -> Result<[u8; BLOCK_BYTES]> {
    valid_mask(input.len())?;
    let mut block = [0u8; BLOCK_BYTES];
    block[..input.len()].copy_from_slice(input);
    Ok(block)
}

/// Classify all 64 bytes locally, always reporting `BackendId::Scalar`.
///
/// Bit `i` describes byte `i`, independently of host endianness. Quotes,
/// backslashes, and punctuation are raw, without string-state filtering.
/// Whitespace is exactly space, tab, CR, and LF; controls are bytes below
/// `0x20`; digits are ASCII `0` through `9`; non-ASCII is `0x80..=0xff`.
/// Tab, CR, and LF intentionally belong to both whitespace and controls.
///
/// All byte patterns are accepted, including invalid JSON and invalid UTF-8.
/// A padded block's masks must subsequently be clipped to its live length.
#[inline]
pub fn classify_scalar_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    let mut raw = RawMasks {
        backend: BackendId::Scalar,
        ..RawMasks::default()
    };
    for (index, &byte) in input.iter().enumerate() {
        // An actual 64-byte array bounds index to 0..64, including bit 63.
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
            _ => {}
        }
        if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
            raw.classes.whitespace_mask |= bit;
        }
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
