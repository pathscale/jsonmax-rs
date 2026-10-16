//! AArch64 translation of the upstream 32-byte AVX interface.
use core::arch::aarch64::*;
#[allow(non_camel_case_types)]
pub type m256i = [uint8x16_t; 2];
pub mod avx {
    use super::*;
    #[inline(always)]
    pub fn mm256i(x: i8) -> m256i {
        unsafe { [vdupq_n_u8(x as u8); 2] }
    }
    #[inline(always)]
    pub unsafe fn u8_to_m256i(s: &[u8], i: usize) -> m256i {
        assert!(i <= s.len() && s.len() - i >= 32);
        [vld1q_u8(s.as_ptr().add(i)), vld1q_u8(s.as_ptr().add(i + 16))]
    }
    #[inline]
    pub unsafe fn u8_to_m256i_rest(s: &[u8], i: usize) -> m256i {
        assert!(i <= s.len() && s.len() - i <= 32);
        let mut tail = [0u8; 32];
        tail[..s.len() - i].copy_from_slice(&s[i..]);
        u8_to_m256i(&tail, 0)
    }
}
#[inline(always)]
pub fn mm256_cmpeq_epi8(x: m256i, y: m256i) -> m256i {
    unsafe { [vceqq_u8(x[0], y[0]), vceqq_u8(x[1], y[1])] }
}
#[inline(always)]
pub fn mm256_movemask_epi8(x: m256i) -> u32 {
    unsafe {
        let w = vld1_u8([1u8, 2, 4, 8, 16, 32, 64, 128].as_ptr());
        let mask = |v| (vaddv_u8(vand_u8(vget_low_u8(v), w)) as u32) | ((vaddv_u8(vand_u8(vget_high_u8(v), w)) as u32) << 8);
        mask(x[0]) | (mask(x[1]) << 16)
    }
}
#[target_feature(enable = "neon,aes")]
pub unsafe fn prefix_xor(bits: u64) -> u64 {
    vmull_p64(bits, u64::MAX) as u64
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prefix_matches_scalar() {
        if !std::arch::is_aarch64_feature_detected!("aes") {
            return;
        }
        let mut bits = 0x123456789abcdef0u64;
        for _ in 0..10000 {
            bits ^= bits << 13;
            bits ^= bits >> 7;
            bits ^= bits << 17;
            let mut expected = 0;
            let mut inside = 0;
            for i in 0..64 {
                inside ^= (bits >> i) & 1;
                expected |= inside << i;
            }
            assert_eq!(unsafe { prefix_xor(bits) }, expected);
        }
    }
    #[test]
    fn masks_and_every_tail() {
        for n in 1..=64 {
            let mut b = vec![b'x'; n];
            for pos in 0..n {
                b[pos] = b'"';
                let start = (pos / 32) * 32;
                let chunk = unsafe {
                    if start + 32 <= n {
                        avx::u8_to_m256i(&b, start)
                    } else {
                        avx::u8_to_m256i_rest(&b, start)
                    }
                };
                let mask = mm256_movemask_epi8(mm256_cmpeq_epi8(chunk, avx::mm256i(b'"' as i8)));
                assert_eq!(mask, 1 << (pos - start));
                b[pos] = b'x';
            }
        }
    }
}
