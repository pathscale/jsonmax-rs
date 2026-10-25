//! Allocation-free JSON serialization with bounded SIMD escape-run discovery.
//!
//! Measurement and emission share their spelling rules. The complete append
//! length is checked and reserved before any output, including wrapper quotes.

use super::{
    ByteSink, Error, ErrorCode, ErrorSource, EscapePolicy, Result, BACKSLASH, MAX_ESCAPE_BYTES,
    QUOTE,
};

/// Find the first byte requiring escaping under `policy`.
///
/// Accepts arbitrary bytes, including invalid UTF-8, without narrowing the
/// returned slice-relative index. Quotes, backslashes, and controls below
/// `0x20` always match. Solidus and non-ASCII bytes match only when requested.
/// SIMD loads stay inside the supplied slice; short tails use scalar discovery.
#[inline]
pub fn find_escape(bytes: &[u8], policy: EscapePolicy) -> Option<usize> {
    EscapeFinder::new(bytes.len(), policy).find(bytes)
}

/// Measure serialized content, excluding wrapper quotes.
///
/// Input or output exceeding `u32::MAX` is `PayloadTooLarge` from `Encode` at
/// `u32::MAX`. Non-ASCII UTF-8 is preserved unless `ascii_only` requests UTF-16
/// escape spelling. No normalization or allocation is performed.
pub fn escaped_length(text: &str, policy: EscapePolicy) -> Result<u32> {
    checked_size(text.len())?;
    let finder = EscapeFinder::new(text.len(), policy);
    measure(text, &finder)
}

/// Append serialized content without wrapper quotes.
///
/// Checks the entire length, then reserves exactly that append size before
/// writing. Sink errors propagate unchanged and may leave an emitted prefix.
pub fn write_escaped(text: &str, policy: EscapePolicy, sink: &mut dyn ByteSink) -> Result<()> {
    write(text, policy, sink, false)
}

/// Append a complete JSON string, including its double quotes.
///
/// The content and both quotes must fit the 32-bit ABI length. All size checks
/// and the full reservation precede even the opening quote. A sink write error
/// can leave a prefix, including an unmatched opening quote.
pub fn write_string(text: &str, policy: EscapePolicy, sink: &mut dyn ByteSink) -> Result<()> {
    write(text, policy, sink, true)
}

fn write(text: &str, policy: EscapePolicy, sink: &mut dyn ByteSink, quoted: bool) -> Result<()> {
    checked_size(text.len())?;
    // Detect once for both passes, rather than once for every emitted escape.
    let finder = EscapeFinder::new(text.len(), policy);
    let content_len = measure(text, &finder)?;
    reserve_append(content_len, quoted, sink)?;
    if quoted {
        sink.write_byte(QUOTE)?;
    }
    visit_runs(text, &finder, |bytes| sink.write_bytes(bytes))?;
    if quoted {
        sink.write_byte(QUOTE)?;
    }
    Ok(())
}

fn size_error() -> Error {
    Error::new(
        ErrorCode::PayloadTooLarge,
        ErrorSource::Encode,
        u32::MAX,
        "serialized string exceeds the 32-bit ABI length limit",
    )
}

fn checked_size(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| size_error())
}

fn checked_add_length(length: u32, additional: usize) -> Result<u32> {
    length
        .checked_add(checked_size(additional)?)
        .ok_or_else(size_error)
}

fn reserve_append(content_len: u32, quoted: bool, sink: &mut dyn ByteSink) -> Result<()> {
    let append_len = checked_add_length(content_len, if quoted { 2 } else { 0 })?;
    // Both operands originated in real slice lengths or small escape widths.
    // On 32-bit hosts every u32 value is representable as usize as well.
    sink.reserve(append_len as usize)
}

fn measure(text: &str, finder: &EscapeFinder) -> Result<u32> {
    let mut length = 0;
    visit_runs(text, finder, |bytes| {
        length = checked_add_length(length, bytes.len())?;
        Ok(())
    })?;
    Ok(length)
}

/// Visit nonempty borrowed runs or stack-encoded scalars in output order.
fn visit_runs(
    mut text: &str,
    finder: &EscapeFinder,
    mut visit: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let mut encoded = [0; MAX_ESCAPE_BYTES];
    while !text.is_empty() {
        let run_len = finder.find(text.as_bytes()).unwrap_or(text.len());
        if run_len != 0 {
            visit(&text.as_bytes()[..run_len])?;
            // Without ascii_only, every matching byte is ASCII. With it, the
            // first non-ASCII match in valid UTF-8 is a sequence's leading byte.
            // Consequently discovery always ends this run at a char boundary.
            text = &text[run_len..];
        }
        if let Some(scalar) = text.chars().next() {
            let len = encode_scalar(scalar, finder.policy, &mut encoded);
            visit(&encoded[..len])?;
            text = &text[scalar.len_utf8()..];
        }
    }
    Ok(())
}

/// Encode a scalar with the same rules used by both public output functions.
fn encode_scalar(scalar: char, policy: EscapePolicy, output: &mut [u8; MAX_ESCAPE_BYTES]) -> usize {
    let short = match scalar {
        '"' => Some(QUOTE),
        '\\' => Some(BACKSLASH),
        '/' if policy.escape_solidus => Some(b'/'),
        '\u{0008}' => Some(b'b'),
        '\u{000c}' => Some(b'f'),
        '\n' => Some(b'n'),
        '\r' => Some(b'r'),
        '\t' => Some(b't'),
        _ => None,
    };
    if let Some(letter) = short {
        output[0] = BACKSLASH;
        output[1] = letter;
        return 2;
    }
    if scalar < '\u{20}' || (policy.ascii_only && !scalar.is_ascii()) {
        let mut storage = [0; 2];
        let units = scalar.encode_utf16(&mut storage);
        for (index, &unit) in units.iter().enumerate() {
            let start = index * 6;
            output[start..start + 6].copy_from_slice(&hex_escape(unit));
        }
        return units.len() * 6;
    }
    scalar.encode_utf8(output).len()
}

fn hex_escape(unit: u16) -> [u8; 6] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    [
        BACKSLASH,
        b'u',
        HEX[usize::from(unit >> 12)],
        HEX[usize::from((unit >> 8) & 15)],
        HEX[usize::from((unit >> 4) & 15)],
        HEX[usize::from(unit & 15)],
    ]
}

type EscapeSearch = unsafe fn(&[u8], EscapePolicy) -> Option<usize>;

/// A private function pointer whose target-feature requirements were checked.
/// No public API accepts a caller-provided capability record or search pointer.
pub(crate) struct EscapeFinder {
    policy: EscapePolicy,
    search: EscapeSearch,
}

impl EscapeFinder {
    pub(crate) fn new(_input_len: usize, policy: EscapePolicy) -> Self {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        if _input_len >= 32
            && jmax_dispatch::detect::detect().supports(jmax_dispatch::BackendId::Avx2)
        {
            return Self {
                policy,
                search: x86::find_avx2,
            };
        }
        #[cfg(target_arch = "aarch64")]
        if _input_len >= 16
            && jmax_dispatch::detect::detect().supports(jmax_dispatch::BackendId::Neon)
        {
            return Self {
                policy,
                search: neon::find_neon,
            };
        }
        Self {
            policy,
            search: find_scalar,
        }
    }

    #[inline]
    pub(crate) fn find(&self, bytes: &[u8]) -> Option<usize> {
        // SAFETY: new is the only constructor. Its fresh dispatch detection
        // checks usable CPU/OS state before installing a native function.
        // Scalar has no feature requirements; each native function bounds its
        // own loads, including when this slice is shorter than the size hint.
        unsafe { (self.search)(bytes, self.policy) }
    }
}

#[inline]
fn find_scalar(bytes: &[u8], policy: EscapePolicy) -> Option<usize> {
    bytes.iter().position(|&byte| {
        byte == QUOTE
            || byte == BACKSLASH
            || byte < 0x20
            || (policy.escape_solidus && byte == b'/')
            || (policy.ascii_only && byte >= 0x80)
    })
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod x86 {
    use super::{find_scalar, EscapePolicy, BACKSLASH, QUOTE};
    #[cfg(target_arch = "x86")]
    use core::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::*;

    /// Caller must establish usable AVX2 CPU and OS register-state support.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn find_avx2(bytes: &[u8], policy: EscapePolicy) -> Option<usize> {
        // SAFETY: The caller checks AVX2 usability. The subtraction guard
        // proves each unaligned 32-byte load is wholly within bytes. cursor
        // never exceeds len and advances only by a complete vector width.
        unsafe {
            let quote = _mm256_set1_epi8(QUOTE as i8);
            let backslash = _mm256_set1_epi8(BACKSLASH as i8);
            let control_max = _mm256_set1_epi8(0x1f);
            let solidus = _mm256_set1_epi8(b'/' as i8);
            let mut cursor = 0;
            while bytes.len() - cursor >= 32 {
                let chunk = _mm256_loadu_si256(bytes.as_ptr().add(cursor).cast::<__m256i>());
                // Unsigned minimum includes only bytes <= 0x1f, rather than
                // accidentally classifying high-bit bytes as signed controls.
                let controls = _mm256_cmpeq_epi8(_mm256_min_epu8(chunk, control_max), chunk);
                let mut matches = _mm256_or_si256(
                    controls,
                    _mm256_or_si256(
                        _mm256_cmpeq_epi8(chunk, quote),
                        _mm256_cmpeq_epi8(chunk, backslash),
                    ),
                );
                if policy.escape_solidus {
                    matches = _mm256_or_si256(matches, _mm256_cmpeq_epi8(chunk, solidus));
                }
                let mut mask = _mm256_movemask_epi8(matches) as u32;
                if policy.ascii_only {
                    mask |= _mm256_movemask_epi8(chunk) as u32;
                }
                if mask != 0 {
                    return Some(cursor + mask.trailing_zeros() as usize);
                }
                cursor += 32;
            }
            find_scalar(&bytes[cursor..], policy).map(|index| cursor + index)
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use super::{find_scalar, EscapePolicy, BACKSLASH, QUOTE};
    use core::arch::aarch64::*;

    /// Caller must establish usable AArch64 NEON support.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn find_neon(bytes: &[u8], policy: EscapePolicy) -> Option<usize> {
        // SAFETY: The caller checks NEON usability. The loop guard proves each
        // 16-byte unaligned load is within bytes. The only store targets a
        // fully initialized 16-byte local array. No tail load uses padding.
        unsafe {
            let quote = vdupq_n_u8(QUOTE);
            let backslash = vdupq_n_u8(BACKSLASH);
            let control_end = vdupq_n_u8(0x20);
            let solidus = vdupq_n_u8(b'/');
            let non_ascii_start = vdupq_n_u8(0x80);
            let mut cursor = 0;
            while bytes.len() - cursor >= 16 {
                let chunk = vld1q_u8(bytes.as_ptr().add(cursor));
                let mut matches = vorrq_u8(
                    vcltq_u8(chunk, control_end),
                    vorrq_u8(vceqq_u8(chunk, quote), vceqq_u8(chunk, backslash)),
                );
                if policy.escape_solidus {
                    matches = vorrq_u8(matches, vceqq_u8(chunk, solidus));
                }
                if policy.ascii_only {
                    matches = vorrq_u8(matches, vcgeq_u8(chunk, non_ascii_start));
                }
                if vmaxvq_u8(matches) != 0 {
                    let weights = vcombine_u8(
                        vcreate_u8(u64::from_ne_bytes([1, 2, 4, 8, 16, 32, 64, 128])),
                        vcreate_u8(u64::from_ne_bytes([1, 2, 4, 8, 16, 32, 64, 128])),
                    );
                    let halves = vpaddlq_u32(vpaddlq_u16(vpaddlq_u8(vandq_u8(matches, weights))));
                    let mask = vgetq_lane_u64(halves, 0) | (vgetq_lane_u64(halves, 1) << 8);
                    return Some(cursor + mask.trailing_zeros() as usize);
                }
                cursor += 16;
            }
            find_scalar(&bytes[cursor..], policy).map(|index| cursor + index)
        }
    }
}
