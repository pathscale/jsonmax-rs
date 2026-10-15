//! Two-lane AVX2 quote classification and the bounded scanner pipeline.
//!
//! Safe entry points select the portable implementation when AVX2 is not
//! available. Quote state is committed only after the complete block succeeds.

use super::*;

#[cfg(target_arch = "x86")]
use core::arch::x86 as arch;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64 as arch;

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

    let block = tails::load_block(input)?;
    classify_loaded(&block, input.len(), carry)
}

/// Classify a complete block without copying it into tail storage.
///
/// # Errors
///
/// Returns `InternalError` for noncanonical quote carry. Errors leave all
/// caller state unchanged, including the opaque UTF-8 fields.
pub fn classify_block(
    input: &[u8; BLOCK_BYTES],
    carry: &mut ScanCarry,
) -> Result<BlockMasks> {
    validate_carry(carry)?;
    classify_loaded(input, BLOCK_BYTES, carry)
}

/// Run the shared pipeline after the public caller validates length and carry.
fn classify_loaded(
    input: &[u8; BLOCK_BYTES],
    len: usize,
    carry: &mut ScanCarry,
) -> Result<BlockMasks> {
    let raw = classify_raw(input);
    let mut next_carry = *carry;
    // Both callers establish len <= BLOCK_BYTES before this narrowing cast.
    let quotes = quotes::quote_masks(
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
        let (quote_mask, backslash_mask) = unsafe { quote_backslash_masks_avx2(input) };
        return RawMasks {
            quote_mask,
            backslash_mask,
            classes: structural::classify_structural(input),
            backend: BACKEND,
        };
    }

    tails::classify_scalar_raw(input)
}

/// The caller must establish AVX2 CPU and OS support before entering here.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn quote_backslash_masks_avx2(input: &[u8; BLOCK_BYTES]) -> (u64, u64) {
    // SAFETY: input is a readable 64-byte object. The two loads cover exactly
    // bytes 0..32 and 32..64 and loadu imposes no vector alignment requirement.
    // The function's target-feature precondition permits these instructions.
    unsafe {
        let low = arch::_mm256_loadu_si256(input.as_ptr().cast::<arch::__m256i>());
        let high = arch::_mm256_loadu_si256(
            input.as_ptr().add(LANE_BYTES).cast::<arch::__m256i>(),
        );
        let quote = arch::_mm256_set1_epi8(b'"' as i8);
        let backslash = arch::_mm256_set1_epi8(b'\\' as i8);

        // Movemask returns i32. Convert through u32 before widening so a match at
        // byte 31 cannot sign-extend into the independent upper 32-byte lane.
        let low_quotes = arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(low, quote))
            as u32 as u64;
        let high_quotes = arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(high, quote))
            as u32 as u64;
        let low_backslashes =
            arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(low, backslash)) as u32 as u64;
        let high_backslashes =
            arch::_mm256_movemask_epi8(arch::_mm256_cmpeq_epi8(high, backslash)) as u32 as u64;

        (
            low_quotes | (high_quotes << LANE_BYTES),
            low_backslashes | (high_backslashes << LANE_BYTES),
        )
    }
}
