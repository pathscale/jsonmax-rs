//! Retained native utilities with checked borrows and canonical ABI types.
//!
//! Resolving a reference checks it against the caller's selected storage. It
//! does not establish provenance, interpret JSON escapes, or issue a validation
//! proof. Endpoint lookup borrows the model without changing its ordering.

use alloc::string::String;
use jmax_abi::{
    buffer::Payload,
    error::{Error, ErrorCode, ErrorSource, Result},
    span::SpanSource,
};

pub use jmax_abi::endpoint::{
    EndpointField, EndpointSchemaSpec, EndpointType, EnumVariantSpec, ServiceSpec,
};
pub use jmax_abi::span::{Span32, StrRef};
pub use jmax_abi::traits::{ByteSink, JsonDecode, JsonEncode, SchemaProvider};

/// Borrow the selected string after checking its range, UTF-8, and scalar count.
///
/// Scalar counts measure Unicode scalar values, including combining characters,
/// rather than UTF-8 bytes or graphemes. Only the referenced bytes are examined;
/// unrelated bytes in either buffer need not contain valid UTF-8 or JSON.
/// Invalid ranges and counts use `ConstraintViolation`; invalid UTF-8 uses
/// `InvalidUtf8`. All errors have `Decode` source. UTF-8 offsets are relative to
/// the selected raw payload or decoded arena, respectively.
pub fn resolve_str<'a>(
    payload: &'a Payload,
    decoded: &'a [u8],
    reference: StrRef,
) -> Result<&'a str> {
    let owner = match reference.source {
        SpanSource::RawPayload => payload.as_slice(),
        SpanSource::DecodedArena => decoded,
    };
    let bytes = checked_bytes(owner, reference.span)?;
    let text = core::str::from_utf8(bytes).map_err(|error| {
        // The checked span bounds valid_up_to and its absolute offset by u32.
        // Saturating diagnostics also avoid truncation on wider host targets.
        let relative = u32::try_from(error.valid_up_to()).unwrap_or(u32::MAX);
        Error::new(
            ErrorCode::InvalidUtf8,
            ErrorSource::Decode,
            reference.span.offset.saturating_add(relative),
            "compatibility string is not valid UTF-8",
        )
    })?;
    if u32::try_from(text.chars().count()).ok() != Some(reference.scalar_len) {
        return Err(Error::new(
            ErrorCode::ConstraintViolation,
            ErrorSource::Decode,
            reference.span.offset,
            "compatibility string scalar count does not match reference",
        ));
    }
    Ok(text)
}

/// Borrow raw bytes after checking 32-bit end arithmetic and payload bounds.
///
/// Empty spans at the payload end are valid. Raw bytes need not be UTF-8, align
/// with a string boundary, or constitute JSON. An invalid span returns
/// `ConstraintViolation` with `Decode` source and the span's starting offset.
pub fn checked_raw_span(payload: &Payload, span: Span32) -> Result<&[u8]> {
    checked_bytes(payload.as_slice(), span)
}

fn checked_bytes(bytes: &[u8], span: Span32) -> Result<&[u8]> {
    span.slice(bytes).ok_or_else(|| {
        Error::new(
            ErrorCode::ConstraintViolation,
            ErrorSource::Decode,
            span.offset,
            "compatibility span is out of bounds or its end overflows",
        )
    })
}

/// Find one endpoint using exact, case-sensitive service and endpoint names.
///
/// Empty or absent names return `MissingRequiredField`. Duplicate matching
/// service names are ambiguous even if only one service contains the endpoint;
/// duplicate matching endpoint names within the selected service are likewise
/// rejected with `DuplicateObjectKey`. Unrelated names are not validated by
/// this lookup. All errors have `Decode` source.
pub fn find_endpoint<'a>(
    services: &'a [ServiceSpec],
    service: &str,
    endpoint: &str,
) -> Result<&'a EndpointSchemaSpec> {
    if service.is_empty() || endpoint.is_empty() {
        return Err(Error::new(
            ErrorCode::MissingRequiredField,
            ErrorSource::Decode,
            0,
            "service and endpoint lookup names must be nonempty",
        ));
    }
    let mut matches = services.iter().filter(|item| item.name == service);
    let selected = matches.next().ok_or_else(|| {
        Error::new(
            ErrorCode::MissingRequiredField,
            ErrorSource::Decode,
            0,
            "service lookup name was not found",
        )
    })?;
    if matches.next().is_some() {
        return Err(Error::new(
            ErrorCode::DuplicateObjectKey,
            ErrorSource::Decode,
            0,
            "service lookup name is ambiguous",
        ));
    }
    let mut matches = selected.endpoints.iter().filter(|item| item.name == endpoint);
    let selected = matches.next().ok_or_else(|| {
        Error::new(
            ErrorCode::MissingRequiredField,
            ErrorSource::Decode,
            0,
            "endpoint lookup name was not found",
        )
    })?;
    if matches.next().is_some() {
        return Err(Error::new(
            ErrorCode::DuplicateObjectKey,
            ErrorSource::Decode,
            0,
            "endpoint lookup name is ambiguous",
        ));
    }
    Ok(selected)
}

/// Validate an ASCII header value and return an exact, owned copy.
///
/// Horizontal tab and printable ASCII are permitted, including empty values
/// and leading or trailing spaces. All other controls, DEL, and non-ASCII
/// bytes return `InvalidToken` with `Encode` source at the first invalid byte.
/// This validates a value only; it performs no escaping or authentication.
/// Allocation failure returns `BudgetExhausted` with `Encode` source.
pub fn encode_header_value(value: &str) -> Result<String> {
    if let Some(offset) = value.bytes().position(|byte| {
        byte != b'\t' && !(b' '..=b'~').contains(&byte)
    }) {
        return Err(Error::new(
            ErrorCode::InvalidToken,
            ErrorSource::Encode,
            u32::try_from(offset).unwrap_or(u32::MAX),
            "header value contains a control or non-ASCII byte",
        ));
    }
    let mut output = String::new();
    output.try_reserve_exact(value.len()).map_err(|_| {
        Error::new(
            ErrorCode::BudgetExhausted,
            ErrorSource::Encode,
            0,
            "header value allocation failed",
        )
    })?;
    output.push_str(value);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{vec, vec::Vec};
    use core::any::TypeId;

    const SOURCES: [SpanSource; 2] = [
        SpanSource::RawPayload,
        SpanSource::DecodedArena,
    ];

    fn reference(source: SpanSource, offset: u32, len: u32, scalar_len: u32) -> StrRef {
        StrRef { span: Span32::new(offset, len), source, scalar_len }
    }

    fn assert_error<T>(
        result: Result<T>,
        code: ErrorCode,
        source: ErrorSource,
        offset: u32,
    ) -> Error {
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("expected a utility error"),
        };
        assert_eq!(error.code, code);
        assert_eq!(error.source, source);
        assert_eq!(error.byte_offset, offset);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
        error
    }

    fn endpoint(service: &str, name: &str, code: u32) -> EndpointSchemaSpec {
        EndpointSchemaSpec {
            code,
            name: String::from(name),
            service: String::from(service),
            roles: vec![String::from("reader")],
            params: vec![EndpointField {
                name: String::from("id"),
                ty: EndpointType::UInt32,
                description: String::from("Identifier"),
            }],
            response: vec![EndpointField {
                name: String::from("state"),
                ty: EndpointType::Enum {
                    name: String::from("State"),
                    variants: vec![EnumVariantSpec {
                        name: String::from("Ready"),
                        value: i32::MIN,
                        description: String::from("Ready state"),
                    }],
                },
                description: String::from("Current state"),
            }],
            stream_response: Some(EndpointType::BlockchainDecimal),
            description: String::from("Endpoint description"),
        }
    }

    fn service(name: &str, id: u16, endpoints: Vec<EndpointSchemaSpec>) -> ServiceSpec {
        ServiceSpec { id, name: String::from(name), endpoints }
    }

    #[test]
    fn retained_exports_have_the_canonical_abi_identity() {
        assert_eq!(TypeId::of::<EndpointField>(), TypeId::of::<jmax_abi::EndpointField>());
        assert_eq!(TypeId::of::<EndpointSchemaSpec>(), TypeId::of::<jmax_abi::EndpointSchemaSpec>());
        assert_eq!(TypeId::of::<EndpointType>(), TypeId::of::<jmax_abi::EndpointType>());
        assert_eq!(TypeId::of::<EnumVariantSpec>(), TypeId::of::<jmax_abi::EnumVariantSpec>());
        assert_eq!(TypeId::of::<ServiceSpec>(), TypeId::of::<jmax_abi::ServiceSpec>());
        assert_eq!(TypeId::of::<Span32>(), TypeId::of::<jmax_abi::Span32>());
        assert_eq!(TypeId::of::<StrRef>(), TypeId::of::<jmax_abi::StrRef>());
        assert_eq!(TypeId::of::<dyn ByteSink>(), TypeId::of::<dyn jmax_abi::ByteSink>());
        assert_eq!(TypeId::of::<dyn JsonEncode>(), TypeId::of::<dyn jmax_abi::JsonEncode>());
    }

    #[test]
    fn raw_span_vectors_borrow_both_payload_storage_variants() {
        for payload in [Payload::from_slice(b"abcd"), Payload::from_vec(b"abcd".to_vec())] {
            for (span, expected) in [
                (Span32::new(0, 4), &b"abcd"[..]),
                (Span32::new(1, 2), &b"bc"[..]),
                (Span32::new(3, 1), &b"d"[..]),
                (Span32::new(0, 0), &b""[..]),
                (Span32::new(2, 0), &b""[..]),
                (Span32::new(4, 0), &b""[..]),
            ] {
                let actual = checked_raw_span(&payload, span).unwrap();
                assert_eq!(actual, expected);
                assert_eq!(actual.as_ptr(), payload.as_slice()[span.offset as usize..].as_ptr());
            }
        }
    }

    #[test]
    fn raw_span_rejects_out_of_bounds_empty_and_overflowing_ranges() {
        let payload = Payload::from_slice(b"abcd");
        for span in [
            Span32::new(5, 0), Span32::new(4, 1), Span32::new(3, 2),
            Span32::new(0, 5), Span32::new(u32::MAX, 0),
            Span32::new(u32::MAX - 1, 1), Span32::new(u32::MAX - 1, 2),
            Span32::new(u32::MAX, 1), Span32::new(1, u32::MAX),
        ] {
            assert_error(checked_raw_span(&payload, span), ErrorCode::ConstraintViolation,
                ErrorSource::Decode, span.offset);
        }
        for payload in [Payload::from_slice(b""), Payload::from_vec(Vec::new())] {
            assert_eq!(checked_raw_span(&payload, Span32::new(0, 0)).unwrap(), b"");
            for span in [Span32::new(0, 1), Span32::new(1, 0)] {
                assert_error(checked_raw_span(&payload, span), ErrorCode::ConstraintViolation,
                    ErrorSource::Decode, span.offset);
            }
        }
    }

    #[test]
    fn raw_spans_preserve_arbitrary_bytes_and_partial_utf8() {
        let payload = Payload::from_slice(b"\0\xff\xc3\xa9\r\n");
        assert_eq!(checked_raw_span(&payload, Span32::new(0, 6)).unwrap(), b"\0\xff\xc3\xa9\r\n");
        assert_eq!(checked_raw_span(&payload, Span32::new(3, 1)).unwrap(), b"\xa9");
    }

    #[test]
    fn string_source_selection_preserves_json_escape_bytes_and_owner_pointers() {
        let payload = Payload::from_slice(br#""a\u00e9""#);
        let decoded = "aé".as_bytes();
        let raw = resolve_str(&payload, decoded, reference(SpanSource::RawPayload, 1, 7, 7)).unwrap();
        let logical = resolve_str(&payload, decoded, reference(SpanSource::DecodedArena, 0, 3, 2)).unwrap();
        assert_eq!(raw, "a\\u00e9");
        assert_eq!(logical, "aé");
        assert_eq!(raw.as_ptr(), payload.as_slice()[1..].as_ptr());
        assert_eq!(logical.as_ptr(), decoded.as_ptr());
    }

    #[test]
    fn string_vectors_count_scalars_and_preserve_exact_unicode_bytes() {
        for (text, count) in [
            ("", 0), ("ASCII", 5), ("é", 1), ("猫", 1), ("😀", 1),
            ("e\u{301}", 2), ("é猫😀", 3), ("\0\t\r\n", 4), ("\u{feff}", 1),
        ] {
            let mut bytes = vec![0xff];
            bytes.extend_from_slice(text.as_bytes());
            bytes.push(0xff);
            for payload in [Payload::from_slice(&bytes), Payload::from_vec(bytes.clone())] {
                for source in SOURCES {
                    let actual = resolve_str(&payload, &bytes, reference(
                        source, 1, text.len() as u32, count,
                    )).unwrap();
                    assert_eq!(actual.as_bytes(), text.as_bytes());
                    let owner = match source {
                        SpanSource::RawPayload => payload.as_slice(),
                        SpanSource::DecodedArena => bytes.as_slice(),
                    };
                    assert_eq!(actual.as_ptr(), owner[1..].as_ptr());
                }
            }
        }
    }

    #[test]
    fn selected_owner_alone_controls_string_bounds_and_validity() {
        let payload = Payload::from_slice(b"a");
        let decoded = b"\xffxy";
        assert_eq!(resolve_str(&payload, decoded,
            reference(SpanSource::DecodedArena, 1, 2, 2)).unwrap(), "xy");
        assert_error(resolve_str(&payload, decoded, reference(SpanSource::RawPayload, 1, 2, 2)),
            ErrorCode::ConstraintViolation, ErrorSource::Decode, 1);
        assert_eq!(resolve_str(&payload, decoded,
            reference(SpanSource::RawPayload, 0, 1, 1)).unwrap(), "a");
        assert_error(resolve_str(&payload, decoded, reference(SpanSource::DecodedArena, 0, 1, 1)),
            ErrorCode::InvalidUtf8, ErrorSource::Decode, 0);
        let payload = Payload::from_slice(b"long");
        assert_error(resolve_str(&payload, b"x", reference(SpanSource::DecodedArena, 1, 3, 3)),
            ErrorCode::ConstraintViolation, ErrorSource::Decode, 1);
    }

    #[test]
    fn strings_accept_empty_ranges_at_each_owner_end() {
        let payload = Payload::from_slice(b"abcd");
        for (source, end) in [(SpanSource::RawPayload, 4), (SpanSource::DecodedArena, 2)] {
            for offset in [0, end] {
                assert_eq!(resolve_str(&payload, b"xy", reference(source, offset, 0, 0)).unwrap(), "");
                assert_error(resolve_str(&payload, b"xy", reference(source, offset, 0, 1)),
                    ErrorCode::ConstraintViolation, ErrorSource::Decode, offset);
            }
            assert_error(resolve_str(&payload, b"xy", reference(source, end + 1, 0, 0)),
                ErrorCode::ConstraintViolation, ErrorSource::Decode, end + 1);
        }
        let payload = Payload::from_slice(b"");
        for source in SOURCES {
            assert_eq!(resolve_str(&payload, b"", reference(source, 0, 0, 0)).unwrap(), "");
        }
    }

    #[test]
    fn incorrect_scalar_metadata_never_truncates_or_reinterprets_a_string() {
        let payload = Payload::from_slice("é猫".as_bytes());
        for source in SOURCES {
            assert_eq!(resolve_str(&payload, payload.as_slice(), reference(source, 0, 5, 2)).unwrap(), "é猫");
            for count in [0, 1, 3, 5, u32::MAX] {
                assert_error(resolve_str(&payload, payload.as_slice(), reference(source, 0, 5, count)),
                    ErrorCode::ConstraintViolation, ErrorSource::Decode, 0);
            }
        }
    }

    #[test]
    fn malformed_utf8_vectors_report_the_first_invalid_owner_offset() {
        for invalid in [
            &b"\xff"[..], &b"\x80"[..], &b"\xc0\xaf"[..], &b"\xed\xa0\x80"[..],
            &b"\xf4\x90\x80\x80"[..], &b"\xe2\x82"[..], &b"\xc2"[..],
        ] {
            let mut bytes = b"__ok".to_vec();
            bytes.extend_from_slice(invalid);
            let payload = Payload::from_slice(&bytes);
            for source in SOURCES {
                // Scalar metadata is deliberately wrong: UTF-8 failure comes first.
                assert_error(resolve_str(&payload, &bytes, reference(
                    source, 2, invalid.len() as u32 + 2, u32::MAX,
                )), ErrorCode::InvalidUtf8, ErrorSource::Decode, 4);
            }
        }
    }

    #[test]
    fn split_multibyte_strings_are_invalid_even_inside_valid_owners() {
        let bytes = "é猫".as_bytes();
        let payload = Payload::from_slice(bytes);
        for source in SOURCES {
            for (offset, len, invalid_at) in [(0, 1, 0), (1, 1, 1), (2, 2, 2), (3, 1, 3), (0, 3, 2)] {
                assert_error(resolve_str(&payload, bytes, reference(source, offset, len, 1)),
                    ErrorCode::InvalidUtf8, ErrorSource::Decode, invalid_at);
            }
        }
    }

    #[test]
    fn string_ranges_fail_before_utf8_and_scalar_validation() {
        let payload = Payload::from_slice(b"\xff");
        for source in SOURCES {
            for span in [
                Span32::new(0, 2), Span32::new(2, 0), Span32::new(u32::MAX, 0),
                Span32::new(u32::MAX, 1), Span32::new(1, u32::MAX),
                Span32::new(u32::MAX - 1, 2),
            ] {
                assert_error(resolve_str(&payload, b"\xff", StrRef { span, source, scalar_len: u32::MAX }),
                    ErrorCode::ConstraintViolation, ErrorSource::Decode, span.offset);
            }
        }
    }

    #[test]
    fn cloned_payload_owners_keep_resolved_borrows_in_the_retained_storage() {
        for original in [Payload::from_slice(b"abc"), Payload::from_vec(b"abc".to_vec())] {
            let retained = original.clone();
            let owner_pointer = original.as_slice().as_ptr();
            drop(original);
            let text = resolve_str(&retained, b"", reference(SpanSource::RawPayload, 0, 3, 3)).unwrap();
            assert_eq!(text, "abc");
            assert_eq!(text.as_ptr(), owner_pointer);
            assert_eq!(checked_raw_span(&retained, Span32::new(0, 3)).unwrap(), text.as_bytes());
        }
    }

    #[test]
    fn header_values_preserve_empty_whitespace_punctuation_and_auth_text() {
        for value in [
            "", " ", "\t", "  leading and trailing  ", "\t a\t b \t",
            "Bearer abc.DEF_123-+/=", "name=\"value\"; path=/; x=%0d%0a", "!~\\:[]{}",
        ] {
            let output = encode_header_value(value).unwrap();
            assert_eq!(output.as_bytes(), value.as_bytes());
        }
        let mut original = String::from("Bearer token");
        let output = encode_header_value(&original).unwrap();
        original.clear();
        assert_eq!(output, "Bearer token");
    }

    #[test]
    fn header_ascii_matrix_accepts_only_tab_and_printable_bytes() {
        for byte in 0_u8..=127 {
            let bytes = [b'o', b'k', byte, b'!'];
            let input = core::str::from_utf8(&bytes).unwrap();
            if byte == 9 || (32..=126).contains(&byte) {
                assert_eq!(encode_header_value(input).unwrap().as_bytes(), bytes);
            } else {
                assert_error(encode_header_value(input), ErrorCode::InvalidToken,
                    ErrorSource::Encode, 2);
            }
        }
    }

    #[test]
    fn header_rejects_non_ascii_scalar_vectors_at_the_first_utf8_byte() {
        for suffix in ["é", "猫", "😀", "\u{80}", "\u{85}", "\u{a0}", "\u{301}", "\u{2028}", "\u{feff}", "\u{10ffff}"] {
            let mut input = String::from("ok\t");
            input.push_str(suffix);
            input.push_str("tail");
            assert_error(encode_header_value(&input), ErrorCode::InvalidToken,
                ErrorSource::Encode, 3);
        }
    }

    #[test]
    fn header_injection_and_multiple_invalid_bytes_return_the_first_error() {
        for (input, offset) in [
            ("\r\nInjected: yes", 0), ("safe\r\nInjected: yes", 4),
            ("safe\n", 4), ("safe\0", 4), ("safe\u{7f}", 4),
            ("\t\ré", 1), ("aé\r", 1), ("a\0\r\n", 1),
        ] {
            assert_error(encode_header_value(input), ErrorCode::InvalidToken,
                ErrorSource::Encode, offset);
        }
    }

    #[test]
    fn endpoint_lookup_returns_the_original_complete_endpoint_without_changes() {
        let services = vec![
            service("unused", 0, vec![]),
            service("Market", u16::MAX, vec![
                endpoint("Market", "other", 0), endpoint("Market", "Get", u32::MAX),
            ]),
        ];
        let before = services.clone();
        let found: &jmax_abi::EndpointSchemaSpec = find_endpoint(&services, "Market", "Get").unwrap();
        assert!(core::ptr::eq(found, &services[1].endpoints[1]));
        assert_eq!(found, &before[1].endpoints[1]);
        assert_eq!(found.code, u32::MAX);
        assert_eq!(services, before);
    }

    #[test]
    fn endpoint_missing_and_empty_queries_have_canonical_errors() {
        let services = vec![service("Api", 1, vec![endpoint("Api", "Get", 1)])];
        for (service_name, endpoint_name) in [
            ("", "Get"), ("Api", ""), ("", ""), ("Missing", "Get"),
            ("Api", "Missing"), ("api", "Get"), ("Api", "get"),
            (" Api", "Get"), ("Api", "Get "),
        ] {
            assert_error(find_endpoint(&services, service_name, endpoint_name),
                ErrorCode::MissingRequiredField, ErrorSource::Decode, 0);
        }
        assert_error(find_endpoint(&[], "Api", "Get"),
            ErrorCode::MissingRequiredField, ErrorSource::Decode, 0);
        let empty_names = vec![service("", 0, vec![endpoint("", "", 0)])];
        assert_error(find_endpoint(&empty_names, "", ""),
            ErrorCode::MissingRequiredField, ErrorSource::Decode, 0);
    }

    #[test]
    fn endpoint_names_are_exact_unicode_and_nonempty_whitespace_is_not_trimmed() {
        let services = vec![
            service("é", 1, vec![endpoint("é", "猫", 1)]),
            service(" \t", 2, vec![endpoint(" \t", " Get ", 2)]),
        ];
        assert!(core::ptr::eq(find_endpoint(&services, "é", "猫").unwrap(), &services[0].endpoints[0]));
        assert!(core::ptr::eq(find_endpoint(&services, " \t", " Get ").unwrap(), &services[1].endpoints[0]));
        assert_error(find_endpoint(&services, "e\u{301}", "猫"),
            ErrorCode::MissingRequiredField, ErrorSource::Decode, 0);
        assert_error(find_endpoint(&services, " \t", "Get"),
            ErrorCode::MissingRequiredField, ErrorSource::Decode, 0);
    }

    #[test]
    fn duplicate_service_matches_are_ambiguous_even_without_a_second_endpoint() {
        for second_endpoints in [vec![], vec![endpoint("Api", "Other", 2)], vec![endpoint("Api", "Get", 2)]] {
            let services = vec![
                service("Api", 1, vec![endpoint("Api", "Get", 1)]),
                service("Other", 2, vec![]),
                service("Api", 3, second_endpoints),
            ];
            for endpoint_name in ["Get", "Missing"] {
                assert_error(find_endpoint(&services, "Api", endpoint_name),
                    ErrorCode::DuplicateObjectKey, ErrorSource::Decode, 0);
            }
        }
    }

    #[test]
    fn duplicate_endpoint_matches_are_ambiguous_even_when_identical() {
        let first = endpoint("Api", "Get", 1);
        for second in [first.clone(), endpoint("Api", "Get", u32::MAX)] {
            let services = vec![service("Api", 1, vec![
                first.clone(), endpoint("Api", "Other", 2), second,
            ])];
            assert_error(find_endpoint(&services, "Api", "Get"),
                ErrorCode::DuplicateObjectKey, ErrorSource::Decode, 0);
        }
    }

    #[test]
    fn unrelated_duplicate_names_and_same_endpoint_names_in_other_services_do_not_match() {
        let services = vec![
            service("Other", 1, vec![endpoint("Other", "Get", 1)]),
            service("Other", 2, vec![endpoint("Other", "Get", 2)]),
            service("Api", 3, vec![
                endpoint("Api", "unused", 1), endpoint("Api", "Get", 2),
                endpoint("Api", "unused", 3), endpoint("Api", "get", 4),
            ]),
        ];
        assert!(core::ptr::eq(find_endpoint(&services, "Api", "Get").unwrap(), &services[2].endpoints[1]));
        assert!(core::ptr::eq(find_endpoint(&services, "Api", "get").unwrap(), &services[2].endpoints[3]));
        assert_error(find_endpoint(&services, "Api", "Missing"),
            ErrorCode::MissingRequiredField, ErrorSource::Decode, 0);
    }

    #[test]
    fn string_utilities_interoperate_with_native_buffer_views() {
        let payload = Payload::from_slice(br#""a\u00e9""#);
        let decoded = "aé".as_bytes();
        let owner = jmax_buffer::OwnerBytes::new(payload.as_slice(), decoded).unwrap();
        for value in [
            reference(SpanSource::RawPayload, 1, 7, 7),
            reference(SpanSource::DecodedArena, 0, 3, 2),
            reference(SpanSource::DecodedArena, 3, 0, 0),
        ] {
            let retained = resolve_str(&payload, decoded, value).unwrap();
            let native = owner.string(value).unwrap();
            assert_eq!(retained, native.as_str());
            assert_eq!(retained.as_ptr(), native.as_str().as_ptr());
        }
        let span = Span32::new(2, 6);
        assert_eq!(checked_raw_span(&payload, span).unwrap(),
            owner.view(SpanSource::RawPayload, span).unwrap().as_bytes());
        for value in [
            reference(SpanSource::DecodedArena, 0, 3, 3),
            reference(SpanSource::DecodedArena, 1, 1, 1),
        ] {
            let retained = resolve_str(&payload, decoded, value).unwrap_err();
            let native = owner.string(value).unwrap_err();
            assert_eq!((retained.code, retained.source, retained.byte_offset),
                (native.code, native.source, native.byte_offset));
        }
    }

    #[test]
    fn retained_byte_sink_and_header_values_integrate_with_native_json_writer() {
        let value = encode_header_value(" \tBearer abc  ").unwrap();
        let expected = b"\" \\tBearer abc  \"";
        let mut sink = crate::BoundedSink::new(expected.len() as u32);
        {
            let retained: &mut dyn ByteSink = &mut sink;
            retained.reserve(expected.len()).unwrap();
            assert_eq!(retained.len(), 0);
            let mut writer = jmax_encode::JsonWriter::new(retained, jmax_encode::EncodeLimits::default());
            writer.string(&value).unwrap();
            assert_eq!(writer.finish().unwrap(), expected.len());
        }
        assert_eq!(sink.as_slice(), expected);
        let payload = Payload::from_vec(sink.into_vec());
        assert_eq!(checked_raw_span(&payload, Span32::new(0, expected.len() as u32)).unwrap(), expected);
        let mut sink = crate::BoundedSink::new(expected.len() as u32 - 1);
        let mut writer = jmax_encode::JsonWriter::new(&mut sink, jmax_encode::EncodeLimits::default());
        assert_eq!(writer.string(&value).unwrap_err().code, ErrorCode::PayloadTooLarge);
    }
}
