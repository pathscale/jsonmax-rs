//! Allocation-free base64 and hexadecimal validation over decoded string bytes.
//!
//! Successful validation returns the decoded byte count without decoding. Error
//! indices refer to the supplied slice, including a required hexadecimal prefix.
//! All arithmetic is bounded or checked; no input byte outside the slice is read.

use crate::{
    Base64Policy, FormatFailure, FormatFailureKind, FormatResult, HexCase, HexPolicy,
    Padding,
};

/// Validates standard or URL-safe base64 with the selected padding policy.
///
/// Alphabets are exclusive, whitespace is invalid, and unused bits in a partial
/// final quantum must be zero. Bytes and padding placement are checked from left
/// to right, followed by missing content and padding, then discarded tail bits.
/// Missing padding is reported at the slice end; excess or misplaced padding is
/// reported at the offending byte. A noncanonical tail names its last data byte.
pub fn validate_base64(input: &[u8], policy: Base64Policy) -> FormatResult<u32> {
    let mut data_len = 0usize;
    let mut padding_len = 0u8;
    let mut last_value = 0u8;

    for (index, &byte) in input.iter().enumerate() {
        if byte == b'=' {
            if policy.padding == Padding::Forbidden {
                return Err(failure(FormatFailureKind::InvalidPadding, index));
            }
            let allowed_padding = match data_len % 4 {
                2 => 2,
                3 => 1,
                _ => return Err(failure(FormatFailureKind::InvalidPadding, index)),
            };
            if padding_len == allowed_padding {
                return Err(failure(FormatFailureKind::InvalidPadding, index));
            }
            padding_len += 1;
            continue;
        }

        let value = match base64_value(byte, policy.url_safe) {
            Some(value) => value,
            None => return Err(failure(FormatFailureKind::InvalidByte, index)),
        };
        if padding_len != 0 {
            return Err(failure(FormatFailureKind::InvalidPadding, index));
        }
        // There is at most one increment per byte in the supplied slice.
        data_len += 1;
        last_value = value;
    }

    let remainder = data_len % 4;
    if padding_len != 0 {
        // The scan has already limited padding to the canonical maximum for
        // this quantum. A short final quantum can therefore only be missing '='.
        if input.len() % 4 != 0 {
            return Err(failure(FormatFailureKind::InvalidPadding, input.len()));
        }
    } else {
        if remainder == 1 {
            return Err(failure(FormatFailureKind::InvalidLength, input.len()));
        }
        if policy.padding == Padding::Required && remainder != 0 {
            return Err(failure(FormatFailureKind::InvalidPadding, input.len()));
        }
    }

    let discarded_mask = match remainder {
        2 => 0x0f,
        3 => 0x03,
        _ => 0,
    };
    if last_value & discarded_mask != 0 {
        // A nonzero mask implies at least two data bytes, so subtraction is safe.
        return Err(failure(FormatFailureKind::NonCanonical, data_len - 1));
    }

    // Dividing before multiplying avoids an intermediate encoded-length
    // overflow. Both terms together are no greater than data_len.
    let decoded_len = (data_len / 4) * 3 + (remainder * 3) / 4;
    decoded_count(decoded_len, input.len())
}

/// Validates ASCII hex digits, optionally following exactly lowercase `0x`.
///
/// Length (including any exact byte count) is checked before prefix and digits.
/// Empty digits encode zero bytes. Letter-case restrictions apply only to hex
/// digits, and never to the required prefix. A forbidden prefix fails at its
/// non-hexadecimal `x` or `X` byte. Length failures use the full slice length.
pub fn validate_hex(input: &[u8], policy: HexPolicy) -> FormatResult<u32> {
    let prefix_len = if policy.prefix { 2 } else { 0 };
    let digits_len = match input.len().checked_sub(prefix_len) {
        Some(len) => len,
        None => return Err(failure(FormatFailureKind::InvalidLength, input.len())),
    };
    if digits_len % 2 != 0 {
        return Err(failure(FormatFailureKind::InvalidLength, input.len()));
    }
    if let Some(exact_bytes) = policy.exact_bytes {
        if hex_encoded_len(exact_bytes, prefix_len) != Some(input.len()) {
            return Err(failure(FormatFailureKind::InvalidLength, input.len()));
        }
    }

    if policy.prefix {
        if input[0] != b'0' {
            return Err(failure(FormatFailureKind::InvalidByte, 0));
        }
        if input[1] != b'x' {
            return Err(failure(FormatFailureKind::InvalidByte, 1));
        }
    }

    for (offset, &byte) in input[prefix_len..].iter().enumerate() {
        let kind = match byte {
            b'0'..=b'9' => continue,
            b'a'..=b'f' if policy.case != HexCase::Upper => continue,
            b'A'..=b'F' if policy.case != HexCase::Lower => continue,
            b'a'..=b'f' | b'A'..=b'F' => FormatFailureKind::NonCanonical,
            _ => FormatFailureKind::InvalidByte,
        };
        // offset belongs to the suffix, so the sum is strictly below input.len().
        return Err(failure(kind, prefix_len + offset));
    }

    decoded_count(digits_len / 2, input.len())
}

fn base64_value(byte: u8, url_safe: bool) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' if !url_safe => Some(62),
        b'/' if !url_safe => Some(63),
        b'-' if url_safe => Some(62),
        b'_' if url_safe => Some(63),
        _ => None,
    }
}

fn hex_encoded_len(exact_bytes: u32, prefix_len: usize) -> Option<usize> {
    usize::try_from(exact_bytes).ok()?.checked_mul(2)?.checked_add(prefix_len)
}

fn decoded_count(decoded_len: usize, input_len: usize) -> FormatResult<u32> {
    u32::try_from(decoded_len)
        .map_err(|_| failure(FormatFailureKind::Limit, input_len))
}

fn failure(kind: FormatFailureKind, index: usize) -> FormatFailure {
    FormatFailure::new(kind, index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FormatFailureKind::{
        InvalidByte, InvalidLength, InvalidPadding, Limit, NonCanonical,
    };

    const STANDARD: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    const URL_SAFE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    const PADDING_POLICIES: [Padding; 3] =
        [Padding::Required, Padding::Forbidden, Padding::Optional];

    fn base64_policy(url_safe: bool, padding: Padding) -> Base64Policy {
        Base64Policy { url_safe, padding }
    }

    fn assert_failure(result: FormatResult<u32>, kind: FormatFailureKind, index: usize) {
        assert_eq!(result, Err(FormatFailure::new(kind, index)));
    }

    #[test]
    fn base64_rfc4648_vectors_under_every_policy_and_alphabet() {
        let vectors: &[(&[u8], &[u8], u32)] = &[
            (b"", b"", 0),
            (b"Zg==", b"Zg", 1),
            (b"Zm8=", b"Zm8", 2),
            (b"Zm9v", b"Zm9v", 3),
            (b"Zm9vYg==", b"Zm9vYg", 4),
            (b"Zm9vYmE=", b"Zm9vYmE", 5),
            (b"Zm9vYmFy", b"Zm9vYmFy", 6),
        ];
        for url_safe in [false, true] {
            for &(padded, unpadded, count) in vectors {
                assert_eq!(
                    validate_base64(padded, base64_policy(url_safe, Padding::Required)),
                    Ok(count),
                );
                assert_eq!(
                    validate_base64(unpadded, base64_policy(url_safe, Padding::Forbidden)),
                    Ok(count),
                );
                for input in [padded, unpadded] {
                    assert_eq!(
                        validate_base64(input, base64_policy(url_safe, Padding::Optional)),
                        Ok(count),
                    );
                }
                if padded != unpadded {
                    assert_failure(
                        validate_base64(unpadded, base64_policy(url_safe, Padding::Required)),
                        InvalidPadding,
                        unpadded.len(),
                    );
                    assert_failure(
                        validate_base64(padded, base64_policy(url_safe, Padding::Forbidden)),
                        InvalidPadding,
                        unpadded.len(),
                    );
                }
            }
        }
        assert_eq!(validate_base64(b"Zg==", Base64Policy::default()), Ok(1));
    }

    #[test]
    fn base64_alphabets_are_exclusive_including_partial_quanta() {
        for (standard, url) in [
            (&b"++//"[..], &b"--__"[..]),
            (&b"/+/+"[..], &b"_-_-"[..]),
            (&b"+w=="[..], &b"-w=="[..]),
            (&b"/w=="[..], &b"_w=="[..]),
            (&b"//8="[..], &b"__8="[..]),
        ] {
            let count = if standard.ends_with(b"==") { 1 }
                else if standard.ends_with(b"=") { 2 } else { 3 };
            assert_eq!(validate_base64(standard, Base64Policy::default()), Ok(count));
            assert_eq!(
                validate_base64(url, base64_policy(true, Padding::Required)),
                Ok(count),
            );
            assert_failure(validate_base64(url, Base64Policy::default()), InvalidByte, 0);
            assert_failure(
                validate_base64(standard, base64_policy(true, Padding::Required)),
                InvalidByte,
                0,
            );
        }
        for padding in PADDING_POLICIES {
            assert_failure(
                validate_base64(b"A+_A", base64_policy(false, padding)), InvalidByte, 2,
            );
            assert_failure(
                validate_base64(b"A+_A", base64_policy(true, padding)), InvalidByte, 1,
            );
        }
    }

    #[test]
    fn base64_checks_all_byte_values_in_every_full_quantum_position() {
        for url_safe in [false, true] {
            let alphabet = if url_safe { URL_SAFE } else { STANDARD };
            for padding in PADDING_POLICIES {
                for index in 0..4 {
                    for byte in 0..=u8::MAX {
                        // '=' has structural meaning and is covered separately.
                        if byte == b'=' {
                            continue;
                        }
                        let mut input = *b"AAAA";
                        input[index] = byte;
                        let result = validate_base64(&input, base64_policy(url_safe, padding));
                        if alphabet.contains(&byte) {
                            assert_eq!(result, Ok(3));
                        } else {
                            assert_failure(result, InvalidByte, index);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn base64_rejects_whitespace_controls_and_non_ascii_without_trimming() {
        for input in [
            &b" Zg=="[..], &b"\tZg=="[..], &b"\nZg=="[..],
            &b"\rZg=="[..], &b"\x0bZg=="[..], &b"\x0cZg=="[..],
            &b"\0Zg=="[..], &b"\xffZg=="[..], &b"\xc2\xa0Zg=="[..],
        ] {
            assert_failure(validate_base64(input, Base64Policy::default()), InvalidByte, 0);
        }
        for input in [
            &b"Zg== "[..], &b"Zg==\n"[..], &b"Zg==\r\n"[..],
            &b"Zg==\0"[..], &b"Zg==\xff"[..], &b"Zg==\xc2\xa0"[..],
        ] {
            assert_failure(validate_base64(input, Base64Policy::default()), InvalidByte, 4);
        }
        assert_failure(validate_base64(b"Z g=", Base64Policy::default()), InvalidByte, 1);
    }

    #[test]
    fn base64_reports_padding_placement_excess_and_missing_bytes() {
        let cases: &[(&[u8], usize)] = &[
            (b"=", 0), (b"==", 0), (b"====", 0),
            (b"=AAA", 0), (b"A=AA", 1), (b"A===", 1),
            (b"AA=A", 3), (b"AA==A", 4), (b"AAA=A", 4),
            (b"AA===", 4), (b"AAA==", 4), (b"AAAA=", 4),
            (b"AAAA====", 4), (b"AA==AA==", 4),
            (b"AA=", 3), (b"AAAAAA=", 7),
        ];
        for url_safe in [false, true] {
            for padding in [Padding::Required, Padding::Optional] {
                for &(input, index) in cases {
                    assert_failure(
                        validate_base64(input, base64_policy(url_safe, padding)),
                        InvalidPadding,
                        index,
                    );
                }
            }
            for &(input, _) in cases {
                let index = input.iter().position(|&byte| byte == b'=').unwrap();
                assert_failure(
                    validate_base64(input, base64_policy(url_safe, Padding::Forbidden)),
                    InvalidPadding,
                    index,
                );
            }
        }
    }

    #[test]
    fn base64_unpadded_length_boundaries_and_required_padding() {
        let input = [b'A'; 129];
        for len in 0..=input.len() {
            for padding in PADDING_POLICIES {
                let result = validate_base64(&input[..len], base64_policy(false, padding));
                if len % 4 == 1 {
                    assert_failure(result, InvalidLength, len);
                } else if padding == Padding::Required && len % 4 != 0 {
                    assert_failure(result, InvalidPadding, len);
                } else {
                    assert_eq!(result, Ok(((len / 4) * 3 + (len % 4) * 3 / 4) as u32));
                }
            }
        }
    }

    #[test]
    fn base64_exhaustively_checks_discarded_bits_in_both_tail_sizes() {
        for url_safe in [false, true] {
            let alphabet = if url_safe { URL_SAFE } else { STANDARD };
            for prefix_len in [0, 4, 8] {
                for tail_len in [2, 3] {
                    for value in 0..64 {
                        let mut input = [b'A'; 16];
                        let data_len = prefix_len + tail_len;
                        input[data_len - 1] = alphabet[value];
                        let padded_len = prefix_len + 4;
                        input[data_len..padded_len].fill(b'=');
                        let mask = if tail_len == 2 { 15 } else { 3 };
                        for padding in PADDING_POLICIES {
                            for len in [data_len, padded_len] {
                                if (padding == Padding::Required && len == data_len)
                                    || (padding == Padding::Forbidden && len == padded_len)
                                {
                                    continue;
                                }
                                let result = validate_base64(
                                    &input[..len], base64_policy(url_safe, padding),
                                );
                                if value & mask == 0 {
                                    assert_eq!(result, Ok((prefix_len / 4 * 3 + tail_len - 1) as u32));
                                } else {
                                    assert_failure(result, NonCanonical, data_len - 1);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn base64_structure_precedes_tail_bits_and_first_byte_failure_wins() {
        assert_failure(validate_base64(b"Zh==", Base64Policy::default()), NonCanonical, 1);
        assert_failure(validate_base64(b"Zm9=", Base64Policy::default()), NonCanonical, 2);
        assert_failure(validate_base64(b"Zh=", Base64Policy::default()), InvalidPadding, 3);
        assert_failure(validate_base64(b"Zh===", Base64Policy::default()), InvalidPadding, 4);
        assert_failure(validate_base64(b"Zh=A", Base64Policy::default()), InvalidPadding, 3);
        assert_failure(validate_base64(b"!===", Base64Policy::default()), InvalidByte, 0);
        assert_failure(validate_base64(b"Zg==!", Base64Policy::default()), InvalidByte, 4);
        assert_failure(validate_base64(b"Zg==A!", Base64Policy::default()), InvalidPadding, 4);
    }

    #[test]
    fn hex_accepts_empty_numeric_mixed_case_and_fixed_length_vectors() {
        let vectors: &[(&[u8], u32)] = &[
            (b"", 0), (b"00", 1), (b"ff", 1), (b"FF", 1),
            (b"0123456789", 5), (b"abcdefABCDEF", 6),
            (b"DeAdBeEf", 4), (b"00000000", 4),
        ];
        for &(input, count) in vectors {
            assert_eq!(validate_hex(input, HexPolicy::default()), Ok(count));
            assert_eq!(validate_hex(input, HexPolicy {
                exact_bytes: Some(count), ..HexPolicy::default()
            }), Ok(count));
        }
        for case in [HexCase::Any, HexCase::Lower, HexCase::Upper] {
            let policy = HexPolicy { case, prefix: true, exact_bytes: None };
            assert_eq!(validate_hex(b"0x", policy), Ok(0));
            assert_eq!(validate_hex(b"0x0123456789", policy), Ok(5));
            assert_eq!(validate_hex(b"0x", HexPolicy {
                exact_bytes: Some(0), ..policy
            }), Ok(0));
        }
        assert_eq!(validate_hex(b"0xdeadbeef", HexPolicy {
            case: HexCase::Lower, prefix: true, exact_bytes: Some(4),
        }), Ok(4));
        assert_eq!(validate_hex(b"0xDEADBEEF", HexPolicy {
            case: HexCase::Upper, prefix: true, exact_bytes: Some(4),
        }), Ok(4));
    }

    #[test]
    fn hex_checks_all_byte_values_at_each_digit_position_and_case_policy() {
        for case in [HexCase::Any, HexCase::Lower, HexCase::Upper] {
            for prefix in [false, true] {
                let policy = HexPolicy { case, prefix, exact_bytes: Some(2) };
                let start = if prefix { 2 } else { 0 };
                let len = start + 4;
                for index in start..len {
                    for byte in 0..=u8::MAX {
                        let mut buffer = *b"0x0000";
                        if !prefix {
                            buffer[..4].fill(b'0');
                        }
                        buffer[index] = byte;
                        let result = validate_hex(&buffer[..len], policy);
                        if b"0123456789".contains(&byte)
                            || (b"abcdef".contains(&byte) && case != HexCase::Upper)
                            || (b"ABCDEF".contains(&byte) && case != HexCase::Lower)
                        {
                            assert_eq!(result, Ok(2));
                        } else if b"abcdefABCDEF".contains(&byte) {
                            assert_failure(result, NonCanonical, index);
                        } else {
                            assert_failure(result, InvalidByte, index);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn hex_requires_exact_lowercase_prefix_and_never_accepts_it_implicitly() {
        let policy = HexPolicy { prefix: true, ..HexPolicy::default() };
        for &(input, index) in &[
            (&b"0X"[..], 1), (&b"0X00"[..], 1), (&b"1x00"[..], 0),
            (&b"xx00"[..], 0), (&b"0y00"[..], 1), (&b"0\0"[..], 1),
            (&b"\xffx00"[..], 0), (&b"0\xff00"[..], 1),
            (&b"0000"[..], 1), (&b"0x0x"[..], 3),
        ] {
            assert_failure(validate_hex(input, policy), InvalidByte, index);
        }
        for input in [&b""[..], &b"0"[..], &b"0x0"[..]] {
            assert_failure(validate_hex(input, policy), InvalidLength, input.len());
        }
        for case in [HexCase::Any, HexCase::Lower, HexCase::Upper] {
            for input in [&b"0x"[..], &b"0X"[..], &b"0x00"[..], &b"0XFF"[..]] {
                assert_failure(validate_hex(input, HexPolicy {
                    case, ..HexPolicy::default()
                }), InvalidByte, 1);
            }
        }
    }

    #[test]
    fn hex_rejects_odd_and_wrong_exact_lengths_before_reading_digits() {
        let input = [b'!'; 66];
        for prefix in [false, true] {
            for len in 0..=input.len() {
                let policy = HexPolicy { prefix, ..HexPolicy::default() };
                if len < (if prefix { 2 } else { 0 }) || len % 2 != 0 {
                    assert_failure(validate_hex(&input[..len], policy), InvalidLength, len);
                }
                for exact in [0, 1, 2, 20, 32, u32::MAX / 2, u32::MAX] {
                    let policy = HexPolicy { exact_bytes: Some(exact), ..policy };
                    let expected = u64::from(exact) * 2 + if prefix { 2 } else { 0 };
                    if len as u64 != expected {
                        assert_failure(validate_hex(&input[..len], policy), InvalidLength, len);
                    }
                }
            }
        }
        assert_failure(validate_hex(b"0", HexPolicy::default()), InvalidLength, 1);
        assert_failure(validate_hex(b"", HexPolicy {
            exact_bytes: Some(1), ..HexPolicy::default()
        }), InvalidLength, 0);
        assert_failure(validate_hex(b"00", HexPolicy {
            exact_bytes: Some(0), ..HexPolicy::default()
        }), InvalidLength, 2);
    }

    #[test]
    fn hex_rejects_whitespace_separators_controls_and_non_ascii() {
        let cases: &[(&[u8], usize)] = &[
            (b" 0", 0), (b"00 0", 2), (b"00\n0", 2),
            (b"0\t", 1), (b"0\r", 1), (b"0\0", 1),
            (b"0g", 1), (b"0G", 1), (b"+0", 0), (b"-0", 0),
            (b"0_", 1), (b"0:", 1), (b"\xff0", 0),
            (b"\xc2\xa0", 0), (b"\xef\xbc\x90\x30", 0),
        ];
        for &(input, index) in cases {
            // Odd total lengths are structurally invalid before digit checks.
            if input.len() % 2 != 0 {
                assert_failure(validate_hex(input, HexPolicy::default()), InvalidLength, input.len());
            } else {
                assert_failure(validate_hex(input, HexPolicy::default()), InvalidByte, index);
            }
        }
        assert_failure(validate_hex(b"aA!0", HexPolicy {
            case: HexCase::Upper, ..HexPolicy::default()
        }), NonCanonical, 0);
        assert_failure(validate_hex(b"Aa!0", HexPolicy {
            case: HexCase::Lower, ..HexPolicy::default()
        }), NonCanonical, 0);
        assert_failure(validate_hex(b"!Aa0", HexPolicy {
            case: HexCase::Lower, ..HexPolicy::default()
        }), InvalidByte, 0);
    }

    #[test]
    fn hex_fixed_address_and_hash_policies_have_exact_counts() {
        for count in [20u32, 32] {
            let policy = HexPolicy {
                case: HexCase::Any, prefix: true, exact_bytes: Some(count),
            };
            let mut input = [b'f'; 68];
            input[..2].copy_from_slice(b"0x");
            let len = 2 + count as usize * 2;
            assert_eq!(validate_hex(&input[..len], policy), Ok(count));
            assert_failure(validate_hex(&input[..len - 1], policy), InvalidLength, len - 1);
            assert_failure(validate_hex(&input[..len + 2], policy), InvalidLength, len + 2);
            input[len - 1] = b'!';
            assert_failure(validate_hex(&input[..len], policy), InvalidByte, len - 1);
        }
    }

    #[test]
    fn validators_observe_slice_boundaries_and_report_relative_indices() {
        let base64 = b"!Zg==?";
        assert_eq!(validate_base64(&base64[1..5], Base64Policy::default()), Ok(1));
        assert_eq!(validate_base64(&base64[1..3], base64_policy(false, Padding::Optional)), Ok(1));
        assert_failure(validate_base64(&base64[1..4], Base64Policy::default()), InvalidPadding, 3);
        let bad_base64 = b"!Zh==?";
        assert_failure(validate_base64(&bad_base64[1..5], Base64Policy::default()), NonCanonical, 1);

        let hex = b"!0xDeAd?";
        let policy = HexPolicy { prefix: true, ..HexPolicy::default() };
        assert_eq!(validate_hex(&hex[1..7], policy), Ok(2));
        assert_eq!(validate_hex(&hex[3..7], HexPolicy::default()), Ok(2));
        assert_failure(validate_hex(&hex[1..6], policy), InvalidLength, 5);
        let bad_hex = b"!0xDeAg?";
        assert_failure(validate_hex(&bad_hex[1..7], policy), InvalidByte, 5);
        assert_eq!(validate_hex(&hex[3..3], HexPolicy::default()), Ok(0));
        assert_eq!(validate_base64(&base64[1..1], Base64Policy::default()), Ok(0));
    }

    #[test]
    fn checked_length_arithmetic_does_not_wrap_and_counts_use_all_u32_bits() {
        assert_eq!(hex_encoded_len(0, 0), Some(0));
        assert_eq!(hex_encoded_len(0, 2), Some(2));
        assert_eq!(hex_encoded_len(32, 2), Some(66));
        assert_eq!(hex_encoded_len(1, usize::MAX), None);
        assert_eq!(decoded_count(0, 0), Ok(0));
        if let Ok(max) = usize::try_from(u32::MAX) {
            assert_eq!(decoded_count(max, 17), Ok(u32::MAX));
            if let Some(overflow) = max.checked_add(1) {
                assert_failure(decoded_count(overflow, 17), Limit, 17);
                assert_failure(decoded_count(overflow, overflow), Limit, overflow);
                assert_eq!(decoded_count(overflow, overflow).unwrap_err().index, u32::MAX);
            }
        }
        let expected = u64::from(u32::MAX) * 2 + 2;
        assert_eq!(hex_encoded_len(u32::MAX, 2), usize::try_from(expected).ok());
        assert_failure(validate_hex(b"00", HexPolicy {
            exact_bytes: Some(u32::MAX), ..HexPolicy::default()
        }), InvalidLength, 2);
    }
}
