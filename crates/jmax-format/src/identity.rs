//! Allocation-free NanoId, blockchain address, and transaction hash validation.
//!
//! Inputs are decoded string bytes. Length is checked before contents, and
//! failure indices are relative to the supplied slice. No normalization,
//! whitespace trimming, decoding, or allocation is performed.

use crate::binary::validate_hex;
use crate::{FormatFailure, FormatFailureKind, FormatResult, HexCase, HexPolicy};

/// Validates exactly `len` ASCII letters, decimal digits, underscores, or hyphens.
///
/// A zero configured length is invalid, including for empty input. Length
/// failures name the slice end; otherwise the first invalid byte is reported.
/// Acceptance makes no claim about randomness, entropy, or uniqueness.
pub fn validate_nanoid(input: &[u8], len: u8) -> FormatResult<()> {
    if len == 0 || input.len() != usize::from(len) {
        return Err(FormatFailure::new(
            FormatFailureKind::InvalidLength,
            input.len(),
        ));
    }

    for (index, &byte) in input.iter().enumerate() {
        if !matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-') {
            return Err(FormatFailure::new(FormatFailureKind::InvalidByte, index));
        }
    }
    Ok(())
}

/// Validates a nonempty run of ASCII letters, decimal digits, underscores or hyphens.
///
/// The length is not constrained here; a schema bounds it with its own length limits.
/// Empty input fails with InvalidLength at 0; otherwise the first invalid byte is reported.
pub fn validate_nanoid_chars(input: &[u8]) -> FormatResult<()> {
    if input.is_empty() {
        return Err(FormatFailure::new(FormatFailureKind::InvalidLength, 0));
    }
    match input.iter().position(|byte| !matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-')) {
        Some(index) => Err(FormatFailure::new(FormatFailureKind::InvalidByte, index)),
        None => Ok(()),
    }
}

/// Validates a nonempty run of ASCII letters and decimal digits (the Base62 alphabet).
///
/// Failures are reported as in [`validate_nanoid_chars`].
pub fn validate_base62(input: &[u8]) -> FormatResult<()> {
    if input.is_empty() {
        return Err(FormatFailure::new(FormatFailureKind::InvalidLength, 0));
    }
    match input.iter().position(|byte| !byte.is_ascii_alphanumeric()) {
        Some(index) => Err(FormatFailure::new(FormatFailureKind::InvalidByte, index)),
        None => Ok(()),
    }
}

/// Validates exactly lowercase `0x` followed by 40 hex digits of either case.
///
/// This is lexical validation only. It does not verify a chain, checksum,
/// signature, existence, or authorization. Length failures name the slice end;
/// prefix and digit failures name the first invalid byte.
pub fn validate_address(input: &[u8]) -> FormatResult<()> {
    validate_hex(input, HexPolicy {
        case: HexCase::Any,
        prefix: true,
        exact_bytes: Some(20),
    }).map(|_| ())
}

/// Validates exactly lowercase `0x` followed by 64 hex digits of either case.
///
/// This is lexical validation only. It does not verify a chain, checksum,
/// signature, existence, or authorization. Length failures name the slice end;
/// prefix and digit failures name the first invalid byte.
pub fn validate_transaction_hash(input: &[u8]) -> FormatResult<()> {
    validate_hex(input, HexPolicy {
        case: HexCase::Any,
        prefix: true,
        exact_bytes: Some(32),
    }).map(|_| ())
}
