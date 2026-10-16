//! Bounded tail storage and the architecture-independent raw fallback.
//!
//! These functions use only safe, allocation-free operations and remain
//! available on every target, including no_std and non-AArch64 builds.

use super::*;

/// Copy at most 64 bytes into a completely initialized, zero-padded block.
///
/// Only the supplied slice is read. No alignment, readable suffix, or caller
/// padding is required. Consumers must retain the original input length to
/// exclude padding from observable masks and quote-carry transitions.
///
/// # Errors
///
/// Inputs longer than 64 bytes return `InvalidToken` from `Scanner` at offset
/// zero, before any copy or length narrowing occurs.
pub fn load_block(input: &[u8]) -> Result<[u8; BLOCK_BYTES]> {
    if input.len() > BLOCK_BYTES {
        return Err(Error::new(
            ErrorCode::InvalidToken,
            ErrorSource::Scanner,
            0,
            "scanner block exceeds 64 bytes",
        ));
    }

    let mut block = [0u8; BLOCK_BYTES];
    block[..input.len()].copy_from_slice(input);
    Ok(block)
}

/// Classify every byte in a complete block with the portable scalar path.
///
/// Bit `i` describes byte `i`, independently of alignment and host endianness.
/// Quotes, backslashes, and lexical classes are raw: no quoting, grammar, or
/// UTF-8 validation occurs. JSON whitespace is space, tab, CR, and LF; control
/// bytes are below 0x20, digits are ASCII, and non-ASCII bytes are at least 0x80.
/// Tab, CR, and LF therefore appear in both whitespace and control masks.
///
/// The backend is always `Scalar`, even on a NEON-capable machine. This local
/// implementation does not invoke the selected classifier, structural module,
/// or quote resolver, so fallback selection cannot recurse.
pub fn classify_scalar_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    let mut masks = RawMasks {
        backend: BackendId::Scalar,
        ..RawMasks::default()
    };
    for (position, &byte) in input.iter().enumerate() {
        // The fixed array bounds position to 0..64, including bit 63.
        let bit = 1u64 << position;
        match byte {
            b'"' => masks.quote_mask |= bit,
            b'\\' => masks.backslash_mask |= bit,
            b':' => masks.classes.colon_mask |= bit,
            b',' => masks.classes.comma_mask |= bit,
            b'{' => masks.classes.lbrace_mask |= bit,
            b'}' => masks.classes.rbrace_mask |= bit,
            b'[' => masks.classes.lbracket_mask |= bit,
            b']' => masks.classes.rbracket_mask |= bit,
            _ => {}
        }
        if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
            masks.classes.whitespace_mask |= bit;
        }
        if byte < 0x20 {
            masks.classes.control_mask |= bit;
        }
        if byte.is_ascii_digit() {
            masks.classes.digit_mask |= bit;
        }
        if byte >= 0x80 {
            masks.classes.non_ascii_mask |= bit;
        }
    }
    masks
}
