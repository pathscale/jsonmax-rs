use super::avx;
use super::bit;
use super::error::ErrorKind;
use super::result::Result;
use super::utf8::{BACKSLASH, COLON, LEFT_BRACE, QUOTE, RIGHT_BRACE};
#[cfg(any(feature = "portable", not(any(target_arch = "aarch64", target_arch = "x86_64"))))]
use emulated::{m256i, mm256_cmpeq_epi8, mm256_movemask_epi8};
#[cfg(all(target_arch = "aarch64", not(feature = "portable")))]
use neon::{m256i, mm256_cmpeq_epi8, mm256_movemask_epi8};
#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
use x86::{m256i, mm256_cmpeq_epi8, mm256_movemask_epi8};

#[derive(Debug)]
pub struct IndexBuilder {
    backslash: m256i,
    quote: m256i,
    colon: m256i,
    left_brace: m256i,
    right_brace: m256i,

    b_backslash: Vec<u64>,
    pub(crate) b_quote: Vec<u64>,
    b_colon: Vec<u64>,
    b_left: Vec<u64>,
    b_right: Vec<u64>,
    b_string_mask: Vec<u64>,

    s_left: Vec<(usize, u64)>,

    pub(crate) index: Vec<Vec<u64>>,
    depth: usize,
}

impl IndexBuilder {
    pub fn new(depth: usize) -> Self {
        let index = vec![Vec::new(); depth];

        Self {
            backslash: avx::mm256i(BACKSLASH as i8),
            quote: avx::mm256i(QUOTE as i8),
            colon: avx::mm256i(COLON as i8),
            left_brace: avx::mm256i(LEFT_BRACE as i8),
            right_brace: avx::mm256i(RIGHT_BRACE as i8),

            b_backslash: Vec::new(),
            b_quote: Vec::new(),
            b_colon: Vec::new(),
            b_left: Vec::new(),
            b_right: Vec::new(),
            b_string_mask: Vec::new(),

            s_left: Vec::new(),

            index,
            depth,
        }
    }

    #[inline(always)]
    pub fn build_structural_indices(&mut self, rec: &[u8]) -> Result<()> {
        let b_len = (rec.len() + 63) / 64;

        self.b_backslash.clear();
        self.b_quote.clear();
        self.b_colon.clear();
        self.b_left.clear();
        self.b_right.clear();
        self.b_string_mask.clear();
        for b in &mut self.index {
            b.clear();
        }

        if b_len > self.b_backslash.capacity() {
            self.b_backslash.reserve_exact(b_len);
            self.b_quote.reserve_exact(b_len);
            self.b_colon.reserve_exact(b_len);
            self.b_left.reserve_exact(b_len);
            self.b_right.reserve_exact(b_len);
            self.b_string_mask.reserve_exact(b_len);
            for b in self.index.iter_mut() {
                b.reserve_exact(b_len);
            }
        }

        build_structural_character_bitmap(
            rec,
            &mut self.b_backslash,
            &mut self.b_quote,
            &mut self.b_colon,
            &mut self.b_left,
            &mut self.b_right,
            &self.backslash,
            &self.quote,
            &self.colon,
            &self.left_brace,
            &self.right_brace,
        );

        build_structural_quote_bitmap(&self.b_backslash, &mut self.b_quote);

        build_string_mask_bitmap(&self.b_quote, &mut self.b_string_mask);

        for (i, b) in self.b_string_mask.iter().enumerate() {
            self.b_colon[i] &= *b;
            self.b_left[i] &= *b;
            self.b_right[i] &= *b;
        }

        build_leveled_colon_bitmap(
            &self.b_colon,
            &self.b_left,
            &self.b_right,
            self.depth,
            &mut self.s_left,
            &mut self.index,
        )
    }
}


#[inline]
pub fn build_structural_character_bitmap(s: &[u8], b_backslash: &mut Vec<u64>, b_quote: &mut Vec<u64>, b_colon: &mut Vec<u64>, b_left: &mut Vec<u64>, b_right: &mut Vec<u64>, m_backslash: &m256i, m_quote: &m256i, m_colon: &m256i, m_left: &m256i, m_right: &m256i) {
    // Check AVX2 once, then classify directly from each full input block.
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    if std::arch::is_x86_feature_detected!("avx2") {
        let needles = [m_backslash, m_quote, m_colon, m_left, m_right];
        for chunk in s.chunks(64) {
            let mut tail = [0u8; 64];
            let block = if chunk.len() == 64 {
                chunk
            } else {
                tail[..chunk.len()].copy_from_slice(chunk);
                &tail[..]
            };
            let masks = unsafe { ::x86::classify64(block, needles) };
            let valid = if chunk.len() == 64 { !0 } else { (1u64 << chunk.len()) - 1 };
            b_backslash.push(masks[0] & valid);
            b_quote.push(masks[1] & valid);
            b_colon.push(masks[2] & valid);
            b_left.push(masks[3] & valid);
            b_right.push(masks[4] & valid);
        }
        return;
    }
    let n = s.len();
    let mut i = 0;
    while i + 63 < n {
        let s1 = unsafe { avx::u8_to_m256i(s, i) };
        let s2 = unsafe { avx::u8_to_m256i(s, i + 32) };

        b_backslash.push(mbitmap(&s1, &s2, m_backslash));
        b_quote.push(mbitmap(&s1, &s2, m_quote));
        b_colon.push(mbitmap(&s1, &s2, m_colon));
        b_left.push(mbitmap(&s1, &s2, m_left));
        b_right.push(mbitmap(&s1, &s2, m_right));

        i += 64;
    }

    if i + 32 < n {
        let s1 = unsafe { avx::u8_to_m256i(s, i) };
        let s2 = unsafe { avx::u8_to_m256i_rest(s, i + 32) };

        b_backslash.push(mbitmap(&s1, &s2, m_backslash));
        b_quote.push(mbitmap(&s1, &s2, m_quote));
        b_colon.push(mbitmap(&s1, &s2, m_colon));
        b_left.push(mbitmap(&s1, &s2, m_left));
        b_right.push(mbitmap(&s1, &s2, m_right));
    } else if i + 32 == n {
        let s1 = unsafe { avx::u8_to_m256i(s, i) };

        b_backslash.push(mbitmap_partial(&s1, m_backslash));
        b_quote.push(mbitmap_partial(&s1, m_quote));
        b_colon.push(mbitmap_partial(&s1, m_colon));
        b_left.push(mbitmap_partial(&s1, m_left));
        b_right.push(mbitmap_partial(&s1, m_right));
    } else if i < n {
        let s1 = unsafe { avx::u8_to_m256i_rest(s, i) };

        b_backslash.push(mbitmap_partial(&s1, m_backslash));
        b_quote.push(mbitmap_partial(&s1, m_quote));
        b_colon.push(mbitmap_partial(&s1, m_colon));
        b_left.push(mbitmap_partial(&s1, m_left));
        b_right.push(mbitmap_partial(&s1, m_right));
    }
}

#[inline]
fn mbitmap(s1: &m256i, s2: &m256i, m: &m256i) -> u64 {
    let i1 = mm256_movemask_epi8(mm256_cmpeq_epi8(*s1, *m));
    let i2 = mm256_movemask_epi8(mm256_cmpeq_epi8(*s2, *m));
    u64::from(i1 as u32) | (u64::from(i2 as u32) << 32)
}

#[inline]
fn mbitmap_partial(s: &m256i, m: &m256i) -> u64 {
    u64::from(mm256_movemask_epi8(mm256_cmpeq_epi8(*s, *m)) as u32)
}


#[inline]
pub fn build_structural_quote_bitmap(b_backslash: &[u64], b_quote: &mut Vec<u64>) {
    assert_eq!(b_backslash.len(), b_quote.len());
    let mut incoming_odd = false;
    for (&slashes, quotes) in b_backslash.iter().zip(b_quote.iter_mut()) {
        // Run starts on alternating bit positions. Addition carries through each
        // run to its first non-backslash byte. Bit zero may be an escaped
        // follower from the preceding block; suppress its new run start.
        let starts = slashes & !(slashes << 1);
        let even = 0x5555_5555_5555_5555u64;
        let even_starts = starts & even & !(incoming_odd as u64);
        let odd_starts = (starts & !even) | ((slashes & 1) & incoming_odd as u64);
        let even_carries = slashes.wrapping_add(even_starts);
        let odd_carries = slashes.wrapping_add(odd_starts);
        let escaped = ((even_carries & !even) | (odd_carries & even)) & !slashes
            | incoming_odd as u64;
        *quotes &= !escaped;
        let trailing = slashes.leading_ones();
        incoming_odd = if trailing == 64 { incoming_odd } else { trailing & 1 != 0 };
    }
}

#[inline]
pub fn build_string_mask_bitmap(b_quote: &[u64], b_string_mask: &mut Vec<u64>) {
    let mut inside = false;
    for &quotes in b_quote {
        let prefix = prefix_xor(quotes);
        // Preserve upstream convention: opening quotes count as outside,
        // closing quotes count as inside. Structural bytes cannot be quotes.
        let incoming = if inside { !0u64 } else { 0 };
        b_string_mask.push(!(prefix ^ quotes ^ incoming));
        inside ^= quotes.count_ones() & 1 != 0;
    }
}

#[inline]
fn prefix_xor(bits: u64) -> u64 {
    #[cfg(all(target_arch = "aarch64", not(feature = "portable")))]
    if std::arch::is_aarch64_feature_detected!("aes") {
        // AES feature includes PMULL on this Rust target. Runtime checked.
        return unsafe { ::neon::prefix_xor(bits) };
    }
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    if std::arch::is_x86_feature_detected!("pclmulqdq") {
        return unsafe { ::x86::prefix_xor(bits) };
    }
    prefix_xor_scalar(bits)
}

#[inline]
fn prefix_xor_scalar(mut bits: u64) -> u64 {
    bits ^= bits << 1;
    bits ^= bits << 2;
    bits ^= bits << 4;
    bits ^= bits << 8;
    bits ^= bits << 16;
    bits ^ (bits << 32)
}

#[inline]
pub fn build_leveled_colon_bitmap(b_colon: &[u64], b_left: &[u64], b_right: &[u64], l: usize, s_left: &mut Vec<(usize, u64)>, index: &mut Vec<Vec<u64>>) -> Result<()> {
    for b in index.iter_mut() {
        b.extend(b_colon);
    }
    s_left.clear();
    for i in 0..b_right.len() {
        let mut m_left = b_left[i];
        let mut m_right = b_right[i];
        loop {
            let m_rightbit = bit::e(m_right);
            let mut m_leftbit = bit::e(m_left);
            while m_leftbit != 0 && (m_rightbit == 0 || m_leftbit < m_rightbit) {
                s_left.push((i, m_leftbit));
                m_left = bit::r(m_left);
                m_leftbit = bit::e(m_left);
            }
            if m_rightbit != 0 {
                let (j, mlb) = s_left.pop().ok_or_else(|| ErrorKind::InvalidRecord)?;
                m_leftbit = mlb;
                if s_left.len() > 0 {
                    let upper_l = s_left.len() - 1;
                    if upper_l < l {
                        if i == j {
                            index[upper_l][i] &= !(m_rightbit.wrapping_sub(m_leftbit));
                        } else {
                            index[upper_l][j] &= m_leftbit.wrapping_sub(1);
                            index[upper_l][i] &= !(m_rightbit.wrapping_sub(1));
                            for k in j + 1..i {
                                index[upper_l][k] = 0
                            }
                        }
                    }
                }
            }
            m_right = bit::r(m_right);
            if m_rightbit == 0 {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::avx;
    use super::super::utf8::QUOTE;

    #[test]
    fn test_build_structural_character_bitmap() {
        let c = QUOTE;
        let m = avx::mm256i(c as i8);

        macro_rules! s {
            ($( [ $c:expr ; $n:expr ] ),*) => {{
                let mut v = Vec::new();
                $( v.extend_from_slice(&[$c; $n]); )*
                v
            }}
        }

        struct TestCase {
            s: Vec<u8>,
            d: Vec<u64>,
        }
        #[allow(overflowing_literals)]
        let test_cases = vec![
            TestCase {
                s: vec![],
                d: vec![],
            },
            TestCase {
                s: vec![0xff; 32],
                d: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                s: s!([c; 1], [0xff; 31]),
                d: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001,
                ],
            },
            TestCase {
                s: s!([0xff; 1], [c; 1], [0xff; 30]),
                d: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000010,
                ],
            },
            TestCase {
                s: s!([0xff; 30], [c; 1], [0xff; 1]),
                d: vec![
                    0b00000000_00000000_00000000_00000000_01000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                s: s!([0xff; 31], [c; 1]),
                d: vec![
                    0b00000000_00000000_00000000_00000000_10000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                s: vec![c; 32],
                d: vec![
                    0b00000000_00000000_00000000_00000000_11111111_11111111_11111111_11111111,
                ],
            },
            TestCase {
                s: vec![0xff; 64],
                d: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                s: s!([c; 1], [0xff; 63]),
                d: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001,
                ],
            },
            TestCase {
                s: s!([0xff; 8], [c; 1], [0xff; 55]),
                d: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000001_00000000,
                ],
            },
            TestCase {
                s: s!([0xff; 16], [c; 1], [0xff; 47]),
                d: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000001_00000000_00000000,
                ],
            },
            TestCase {
                s: s!([0xff; 24], [c; 1], [0xff; 39]),
                d: vec![
                    0b00000000_00000000_00000000_00000000_00000001_00000000_00000000_00000000,
                ],
            },
            TestCase {
                s: s!([0xff; 32], [c; 1], [0xff; 31]),
                d: vec![
                    0b00000000_00000000_00000000_00000001_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                s: s!([0xff; 40], [c; 1], [0xff; 23]),
                d: vec![
                    0b00000000_00000000_00000001_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                s: s!([0xff; 48], [c; 1], [0xff; 15]),
                d: vec![
                    0b00000000_00000001_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                s: s!([0xff; 56], [c; 1], [0xff; 7]),
                d: vec![
                    0b00000001_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                s: s!([0xff; 63], [c; 1]),
                d: vec![
                    0b10000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                s: s!([c; 32], [0xff; 32]),
                d: vec![
                    0b00000000_00000000_00000000_00000000_11111111_11111111_11111111_11111111,
                ],
            },
            TestCase {
                s: s!([0xff; 32], [c; 32]),
                d: vec![
                    0b11111111_11111111_11111111_11111111_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                s: vec![c; 64],
                d: vec![
                    0b11111111_11111111_11111111_11111111_11111111_11111111_11111111_11111111,
                ],
            },
            TestCase {
                s: vec![0xff; 96],
                d: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                s: s!([c; 1], [0xff; 95]),
                d: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                s: s!([0xff; 17], [c; 1], [0xff; 78]),
                d: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000010_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                s: s!([0xff; 31], [c; 1], [0xff; 64]),
                d: vec![
                    0b00000000_00000000_00000000_00000000_10000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                s: s!([0xff; 32], [c; 1], [0xff; 63]),
                d: vec![
                    0b00000000_00000000_00000000_00000001_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                s: s!([0xff; 45], [c; 1], [0xff; 50]),
                d: vec![
                    0b00000000_00000000_00100000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                s: s!([0xff; 63], [c; 1], [0xff; 32]),
                d: vec![
                    0b10000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                s: s!([0xff; 64], [c; 1], [0xff; 31]),
                d: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001,
                ],
            },
            TestCase {
                s: s!([0xff; 73], [c; 1], [0xff; 22]),
                d: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000010_00000000,
                ],
            },
            TestCase {
                s: s!([0xff; 83], [c; 1], [0xff; 12]),
                d: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00001000_00000000_00000000,
                ],
            },
            TestCase {
                s: s!([0xff; 95], [c; 1]),
                d: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_10000000_00000000_00000000_00000000,
                ],
            },
        ];
        for t in test_cases {
            let mut d = Vec::with_capacity((t.s.len() + 1) / 2);
            build_structural_character_bitmap(
                &t.s,
                &mut d,
                &mut vec![],
                &mut vec![],
                &mut vec![],
                &mut vec![],
                &m,
                &m,
                &m,
                &m,
                &m,
            );
            assert_eq!(t.d, d);
        }
    }

    #[test]
    fn test_build_structural_quote_bitmap() {
        struct TestCase {
            b_backslash: Vec<u64>,
            b_quote: Vec<u64>,
            want: Vec<u64>,
        }
        let test_cases = vec![
            TestCase {
                b_backslash: vec![],
                b_quote: vec![],
                want: vec![],
            },
            TestCase {
                b_backslash: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000010,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000010,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000100_00000000,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00001000_00000000,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000110_00000000,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00001000_00000000,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00001000_00000000,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000111_00000000,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00001000_00000000,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000111_10000000,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00001000_00000000,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00001000_00000000,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000111_11000000,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00001000_00000000,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000111_11100000,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00001000_00000000,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00001000_00000000,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b00000000_00000110_00000000_00000000_00000000_00100000_00000000_00000000,
                    0b00000000_00000000_00011110_00000000_00000000_00000000_00001110_00000000,
                ],
                b_quote: vec![
                    0b00000000_00001000_00000000_00000000_00100000_01000000_00000000_00000010,
                    0b00000000_00000000_00100000_00000000_00000000_00000000_00010000_00001000,
                ],
                want: vec![
                    0b00000000_00001000_00000000_00000000_00100000_00000000_00000000_00000010,
                    0b00000000_00000000_00100000_00000000_00000000_00000000_00000000_00001000,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b10000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b11000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b11100000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b11110000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000010,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b10000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000010,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000010,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b11000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000010,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b11100000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000010,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000010,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b10001111_10000001_11111110_00000000_00000000_00000000_00000000_00000000,
                    0b10000000_00000000_00000000_00000111_00011110_00000000_00000000_00000001,
                    0b00000000_00000000_00000000_00000000_00000000_01100000_00100000_00000000,
                ],
                b_quote: vec![
                    0b00010000_00000010_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00001000_00100000_00000000_00000000_00000010,
                    0b00000000_00000000_00000000_00000000_00000000_10000000_01000000_00000001,
                ],
                want: vec![
                    0b00000000_00000010_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00100000_00000000_00000000_00000010,
                    0b00000000_00000000_00000000_00000000_00000000_10000000_00000000_00000000,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b10000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b11111111_11111111_11111111_11111111_11111111_11111111_11111111_11111111,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000010,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000010,
                ],
            },
            TestCase {
                b_backslash: vec![
                    0b11000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b11111111_11111111_11111111_11111111_11111111_11111111_11111111_11111111,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001,
                ],
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000010,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
        ];
        for t in test_cases {
            let mut b_quote = t.b_quote.clone();
            build_structural_quote_bitmap(&t.b_backslash, &mut b_quote);
            assert_eq!(t.want, b_quote);
        }
    }

    #[test]
    fn test_build_string_mask_bitmap() {
        struct TestCase {
            b_quote: Vec<u64>,
            want: Vec<u64>,
        }
        let test_cases = vec![
            TestCase {
                b_quote: vec![],
                want: vec![],
            },
            TestCase {
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                want: vec![
                    0b11111111_11111111_11111111_11111111_11111111_11111111_11111111_11111111,
                ],
            },
            TestCase {
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000100,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000111,
                ],
            },
            TestCase {
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_01000000_00000100,
                ],
                want: vec![
                    0b11111111_11111111_11111111_11111111_11111111_11111111_10000000_00000111,
                ],
            },
            TestCase {
                b_quote: vec![
                    0b00000000_00000000_00000000_00000010_00000000_00000000_01000000_00000100,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000011_11111111_11111111_10000000_00000111,
                ],
            },
            TestCase {
                b_quote: vec![
                    0b00000000_01000000_00000000_00000010_00000000_00000000_01000000_00000100,
                ],
                want: vec![
                    0b11111111_10000000_00000000_00000011_11111111_11111111_10000000_00000111,
                ],
            },
            TestCase {
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                want: vec![
                    0b11111111_11111111_11111111_11111111_11111111_11111111_11111111_11111111,
                    0b11111111_11111111_11111111_11111111_11111111_11111111_11111111_11111111,
                ],
            },
            TestCase {
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000100,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000111,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                b_quote: vec![
                    0b00010000_00000000_00000000_00000000_00000000_00000000_00010000_00100000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                want: vec![
                    0b00011111_11111111_11111111_11111111_11111111_11111111_11100000_00111111,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                b_quote: vec![
                    0b00010000_00000000_00000000_00000000_00000000_00000000_00010000_00100000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00001000,
                ],
                want: vec![
                    0b00011111_11111111_11111111_11111111_11111111_11111111_11100000_00111111,
                    0b11111111_11111111_11111111_11111111_11111111_11111111_11111111_11110000,
                ],
            },
            TestCase {
                b_quote: vec![
                    0b00010000_00000000_00000000_00000000_00000000_00000000_00010000_00100000,
                    0b00000000_00000000_00000000_00000000_10000000_00000000_00000000_00001000,
                ],
                want: vec![
                    0b00011111_11111111_11111111_11111111_11111111_11111111_11100000_00111111,
                    0b00000000_00000000_00000000_00000000_11111111_11111111_11111111_11110000,
                ],
            },
            TestCase {
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                want: vec![
                    0b11111111_11111111_11111111_11111111_11111111_11111111_11111111_11111111,
                    0b11111111_11111111_11111111_11111111_11111111_11111111_11111111_11111111,
                    0b11111111_11111111_11111111_11111111_11111111_11111111_11111111_11111111,
                ],
            },
            TestCase {
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000001_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000001_11111111,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
            },
            TestCase {
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000001_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00100000_00000000_00000000,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000001_11111111,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b11111111_11111111_11111111_11111111_11111111_11000000_00000000_00000000,
                ],
            },
            TestCase {
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000001_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00010000_00000000_00000000_00100000_00000000_00000000,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000001_11111111,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00000000_00000000_00011111_11111111_11111111_11000000_00000000_00000000,
                ],
            },
            TestCase {
                b_quote: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000001_00000000,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b00001000_00000000_00010000_00000000_00000000_00100000_00000000_00000000,
                ],
                want: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000001_11111111,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    0b11110000_00000000_00011111_11111111_11111111_11000000_00000000_00000000,
                ],
            },
        ];
        for t in test_cases {
            let mut b_string_mask = Vec::with_capacity(t.b_quote.len());
            build_string_mask_bitmap(&t.b_quote, &mut b_string_mask);
            assert_eq!(t.want, b_string_mask);
        }
    }

    #[test]
    fn test_build_leveled_colon_bitmap() {
        struct TestCase {
            b_colon: Vec<u64>,
            b_left: Vec<u64>,
            b_right: Vec<u64>,
            l: usize,
            want: Vec<Vec<u64>>,
        }
        let test_cases = vec![
            TestCase {
                b_colon: vec![],
                b_left: vec![],
                b_right: vec![],
                l: 1,
                want: vec![vec![]],
            },
            TestCase {
                b_colon: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                b_left: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                b_right: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                l: 1,
                want: vec![
                    vec![
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                    ],
                ],
            },
            TestCase {
                b_colon: vec![
                    0b00000000_00010000_00000000_00000000_00010000_00000000_00000100_00000000,
                ],
                b_left: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001,
                ],
                b_right: vec![
                    0b10000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                l: 1,
                want: vec![
                    vec![
                        0b00000000_00010000_00000000_00000000_00010000_00000000_00000100_00000000,
                    ],
                ],
            },
            TestCase {
                b_colon: vec![
                    0b00000000_00010000_00000000_00000000_00010000_00000000_00000100_00000000,
                ],
                b_left: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001,
                ],
                b_right: vec![
                    0b10000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000,
                ],
                l: 2,
                want: vec![
                    vec![
                        0b00000000_00010000_00000000_00000000_00010000_00000000_00000100_00000000,
                    ],
                    vec![
                        0b00000000_00010000_00000000_00000000_00010000_00000000_00000100_00000000,
                    ],
                ],
            },
            TestCase {
                b_colon: vec![
                    0b00000000_00010000_00000000_00000000_00010000_00000000_00000100_00000000,
                ],
                b_left: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00001000_00000000_00000001,
                ],
                b_right: vec![
                    0b10000000_00000000_01000000_00000000_00000000_00000000_00000000_00000000,
                ],
                l: 1,
                want: vec![
                    vec![
                        0b00000000_00010000_00000000_00000000_00000000_00000000_00000100_00000000,
                    ],
                ],
            },
            TestCase {
                b_colon: vec![
                    0b00000000_00010000_00000000_00000000_00010000_00000000_00000100_00000000,
                ],
                b_left: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00001000_00000000_00000001,
                ],
                b_right: vec![
                    0b10000000_00000000_01000000_00000000_00000000_00000000_00000000_00000000,
                ],
                l: 2,
                want: vec![
                    vec![
                        0b00000000_00010000_00000000_00000000_00000000_00000000_00000100_00000000,
                    ],
                    vec![
                        0b00000000_00010000_00000000_00000000_00010000_00000000_00000100_00000000,
                    ],
                ],
            },
            TestCase {
                b_colon: vec![
                    0b00001000_00001000_00001000_00000000_00000000_00010000_00010000_00010000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000001_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                ],
                b_left: vec![
                    0b00000000_00000000_00000000_00000000_00000001_00000001_00000001_00000001u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                ],
                b_right: vec![
                    0b10000000_10000000_10000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b10000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                ],
                l: 3,
                want: vec![
                    vec![
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00010000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000001_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    ],
                    vec![
                        0b00001000_00000000_00000000_00000000_00000000_00000000_00010000_00010000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000001_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    ],
                    vec![
                        0b00001000_00001000_00000000_00000000_00000000_00010000_00010000_00010000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000001_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    ],
                ],
            },
            TestCase {
                b_colon: vec![
                    0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                ],
                b_left: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000001u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                ],
                b_right: vec![
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b10000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b10000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b10000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    0b10000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                ],
                l: 3,
                want: vec![
                    vec![
                        0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    ],
                    vec![
                        0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    ],
                    vec![
                        0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_00000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                        0b00000000_10000000_00000000_00000000_00000000_00000000_00000000_00000000u64,
                    ],
                ],
            },
        ];
        for t in test_cases {
            let mut index = vec![Vec::new(); t.l];
            let mut s_left = Vec::new();
            let r = build_leveled_colon_bitmap(
                &t.b_colon,
                &t.b_left,
                &t.b_right,
                t.l,
                &mut s_left,
                &mut index,
            );
            assert_eq!(Ok(()), r);
            assert_eq!(t.want, index);
        }
    }
}

#[cfg(test)]
mod prefix_tests {
    use super::{prefix_xor, prefix_xor_scalar};

    #[test]
    fn dispatched_prefix_matches_portable_fallback() {
        let mut state = 0xfedc_ba98_7654_3210u64;
        for bits in (0..64).map(|i| 1u64 << i).chain([0, !0u64].iter().cloned()) {
            assert_eq!(prefix_xor(bits), prefix_xor_scalar(bits));
        }
        for _ in 0..10000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            assert_eq!(prefix_xor(state), prefix_xor_scalar(state));
        }
    }
}
