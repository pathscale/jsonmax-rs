//! Bounded SIMD predicates for JSON string discovery and encoding.

/// Raw classes; bit i always describes byte i in a 64-byte input block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EscapeMasks {
    pub quote_mask: u64,
    pub backslash_mask: u64,
    pub control_mask: u64,
    pub non_ascii_mask: u64,
}

impl EscapeMasks {
    /// Bytes requiring mandatory JSON escaping (including double quotes).
    pub const fn escape_or_control(self) -> u64 {
        self.quote_mask | self.backslash_mask | self.control_mask
    }
}

type MaskFn = unsafe fn(&[u8; 64]) -> EscapeMasks;

/// Per-operation cached capability check for repeated block mask generation.
#[derive(Clone, Copy, Debug)]
pub struct EscapeMasker {
    classify: MaskFn,
}

/// Forward-only escape discovery over fixed, nonoverlapping 64-byte blocks.
///
/// Advancing past an escape clears consumed bits in the active mask, including
/// escaped quotes and the second backslash of a surrogate pair. Each block is
/// classified at most once, even when many escapes share that block. Ordinary
/// runs may cross blocks, so discovery never splits an ordinary UTF-8 scalar.
pub struct EscapeMaskCursor<'a> {
    bytes: &'a [u8],
    masker: EscapeMasker,
    block_base: usize,
    block_end: usize,
    escape_or_control_mask: u64,
}

impl<'a> EscapeMaskCursor<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self::with_masker(bytes, EscapeMasker::new())
    }

    pub fn with_masker(bytes: &'a [u8], masker: EscapeMasker) -> Self {
        Self {
            bytes,
            masker,
            block_base: 0,
            block_end: 0,
            escape_or_control_mask: 0,
        }
    }

    /// First quote, backslash, or control at/after `position`, or input length.
    /// Query positions must move forward; the input is bound to this cursor.
    #[inline]
    pub fn next_special(&mut self, mut position: usize) -> usize {
        // Earlier blocks have no match before the last returned position.
        // A caller may advance by less than the previous discovered run.
        position = position.max(self.block_base);
        while position < self.bytes.len() {
            if position >= self.block_end {
                // An escape may span the boundary. Skip its consumed prefix in
                // the next aligned block without revisiting the previous one.
                self.block_base = position / 64 * 64;
                self.block_end = self.bytes.len().min(self.block_base.saturating_add(64));
                let block = &self.bytes[self.block_base..self.block_end];
                self.escape_or_control_mask = if let Ok(full) = <&[u8; 64]>::try_from(block) {
                    self.masker.classify(full).escape_or_control()
                } else {
                    let mut mask = 0u64;
                    for (index, &byte) in block.iter().enumerate() {
                        if byte == b'"' || byte == b'\\' || byte < 0x20 {
                            mask |= 1u64 << index;
                        }
                    }
                    mask
                };
            }
            // position < block_end <= block_base + 64, so the shift is < 64.
            self.escape_or_control_mask &= u64::MAX << (position - self.block_base);
            if self.escape_or_control_mask != 0 {
                return self.block_base + self.escape_or_control_mask.trailing_zeros() as usize;
            }
            position = self.block_end;
        }
        self.bytes.len()
    }
}

impl Default for EscapeMasker {
    fn default() -> Self {
        Self::new()
    }
}

impl EscapeMasker {
    pub fn new() -> Self {
        #[cfg(target_arch = "aarch64")]
        if jmax_dispatch::detect::detect().supports(jmax_dispatch::BackendId::Neon) {
            return Self {
                classify: neon_masks,
            };
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        if jmax_dispatch::detect::detect().supports(jmax_dispatch::BackendId::Avx2) {
            return Self {
                classify: avx2_masks,
            };
        }
        Self {
            classify: scalar_masks,
        }
    }

    #[inline]
    pub fn classify(&self, bytes: &[u8; 64]) -> EscapeMasks {
        // SAFETY: new is the only constructor and checks CPU/OS features.
        // All kernels read exactly the array, without alignment requirements.
        unsafe { (self.classify)(bytes) }
    }
}

/// Classify one complete block without copying it or requiring padding.
pub fn escape_masks_64(bytes: &[u8; 64]) -> EscapeMasks {
    EscapeMasker::new().classify(bytes)
}

/// Mandatory JSON escape positions, including quotes, backslashes and controls.
pub fn escape_mask_64(bytes: &[u8; 64]) -> u64 {
    escape_masks_64(bytes).escape_or_control()
}

/// Alias documenting the control-byte part of the mandatory escape mask.
pub fn escape_or_control_mask_64(bytes: &[u8; 64]) -> u64 {
    escape_mask_64(bytes)
}

/// Find the first quote, backslash, or control; returns len when none occurs.
/// Loads are bounded even when input ends at an inaccessible page boundary.
pub fn first_escape_or_control(bytes: &[u8]) -> usize {
    super::escape::find_escape(bytes, super::EscapePolicy::default()).unwrap_or(bytes.len())
}

unsafe fn scalar_masks(bytes: &[u8; 64]) -> EscapeMasks {
    let mut masks = EscapeMasks::default();
    for (i, &byte) in bytes.iter().enumerate() {
        let bit = 1u64 << i;
        if byte == b'"' {
            masks.quote_mask |= bit;
        }
        if byte == b'\\' {
            masks.backslash_mask |= bit;
        }
        if byte < 0x20 {
            masks.control_mask |= bit;
        }
        if byte >= 0x80 {
            masks.non_ascii_mask |= bit;
        }
    }
    masks
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn neon_masks(input: &[u8; 64]) -> EscapeMasks {
    use core::arch::aarch64::*;
    // SAFETY: caller checked NEON. Each iteration reads exactly 16 real bytes.
    unsafe {
        let weights = vcombine_u8(
            vcreate_u8(u64::from_ne_bytes([1, 2, 4, 8, 16, 32, 64, 128])),
            vcreate_u8(u64::from_ne_bytes([1, 2, 4, 8, 16, 32, 64, 128])),
        );
        let mut masks = EscapeMasks::default();
        for offset in [0, 16, 32, 48] {
            let bytes = vld1q_u8(input.as_ptr().add(offset));
            masks.quote_mask |=
                u64::from(neon_bits(vceqq_u8(bytes, vdupq_n_u8(b'"')), weights)) << offset;
            masks.backslash_mask |=
                u64::from(neon_bits(vceqq_u8(bytes, vdupq_n_u8(b'\\')), weights)) << offset;
            masks.control_mask |=
                u64::from(neon_bits(vcltq_u8(bytes, vdupq_n_u8(0x20)), weights)) << offset;
            masks.non_ascii_mask |=
                u64::from(neon_bits(vcgeq_u8(bytes, vdupq_n_u8(0x80)), weights)) << offset;
        }
        masks
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
unsafe fn neon_bits(
    value: core::arch::aarch64::uint8x16_t,
    weights: core::arch::aarch64::uint8x16_t,
) -> u16 {
    use core::arch::aarch64::*;
    let pairs = vpaddlq_u8(vandq_u8(value, weights));
    let halves = vpaddlq_u32(vpaddlq_u16(pairs));
    (vgetq_lane_u64(halves, 0) | (vgetq_lane_u64(halves, 1) << 8)) as u16
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn avx2_masks(input: &[u8; 64]) -> EscapeMasks {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::*;
    // SAFETY: caller checked usable AVX2/OS state; both loads are in bounds.
    unsafe {
        let mut masks = EscapeMasks::default();
        for offset in [0, 32] {
            let bytes = _mm256_loadu_si256(input.as_ptr().add(offset).cast::<__m256i>());
            masks.quote_mask |= (_mm256_movemask_epi8(_mm256_cmpeq_epi8(
                bytes,
                _mm256_set1_epi8(b'"' as i8),
            )) as u32 as u64)
                << offset;
            masks.backslash_mask |=
                (_mm256_movemask_epi8(_mm256_cmpeq_epi8(bytes, _mm256_set1_epi8(b'\\' as i8)))
                    as u32 as u64)
                    << offset;
            let controls = _mm256_cmpeq_epi8(_mm256_min_epu8(bytes, _mm256_set1_epi8(0x1f)), bytes);
            masks.control_mask |= (_mm256_movemask_epi8(controls) as u32 as u64) << offset;
            masks.non_ascii_mask |= (_mm256_movemask_epi8(bytes) as u32 as u64) << offset;
        }
        masks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};

    static CLASSIFICATIONS: AtomicUsize = AtomicUsize::new(0);

    unsafe fn counted_masks(bytes: &[u8; 64]) -> EscapeMasks {
        CLASSIFICATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: scalar_masks reads only the supplied array and requires no
        // CPU features. This wrapper counts block calls in this one test.
        unsafe { scalar_masks(bytes) }
    }

    #[test]
    fn dense_skips_classify_each_full_block_once() {
        let bytes = [b'\\'; 1024];
        for width in [1, 2, 6, 12, 63, 64, 65] {
            CLASSIFICATIONS.store(0, Ordering::Relaxed);
            let mut cursor = EscapeMaskCursor::with_masker(
                &bytes,
                EscapeMasker {
                    classify: counted_masks,
                },
            );
            for position in (0..bytes.len()).step_by(width) {
                assert_eq!(cursor.next_special(position), position);
            }
            assert_eq!(cursor.next_special(bytes.len()), bytes.len());
            assert!(CLASSIFICATIONS.load(Ordering::Relaxed) <= bytes.len() / 64);
            if width <= 64 {
                assert_eq!(CLASSIFICATIONS.load(Ordering::Relaxed), bytes.len() / 64);
            }
        }
    }

    #[test]
    fn ordinary_runs_and_partial_tail_preserve_mask_positions() {
        let mut bytes = [b'x'; 145];
        bytes[63] = b'\\';
        bytes[64] = b'"';
        bytes[127] = 0;
        bytes[144] = b'\\';
        let mut cursor = EscapeMaskCursor::new(&bytes);
        for position in 0..=bytes.len() {
            let expected = bytes[position..]
                .iter()
                .position(|&byte| byte == b'\\' || byte == b'"' || byte < 0x20)
                .map_or(bytes.len(), |relative| position + relative);
            assert_eq!(cursor.next_special(position), expected);
        }
    }
}
