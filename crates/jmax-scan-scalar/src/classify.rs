//! Portable scalar classification with transactional quote carry updates.

use super::*;

/// Classify a single block containing zero through 64 bytes.
///
/// Every mask is restricted to the actual input length. Quote state flows into
/// and out of the block; UTF-8 state is preserved without validation. Incomplete
/// strings, escapes, and otherwise invalid JSON are classified, not rejected.
///
/// Returns a scanner `InvalidToken` error for oversized input or `InternalError`
/// for noncanonical quote carry. Any error leaves the caller's carry unchanged.
pub fn classify(input: &[u8], carry: &mut ScanCarry) -> Result<BlockMasks> {
    valid_mask(input.len())?;
    validate_carry(carry)?;
    let block = tails::load_block(input)?;
    // The length check above makes this conversion lossless.
    classify_loaded(&block, input.len() as u8, carry)
}

/// Classify all 64 bytes of an array without copying or padding the input.
///
/// This has the same masks, state transitions, and transactional error behavior
/// as [`classify`] on the array's full slice.
pub fn classify_block(
    input: &[u8; BLOCK_BYTES],
    carry: &mut ScanCarry,
) -> Result<BlockMasks> {
    validate_carry(carry)?;
    classify_loaded(input, BLOCK_BYTES as u8, carry)
}

/// Return full-width lexical masks without applying quote state or validation.
///
/// Bit `i` describes byte `i` regardless of host endianness. Punctuation and
/// other byte classes are raw, including occurrences within strings.
pub fn classify_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    let mut quote_mask = 0;
    let mut backslash_mask = 0;
    for (index, &byte) in input.iter().enumerate() {
        let bit = 1u64 << index;
        match byte {
            b'"' => quote_mask |= bit,
            b'\\' => backslash_mask |= bit,
            _ => {}
        }
    }
    RawMasks {
        quote_mask,
        backslash_mask,
        classes: structural::classify_structural(input),
        backend: BackendId::Scalar,
    }
}

// Both callers validate quote carry and supply a length in 0..=64. Keep the
// pending state private until quote analysis and ABI mask assembly succeed.
fn classify_loaded(
    input: &[u8; BLOCK_BYTES],
    valid_len: u8,
    carry: &mut ScanCarry,
) -> Result<BlockMasks> {
    let raw = classify_raw(input);
    let mut next_carry = *carry;
    let quotes = quotes::quote_masks(
        raw.quote_mask,
        raw.backslash_mask,
        valid_len,
        &mut next_carry,
    )?;
    let masks = raw.finish(quotes, usize::from(valid_len))?;
    *carry = next_carry;
    Ok(masks)
}
