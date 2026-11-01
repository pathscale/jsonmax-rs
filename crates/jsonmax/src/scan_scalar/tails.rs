//! Bounded block loading and an independent, portable raw classifier.

use super::*;

/// Copy zero through 64 available bytes into a zero-padded block.
///
/// The input needs no alignment or readable padding. Only bytes within the
/// slice are read, and the returned array owns its storage. Consumers must
/// retain the original length when analyzing quotes and assembling ABI masks:
/// the zero padding is storage, not input.
///
/// # Errors
///
/// An input longer than 64 bytes returns `InvalidToken`, source `Scanner`, at
/// offset zero, before any bytes are copied or any length is narrowed.
pub fn load_block(input: &[u8]) -> Result<[u8; BLOCK_BYTES]> {
    if input.len() > BLOCK_BYTES {
        return Err(Error::new(
            ErrorCode::InvalidToken,
            ErrorSource::Scanner,
            0,
            "scanner block exceeds 64 bytes",
        ));
    }

    let mut block = [0; BLOCK_BYTES];
    block[..input.len()].copy_from_slice(input);
    Ok(block)
}

/// Classify every byte of a full block using only unsigned scalar operations.
///
/// Bit `i` describes byte `i` independently of alignment and host endianness.
/// All masks are raw: quotes include escaped quotes, punctuation includes
/// string contents, and zero padding contributes control bits. Final assembly
/// must clip these masks to the actual input length.
///
/// This reference fallback always reports `BackendId::Scalar`. It performs no
/// allocation, quote-state processing, JSON or UTF-8 validation, or calls into
/// the other scanner modules, so it remains usable on every target.
pub fn classify_scalar_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    let mut masks = RawMasks::default();
    for (index, &byte) in input.iter().enumerate() {
        // The array length bounds every shift to 0..=63.
        let bit = 1u64 << index;
        match byte {
            b'"' => masks.quote_mask |= bit,
            b'\\' => masks.backslash_mask |= bit,
            b':' => masks.classes.colon_mask |= bit,
            b',' => masks.classes.comma_mask |= bit,
            b'{' => masks.classes.lbrace_mask |= bit,
            b'}' => masks.classes.rbrace_mask |= bit,
            b'[' => masks.classes.lbracket_mask |= bit,
            b']' => masks.classes.rbracket_mask |= bit,
            b' ' | b'\t' | b'\r' | b'\n' => masks.classes.whitespace_mask |= bit,
            _ => {}
        }
        // Tab, CR, and LF belong to both whitespace and control classes.
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
