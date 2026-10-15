//! Allocation-free UUID and IP address validation over decoded string bytes.
//!
//! No normalization, address resolution, or platform address parser is used.
//! Component widths and missing or excess address groups are length failures,
//! reported at the end of the supplied slice. Invalid characters, separators,
//! and out-of-range IPv4 octets are byte failures at the offending byte.

use crate::{FormatFailure, FormatFailureKind, FormatResult};

/// Validates the 36-byte, hyphenated UUID spelling with either hex case.
///
/// All version and variant bits are accepted, including the nil UUID. Length
/// is checked before characters; braces, prefixes, and compact forms fail.
pub fn validate_uuid(input: &[u8]) -> FormatResult<()> {
    if input.len() != 36 {
        return Err(length_failure(input));
    }

    for (index, &byte) in input.iter().enumerate() {
        let valid = if matches!(index, 8 | 13 | 18 | 23) {
            byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        };
        if !valid {
            return Err(FormatFailure::new(FormatFailureKind::InvalidByte, index));
        }
    }
    Ok(())
}

/// Validates four decimal IPv4 octets without leading zeros or extra syntax.
///
/// A leading-zero octet fails at its second digit with `NonCanonical`.
pub fn validate_ipv4(input: &[u8]) -> FormatResult<()> {
    validate_ipv4_from(input, 0)
}

/// Validates IPv6 hex groups, one optional `::`, and a terminal IPv4 tail.
///
/// Compression must replace at least one of eight groups. A dotted tail uses
/// two groups and follows exactly the same rules as [`validate_ipv4`]. Zones,
/// brackets, network prefixes, whitespace, and ports are not accepted.
pub fn validate_ipv6(input: &[u8]) -> FormatResult<()> {
    if input.is_empty() {
        return Err(length_failure(input));
    }

    let mut position = 0;
    let mut groups = 0;
    let mut compressed = false;

    if input[0] == b':' {
        if input.get(1) != Some(&b':') {
            return Err(FormatFailure::new(FormatFailureKind::InvalidByte, 0));
        }
        compressed = true;
        position = 2;
    }

    while position < input.len() {
        if input[position] == b':' {
            return Err(FormatFailure::new(
                FormatFailureKind::InvalidByte,
                position,
            ));
        }

        let start = position;
        // Identify a dotted tail before applying hex-group width rules: an
        // IPv4 octet such as 001 must retain its NonCanonical diagnostic.
        let mut end = start;
        while end < input.len() && !matches!(input[end], b':' | b'.') {
            end += 1;
        }

        if input.get(end) == Some(&b'.') {
            // Keep the original slice so all failures, including length
            // failures, already have their full-input indices.
            validate_ipv4_from(input, start)?;
            groups += 2;
            break;
        }

        for (offset, &byte) in input[start..end].iter().enumerate() {
            if !byte.is_ascii_hexdigit() {
                return Err(FormatFailure::new(
                    FormatFailureKind::InvalidByte,
                    start + offset,
                ));
            }
            if offset == 4 {
                return Err(length_failure(input));
            }
        }

        groups += 1;
        if groups > 8 || (compressed && groups == 8) {
            return Err(length_failure(input));
        }
        if end == input.len() {
            break;
        }

        // `end` names a colon. Only a checked second colon can advance the
        // cursor by two, so the cursor never exceeds the input length.
        if input.get(end + 1) == Some(&b':') {
            if compressed {
                return Err(FormatFailure::new(FormatFailureKind::InvalidByte, end));
            }
            compressed = true;
            if groups == 8 {
                return Err(length_failure(input));
            }
            position = end + 2;
        } else {
            position = end + 1;
            if position == input.len() {
                return Err(length_failure(input));
            }
        }
    }

    if (compressed && groups >= 8) || (!compressed && groups != 8) {
        return Err(length_failure(input));
    }
    Ok(())
}

/// Validates IPv6 if any colon occurs, otherwise validates IPv4.
///
/// The entire slice is always the address; no port or prefix is removed.
pub fn validate_ip(input: &[u8]) -> FormatResult<()> {
    if input.contains(&b':') {
        validate_ipv6(input)
    } else {
        validate_ipv4(input)
    }
}

fn length_failure(input: &[u8]) -> FormatFailure {
    FormatFailure::new(FormatFailureKind::InvalidLength, input.len())
}

// `start` is either zero or the start of an IPv6 component inside `input`.
// Parsing the suffix directly avoids allocating or translating error indices.
fn validate_ipv4_from(input: &[u8], start: usize) -> FormatResult<()> {
    let mut position = start;
    for octet in 0..4 {
        if position == input.len() {
            return Err(length_failure(input));
        }

        let first = position;
        let mut value = 0u16;
        while let Some(&byte) = input.get(position) {
            if !byte.is_ascii_digit() {
                break;
            }
            if position > first && input[first] == b'0' {
                return Err(FormatFailure::new(
                    FormatFailureKind::NonCanonical,
                    position,
                ));
            }
            if position - first == 3 {
                return Err(length_failure(input));
            }
            // At most three digits are accumulated, so u16 cannot overflow.
            value = value * 10 + u16::from(byte - b'0');
            if value > 255 {
                return Err(FormatFailure::new(
                    FormatFailureKind::InvalidByte,
                    position,
                ));
            }
            position += 1;
        }

        if position == first {
            return Err(FormatFailure::new(
                FormatFailureKind::InvalidByte,
                position,
            ));
        }

        if octet < 3 {
            if position == input.len() {
                return Err(length_failure(input));
            }
            if input[position] != b'.' {
                return Err(FormatFailure::new(
                    FormatFailureKind::InvalidByte,
                    position,
                ));
            }
            position += 1;
        } else if position < input.len() {
            return Err(if input[position] == b'.' {
                length_failure(input)
            } else {
                FormatFailure::new(FormatFailureKind::InvalidByte, position)
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FormatFailureKind::{InvalidByte, InvalidLength, NonCanonical};

    fn assert_failure(
        result: FormatResult<()>,
        kind: FormatFailureKind,
        index: usize,
    ) {
        assert_eq!(result, Err(FormatFailure::new(kind, index)));
    }

    #[test]
    fn uuid_accepts_all_hex_cases_and_unrestricted_bits() {
        for input in [
            &b"00000000-0000-0000-0000-000000000000"[..],
            &b"ffffffff-ffff-ffff-ffff-ffffffffffff"[..],
            &b"FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF"[..],
            &b"01234567-89ab-cdef-ABCD-0123456789aF"[..],
            &b"550e8400-e29b-41d4-a716-446655440000"[..],
        ] {
            assert_eq!(validate_uuid(input), Ok(()));
        }
    }

    #[test]
    fn uuid_rejects_every_wrong_length_before_inspecting_bytes() {
        let bytes = [b'!'; 80];
        for len in 0..=bytes.len() {
            if len != 36 {
                assert_failure(validate_uuid(&bytes[..len]), InvalidLength, len);
            }
        }
        for input in [
            &b"00000000000000000000000000000000"[..],
            &b"{00000000-0000-0000-0000-000000000000}"[..],
            &b"urn:uuid:00000000-0000-0000-0000-000000000000"[..],
            &b" 00000000-0000-0000-0000-000000000000"[..],
            &b"00000000-0000-0000-0000-000000000000\n"[..],
        ] {
            assert_failure(validate_uuid(input), InvalidLength, input.len());
        }
    }

    #[test]
    fn uuid_exhaustively_checks_every_byte_at_every_position() {
        let valid = *b"01234567-89ab-cdef-0123-456789abcdef";
        for index in 0..valid.len() {
            for byte in 0..=u8::MAX {
                let mut input = valid;
                input[index] = byte;
                let allowed = if matches!(index, 8 | 13 | 18 | 23) {
                    byte == b'-'
                } else {
                    matches!(byte, b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F')
                };
                if allowed {
                    assert_eq!(validate_uuid(&input), Ok(()));
                } else {
                    assert_failure(validate_uuid(&input), InvalidByte, index);
                }
            }
        }
        let mut multiple_errors = valid;
        multiple_errors[2] = b'g';
        multiple_errors[8] = b'_';
        assert_failure(validate_uuid(&multiple_errors), InvalidByte, 2);
    }

    #[test]
    fn ipv4_accepts_boundary_and_representative_addresses() {
        for input in [
            &b"0.0.0.0"[..],
            &b"255.255.255.255"[..],
            &b"1.2.3.4"[..],
            &b"10.20.30.40"[..],
            &b"127.0.0.1"[..],
            &b"192.0.2.1"[..],
            &b"198.51.100.254"[..],
            &b"203.0.113.255"[..],
            &b"9.99.100.249"[..],
        ] {
            assert_eq!(validate_ipv4(input), Ok(()));
            assert_eq!(validate_ip(input), Ok(()));
        }
    }

    // Values supplied by the tests are in 0..=999. No allocation or formatting
    // machinery is needed to exercise every three-digit octet value.
    fn append_decimal(buffer: &mut [u8], position: &mut usize, value: u16) {
        if value >= 100 {
            buffer[*position] = b'0' + (value / 100) as u8;
            *position += 1;
        }
        if value >= 10 {
            buffer[*position] = b'0' + ((value / 10) % 10) as u8;
            *position += 1;
        }
        buffer[*position] = b'0' + (value % 10) as u8;
        *position += 1;
    }

    #[test]
    fn ipv4_exhaustively_checks_octet_values_in_each_position() {
        for selected in 0..4 {
            for value in 0..=999u16 {
                let mut buffer = [0u8; 15];
                let mut len = 0;
                let mut selected_start = 0;
                for octet in 0..4 {
                    if octet != 0 {
                        buffer[len] = b'.';
                        len += 1;
                    }
                    if octet == selected {
                        selected_start = len;
                        append_decimal(&mut buffer, &mut len, value);
                    } else {
                        append_decimal(&mut buffer, &mut len, 1);
                    }
                }
                let input = &buffer[..len];
                if value <= 255 {
                    assert_eq!(validate_ipv4(input), Ok(()));
                } else {
                    assert_failure(validate_ipv4(input), InvalidByte, selected_start + 2);
                }
            }
        }
    }

    #[test]
    fn ipv4_reports_missing_excess_and_overlong_content_at_input_end() {
        for input in [
            &b""[..], &b"1"[..], &b"1.2"[..], &b"1.2.3"[..],
            &b"1.2.3."[..], &b"1.2.3.4."[..], &b"1.2.3.4.5"[..],
            &b"1234.2.3.4"[..], &b"1.1234.3.4"[..],
            &b"1.2.1234.4"[..], &b"1.2.3.1234"[..],
        ] {
            assert_failure(validate_ipv4(input), InvalidLength, input.len());
        }
    }

    #[test]
    fn ipv4_reports_structure_range_and_noncanonical_errors() {
        let cases: &[(&[u8], FormatFailureKind, usize)] = &[
            (b".1.2.3", InvalidByte, 0),
            (b"1..2.3", InvalidByte, 2),
            (b"1.2..3", InvalidByte, 4),
            (b"1.2.3..", InvalidByte, 6),
            (b"256.1.2.3", InvalidByte, 2),
            (b"1.256.2.3", InvalidByte, 4),
            (b"1.2.256.3", InvalidByte, 6),
            (b"1.2.3.256", InvalidByte, 8),
            (b"999.1.2.3", InvalidByte, 2),
            (b"01.2.3.4", NonCanonical, 1),
            (b"1.02.3.4", NonCanonical, 3),
            (b"1.2.03.4", NonCanonical, 5),
            (b"1.2.3.04", NonCanonical, 7),
            (b"0000.1.2.3", NonCanonical, 1),
            (b"+1.2.3.4", InvalidByte, 0),
            (b"-1.2.3.4", InvalidByte, 0),
            (b" 1.2.3.4", InvalidByte, 0),
            (b"1.2.3.4 ", InvalidByte, 7),
            (b"1.2.3.4\n", InvalidByte, 7),
            (b"1,2.3.4", InvalidByte, 1),
            (b"0x1.2.3.4", InvalidByte, 1),
            (b"1.2.3.4:80", InvalidByte, 7),
            (b"1.2.3.4/24", InvalidByte, 7),
            (b"1.2.3.4\0", InvalidByte, 7),
            (b"1.2.3.\xff", InvalidByte, 6),
            (b"\xc2\xb9.2.3.4", InvalidByte, 0),
        ];
        for &(input, kind, index) in cases {
            assert_failure(validate_ipv4(input), kind, index);
        }
    }

    #[test]
    fn ipv4_requires_ascii_dots_at_each_separator() {
        for index in [1, 3, 5] {
            for byte in 0..=u8::MAX {
                let mut input = *b"1.2.3.4";
                input[index] = byte;
                assert_eq!(validate_ipv4(&input).is_ok(), byte == b'.');
            }
        }
    }

    #[test]
    fn ipv6_accepts_full_compressed_and_mixed_addresses() {
        for input in [
            &b"::"[..], &b"::1"[..], &b"1::"[..],
            &b"0:0:0:0:0:0:0:0"[..],
            &b"1:2:3:4:5:6:7:8"[..],
            &b"ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"[..],
            &b"ABCD:ef01:2345:6789:aBcD:EF01:2345:6789"[..],
            &b"2001:db8::1"[..], &b"fe80::abcd"[..],
            &b"1::2:3:4:5:6:7"[..], &b"1:2:3:4:5:6::7"[..],
            &b"::192.0.2.1"[..], &b"::ffff:192.0.2.1"[..],
            &b"0:0:0:0:0:0:0.0.0.0"[..],
            &b"1:2:3:4:5:6:255.255.255.255"[..],
            &b"ffff:ffff:ffff:ffff:ffff:ffff:255.255.255.255"[..],
            &b"2001:db8:0:0:0:0:192.0.2.1"[..],
            &b"1:2:3:4:5::192.0.2.1"[..],
            &b"1::2:3:4:5:192.0.2.1"[..],
        ] {
            assert_eq!(validate_ipv6(input), Ok(()), "input: {:?}", input);
            assert_eq!(validate_ip(input), Ok(()));
        }
    }

    fn append_hex_groups(buffer: &mut [u8], position: &mut usize, groups: usize) {
        for group in 0..groups {
            if group != 0 {
                buffer[*position] = b':';
                *position += 1;
            }
            buffer[*position..*position + 4].copy_from_slice(b"aBcD");
            *position += 4;
        }
    }

    #[test]
    fn ipv6_compression_accepts_every_legal_position_and_group_count() {
        for groups in 0..=7 {
            for left in 0..=groups {
                let mut buffer = [0u8; 39];
                let mut len = 0;
                append_hex_groups(&mut buffer, &mut len, left);
                buffer[len..len + 2].copy_from_slice(b"::");
                len += 2;
                append_hex_groups(&mut buffer, &mut len, groups - left);
                assert_eq!(validate_ipv6(&buffer[..len]), Ok(()));
            }
        }

        for groups in 0..=5 {
            for left in 0..=groups {
                let mut buffer = [0u8; 45];
                let mut len = 0;
                append_hex_groups(&mut buffer, &mut len, left);
                buffer[len..len + 2].copy_from_slice(b"::");
                len += 2;
                append_hex_groups(&mut buffer, &mut len, groups - left);
                if groups != left {
                    buffer[len] = b':';
                    len += 1;
                }
                let tail = b"255.255.255.255";
                buffer[len..len + tail.len()].copy_from_slice(tail);
                len += tail.len();
                assert_eq!(validate_ipv6(&buffer[..len]), Ok(()));
            }
        }
    }

    #[test]
    fn ipv6_reports_missing_excess_and_overlong_content_at_input_end() {
        for input in [
            &b""[..], &b"1"[..], &b"1:2:3:4:5:6:7"[..],
            &b"1:2:3:4:5:6:7:8:9"[..], &b"1:2:3:4:5:6:7:8:"[..],
            &b"1:2:3:4:5:6:7:"[..], &b"1:"[..], &b"::1:"[..],
            &b"1:2:3:4:5:6:7:8::"[..], &b"::1:2:3:4:5:6:7:8"[..],
            &b"1:2:3:4::5:6:7:8"[..], &b"::12345"[..],
            &b"12345::"[..], &b"::00000"[..],
            &b"192.0.2.1"[..], &b"1:2:3:4:5:192.0.2.1"[..],
            &b"1:2:3:4:5:6:7:192.0.2.1"[..],
            &b"1:2:3:4:5:6::192.0.2.1"[..],
            &b"::1:2:3:4:5:6:192.0.2.1"[..],
            &b"::ffff:192.0.2"[..], &b"::ffff:192.0.2."[..],
            &b"::ffff:192.0.2.1.2"[..], &b"::ffff:1234.0.2.1"[..],
        ] {
            assert_failure(validate_ipv6(input), InvalidLength, input.len());
        }
    }

    #[test]
    fn ipv6_reports_structure_and_forbidden_syntax_at_offending_byte() {
        let cases: &[(&[u8], usize)] = &[
            (b":", 0), (b":1", 0), (b":1:2:3:4:5:6:7", 0),
            (b":::1", 2), (b"1:::2", 3), (b"1::::2", 3),
            (b"::1::", 3), (b"1::2::3", 4), (b"1:2::3::4", 6),
            (b"::g", 2), (b"::ffffg", 6), (b"::0x1", 3),
            (b"::+1", 2), (b"::-1", 2),
            (b"[::1]", 0), (b"::1%lo0", 3), (b"fe80::1%2", 7),
            (b"::1/128", 3), (b"::1 ", 3), (b" ::1", 0),
            (b"::1\n", 3), (b"::1\0", 3), (b"::\xff", 2),
            (b"v1.fe80", 0), (b"::ffff:192..2.1", 11),
            (b"::ffff:.0.2.1", 7), (b"::ffff:192.0.2.1:80", 16),
            (b"::ffff:192.0.2.1::", 16),
            (b"::ffff:192.0.2.1/24", 16),
            (b"::ffff:192.0.2.1%eth0", 16),
            (b"::ffff:192.0.2.\xff", 15),
        ];
        for &(input, index) in cases {
            assert_failure(validate_ipv6(input), InvalidByte, index);
        }
    }

    #[test]
    fn ipv6_dotted_tails_preserve_ipv4_rules_and_full_input_indices() {
        let prefix = b"::ffff:";
        for suffix in [
            &b"0.0.0.0"[..], &b"255.255.255.255"[..],
            &b"01.2.3.4"[..], &b"1.02.3.4"[..], &b"1.2.03.4"[..],
            &b"1.2.3.04"[..], &b"00000.1.2.3"[..],
            &b"256.1.2.3"[..], &b"1.2.3.256"[..],
            &b"1234.2.3.4"[..], &b"1.2.3"[..], &b"1.2.3.4.5"[..],
            &b"1..3.4"[..], &b"1.2.3.4:80"[..], &b"a.2.3.4"[..],
        ] {
            let mut buffer = [0u8; 40];
            buffer[..prefix.len()].copy_from_slice(prefix);
            let len = prefix.len() + suffix.len();
            buffer[prefix.len()..len].copy_from_slice(suffix);
            let expected = validate_ipv4(suffix).map_err(|failure| {
                FormatFailure::new(failure.kind, prefix.len() + failure.index as usize)
            });
            assert_eq!(validate_ipv6(&buffer[..len]), expected);
        }
    }

    #[test]
    fn ip_dispatches_only_on_colon_and_rejects_ports() {
        for input in [
            &b""[..], &b"1.2.3.4"[..], &b"::1"[..],
            &b"1.2.3.4:80"[..], &b"127.0.0.1:0"[..],
            &b"[::1]:443"[..], &b"[::1]"[..],
            &b"example.com:443"[..], &b"example.com"[..],
            &b"localhost"[..], &b"1.2.3.4/24"[..],
            &b"::1/128"[..], &b"1.2.3.4\0"[..], &b"\xff:\xfe"[..],
        ] {
            let expected = if input.contains(&b':') {
                validate_ipv6(input)
            } else {
                validate_ipv4(input)
            };
            assert_eq!(validate_ip(input), expected);
        }
        for input in [
            &b"1.2.3.4:80"[..], &b"127.0.0.1:0"[..],
            &b"[::1]:443"[..], &b"example.com:443"[..],
        ] {
            assert!(validate_ip(input).is_err());
        }
    }

    #[test]
    fn validators_respect_slice_boundaries_and_relative_indices() {
        let uuid = b"!00000000-0000-0000-0000-000000000000?";
        assert_eq!(validate_uuid(&uuid[1..37]), Ok(()));
        let ipv4 = b"!192.0.2.1:80";
        assert_eq!(validate_ipv4(&ipv4[1..10]), Ok(()));
        let ipv6 = b"!::ffff:192.0.2.1%zone";
        assert_eq!(validate_ipv6(&ipv6[1..17]), Ok(()));
        assert_eq!(validate_ip(&ipv6[1..17]), Ok(()));
        let invalid = b"!::ffff:192.0.2.256?";
        assert_failure(validate_ipv6(&invalid[1..19]), InvalidByte, 17);
        let truncated = b"!1.2.3.4?";
        assert_failure(validate_ipv4(&truncated[1..6]), InvalidLength, 5);
    }

    #[test]
    fn arbitrary_single_bytes_and_truncated_inputs_do_not_succeed() {
        for byte in 0..=u8::MAX {
            let input = [byte];
            assert!(validate_uuid(&input).is_err());
            assert!(validate_ipv4(&input).is_err());
            assert!(validate_ipv6(&input).is_err());
            assert!(validate_ip(&input).is_err());
        }
        let uuid = b"00000000-0000-0000-0000-000000000000";
        for len in 0..uuid.len() {
            assert_failure(validate_uuid(&uuid[..len]), InvalidLength, len);
        }
        let ipv4 = b"255.255.255.255";
        for len in 0..ipv4.len() {
            let result = validate_ipv4(&ipv4[..len]);
            // The final octet may end after one or two digits and still be a
            // complete valid address. Every earlier truncation is incomplete.
            assert_eq!(result.is_ok(), len == 13 || len == 14);
        }
    }
}
