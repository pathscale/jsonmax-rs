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
