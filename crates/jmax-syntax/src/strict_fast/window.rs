//! A 64-byte classification window over the input.
//!
//! The acceptor's serial chain is the offset `i`: each token's end decides where the
//! next one starts. Finding that end by loading, comparing and reducing a vector at `i`
//! puts a vector load, compare, move-mask and bit scan on the chain for every string
//! and every whitespace run. The window moves that work off the chain: it classifies
//! the 64 bytes at `base` once (independent of `i`), and a token end then costs a
//! shift and a bit scan on a register.
//!
//! Bit `k` of each mask describes `input[base + k]`. Bytes past the end of the input
//! read as zero, which is a control byte: a string scan stops there at `input.len()`,
//! and whitespace skipping stops there too, exactly where the byte-wise scans stop.

#[cfg(target_arch = "x86_64")]
pub(super) use imp::Window;

#[cfg(target_arch = "x86_64")]
mod imp {

/// Masks for the 64 bytes at `base`.
#[derive(Clone, Copy)]
pub(crate) struct Window {
    base: usize,
    /// `"`, `\` or a control byte below 0x20.
    special: u64,
    /// Bytes of 0x80 and above.
    high: u64,
    /// Bytes that are not JSON whitespace (space, tab, CR, LF).
    nonws: u64,
}

impl Window {
    /// A window that covers nothing: the first query reloads it.
    pub(crate) const EMPTY: Window = Window {
        base: usize::MAX / 2,
        special: 0,
        high: 0,
        nonws: 0,
    };

    #[inline(always)]
    fn load(input: &[u8], base: usize) -> Window {
        let (special, high, nonws) = match input.get(base..).and_then(|s| s.first_chunk::<64>()) {
            Some(block) => classify(block),
            None => {
                let mut tail = [0u8; 64];
                if let Some(rest) = input.get(base..) {
                    tail[..rest.len()].copy_from_slice(rest);
                }
                classify(&tail)
            }
        };
        Window {
            base,
            special,
            high,
            nonws,
        }
    }

    /// Index of the first byte at or after `i` that is not JSON whitespace, or
    /// `input.len()` (or `i`, when `i` is already past it) when only whitespace remains.
    #[inline(always)]
    pub(crate) fn skip_ws(&mut self, input: &[u8], mut i: usize) -> usize {
        if !matches!(input.get(i), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            return i;
        }
        loop {
            let mut off = i.wrapping_sub(self.base);
            if off >= 64 {
                *self = Window::load(input, i);
                off = 0;
            }
            let rest = self.nonws >> off;
            if rest != 0 {
                return i + rest.trailing_zeros() as usize;
            }
            // Whitespace to the end of the window: continue in the next one.
            i = self.base + 64;
        }
    }

    /// The first `"`, `\` or control byte at or after `from` (`input.len()` when there is
    /// none), and whether every byte before it is ASCII.
    #[inline(always)]
    pub(crate) fn find_special_ascii(&mut self, input: &[u8], mut from: usize) -> (usize, bool) {
        let mut ascii = true;
        loop {
            let mut off = from.wrapping_sub(self.base);
            if off >= 64 {
                *self = Window::load(input, from);
                off = 0;
            }
            let special = self.special >> off;
            let high = self.high >> off;
            if special != 0 {
                let lane = special.trailing_zeros();
                let before = (1u64 << lane) - 1;
                return (from + lane as usize, ascii && high & before == 0);
            }
            ascii &= high == 0;
            from = self.base + 64;
        }
    }
}

#[cfg(target_feature = "avx512bw")]
#[inline(always)]
fn classify(block: &[u8; 64]) -> (u64, u64, u64) {
    use core::arch::x86_64::*;
    // SAFETY: AVX-512BW is enabled at compile time and the load reads exactly the 64 bytes of `block`.
    unsafe {
        let v = _mm512_loadu_si512(block.as_ptr().cast());
        let special = _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'"' as i8))
            | _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'\\' as i8))
            | _mm512_cmplt_epu8_mask(v, _mm512_set1_epi8(0x20));
        let high = _mm512_movepi8_mask(v);
        let ws = _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b' ' as i8))
            | _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'\n' as i8))
            | _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'\r' as i8))
            | _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'\t' as i8));
        (special, high, !ws)
    }
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx2", not(target_feature = "avx512bw")))]
#[inline(always)]
fn classify(block: &[u8; 64]) -> (u64, u64, u64) {
    use core::arch::x86_64::*;
    // SAFETY: AVX2 is enabled at compile time and each load reads 32 bytes inside `block`.
    unsafe {
        let quote = _mm256_set1_epi8(b'"' as i8);
        let slash = _mm256_set1_epi8(b'\\' as i8);
        let limit = _mm256_set1_epi8(0x1f);
        let space = _mm256_set1_epi8(b' ' as i8);
        let lf = _mm256_set1_epi8(b'\n' as i8);
        let cr = _mm256_set1_epi8(b'\r' as i8);
        let tab = _mm256_set1_epi8(b'\t' as i8);
        let half = |at: usize| {
            let v = _mm256_loadu_si256(block.as_ptr().add(at).cast());
            let special = _mm256_or_si256(
                _mm256_or_si256(_mm256_cmpeq_epi8(v, quote), _mm256_cmpeq_epi8(v, slash)),
                _mm256_cmpeq_epi8(_mm256_min_epu8(v, limit), v),
            );
            let ws = _mm256_or_si256(
                _mm256_or_si256(_mm256_cmpeq_epi8(v, space), _mm256_cmpeq_epi8(v, lf)),
                _mm256_or_si256(_mm256_cmpeq_epi8(v, cr), _mm256_cmpeq_epi8(v, tab)),
            );
            (
                _mm256_movemask_epi8(special) as u32,
                _mm256_movemask_epi8(v) as u32,
                _mm256_movemask_epi8(ws) as u32,
            )
        };
        let (s0, h0, w0) = half(0);
        let (s1, h1, w1) = half(32);
        (
            u64::from(s0) | u64::from(s1) << 32,
            u64::from(h0) | u64::from(h1) << 32,
            !(u64::from(w0) | u64::from(w1) << 32),
        )
    }
}

#[cfg(all(target_arch = "x86_64", not(target_feature = "avx2")))]
#[inline(always)]
fn classify(block: &[u8; 64]) -> (u64, u64, u64) {
    use core::arch::x86_64::*;
    // SAFETY: SSE2 is baseline on x86_64 and each load reads 16 bytes inside `block`.
    unsafe {
        let quote = _mm_set1_epi8(b'"' as i8);
        let slash = _mm_set1_epi8(b'\\' as i8);
        let limit = _mm_set1_epi8(0x1f);
        let space = _mm_set1_epi8(b' ' as i8);
        let lf = _mm_set1_epi8(b'\n' as i8);
        let cr = _mm_set1_epi8(b'\r' as i8);
        let tab = _mm_set1_epi8(b'\t' as i8);
        let quarter = |at: usize| {
            let v = _mm_loadu_si128(block.as_ptr().add(at).cast());
            let special = _mm_or_si128(
                _mm_or_si128(_mm_cmpeq_epi8(v, quote), _mm_cmpeq_epi8(v, slash)),
                _mm_cmpeq_epi8(_mm_min_epu8(v, limit), v),
            );
            let ws = _mm_or_si128(
                _mm_or_si128(_mm_cmpeq_epi8(v, space), _mm_cmpeq_epi8(v, lf)),
                _mm_or_si128(_mm_cmpeq_epi8(v, cr), _mm_cmpeq_epi8(v, tab)),
            );
            (
                _mm_movemask_epi8(special) as u64,
                _mm_movemask_epi8(v) as u64,
                _mm_movemask_epi8(ws) as u64,
            )
        };
        let (s0, h0, w0) = quarter(0);
        let (s1, h1, w1) = quarter(16);
        let (s2, h2, w2) = quarter(32);
        let (s3, h3, w3) = quarter(48);
        (
            s0 | s1 << 16 | s2 << 32 | s3 << 48,
            h0 | h1 << 16 | h2 << 32 | h3 << 48,
            !(w0 | w1 << 16 | w2 << 32 | w3 << 48),
        )
    }
}
}

/// Other architectures keep the byte-wise scanners: this window caches nothing.
#[cfg(not(target_arch = "x86_64"))]
#[derive(Clone, Copy)]
pub(super) struct Window;

#[cfg(not(target_arch = "x86_64"))]
impl Window {
    pub(super) const EMPTY: Window = Window;

    #[inline(always)]
    pub(super) fn skip_ws(&mut self, input: &[u8], i: usize) -> usize {
        super::skip_ws(input, i)
    }

    #[inline(always)]
    pub(super) fn find_special_ascii(&mut self, input: &[u8], from: usize) -> (usize, bool) {
        super::find_special_ascii(input, from)
    }
}
