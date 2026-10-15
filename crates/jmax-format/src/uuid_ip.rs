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
