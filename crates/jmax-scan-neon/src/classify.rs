//! Four-lane byte classification and transactional quote-carry integration.

use super::*;

/// Classify one block of at most 64 bytes, retaining quote state for the next.
///
/// Errors leave every field of `carry` unchanged. Empty input still validates
/// the quote carry, then returns empty Scalar masks without advancing state.
pub fn classify(input: &[u8], carry: &mut ScanCarry) -> Result<BlockMasks> {
    valid_mask(input.len())?;
    validate_carry(carry)?;
    if input.is_empty() {
        return Ok(BlockMasks::default());
    }

    let block = tails::load_block(input)?;
    classify_loaded(&block, input.len(), carry)
}

/// Classify a complete block without copying it into padded storage.
pub fn classify_block(
    input: &[u8; BLOCK_BYTES],
    carry: &mut ScanCarry,
) -> Result<BlockMasks> {
    validate_carry(carry)?;
    classify_loaded(input, BLOCK_BYTES, carry)
}

// Both callers validate carry and guarantee len <= BLOCK_BYTES before entry.
// Keeping the pending state private also covers any error from quote analysis
// or final assembly, rather than relying on those callees to roll state back.
fn classify_loaded(
    input: &[u8; BLOCK_BYTES],
    len: usize,
    carry: &mut ScanCarry,
) -> Result<BlockMasks> {
    let raw = classify_raw(input);
    let mut pending = *carry;
    let quotes = quotes::quote_masks(
        raw.quote_mask,
        raw.backslash_mask,
        len as u8,
        &mut pending,
    )?;
    let masks = raw.finish(quotes, len)?;
    *carry = pending;
    Ok(masks)
}

/// Return raw full-block masks with a tag identifying the executed backend.
///
/// Bit `i` always denotes input byte `i`. CPU support is checked here even
/// when the caller has already selected this crate through a dispatcher.
pub fn classify_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    #[cfg(target_arch = "aarch64")]
    if is_supported() {
        // SAFETY: is_supported establishes NEON availability for this build
        // and CPU. The array contains all 64 bytes read by the four loads.
        let (quote_mask, backslash_mask) = unsafe { classify_quotes_neon(input) };
        // This safe entry point independently applies the same support check,
        // so the structural and quote classifiers select the same backend.
        return RawMasks {
            quote_mask,
            backslash_mask,
            classes: structural::classify_structural(input),
            backend: BackendId::Neon,
        };
    }

    tails::classify_scalar_raw(input)
}

/// Reduce 0x00/0xff comparison lanes in byte-address order.
///
/// Each half sums distinct powers of two, with a maximum sum of 255. No
/// vector-to-integer reinterpretation or native-endian integer load is used.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn comparison_mask(value: core::arch::aarch64::uint8x16_t) -> u16 {
    use core::arch::aarch64::*;

    let weights = [1u8, 2, 4, 8, 16, 32, 64, 128];
    // SAFETY: callers establish NEON availability. The weight load reads
    // exactly eight initialized bytes and has no alignment requirement.
    unsafe {
        let weights = vld1_u8(weights.as_ptr());
        let low = vaddv_u8(vand_u8(vget_low_u8(value), weights));
        let high = vaddv_u8(vand_u8(vget_high_u8(value), weights));
        u16::from(low) | (u16::from(high) << 8)
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn classify_quotes_neon(input: &[u8; BLOCK_BYTES]) -> (u64, u64) {
    use core::arch::aarch64::*;

    let mut quote_mask = 0u64;
    let mut backslash_mask = 0u64;
    // SAFETY: the only caller checks NEON support. For lane 0..4, offsets
    // 0, 16, 32, 48 each leave 16 readable bytes in the fixed-size array.
    // vld1q_u8 accepts unaligned pointers. All helper calls inherit NEON.
    unsafe {
        let quote = vdupq_n_u8(b'"');
        let backslash = vdupq_n_u8(b'\\');
        for lane in 0..VECTOR_LANES {
            let offset = lane * LANE_BYTES;
            let bytes = vld1q_u8(input.as_ptr().add(offset));
            let quotes = comparison_mask(vceqq_u8(bytes, quote));
            let backslashes = comparison_mask(vceqq_u8(bytes, backslash));
            // Widen before shifting, including the highest lane and bit 63.
            quote_mask |= u64::from(quotes) << offset;
            backslash_mask |= u64::from(backslashes) << offset;
        }
    }
    (quote_mask, backslash_mask)
}
