//! Bounded, immutable dispatch for the frozen named-format registry.
//!
//! Names are exact, case-sensitive spellings. Inputs are decoded string bytes;
//! dispatch performs no parsing, normalization, allocation, or global mutation.

use crate::{
    binary, identity, uuid_ip, Base64Policy, Error, ErrorCode, ErrorSource,
    FormatFailure, FormatFailureKind, FormatId, FormatOptions, FormatResult, Result,
    MAX_FORMAT_NAME_BYTES,
};

/// Resolves one of the ten canonical [`FormatId::name`] spellings.
///
/// The byte-length budget is checked before name matching. Oversized names
/// produce `BudgetExhausted`; all other unknown names, including the empty
/// name, produce `UnsupportedSchemaFeature`. Errors carry `SchemaVm` as their
/// source and zero byte, schema, and field locations for callers to contextualize.
pub fn lookup(name: &str) -> Result<FormatId> {
    if name.len() > MAX_FORMAT_NAME_BYTES {
        return Err(Error::new(
            ErrorCode::BudgetExhausted,
            ErrorSource::SchemaVm,
            0,
            "format name limit exceeded",
        ));
    }

    match name {
        "uuid" => Ok(FormatId::Uuid),
        "ipv4" => Ok(FormatId::Ipv4),
        "ipv6" => Ok(FormatId::Ipv6),
        "ip" => Ok(FormatId::IpAddr),
        "base64" => Ok(FormatId::Base64),
        "base64url" => Ok(FormatId::Base64Url),
        "hex" => Ok(FormatId::Hex),
        "nanoid" => Ok(FormatId::NanoId),
        "blockchain-address" => Ok(FormatId::BlockchainAddress),
        "blockchain-transaction-hash" => Ok(FormatId::BlockchainTransactionHash),
        _ => Err(Error::new(
            ErrorCode::UnsupportedSchemaFeature,
            ErrorSource::SchemaVm,
            0,
            "unsupported format name",
        )),
    }
}

/// Validates decoded string bytes using the selected format and its options.
///
/// The input budget is inclusive and checked before any format-specific work.
/// Its failure index is the slice length, saturated to `u32::MAX`. Only hex,
/// base64 padding, and NanoId length have configurable format policies; address
/// and transaction-hash policies remain fixed regardless of `options.hex`.
/// Successful binary validation discards the decoded byte count.
pub fn validate(id: FormatId, input: &[u8], options: &FormatOptions) -> FormatResult<()> {
    // Checked conversion also rejects slices longer than u32::MAX without
    // truncating their length or narrowing the configured budget to usize.
    if u32::try_from(input.len()).map_or(true, |len| len > options.max_input_bytes) {
        return Err(FormatFailure::new(FormatFailureKind::Limit, input.len()));
    }

    match id {
        FormatId::Uuid => uuid_ip::validate_uuid(input),
        FormatId::Ipv4 => uuid_ip::validate_ipv4(input),
        FormatId::Ipv6 => uuid_ip::validate_ipv6(input),
        FormatId::IpAddr => uuid_ip::validate_ip(input),
        FormatId::Base64 => binary::validate_base64(input, Base64Policy {
            url_safe: false,
            padding: options.base64_padding,
        }).map(|_| ()),
        FormatId::Base64Url => binary::validate_base64(input, Base64Policy {
            url_safe: true,
            padding: options.base64_padding,
        }).map(|_| ()),
        FormatId::Hex => binary::validate_hex(input, options.hex).map(|_| ()),
        FormatId::NanoId => identity::validate_nanoid(input, options.nanoid_len),
        FormatId::BlockchainAddress => identity::validate_address(input),
        FormatId::BlockchainTransactionHash => identity::validate_transaction_hash(input),
    }
}

/// Resolves a canonical name and validates its decoded string bytes.
///
/// Name errors take precedence over input errors. Format failures are mapped
/// through [`FormatFailure::into_error`] with offset zero and source `SchemaVm`.
/// Callers must attach the enclosing raw token's location themselves: a decoded
/// failure index cannot be added to a raw JSON offset when escapes are present.
pub fn validate_named(name: &str, input: &[u8], options: &FormatOptions) -> Result<()> {
    let id = lookup(name)?;
    validate(id, input, options).map_err(|failure| failure.into_error(0, ErrorSource::SchemaVm))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FormatFailureKind::{
        InvalidByte, InvalidLength, InvalidPadding, Limit, NonCanonical,
    };
    use crate::{HexCase, HexPolicy, Padding, DEFAULT_MAX_INPUT_BYTES, REGISTRY_VERSION};

    // Spellings and discriminants are independent frozen vectors, rather than
    // expectations derived from the registry implementation under test.
    const FORMATS: [(FormatId, u16, &str, &[u8]); 10] = [
        (FormatId::Uuid, 1, "uuid", b"550e8400-e29b-41d4-a716-446655440000"),
        (FormatId::Ipv4, 2, "ipv4", b"192.0.2.1"),
        (FormatId::Ipv6, 3, "ipv6", b"2001:db8::1"),
        (FormatId::IpAddr, 4, "ip", b"::ffff:192.0.2.1"),
        (FormatId::Base64, 5, "base64", b"+w=="),
        (FormatId::Base64Url, 6, "base64url", b"-w=="),
        (FormatId::Hex, 7, "hex", b"DeAdBeEf"),
        (FormatId::NanoId, 8, "nanoid", b"V1StGXR8_Z5jdHi6B-myT"),
        (
            FormatId::BlockchainAddress, 9, "blockchain-address",
            b"0x52908400098527886E0F7030069857D2E4169EE7",
        ),
        (
            FormatId::BlockchainTransactionHash, 10, "blockchain-transaction-hash",
            b"0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ),
    ];

    fn assert_failure(result: FormatResult<()>, kind: FormatFailureKind, index: usize) {
        assert_eq!(result, Err(FormatFailure::new(kind, index)));
    }

    fn assert_abi_error(error: &Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::SchemaVm);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn frozen_registry_round_trips_every_identifier_and_spelling() {
        assert_eq!(REGISTRY_VERSION, 1);
        assert_eq!(MAX_FORMAT_NAME_BYTES, 64);
        for (id, serialized, name, input) in FORMATS {
            assert_eq!(id as u16, serialized);
            assert_eq!(FormatId::from_u16(serialized), Some(id));
            assert_eq!(id.name(), name);
            assert_eq!(lookup(name), Ok(id));
            assert_eq!(validate(id, input, &FormatOptions::default()), Ok(()));
            assert_eq!(validate_named(name, input, &FormatOptions::default()), Ok(()));
        }
        for value in [0, 11, 255, u16::MAX] {
            assert_eq!(FormatId::from_u16(value), None);
        }
        for (index, (id, _, name, _)) in FORMATS.iter().enumerate() {
            for (other_id, _, other_name, _) in &FORMATS[index + 1..] {
                assert_ne!(id, other_id);
                assert_ne!(name, other_name);
            }
        }
    }

    #[test]
    fn lookup_rejects_aliases_wrappers_suffix_grammar_and_unicode() {
        for name in [
            "", "UUID", "Uuid", "IPv4", "IPv6", "ipaddr", "ip-address",
            "base64-url", "base64url:optional", "hex:20", "hex(20)",
            "nanoId", "nanoid:21", "address", "transaction-hash", "sha256",
            "uuid ", " uuid", "uuid\n", "uuid\0", "\0uuid", "\"uuid\"",
            "ｕｕｉｄ", "uuíd", "🦀", "blockchain_address",
            "blockchain-transaction-hash/32",
        ] {
            assert_abi_error(&lookup(name).unwrap_err(), ErrorCode::UnsupportedSchemaFeature);
        }

        for (_, _, name, _) in FORMATS {
            let mut bytes = [b'!'; MAX_FORMAT_NAME_BYTES];
            bytes[..name.len()].copy_from_slice(name.as_bytes());
            let suffixed = core::str::from_utf8(&bytes[..name.len() + 1]).unwrap();
            assert_abi_error(&lookup(suffixed).unwrap_err(), ErrorCode::UnsupportedSchemaFeature);
            bytes[1..name.len() + 1].copy_from_slice(name.as_bytes());
            bytes[0] = b'!';
            let prefixed = core::str::from_utf8(&bytes[..name.len() + 1]).unwrap();
            assert_abi_error(&lookup(prefixed).unwrap_err(), ErrorCode::UnsupportedSchemaFeature);

            for index in 0..name.len() {
                if name.as_bytes()[index].is_ascii_lowercase() {
                    bytes[..name.len()].copy_from_slice(name.as_bytes());
                    bytes[index] = bytes[index].to_ascii_uppercase();
                    let mixed_case = core::str::from_utf8(&bytes[..name.len()]).unwrap();
                    assert_abi_error(
                        &lookup(mixed_case).unwrap_err(), ErrorCode::UnsupportedSchemaFeature,
                    );
                }
            }
        }
    }

    #[test]
    fn name_budget_is_inclusive_and_measured_in_utf8_bytes() {
        let ascii = [b'x'; MAX_FORMAT_NAME_BYTES + 1];
        for len in [0, 1, MAX_FORMAT_NAME_BYTES - 1, MAX_FORMAT_NAME_BYTES] {
            let name = core::str::from_utf8(&ascii[..len]).unwrap();
            assert_abi_error(&lookup(name).unwrap_err(), ErrorCode::UnsupportedSchemaFeature);
        }
        let oversized = core::str::from_utf8(&ascii).unwrap();
        assert_abi_error(&lookup(oversized).unwrap_err(), ErrorCode::BudgetExhausted);

        let mut unicode = [0u8; 66];
        for pair in unicode.chunks_exact_mut(2) {
            pair.copy_from_slice("é".as_bytes());
        }
        let at_limit = core::str::from_utf8(&unicode[..64]).unwrap();
        let over_limit = core::str::from_utf8(&unicode).unwrap();
        assert_eq!(at_limit.chars().count(), 32);
        assert_abi_error(&lookup(at_limit).unwrap_err(), ErrorCode::UnsupportedSchemaFeature);
        assert_abi_error(&lookup(over_limit).unwrap_err(), ErrorCode::BudgetExhausted);
        let mut canonical_prefix = [b'x'; 65];
        canonical_prefix[..4].copy_from_slice(b"uuid");
        let name = core::str::from_utf8(&canonical_prefix).unwrap();
        assert_abi_error(&lookup(name).unwrap_err(), ErrorCode::BudgetExhausted);
    }

    #[test]
    fn default_options_preserve_frozen_policies() {
        let options = FormatOptions::default();
        assert_eq!(DEFAULT_MAX_INPUT_BYTES, 1_048_576);
        assert_eq!(options.max_input_bytes, DEFAULT_MAX_INPUT_BYTES);
        assert_eq!(options.nanoid_len, 21);
        assert_eq!(options.hex, HexPolicy {
            case: HexCase::Any, prefix: false, exact_bytes: None,
        });
        assert_eq!(options.base64_padding, Padding::Required);
        assert_failure(validate(FormatId::Hex, b"0x00", &options), InvalidByte, 1);
        assert_failure(validate(FormatId::Base64, b"Zg", &options), InvalidPadding, 2);
        assert_failure(validate(FormatId::NanoId, b"A", &options), InvalidLength, 1);
    }

    #[test]
    fn every_dispatch_arm_checks_the_inclusive_input_budget_first() {
        for (id, _, name, input) in FORMATS {
            let len = u32::try_from(input.len()).unwrap();
            for budget in [len, len + 1, u32::MAX] {
                let options = FormatOptions { max_input_bytes: budget, ..FormatOptions::default() };
                assert_eq!(validate(id, input, &options), Ok(()));
                assert_eq!(validate_named(name, input, &options), Ok(()));
            }
            for budget in [0, len - 1] {
                let options = FormatOptions { max_input_bytes: budget, ..FormatOptions::default() };
                assert_failure(validate(id, input, &options), Limit, input.len());
                let error = validate_named(name, input, &options).unwrap_err();
                assert_abi_error(&error, ErrorCode::BudgetExhausted);
                assert_eq!(error.detail, "format input limit exceeded");
                // Even bytes that would fail immediately must hit the budget first.
                let malformed = [b'!'; 66];
                assert_failure(validate(id, &malformed[..input.len()], &options), Limit, input.len());
            }
            let options = FormatOptions { max_input_bytes: len, ..FormatOptions::default() };
            let malformed = [b'!'; 66];
            assert_failure(validate(id, &malformed[..input.len()], &options), InvalidByte, 0);
        }
    }

    #[test]
    fn zero_budget_allows_empty_input_to_reach_each_validator() {
        let options = FormatOptions { max_input_bytes: 0, ..FormatOptions::default() };
        for (id, _, name, _) in FORMATS {
            if matches!(id, FormatId::Base64 | FormatId::Base64Url | FormatId::Hex) {
                assert_eq!(validate(id, b"", &options), Ok(()));
                assert_eq!(validate_named(name, b"", &options), Ok(()));
            } else {
                assert_failure(validate(id, b"", &options), InvalidLength, 0);
                assert_abi_error(
                    &validate_named(name, b"", &options).unwrap_err(), ErrorCode::FormatMismatch,
                );
            }
            assert_failure(validate(id, b"!", &options), Limit, 1);
        }
    }

    #[test]
    fn uuid_and_ip_ignore_unrelated_options_and_accept_both_ip_families() {
        let options = FormatOptions {
            max_input_bytes: u32::MAX,
            nanoid_len: 0,
            hex: HexPolicy { case: HexCase::Upper, prefix: true, exact_bytes: Some(u32::MAX) },
            base64_padding: Padding::Forbidden,
        };
        for (id, _, name, input) in &FORMATS[..4] {
            assert_eq!(validate(*id, input, &options), Ok(()));
            assert_eq!(validate_named(name, input, &options), Ok(()));
        }
        for input in [&b"0.0.0.0"[..], &b"255.255.255.255"[..], &b"::"[..], &b"::1"[..]] {
            assert_eq!(validate(FormatId::IpAddr, input, &options), Ok(()));
        }
        assert_failure(validate(FormatId::Ipv4, b"::1", &options), InvalidByte, 0);
        assert_failure(validate(FormatId::Ipv6, b"192.0.2.1", &options), InvalidLength, 9);
        for input in [
            &b"192.0.2.1:80"[..], &b"[::1]:443"[..], &b"fe80::1%eth0"[..],
            &b"::1/128"[..], &b"localhost"[..],
        ] {
            assert!(validate(FormatId::IpAddr, input, &options).is_err());
        }
    }

    #[test]
    fn base64_rfc4648_vectors_dispatch_under_every_padding_policy() {
        let vectors: &[(&[u8], &[u8])] = &[
            (b"", b""), (b"Zg==", b"Zg"), (b"Zm8=", b"Zm8"),
            (b"Zm9v", b"Zm9v"), (b"Zm9vYg==", b"Zm9vYg"),
            (b"Zm9vYmE=", b"Zm9vYmE"), (b"Zm9vYmFy", b"Zm9vYmFy"),
        ];
        for id in [FormatId::Base64, FormatId::Base64Url] {
            for padding in [Padding::Required, Padding::Forbidden, Padding::Optional] {
                let options = FormatOptions {
                    base64_padding: padding,
                    nanoid_len: 0,
                    hex: HexPolicy { case: HexCase::Lower, prefix: true, exact_bytes: Some(u32::MAX) },
                    ..FormatOptions::default()
                };
                for &(padded, unpadded) in vectors {
                    for input in [padded, unpadded] {
                        let expected = if padded != unpadded && padding == Padding::Required && input == unpadded {
                            Err(FormatFailure::new(InvalidPadding, input.len()))
                        } else if padded != unpadded && padding == Padding::Forbidden && input == padded {
                            Err(FormatFailure::new(InvalidPadding, unpadded.len()))
                        } else {
                            Ok(())
                        };
                        assert_eq!(validate(id, input, &options), expected);
                        assert_eq!(
                            validate_named(id.name(), input, &options),
                            expected.map_err(|failure| failure.into_error(0, ErrorSource::SchemaVm)),
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn base64_alphabets_are_exclusive_and_noncanonical_tails_stay_errors() {
        for padding in [Padding::Required, Padding::Forbidden, Padding::Optional] {
            let options = FormatOptions { base64_padding: padding, ..FormatOptions::default() };
            assert_eq!(validate(FormatId::Base64, b"++//", &options), Ok(()));
            assert_eq!(validate(FormatId::Base64Url, b"--__", &options), Ok(()));
            assert_failure(validate(FormatId::Base64, b"--__", &options), InvalidByte, 0);
            assert_failure(validate(FormatId::Base64Url, b"++//", &options), InvalidByte, 0);
            assert_failure(validate(FormatId::Base64, b"A+_A", &options), InvalidByte, 2);
            assert_failure(validate(FormatId::Base64Url, b"A+_A", &options), InvalidByte, 1);
            for id in [FormatId::Base64, FormatId::Base64Url] {
                let (two, three): (&[u8], &[u8]) = if padding == Padding::Forbidden {
                    (b"Zh", b"Zm9")
                } else {
                    (b"Zh==", b"Zm9=")
                };
                assert_failure(validate(id, two, &options), NonCanonical, 1);
                assert_failure(validate(id, three, &options), NonCanonical, 2);
                assert_failure(validate(id, b"A", &options), InvalidLength, 1);
                assert_failure(validate(id, b"!===", &options), InvalidByte, 0);
                assert_failure(validate(id, b"AA=A", &options), InvalidPadding, if padding == Padding::Forbidden { 2 } else { 3 });
            }
        }
    }

    #[test]
    fn hex_forwards_case_prefix_and_exact_length_without_using_other_policies() {
        for case in [HexCase::Any, HexCase::Lower, HexCase::Upper] {
            for prefix in [false, true] {
                for exact_bytes in [None, Some(2)] {
                    let options = FormatOptions {
                        nanoid_len: 0,
                        base64_padding: Padding::Forbidden,
                        hex: HexPolicy { case, prefix, exact_bytes },
                        ..FormatOptions::default()
                    };
                    let (lower, upper, numeric): (&[u8], &[u8], &[u8]) = if prefix {
                        (b"0xabcd", b"0xABCD", b"0x0123")
                    } else {
                        (b"abcd", b"ABCD", b"0123")
                    };
                    assert_eq!(validate_named("hex", numeric, &options), Ok(()));
                    for (input, forbidden) in [(lower, HexCase::Upper), (upper, HexCase::Lower)] {
                        if case == forbidden {
                            assert_failure(validate(FormatId::Hex, input, &options), NonCanonical, if prefix { 2 } else { 0 });
                        } else {
                            assert_eq!(validate_named("hex", input, &options), Ok(()));
                        }
                    }
                    let wrong_length = FormatOptions {
                        hex: HexPolicy { exact_bytes: Some(3), ..options.hex }, ..options
                    };
                    assert_failure(validate(FormatId::Hex, numeric, &wrong_length), InvalidLength, numeric.len());
                }
            }
        }
        for (prefix, input) in [(false, &b""[..]), (true, &b"0x"[..])] {
            let options = FormatOptions {
                hex: HexPolicy { prefix, exact_bytes: Some(0), case: HexCase::Upper },
                ..FormatOptions::default()
            };
            assert_eq!(validate_named("hex", input, &options), Ok(()));
        }
        let huge = FormatOptions {
            hex: HexPolicy { exact_bytes: Some(u32::MAX), ..HexPolicy::default() },
            ..FormatOptions::default()
        };
        assert_failure(validate(FormatId::Hex, b"00", &huge), InvalidLength, 2);
    }

    #[test]
    fn nanoid_uses_every_configurable_length_including_zero_and_maximum() {
        let input = [b'_'; 256];
        for len in 1..=u8::MAX {
            let options = FormatOptions {
                nanoid_len: len,
                hex: HexPolicy { prefix: true, exact_bytes: Some(u32::MAX), case: HexCase::Upper },
                base64_padding: Padding::Forbidden,
                ..FormatOptions::default()
            };
            let size = usize::from(len);
            assert_eq!(validate_named("nanoid", &input[..size], &options), Ok(()));
            assert_failure(validate(FormatId::NanoId, &input[..size - 1], &options), InvalidLength, size - 1);
            assert_failure(validate(FormatId::NanoId, &input[..size + 1], &options), InvalidLength, size + 1);
        }
        let options = FormatOptions { nanoid_len: 0, ..FormatOptions::default() };
        assert_failure(validate(FormatId::NanoId, b"", &options), InvalidLength, 0);
        assert_failure(validate(FormatId::NanoId, b"A", &options), InvalidLength, 1);
        let options = FormatOptions { nanoid_len: 1, ..FormatOptions::default() };
        for byte in 0..=u8::MAX {
            let allowed = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-".contains(&byte);
            if allowed {
                assert_eq!(validate(FormatId::NanoId, &[byte], &options), Ok(()));
            } else {
                assert_failure(validate(FormatId::NanoId, &[byte], &options), InvalidByte, 0);
            }
        }
    }

    #[test]
    fn address_and_hash_keep_their_fixed_policies_for_every_hex_option() {
        for (id, len) in [(FormatId::BlockchainAddress, 42), (FormatId::BlockchainTransactionHash, 66)] {
            for case in [HexCase::Any, HexCase::Lower, HexCase::Upper] {
                for prefix in [false, true] {
                    for exact_bytes in [None, Some(0), Some(1), Some(20), Some(32), Some(u32::MAX)] {
                        let options = FormatOptions {
                            nanoid_len: 0,
                            hex: HexPolicy { case, prefix, exact_bytes },
                            base64_padding: Padding::Forbidden,
                            ..FormatOptions::default()
                        };
                        let mut input = [b'0'; 68];
                        input[..2].copy_from_slice(b"0x");
                        for (index, byte) in input[2..len].iter_mut().enumerate() {
                            *byte = b"aBcDeF0123456789"[index % 16];
                        }
                        assert_eq!(validate(id, &input[..len], &options), Ok(()));
                        assert_eq!(validate_named(id.name(), &input[..len], &options), Ok(()));
                        assert_failure(validate(id, &input[..len - 1], &options), InvalidLength, len - 1);
                        assert_failure(validate(id, &input[..len + 2], &options), InvalidLength, len + 2);
                        input[1] = b'X';
                        assert_failure(validate(id, &input[..len], &options), InvalidByte, 1);
                        input[1] = b'x';
                        input[len - 1] = b'g';
                        assert_failure(validate(id, &input[..len], &options), InvalidByte, len - 1);
                    }
                }
            }
        }
    }

    #[test]
    fn dispatch_preserves_failure_kinds_indices_and_named_error_details() {
        let cases: &[(FormatId, &[u8], FormatFailureKind, usize, &str)] = &[
            (FormatId::Uuid, b"short", InvalidLength, 5, "invalid format length"),
            (FormatId::Uuid, b"550e8400_e29b-41d4-a716-446655440000", InvalidByte, 8, "invalid format byte"),
            (FormatId::Ipv4, b"1.2.3", InvalidLength, 5, "invalid format length"),
            (FormatId::Ipv4, b"256.0.0.1", InvalidByte, 2, "invalid format byte"),
            (FormatId::Ipv4, b"01.2.3.4", NonCanonical, 1, "noncanonical format encoding"),
            (FormatId::Ipv6, b"1:2:3", InvalidLength, 5, "invalid format length"),
            (FormatId::Ipv6, b"::g", InvalidByte, 2, "invalid format byte"),
            (FormatId::IpAddr, b"1.2.3", InvalidLength, 5, "invalid format length"),
            (FormatId::IpAddr, b"::ffff:192.0.2.256", InvalidByte, 17, "invalid format byte"),
            (FormatId::IpAddr, b"::ffff:01.2.3.4", NonCanonical, 8, "noncanonical format encoding"),
            (FormatId::Base64, b"A", InvalidLength, 1, "invalid format length"),
            (FormatId::Base64, b"Zg==!", InvalidByte, 4, "invalid format byte"),
            (FormatId::Base64, b"Zg", InvalidPadding, 2, "invalid base64 padding"),
            (FormatId::Base64, b"Zh==", NonCanonical, 1, "noncanonical format encoding"),
            (FormatId::Base64Url, b"A", InvalidLength, 1, "invalid format length"),
            (FormatId::Base64Url, b"/w==", InvalidByte, 0, "invalid format byte"),
            (FormatId::Base64Url, b"_w", InvalidPadding, 2, "invalid base64 padding"),
            (FormatId::Base64Url, b"_x==", NonCanonical, 1, "noncanonical format encoding"),
            (FormatId::Hex, b"0", InvalidLength, 1, "invalid format length"),
            (FormatId::Hex, b"00g0", InvalidByte, 2, "invalid format byte"),
            (FormatId::NanoId, b"A", InvalidLength, 1, "invalid format length"),
            (FormatId::NanoId, b"V1StGXR8!Z5jdHi6B-myT", InvalidByte, 8, "invalid format byte"),
            (FormatId::BlockchainAddress, b"0x00", InvalidLength, 4, "invalid format length"),
            (FormatId::BlockchainAddress, b"0X52908400098527886E0F7030069857D2E4169EE7", InvalidByte, 1, "invalid format byte"),
            (FormatId::BlockchainTransactionHash, b"0x00", InvalidLength, 4, "invalid format length"),
            (
                FormatId::BlockchainTransactionHash,
                b"0xg123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                InvalidByte, 2, "invalid format byte",
            ),
        ];
        let options = FormatOptions::default();
        for &(id, input, kind, index, detail) in cases {
            assert_failure(validate(id, input, &options), kind, index);
            let error = validate_named(id.name(), input, &options).unwrap_err();
            assert_abi_error(&error, ErrorCode::FormatMismatch);
            assert_eq!(error.detail, detail);
        }
    }

    #[test]
    fn named_lookup_errors_precede_input_budget_and_content_errors() {
        let options = FormatOptions { max_input_bytes: 0, ..FormatOptions::default() };
        for name in ["", "UUID", "unknown", "hex:32"] {
            let expected = lookup(name).unwrap_err();
            for input in [&b""[..], &b"!"[..], &b"bad input"[..]] {
                assert_eq!(validate_named(name, input, &options), Err(expected.clone()));
            }
        }
        let bytes = [b'a'; 65];
        let oversized = core::str::from_utf8(&bytes).unwrap();
        assert_eq!(validate_named(oversized, b"!", &options), Err(lookup(oversized).unwrap_err()));
        assert_abi_error(&validate_named("hex", b"!", &options).unwrap_err(), ErrorCode::BudgetExhausted);
    }

    #[test]
    fn named_errors_leave_location_context_for_the_caller() {
        let options = FormatOptions::default();
        let failure = validate(FormatId::Hex, b"00g0", &options).unwrap_err();
        assert_eq!(failure.index, 2);
        let error = validate_named("hex", b"00g0", &options).unwrap_err();
        assert_eq!(error, failure.into_error(0, ErrorSource::SchemaVm));
        let mut located = error.with_schema(17, 23);
        located.byte_offset = 101;
        assert_eq!((located.byte_offset, located.schema_id, located.field_id), (101, 17, 23));
        assert_eq!(located.code, ErrorCode::FormatMismatch);
        assert_eq!(failure.into_error(101, ErrorSource::SchemaVm).byte_offset, 101);
        assert_abi_error(&validate_named("hex", b"00g0", &options).unwrap_err(), ErrorCode::FormatMismatch);
    }

    #[test]
    fn all_dispatch_arms_observe_subslice_boundaries_and_relative_indices() {
        for (id, _, name, input) in FORMATS {
            let mut storage = [b'!'; 70];
            storage[1..input.len() + 1].copy_from_slice(input);
            let options = FormatOptions {
                max_input_bytes: u32::try_from(input.len()).unwrap(), ..FormatOptions::default()
            };
            assert_eq!(validate(id, &storage[1..input.len() + 1], &options), Ok(()));
            assert_eq!(validate_named(name, &storage[1..input.len() + 1], &options), Ok(()));
            storage[1] = b'!';
            assert_failure(validate(id, &storage[1..input.len() + 1], &options), InvalidByte, 0);
            storage[1..input.len() + 1].copy_from_slice(input);
            assert_failure(validate(id, &storage[..input.len() + 1], &options), Limit, input.len() + 1);
        }
        let storage = b"!00g0?";
        assert_failure(validate(FormatId::Hex, &storage[1..5], &FormatOptions::default()), InvalidByte, 2);
    }

    #[test]
    fn decoded_bytes_are_not_trimmed_unquoted_unescaped_or_utf8_repaired() {
        let options = FormatOptions { nanoid_len: 3, ..FormatOptions::default() };
        for input in [&b" A_"[..], &b"A_ "[..], &b"\"A\""[..], &b"A\0B"[..], &b"A\xffB"[..]] {
            assert!(validate_named("nanoid", input, &options).is_err());
        }
        let options = FormatOptions { nanoid_len: 6, ..FormatOptions::default() };
        assert_failure(validate(FormatId::NanoId, b"\\u0041", &options), InvalidByte, 0);
        for input in [&b" Zg=="[..], &b"Zg==\n"[..], &b"\"Zg==\""[..], &b"Zg==\xff"[..]] {
            assert!(validate_named("base64", input, &options).is_err());
        }
        assert_failure(validate(FormatId::Hex, b"\xc3\xa9", &options), InvalidByte, 0);
        assert_failure(validate(FormatId::Ipv4, b"1.2.3.4\0", &options), InvalidByte, 7);
    }
}
