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

    if input.len() == BLOCK_BYTES {
        let block: &[u8; BLOCK_BYTES] = input.try_into().map_err(|_| {
            Error::new(
                ErrorCode::InternalError,
                ErrorSource::Scanner,
                0,
                "full scanner block has an invalid length",
            )
        })?;
        return classify_loaded(block, BLOCK_BYTES, carry);
    }

    let block = tails::load_block(input)?;
    classify_loaded(&block, input.len(), carry)
}

/// Classify a complete block without copying it into padded storage.
pub fn classify_block(input: &[u8; BLOCK_BYTES], carry: &mut ScanCarry) -> Result<BlockMasks> {
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
    finish_loaded(classify_raw(input), len, carry)
}

fn finish_loaded(raw: RawMasks, len: usize, carry: &mut ScanCarry) -> Result<BlockMasks> {
    finish_with_quotes(raw, len, carry, quotes::quote_masks)
}

#[inline]
fn finish_with_quotes(
    raw: RawMasks,
    len: usize,
    carry: &mut ScanCarry,
    quote_fn: impl FnOnce(u64, u64, u8, &mut ScanCarry) -> Result<QuoteMasks>,
) -> Result<BlockMasks> {
    let mut pending = *carry;
    let quotes = quote_fn(raw.quote_mask, raw.backslash_mask, len as u8, &mut pending)?;
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
        return unsafe { structural::classify_neon(input) };
    }

    tails::classify_scalar_raw(input)
}

/// Resolve safe callable bindings once, returning None when native execution
/// is unavailable. The bindings retain the checked CPU/OS feature invariant;
/// unlike the general entry points they do not repeat backend detection.
pub fn cached_classifiers() -> Option<(ClassifyFn, ClassifyBlockFn)> {
    #[cfg(target_arch = "aarch64")]
    if is_supported() {
        return if quotes::accelerated_prefix_available() {
            Some((classify_cached::<true>, classify_block_cached::<true>))
        } else {
            Some((classify_cached::<false>, classify_block_cached::<false>))
        };
    }
    None
}

#[cfg(target_arch = "aarch64")]
fn classify_cached<const ACCELERATED: bool>(
    input: &[u8],
    carry: &mut ScanCarry,
) -> Result<BlockMasks> {
    valid_mask(input.len())?;
    validate_carry(carry)?;
    if input.is_empty() {
        return Ok(BlockMasks::default());
    }
    if input.len() == BLOCK_BYTES {
        let block: &[u8; BLOCK_BYTES] = input.try_into().map_err(|_| {
            Error::new(
                ErrorCode::InternalError,
                ErrorSource::Scanner,
                0,
                "full scanner block has an invalid length",
            )
        })?;
        return finish_with_quotes(
            classify_raw_cached(block),
            BLOCK_BYTES,
            carry,
            quotes::quote_masks_cached::<ACCELERATED>,
        );
    }
    let block = tails::load_block(input)?;
    finish_with_quotes(
        classify_raw_cached(&block),
        input.len(),
        carry,
        quotes::quote_masks_cached::<ACCELERATED>,
    )
}

#[cfg(target_arch = "aarch64")]
fn classify_block_cached<const ACCELERATED: bool>(
    input: &[u8; BLOCK_BYTES],
    carry: &mut ScanCarry,
) -> Result<BlockMasks> {
    validate_carry(carry)?;
    finish_with_quotes(
        classify_raw_cached(input),
        BLOCK_BYTES,
        carry,
        quotes::quote_masks_cached::<ACCELERATED>,
    )
}

#[cfg(target_arch = "aarch64")]
fn classify_raw_cached(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    // SAFETY: only bindings returned after cached_classifiers checked CPU/OS
    // support reach this private helper. All native reads are bounded by input.
    unsafe { structural::classify_neon(input) }
}
