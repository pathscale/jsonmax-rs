//! Two-lane AVX2 quote classification and the bounded scanner pipeline.
//!
//! Safe entry points select the portable implementation when AVX2 is not
//! available. Quote state is committed only after the complete block succeeds.

use super::*;

/// Classify at most 64 bytes, preserving the caller's UTF-8 state verbatim.
///
/// An empty block produces zero masks with the scalar backend tag and leaves
/// carry unchanged. Partial blocks are copied into bounded, padded storage;
/// neither the returned masks nor quote carry include padding bytes.
///
/// # Errors
///
/// Returns `InvalidToken` for more than 64 bytes or `InternalError` for
/// noncanonical quote carry. Errors leave all caller state unchanged.
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

/// Classify a complete block without copying it into tail storage.
///
/// # Errors
///
/// Returns `InternalError` for noncanonical quote carry. Errors leave all
/// caller state unchanged, including the opaque UTF-8 fields.
pub fn classify_block(input: &[u8; BLOCK_BYTES], carry: &mut ScanCarry) -> Result<BlockMasks> {
    validate_carry(carry)?;
    classify_loaded(input, BLOCK_BYTES, carry)
}

/// Run the shared pipeline after the public caller validates length and carry.
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

#[inline(always)]
fn finish_with_quotes(
    raw: RawMasks,
    len: usize,
    carry: &mut ScanCarry,
    quote_fn: impl FnOnce(u64, u64, u8, &mut ScanCarry) -> Result<QuoteMasks>,
) -> Result<BlockMasks> {
    let mut next_carry = *carry;
    // Both callers establish len <= BLOCK_BYTES before this narrowing cast.
    let quotes = quote_fn(
        raw.quote_mask,
        raw.backslash_mask,
        len as u8,
        &mut next_carry,
    )?;
    let masks = raw.finish(quotes, len)?;
    *carry = next_carry;
    Ok(masks)
}

/// Return raw lexical classes for all 64 bytes, independent of quote state.
///
/// The backend identifies the path actually executed. This safe function
/// checks AVX2 support itself and is also available on non-x86 targets.
pub fn classify_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    if is_supported() {
        // SAFETY: is_supported checks AVX2 and OS register-state support, or
        // requires statically enabled AVX2 in no-std builds. The array reference
        // guarantees that both unaligned 32-byte reads remain within bounds.
        return unsafe { structural::classify_avx2(input) };
    }

    tails::classify_scalar_raw(input)
}

/// Resolve safe callable bindings once, returning None when native execution
/// is unavailable. The bindings retain the checked CPU/OS feature invariant;
/// unlike the general entry points they do not repeat backend detection.
pub fn cached_classifiers() -> Option<(ClassifyFn, ClassifyBlockFn)> {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    if is_supported() {
        return if quotes::accelerated_prefix_available() {
            Some((classify_cached::<true>, classify_block_cached::<true>))
        } else {
            Some((classify_cached::<false>, classify_block_cached::<false>))
        };
    }
    None
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
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

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
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

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn classify_raw_cached(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    // SAFETY: only bindings returned after cached_classifiers checked CPU/OS
    // support reach this private helper. All native reads are bounded by input.
    unsafe { structural::classify_avx2(input) }
}
