//! Stable stdarch AVX2 classifier, with a scalar compatibility interface.
//!
//! The indexer checks AVX2 once and loads two 32-byte input registers. On
//! machines without AVX2 the same interface uses bounded scalar operations.
use core::arch::x86_64::*;

#[allow(non_camel_case_types)]
pub type m256i = [u8; 32];

pub mod avx {
    use super::m256i;

    pub fn mm256i(x: i8) -> m256i {
        [x as u8; 32]
    }

    pub unsafe fn u8_to_m256i(s: &[u8], i: usize) -> m256i {
        let mut block = [0; 32];
        block.copy_from_slice(&s[i..][..32]);
        block
    }

    pub unsafe fn u8_to_m256i_rest(s: &[u8], i: usize) -> m256i {
        let mut block = [0; 32];
        block[..s.len() - i].copy_from_slice(&s[i..]);
        block
    }
}

pub fn mm256_cmpeq_epi8(x: m256i, y: m256i) -> m256i {
    let mut equal = [0; 32];
    for i in 0..32 {
        equal[i] = if x[i] == y[i] { 0xff } else { 0 };
    }
    equal
}

pub fn mm256_movemask_epi8(x: m256i) -> u32 {
    x.iter().enumerate().fold(0, |mask, (i, byte)| mask | (((byte >> 7) as u32) << i))
}

/// Caller must detect AVX2 before calling. Each input block is loaded once.
#[target_feature(enable = "avx2")]
pub unsafe fn classify64(s: &[u8], needles: [&m256i; 5]) -> [u64; 5] {
    assert_eq!(s.len(), 64);
    let lo = _mm256_loadu_si256(s.as_ptr() as *const __m256i);
    let hi = _mm256_loadu_si256(s.as_ptr().add(32) as *const __m256i);
    let mut masks = [0; 5];
    for (i, needle) in needles.iter().enumerate() {
        let n = _mm256_loadu_si256(needle.as_ptr() as *const __m256i);
        masks[i] = _mm256_movemask_epi8(_mm256_cmpeq_epi8(lo, n)) as u32 as u64 | ((_mm256_movemask_epi8(_mm256_cmpeq_epi8(hi, n)) as u32 as u64) << 32);
    }
    masks
}

/// PCLMULQDQ is independent of AVX2 and must be detected separately.
#[target_feature(enable = "pclmulqdq")]
pub unsafe fn prefix_xor(bits: u64) -> u64 {
    let product = _mm_clmulepi64_si128(_mm_set_epi64x(0, bits as i64), _mm_set1_epi64x(-1), 0);
    _mm_cvtsi128_si64(product) as u64
}
