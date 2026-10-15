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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FormatFailureKind::{InvalidByte, InvalidLength};

    type Validator = fn(&[u8]) -> FormatResult<()>;

    const FIXED_FORMATS: [(Validator, usize); 2] = [
        (validate_address, 42),
        (validate_transaction_hash, 66),
    ];
    const NANOID_ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
    const HEX_ALPHABET: &[u8; 22] = b"0123456789abcdefABCDEF";

    fn assert_failure(result: FormatResult<()>, kind: FormatFailureKind, index: usize) {
        assert_eq!(result, Err(FormatFailure::new(kind, index)));
    }

    #[test]
    fn nanoid_accepts_deterministic_vectors_and_the_complete_alphabet() {
        for &(input, len) in &[
            (&b"A"[..], 1),
            (&b"z"[..], 1),
            (&b"0"[..], 1),
            (&b"_"[..], 1),
            (&b"-"[..], 1),
            (&b"_-0123456789aAzZ"[..], 16),
            (&b"V1StGXR8_Z5jdHi6B-myT"[..], 21),
            (&b"_____________________"[..], 21),
        ] {
            assert_eq!(validate_nanoid(input, len), Ok(()));
        }
        assert_eq!(validate_nanoid(NANOID_ALPHABET, 64), Ok(()));
        assert_eq!(validate_nanoid(&[b'-'; 255], u8::MAX), Ok(()));
    }

    #[test]
    fn nanoid_checks_every_configured_length_and_all_nearby_input_lengths() {
        let valid = [b'A'; 256];
        let invalid = [b'!'; 256];
        for configured in 1..=u8::MAX {
            let expected_len = usize::from(configured);
            for len in 0..=valid.len() {
                if len == expected_len {
                    assert_eq!(validate_nanoid(&valid[..len], configured), Ok(()));
                    assert_failure(validate_nanoid(&invalid[..len], configured), InvalidByte, 0);
                } else {
                    assert_failure(validate_nanoid(&valid[..len], configured), InvalidLength, len);
                    assert_failure(validate_nanoid(&invalid[..len], configured), InvalidLength, len);
                }
            }
        }
    }

    #[test]
    fn nanoid_zero_length_is_always_invalid() {
        let input = [b'A'; 256];
        for len in 0..=input.len() {
            assert_failure(validate_nanoid(&input[..len], 0), InvalidLength, len);
        }
        assert_failure(validate_nanoid(b"!", 0), InvalidLength, 1);
    }

    #[test]
    fn nanoid_checks_every_byte_at_every_position_in_boundary_lengths() {
        for len in [1u8, 21, u8::MAX] {
            for index in 0..usize::from(len) {
                for byte in 0..=u8::MAX {
                    let mut input = [b'A'; 255];
                    input[index] = byte;
                    let result = validate_nanoid(&input[..usize::from(len)], len);
                    if NANOID_ALPHABET.contains(&byte) {
                        assert_eq!(result, Ok(()));
                    } else {
                        assert_failure(result, InvalidByte, index);
                    }
                }
            }
        }
    }

    #[test]
    fn nanoid_rejects_unicode_raw_escapes_quotes_and_whitespace() {
        let cases: &[(&[u8], u8, usize)] = &[
            (b" A", 2, 0), (b"A ", 2, 1), (b"A\t", 2, 1),
            (b"A\r\n", 3, 1), (b"A\0", 2, 1), (b"A\xff", 2, 1),
            (b"A\xc3\xa9", 3, 1), (b"\xef\xbc\xa1", 3, 0),
            (b"A\xf0\x9f\x98\x80", 5, 1), (b"A\xc2\xa0", 3, 1),
            (b"\\u0041", 6, 0), (b"\"A\"", 3, 0),
            (b"A+B", 3, 1), (b"A/B", 3, 1), (b"A=B", 3, 1),
        ];
        for &(input, len, index) in cases {
            assert_failure(validate_nanoid(input, len), InvalidByte, index);
        }
        assert_eq!(validate_nanoid(b"A", 1), Ok(()));
        assert_failure(validate_nanoid(b"\xc3\xa9", 1), InvalidLength, 2);
    }

    #[test]
    fn nanoid_reports_the_first_invalid_byte_after_the_length_check() {
        assert_failure(validate_nanoid(b"A!B?C", 5), InvalidByte, 1);
        assert_failure(validate_nanoid(b"A!B?C", 4), InvalidLength, 5);
        assert_failure(validate_nanoid(b"!AB", 3), InvalidByte, 0);
        assert_failure(validate_nanoid(b"AB!", 3), InvalidByte, 2);
    }

    #[test]
    fn fixed_formats_accept_zero_maximum_and_unrestricted_hex_cases() {
        assert_eq!(
            validate_address(b"0x52908400098527886E0F7030069857D2E4169EE7"),
            Ok(()),
        );
        assert_eq!(
            validate_address(b"0x0123456789abcdefABCDEF0123456789abcdefAB"),
            Ok(()),
        );
        assert_eq!(
            validate_transaction_hash(
                b"0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            ),
            Ok(()),
        );
        for (validate, len) in FIXED_FORMATS {
            for digit in [b'0', b'f', b'F'] {
                let mut input = [digit; 66];
                input[..2].copy_from_slice(b"0x");
                assert_eq!(validate(&input[..len]), Ok(()));
            }
            let mut mixed = [b'0'; 66];
            mixed[..2].copy_from_slice(b"0x");
            for (index, byte) in mixed[2..len].iter_mut().enumerate() {
                *byte = HEX_ALPHABET[index % HEX_ALPHABET.len()];
            }
            // Arbitrary mixed case is accepted without a checksum policy.
            assert_eq!(validate(&mixed[..len]), Ok(()));
        }
    }

    #[test]
    fn fixed_formats_reject_all_wrong_lengths_before_prefix_or_digits() {
        for (validate, expected_len) in FIXED_FORMATS {
            for first in [b'0', b'!'] {
                let mut input = [b'0'; 132];
                input[0] = first;
                input[1] = b'x';
                for len in 0..=input.len() {
                    if len != expected_len {
                        assert_failure(validate(&input[..len]), InvalidLength, len);
                    } else if first == b'0' {
                        assert_eq!(validate(&input[..len]), Ok(()));
                    } else {
                        assert_failure(validate(&input[..len]), InvalidByte, 0);
                    }
                }
            }
        }
    }

    #[test]
    fn fixed_formats_check_every_byte_in_both_prefix_and_all_digit_positions() {
        for (validate, len) in FIXED_FORMATS {
            for index in 0..len {
                for byte in 0..=u8::MAX {
                    let mut input = [b'0'; 66];
                    input[1] = b'x';
                    input[index] = byte;
                    let allowed = match index {
                        0 => byte == b'0',
                        1 => byte == b'x',
                        _ => HEX_ALPHABET.contains(&byte),
                    };
                    let result = validate(&input[..len]);
                    if allowed {
                        assert_eq!(result, Ok(()));
                    } else {
                        assert_failure(result, InvalidByte, index);
                    }
                }
            }
        }
    }

    #[test]
    fn fixed_formats_reject_wrappers_missing_prefix_and_extra_content() {
        for (validate, len) in FIXED_FORMATS {
            let mut input = [b'0'; 70];
            input[1] = b'x';
            assert_failure(validate(&input[2..len]), InvalidLength, len - 2);
            assert_failure(validate(&input[..2]), InvalidLength, 2);
            assert_failure(validate(&input[..0]), InvalidLength, 0);
            input[len] = b' ';
            assert_failure(validate(&input[..len + 1]), InvalidLength, len + 1);
            input[len] = b'\n';
            assert_failure(validate(&input[..len + 1]), InvalidLength, len + 1);
            input[len] = b'\0';
            assert_failure(validate(&input[..len + 1]), InvalidLength, len + 1);

            let mut wrapped = [b'0'; 70];
            wrapped[0] = b'"';
            wrapped[1..len + 1].copy_from_slice(&input[..len]);
            wrapped[len + 1] = b'"';
            assert_failure(validate(&wrapped[..len + 2]), InvalidLength, len + 2);
            wrapped[0] = b' ';
            assert_failure(validate(&wrapped[..len + 1]), InvalidLength, len + 1);
        }
        let address = b"0x0000000000000000000000000000000000000000";
        let hash = b"0x0000000000000000000000000000000000000000000000000000000000000000";
        assert_failure(validate_transaction_hash(address), InvalidLength, address.len());
        assert_failure(validate_address(hash), InvalidLength, hash.len());
    }

    #[test]
    fn fixed_formats_report_the_first_prefix_or_digit_failure() {
        for (validate, len) in FIXED_FORMATS {
            let mut input = [b'0'; 66];
            input[0] = b'1';
            input[1] = b'X';
            input[2] = b'g';
            input[len - 1] = b'!';
            assert_failure(validate(&input[..len]), InvalidByte, 0);
            input[0] = b'0';
            assert_failure(validate(&input[..len]), InvalidByte, 1);
            input[1] = b'x';
            assert_failure(validate(&input[..len]), InvalidByte, 2);
            input[2] = b'0';
            assert_failure(validate(&input[..len]), InvalidByte, len - 1);
        }
    }

    #[test]
    fn validators_observe_subslice_boundaries_and_relative_error_indices() {
        let nanoid = b"!Ab_09-z?";
        assert_eq!(validate_nanoid(&nanoid[1..8], 7), Ok(()));
        assert_failure(validate_nanoid(&nanoid[1..9], 8), InvalidByte, 7);
        assert_failure(validate_nanoid(&nanoid[1..7], 7), InvalidLength, 6);
        let bad_nanoid = b"!Ab!09-z?";
        assert_failure(validate_nanoid(&bad_nanoid[1..8], 7), InvalidByte, 2);
        assert_failure(validate_nanoid(&nanoid[1..1], 0), InvalidLength, 0);

        for (validate, len) in FIXED_FORMATS {
            let mut storage = [b'!'; 70];
            storage[1..len + 1].fill(b'A');
            storage[1..3].copy_from_slice(b"0x");
            assert_eq!(validate(&storage[1..len + 1]), Ok(()));
            assert_failure(validate(&storage[1..len]), InvalidLength, len - 1);
            storage[len] = b'g';
            assert_failure(validate(&storage[1..len + 1]), InvalidByte, len - 1);
            storage[len] = b'A';
            storage[2] = b'X';
            assert_failure(validate(&storage[1..len + 1]), InvalidByte, 1);

            // The last validated byte is also the final byte of the storage.
            let mut ending = [b'!'; 66];
            let start = ending.len() - len;
            ending[start..].fill(b'f');
            ending[start..start + 2].copy_from_slice(b"0x");
            assert_eq!(validate(&ending[start..]), Ok(()));
        }
    }
}
