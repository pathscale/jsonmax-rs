//! Bounded block classification with guarded AVX-512 byte comparisons.
//!
//! Carry is committed only after quote analysis and ABI mask assembly succeed.
//! Partial blocks use owned, zero-padded storage; only live bytes affect masks
//! or outgoing quote state.

use super::*;

/// Classify at most 64 bytes, preserving carry on every error.
///
/// Oversized input produces `InvalidToken`; noncanonical quote carry produces
/// `InternalError`. Both errors have source `Scanner` and offset zero. Empty
/// input still validates carry, then returns zero masks with a scalar tag.
pub fn classify(input: &[u8], carry: &mut ScanCarry) -> Result<BlockMasks> {
    valid_mask(input.len())?;
    validate_carry(carry)?;
    if input.is_empty() {
        return Ok(BlockMasks::default());
    }

    let block = tails::load_block(input)?;
    classify_validated(&block, input.len(), carry)
}

/// Classify an entire array, detecting CPU support independently of callers.
///
/// The array need not have vector alignment. Invalid quote carry is rejected
/// before classification and remains unchanged.
pub fn classify_block(input: &[u8; BLOCK_BYTES], carry: &mut ScanCarry) -> Result<BlockMasks> {
    validate_carry(carry)?;
    classify_validated(input, BLOCK_BYTES, carry)
}

// Both callers validate the length and carry before entering this pipeline.
fn classify_validated(
    input: &[u8; BLOCK_BYTES],
    len: usize,
    carry: &mut ScanCarry,
) -> Result<BlockMasks> {
    let raw = classify_raw(input);
    let mut next_carry = *carry;
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

/// Classify all 64 bytes without applying string state or modifying carry.
///
/// Bit `i` describes byte `i`, including the highest bit for byte 63. The
/// backend identifies the path actually executed. Unsupported architectures,
/// CPUs, and OS configurations use the complete scalar fallback.
pub fn classify_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    if is_supported() {
        // SAFETY: is_supported checks both required features and, with std,
        // OS-enabled vector state. Without std it requires static features.
        // The reference supplies a complete readable 64-byte object.
        let (quote_mask, backslash_mask) = unsafe { quote_backslash_avx512(input) };
        return RawMasks {
            quote_mask,
            backslash_mask,
            classes: structural::classify_structural(input),
            backend: BACKEND,
        };
    }

    tails::classify_scalar_raw(input)
}

#[cfg(target_arch = "x86")]
use core::arch::x86::{
    __m512i, _mm512_cmpeq_epi8_mask, _mm512_loadu_si512, _mm512_set1_epi8,
};
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::{
    __m512i, _mm512_cmpeq_epi8_mask, _mm512_loadu_si512, _mm512_set1_epi8,
};

/// Caller must establish AVX-512F and AVX-512BW CPU and OS support.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx512f,avx512bw")]
unsafe fn quote_backslash_avx512(input: &[u8; BLOCK_BYTES]) -> (u64, u64) {
    // SAFETY: input covers exactly the 64 bytes read. The unaligned load
    // requires no extra alignment or padding and never reads past the array.
    let bytes = unsafe { _mm512_loadu_si512(input.as_ptr().cast::<__m512i>()) };
    let quotes = _mm512_cmpeq_epi8_mask(bytes, _mm512_set1_epi8(b'"' as i8));
    let backslashes = _mm512_cmpeq_epi8_mask(bytes, _mm512_set1_epi8(b'\\' as i8));
    (quotes, backslashes)
}
