//! Allocation-free JSON serialization with bounded SIMD escape-run discovery.
//!
//! Measurement and emission share their spelling rules. The complete append
//! length is checked and reserved before any output, including wrapper quotes.

use super::{
    ByteSink, Error, ErrorCode, ErrorSource, EscapePolicy, Result, BACKSLASH,
    MAX_ESCAPE_BYTES, QUOTE,
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
pub fn write_escaped(
    text: &str,
    policy: EscapePolicy,
    sink: &mut dyn ByteSink,
) -> Result<()> {
    write(text, policy, sink, false)
}

/// Append a complete JSON string, including its double quotes.
///
/// The content and both quotes must fit the 32-bit ABI length. All size checks
/// and the full reservation precede even the opening quote. A sink write error
/// can leave a prefix, including an unmatched opening quote.
pub fn write_string(
    text: &str,
    policy: EscapePolicy,
    sink: &mut dyn ByteSink,
) -> Result<()> {
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
    length.checked_add(checked_size(additional)?).ok_or_else(size_error)
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
struct EscapeFinder {
    policy: EscapePolicy,
    search: EscapeSearch,
}

impl EscapeFinder {
    fn new(_input_len: usize, policy: EscapePolicy) -> Self {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        if _input_len >= 32
            && jmax_dispatch::detect::detect().supports(jmax_dispatch::BackendId::Avx2)
        {
            return Self { policy, search: x86::find_avx2 };
        }
        #[cfg(target_arch = "aarch64")]
        if _input_len >= 16
            && jmax_dispatch::detect::detect().supports(jmax_dispatch::BackendId::Neon)
        {
            return Self { policy, search: neon::find_neon };
        }
        Self { policy, search: find_scalar }
    }

    #[inline]
    fn find(&self, bytes: &[u8]) -> Option<usize> {
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
        byte == QUOTE || byte == BACKSLASH || byte < 0x20
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
                    // Store in memory order instead of relying on endian-
                    // specific lane packing or a synthesized bitmask.
                    let mut lanes = [0; 16];
                    vst1q_u8(lanes.as_mut_ptr(), matches);
                    return lanes.iter().position(|&lane| lane != 0)
                        .map(|index| cursor + index);
                }
                cursor += 16;
            }
            find_scalar(&bytes[cursor..], policy).map(|index| cursor + index)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Span32, StringLengths};

    const POLICIES: [EscapePolicy; 4] = [
        EscapePolicy { escape_solidus: false, ascii_only: false },
        EscapePolicy { escape_solidus: true, ascii_only: false },
        EscapePolicy { escape_solidus: false, ascii_only: true },
        EscapePolicy { escape_solidus: true, ascii_only: true },
    ];

    struct RecordingSink {
        bytes: [u8; 4096],
        used: usize,
        reserve_calls: usize,
        reserved: usize,
        write_calls: usize,
        write_starts: [usize; 512],
        fail_reserve: bool,
        fail_write: Option<usize>,
        partial_failure: bool,
    }

    impl RecordingSink {
        fn new(prefix: &[u8]) -> Self {
            let mut sink = Self {
                bytes: [0; 4096], used: prefix.len(), reserve_calls: 0,
                reserved: 0, write_calls: 0, write_starts: [0; 512],
                fail_reserve: false, fail_write: None, partial_failure: false,
            };
            sink.bytes[..prefix.len()].copy_from_slice(prefix);
            sink
        }

        fn output(&self) -> &[u8] {
            &self.bytes[..self.used]
        }
    }

    fn sink_error() -> Error {
        Error::new(ErrorCode::BudgetExhausted, ErrorSource::Wire, 123, "sink failure")
            .with_schema(456, 789)
    }

    impl ByteSink for RecordingSink {
        fn reserve(&mut self, additional: usize) -> Result<()> {
            self.reserve_calls += 1;
            self.reserved = additional;
            if self.fail_reserve || additional > self.bytes.len() - self.used {
                return Err(sink_error());
            }
            Ok(())
        }

        fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
            assert!(!bytes.is_empty(), "empty output needs no write call");
            assert_eq!(self.reserve_calls, 1, "reserve the entire append first");
            self.write_starts[self.write_calls] = self.used;
            self.write_calls += 1;
            if self.fail_write == Some(self.write_calls) {
                if self.partial_failure {
                    self.bytes[self.used] = bytes[0];
                    self.used += 1;
                }
                return Err(sink_error());
            }
            if bytes.len() > self.bytes.len() - self.used {
                return Err(sink_error());
            }
            self.bytes[self.used..self.used + bytes.len()].copy_from_slice(bytes);
            self.used += bytes.len();
            Ok(())
        }

        fn len(&self) -> usize {
            self.used
        }
    }

    fn reference_find(bytes: &[u8], policy: EscapePolicy) -> Option<usize> {
        for (index, &byte) in bytes.iter().enumerate() {
            let required = matches!(byte, 0..=31 | b'"' | b'\\');
            if required || (policy.escape_solidus && byte == b'/')
                || (policy.ascii_only && !byte.is_ascii())
            {
                return Some(index);
            }
        }
        None
    }

    fn assert_discovery(bytes: &[u8], policy: EscapePolicy, expected: Option<usize>) {
        assert_eq!(reference_find(bytes, policy), expected);
        assert_eq!(find_scalar(bytes, policy), expected);
        assert_eq!(find_escape(bytes, policy), expected, "bytes: {bytes:?}, policy: {policy:?}");
        // Force selection using a large size hint, then pass any shorter slice.
        // Native paths must still bound their loads independently of that hint.
        assert_eq!(EscapeFinder::new(4096, policy).find(bytes), expected);
    }

    fn assert_encoding(text: &str, policy: EscapePolicy, expected: &[u8]) {
        assert_eq!(escaped_length(text, policy).unwrap() as usize, expected.len());
        let mut content = RecordingSink::new(b"prior:");
        write_escaped(text, policy, &mut content).unwrap();
        assert_eq!(&content.output()[..6], b"prior:");
        assert_eq!(&content.output()[6..], expected, "text: {text:?}, policy: {policy:?}");
        assert_eq!((content.reserve_calls, content.reserved), (1, expected.len()));

        let mut string = RecordingSink::new(b"prior:");
        write_string(text, policy, &mut string).unwrap();
        assert_eq!(&string.output()[..7], b"prior:\"");
        assert_eq!(&string.output()[7..string.used - 1], expected);
        assert_eq!(string.output().last(), Some(&QUOTE));
        assert_eq!((string.reserve_calls, string.reserved), (1, expected.len() + 2));
    }

    fn assert_round_trip(text: &str, policy: EscapePolicy) {
        let mut encoded = RecordingSink::new(b"prefix:");
        write_string(text, policy, &mut encoded).unwrap();
        let input = encoded.output();
        let scan = crate::scan::scan_string(input, 7).unwrap();
        assert_eq!(scan.next_offset as usize, input.len());
        assert_eq!(scan.raw, Span32::new(7, (input.len() - 7) as u32));
        assert_eq!(scan.content.len, escaped_length(text, policy).unwrap());
        let lengths = StringLengths::new(text.len() as u32, text.chars().count() as u32);
        assert_eq!(crate::length::utf8_length(text).unwrap(), lengths);
        assert_eq!(crate::length::decoded_length(input, scan.content).unwrap(), lengths);
        let mut decoded = RecordingSink::new(b"existing:");
        assert_eq!(crate::unescape::unescape(input, scan.content, &mut decoded).unwrap(), lengths);
        assert_eq!(&decoded.output()[..9], b"existing:");
        assert_eq!(&decoded.output()[9..], text.as_bytes());
        assert_eq!((decoded.reserve_calls, decoded.reserved), (1, text.len()));
        if policy.ascii_only {
            assert!(input[7..].is_ascii());
        }
    }

    fn assert_size_error(error: Error) {
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert_eq!(error.source, ErrorSource::Encode);
        assert_eq!(error.byte_offset, u32::MAX);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn discovery_classifies_every_byte_under_every_policy_at_vector_boundaries() {
        let mut bytes = [b'a'; 161];
        for policy in POLICIES {
            assert_discovery(&[], policy, None);
            for value in 0u16..=255 {
                let byte = value as u8;
                for position in [0, 1, 15, 16, 17, 31, 32, 33, 63, 64, 95, 127, 160] {
                    bytes[position] = byte;
                    let expected = reference_find(&bytes, policy);
                    assert_discovery(&bytes, policy, expected);
                    bytes[position] = b'a';
                }
            }
        }
    }

    #[test]
    fn discovery_checks_every_tail_length_alignment_and_logical_slice_end() {
        let mut storage = [b'a'; 196];
        for policy in POLICIES {
            for start in 0..=33 {
                for len in 0..=129 {
                    // The byte immediately outside the slice must not match.
                    storage[start + len] = QUOTE;
                    assert_discovery(&storage[start..start + len], policy, None);
                    assert_discovery(&storage[start..=start + len], policy, Some(len));
                    storage[start + len] = b'a';
                }
            }
        }
    }

    #[test]
    fn discovery_reports_the_first_match_in_every_vector_lane_and_tail() {
        let mut storage = [b'x'; 131];
        for policy in POLICIES {
            for special in [0, 8, 9, 10, 12, 13, 31, QUOTE, BACKSLASH, b'/', 0x80, 0xff] {
                for position in 0..130 {
                    storage[position] = special;
                    storage[130] = QUOTE;
                    let expected = if reference_find(&[special], policy).is_some() {
                        position
                    } else {
                        130
                    };
                    assert_discovery(&storage, policy, Some(expected));
                    storage[position] = b'x';
                    storage[130] = b'x';
                }
            }
        }
    }

    #[test]
    fn arbitrary_bytes_have_no_utf8_or_shared_carry_requirements() {
        let cases: &[&[u8]] = &[
            &[0xff, 0x80, 0xc0, 0xed, 0xa0, 0x80],
            &[0xf4, 0x90, 0x80, 0x80, b'/', 0x7f],
            b"abc\\\"\0tail", b"abc/def", b"\x7f\x80\xff",
        ];
        for policy in POLICIES {
            for &bytes in cases {
                let before = find_escape(bytes, policy);
                assert_discovery(bytes, policy, reference_find(bytes, policy));
                assert_eq!(find_escape(b"ordinary", policy), None);
                assert_eq!(find_escape(bytes, policy), before);
            }
        }
    }

    #[test]
    fn empty_output_and_unescaped_runs_reserve_exactly_once() {
        for policy in POLICIES {
            assert_encoding("", policy, b"");
            let mut empty = RecordingSink::new(b"prior");
            write_escaped("", policy, &mut empty).unwrap();
            assert_eq!((empty.reserve_calls, empty.reserved, empty.write_calls), (1, 0, 0));
            assert_eq!(empty.output(), b"prior");
            let mut quoted = RecordingSink::new(b"");
            write_string("", policy, &mut quoted).unwrap();
            assert_eq!(quoted.output(), b"\"\"");
            assert_eq!((quoted.reserve_calls, quoted.reserved, quoted.write_calls), (1, 2, 2));
            let bytes = [b'a'; 513];
            let text = core::str::from_utf8(&bytes).unwrap();
            let mut run = RecordingSink::new(b"prior");
            write_escaped(text, policy, &mut run).unwrap();
            assert_eq!((run.reserve_calls, run.reserved, run.write_calls), (1, 513, 1));
            assert_eq!(&run.output()[5..], &bytes);
        }
    }

    #[test]
    fn all_controls_use_the_required_short_or_lowercase_unicode_spelling() {
        let expected: [&[u8]; 32] = [
            b"\\u0000", b"\\u0001", b"\\u0002", b"\\u0003",
            b"\\u0004", b"\\u0005", b"\\u0006", b"\\u0007",
            b"\\b", b"\\t", b"\\n", b"\\u000b", b"\\f", b"\\r", b"\\u000e", b"\\u000f",
            b"\\u0010", b"\\u0011", b"\\u0012", b"\\u0013",
            b"\\u0014", b"\\u0015", b"\\u0016", b"\\u0017",
            b"\\u0018", b"\\u0019", b"\\u001a", b"\\u001b",
            b"\\u001c", b"\\u001d", b"\\u001e", b"\\u001f",
        ];
        for policy in POLICIES {
            for control in 0u8..32 {
                let bytes = [control];
                let text = core::str::from_utf8(&bytes).unwrap();
                assert_encoding(text, policy, expected[usize::from(control)]);
                assert_round_trip(text, policy);
            }
        }
    }

    #[test]
    fn permitted_ascii_del_apostrophe_and_solidus_follow_policy_exactly() {
        for policy in POLICIES {
            for byte in 32u8..=127 {
                let bytes = [byte];
                let text = core::str::from_utf8(&bytes).unwrap();
                let expected: &[u8] = match byte {
                    QUOTE => b"\\\"",
                    BACKSLASH => b"\\\\",
                    b'/' if policy.escape_solidus => b"\\/",
                    _ => &bytes,
                };
                assert_encoding(text, policy, expected);
                assert_round_trip(text, policy);
            }
            assert_encoding("'<> &\u{7f}", policy, b"'<> &\x7f");
            assert_encoding("\"\\\u{8}\u{c}\n\r\t", policy, b"\\\"\\\\\\b\\f\\n\\r\\t");
        }
        assert_eq!(EscapePolicy::default(), POLICIES[0]);
        assert_encoding("a/b", POLICIES[0], b"a/b");
        assert_encoding("a/b", POLICIES[1], b"a\\/b");
    }

    #[test]
    fn unicode_boundary_vectors_use_lowercase_bmp_or_surrogate_pairs() {
        let vectors: &[(char, &[u8])] = &[
            ('\u{80}', b"\\u0080"), ('\u{ff}', b"\\u00ff"),
            ('\u{7ff}', b"\\u07ff"), ('\u{800}', b"\\u0800"),
            ('\u{2028}', b"\\u2028"), ('\u{2029}', b"\\u2029"),
            ('\u{d7ff}', b"\\ud7ff"), ('\u{e000}', b"\\ue000"),
            ('\u{feff}', b"\\ufeff"), ('\u{ffff}', b"\\uffff"),
            ('\u{10000}', b"\\ud800\\udc00"), ('\u{103ff}', b"\\ud800\\udfff"),
            ('\u{10400}', b"\\ud801\\udc00"), ('𝄞', b"\\ud834\\udd1e"),
            ('🙂', b"\\ud83d\\ude42"), ('\u{10ffff}', b"\\udbff\\udfff"),
        ];
        for &(scalar, ascii) in vectors {
            let mut bytes = [0; 4];
            let text = scalar.encode_utf8(&mut bytes);
            for policy in POLICIES {
                let expected = if policy.ascii_only { ascii } else { text.as_bytes() };
                assert_encoding(text, policy, expected);
                assert_round_trip(text, policy);
            }
        }
        assert_encoding("é中🙂", POLICIES[2], b"\\u00e9\\u4e2d\\ud83d\\ude42");
        assert_encoding("é中🙂", POLICIES[0], "é中🙂".as_bytes());
    }

    #[test]
    fn serializer_scan_measure_and_unescape_integrate_without_normalization() {
        for text in [
            "", "plain ASCII / ' < > &\u{7f}", "\0\u{1f}\"\\\n\r\t\u{8}\u{c}",
            "é", "e\u{301}", "中文", "🇰🇭", "👩\u{200d}💻", "✈\u{fe0f}",
            "\u{feff}\u{2028}\u{2029}", "\u{d7ff}\u{e000}\u{ffff}\u{10000}\u{10ffff}",
            "prefix\\\"/é\0中\n🙂suffix",
        ] {
            for policy in POLICIES {
                assert_round_trip(text, policy);
            }
        }
    }

    #[test]
    fn utf8_leaders_and_escapes_cross_every_vector_boundary() {
        let mut storage = [b'a'; 256];
        for prefix in 0..=97 {
            for tail in ["é", "中", "🙂", "é/中\n🙂\"\\\0suffix"] {
                storage.fill(b'a');
                storage[prefix..prefix + tail.len()].copy_from_slice(tail.as_bytes());
                let text = core::str::from_utf8(&storage[..prefix + tail.len()]).unwrap();
                for policy in POLICIES {
                    assert_round_trip(text, policy);
                    let mut sink = RecordingSink::new(b"");
                    write_escaped(text, policy, &mut sink).unwrap();
                    assert_eq!(sink.used, escaped_length(text, policy).unwrap() as usize);
                    assert_eq!(&sink.output()[..prefix], &storage[..prefix]);
                }
            }
        }
    }

    #[test]
    fn deterministic_mixed_scalar_vectors_round_trip_under_all_policies() {
        let edges = [
            '\0', '\u{1f}', '"', '\\', '/', '\u{7f}', '\u{80}', '\u{7ff}',
            '\u{800}', '\u{d7ff}', '\u{e000}', '\u{ffff}', '\u{10000}', '\u{10ffff}',
        ];
        let mut state = 0x5e71_c0deu32;
        for iteration in 0..64 {
            let mut input = [0; 512];
            let mut used = 0;
            for index in 0..64 {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let scalar = if index % 3 == 0 {
                    edges[(iteration + index) % edges.len()]
                } else {
                    // Map only the surrogate range to a valid neighboring
                    // scalar; all other generated Unicode values are retained.
                    let value = state % 0x11_0000;
                    char::from_u32(if (0xd800..=0xdfff).contains(&value) { 0xe000 } else { value })
                        .unwrap()
                };
                let mut bytes = [0; 4];
                let encoded = scalar.encode_utf8(&mut bytes).as_bytes();
                input[used..used + encoded.len()].copy_from_slice(encoded);
                used += encoded.len();
            }
            let text = core::str::from_utf8(&input[..used]).unwrap();
            for policy in POLICIES {
                assert_round_trip(text, policy);
            }
        }
    }

    #[test]
    fn sink_reservation_errors_preserve_metadata_and_existing_bytes() {
        for text in ["", "ordinary", "\"\\/\n\0é🙂"] {
            for policy in POLICIES {
                for quoted in [false, true] {
                    let mut sink = RecordingSink::new(b"existing");
                    sink.fail_reserve = true;
                    let result = if quoted {
                        write_string(text, policy, &mut sink)
                    } else {
                        write_escaped(text, policy, &mut sink)
                    };
                    assert_eq!(result.unwrap_err(), sink_error());
                    let expected = escaped_length(text, policy).unwrap() as usize
                        + if quoted { 2 } else { 0 };
                    assert_eq!((sink.reserve_calls, sink.reserved, sink.write_calls), (1, expected, 0));
                    assert_eq!(sink.output(), b"existing");
                }
            }
        }
    }

    #[test]
    fn every_sink_write_error_stops_emission_and_preserves_its_metadata() {
        let text = "ab\"é/\n𝄞\0tail";
        for policy in POLICIES {
            for quoted in [false, true] {
                let mut successful = RecordingSink::new(b"prior:");
                write(text, policy, &mut successful, quoted).unwrap();
                for failed_write in 1..=successful.write_calls {
                    for partial_failure in [false, true] {
                        let mut sink = RecordingSink::new(b"prior:");
                        sink.fail_write = Some(failed_write);
                        sink.partial_failure = partial_failure;
                        let result = if quoted {
                            write_string(text, policy, &mut sink)
                        } else {
                            write_escaped(text, policy, &mut sink)
                        };
                        assert_eq!(result.unwrap_err(), sink_error());
                        assert_eq!(sink.reserve_calls, 1);
                        assert_eq!(sink.reserved, successful.reserved);
                        assert_eq!(sink.write_calls, failed_write);
                        let end = successful.write_starts[failed_write - 1]
                            + usize::from(partial_failure);
                        assert_eq!(sink.output(), &successful.output()[..end]);
                    }
                }
            }
        }
    }

    #[test]
    fn input_and_output_size_guards_cover_host_and_abi_boundaries() {
        for len in [0, 1, u32::MAX as usize - 1, u32::MAX as usize] {
            assert_eq!(checked_size(len).unwrap(), len as u32);
            assert_eq!(checked_add_length(0, len).unwrap(), len as u32);
        }
        for (length, additional, expected) in [
            (0, 0, 0), (u32::MAX, 0, u32::MAX),
            (u32::MAX - 1, 1, u32::MAX), (u32::MAX - 2, 2, u32::MAX),
            (u32::MAX - 6, 6, u32::MAX), (u32::MAX - 12, 12, u32::MAX),
        ] {
            assert_eq!(checked_add_length(length, additional).unwrap(), expected);
        }
        for (length, additional) in [
            (u32::MAX, 1), (u32::MAX - 1, 2),
            (u32::MAX - 5, 6), (u32::MAX - 11, 12),
        ] {
            assert_size_error(checked_add_length(length, additional).unwrap_err());
        }
        if let Some(oversized) = (u32::MAX as usize).checked_add(1) {
            for len in [oversized, usize::MAX] {
                assert_size_error(checked_size(len).unwrap_err());
                assert_size_error(checked_add_length(0, len).unwrap_err());
            }
        }
    }

    #[test]
    fn quote_overflow_precedes_all_sink_calls_even_when_reservation_would_fail() {
        for content_len in [u32::MAX - 1, u32::MAX] {
            let mut sink = RecordingSink::new(b"prior:");
            sink.fail_reserve = true;
            assert_size_error(reserve_append(content_len, true, &mut sink).unwrap_err());
            assert_eq!((sink.reserve_calls, sink.write_calls), (0, 0));
            assert_eq!(sink.output(), b"prior:");
        }
        for (content_len, quoted) in [
            (u32::MAX - 2, true), (u32::MAX, false),
        ] {
            let mut sink = RecordingSink::new(b"prior:");
            // These sizes fit the ABI, so the sink's own failure must survive.
            assert_eq!(reserve_append(content_len, quoted, &mut sink).unwrap_err(), sink_error());
            assert_eq!((sink.reserve_calls, sink.reserved, sink.write_calls),
                (1, u32::MAX as usize, 0));
            assert_eq!(sink.output(), b"prior:");
        }
    }
}
