//! Deterministic wire casing, collision detection, and canonical method names.
//!
//! Conversion uses only ASCII classification and casing. Preservation copies the
//! input bytes exactly, including Unicode, punctuation, and control characters.
//! Collision checks never change the emitted spelling or invent disambiguators.

use super::{model_error, ErrorCode, NameCase, Result, WireName, WireNamePolicy};
use alloc::{collections::BTreeSet, string::String, vec::Vec};

/// Convert a nonempty name according to the shared wire word-boundary rules.
///
/// `Preserve` accepts any nonempty UTF-8 string. Other cases accept ASCII letters
/// and digits, with underscores, hyphens, and ASCII whitespace as separators.
/// Empty separator-delimited words are ignored. An uppercase letter begins a
/// word after a lowercase letter or digit, or between an uppercase letter and a
/// lowercase letter. Digits otherwise stay in their current word.
///
/// # Errors
///
/// Returns `ConstraintViolation` for an empty input, a conversion containing
/// unsupported characters, or a conversion with no remaining words.
pub fn convert_case(name: &str, case: NameCase) -> Result<String> {
    if name.is_empty() {
        return Err(model_error(ErrorCode::ConstraintViolation, "wire name must not be empty"));
    }
    if case == NameCase::Preserve {
        return Ok(String::from(name));
    }
    if !name.is_ascii() {
        return Err(model_error(
            ErrorCode::ConstraintViolation,
            "wire case conversion requires ASCII input",
        ));
    }

    let bytes = name.as_bytes();
    let mut output = String::with_capacity(name.len());
    let mut previous: Option<u8> = None;
    for (index, byte) in name.bytes().enumerate() {
        if byte == b'_' || byte == b'-' || byte.is_ascii_whitespace() {
            previous = None;
            continue;
        }
        if !byte.is_ascii_alphanumeric() {
            return Err(model_error(
                ErrorCode::ConstraintViolation,
                "invalid character in wire case conversion",
            ));
        }

        // Inspect the original spelling, rather than already cased output, so
        // HTTPServer becomes HTTP + Server and HTTP2Server becomes HTTP2 + Server.
        let starts_word = match previous {
            None => true,
            Some(previous) => {
                byte.is_ascii_uppercase()
                    && (previous.is_ascii_lowercase()
                        || previous.is_ascii_digit()
                        || (previous.is_ascii_uppercase()
                            && bytes.get(index + 1).is_some_and(u8::is_ascii_lowercase)))
            }
        };
        let first_word = output.is_empty();
        if starts_word && !first_word {
            match case {
                NameCase::Snake | NameCase::ScreamingSnake => output.push('_'),
                NameCase::Kebab => output.push('-'),
                NameCase::Preserve | NameCase::Camel | NameCase::Pascal => {}
            }
        }
        let rendered = match case {
            NameCase::Preserve => byte,
            NameCase::ScreamingSnake => byte.to_ascii_uppercase(),
            NameCase::Pascal if starts_word => byte.to_ascii_uppercase(),
            NameCase::Camel if starts_word && !first_word => byte.to_ascii_uppercase(),
            NameCase::Snake | NameCase::Kebab | NameCase::Camel | NameCase::Pascal => {
                byte.to_ascii_lowercase()
            }
        };
        output.push(char::from(rendered));
        previous = Some(byte);
    }

    if output.is_empty() {
        return Err(model_error(
            ErrorCode::ConstraintViolation,
            "wire case conversion requires at least one word",
        ));
    }
    Ok(output)
}

/// Resolve names in declaration order, rejecting any collision in their output.
///
/// Comparisons use complete wire strings. With `case_insensitive`, only ASCII
/// letters are folded for comparison; Unicode bytes and emitted names remain
/// unchanged. Sources are copied exactly. An empty input list is valid, and
/// neither successful resolution nor failure mutates the caller's names.
///
/// # Errors
///
/// Propagates `ConstraintViolation` from [`convert_case`]. A collision, including
/// a repeated identical source, returns `DuplicateObjectKey`. Validation stops
/// at the first failing name in declaration order, without returning a partial
/// resolution.
pub fn resolve_wire_names(names: &[String], policy: WireNamePolicy) -> Result<Vec<WireName>> {
    let mut seen = BTreeSet::new();
    let mut resolved = Vec::with_capacity(names.len());
    for source in names {
        let wire = convert_case(source, policy.case)?;
        let key = if policy.case_insensitive {
            wire.to_ascii_lowercase()
        } else {
            wire.clone()
        };
        if !seen.insert(key) {
            return Err(model_error(ErrorCode::DuplicateObjectKey, "duplicate wire name"));
        }
        resolved.push(WireName { source: source.clone(), wire });
    }
    Ok(resolved)
}

/// Form an exact `service.endpoint` key from two canonical model identifiers.
///
/// Components must match `[A-Za-z_][A-Za-z0-9_]*`, excluding the single `_`.
/// No trimming or case conversion is applied.
///
/// # Errors
///
/// Returns `ConstraintViolation` if either component is not an identifier.
pub fn method_name(service: &str, endpoint: &str) -> Result<String> {
    validate_identifier(service)?;
    validate_identifier(endpoint)?;
    let mut method = String::from(service);
    method.push('.');
    method.push_str(endpoint);
    Ok(method)
}

// Keep this grammar identical to registry's identifier validation without
// introducing a dependency on registry construction or its private helpers.
fn validate_identifier(name: &str) -> Result<()> {
    let bytes = name.as_bytes();
    let valid_start = bytes.first().is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_');
    if !valid_start || name == "_"
        || !bytes.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        return Err(model_error(ErrorCode::ConstraintViolation, "invalid ASCII identifier"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Error, ErrorSource};
    use alloc::{format, vec};

    const CONVERTED_CASES: [NameCase; 5] = [
        NameCase::Snake,
        NameCase::Camel,
        NameCase::Pascal,
        NameCase::ScreamingSnake,
        NameCase::Kebab,
    ];

    const ALL_CASES: [NameCase; 6] = [
        NameCase::Preserve,
        NameCase::Snake,
        NameCase::Camel,
        NameCase::Pascal,
        NameCase::ScreamingSnake,
        NameCase::Kebab,
    ];

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| String::from(*name)).collect()
    }

    fn assert_error(error: Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!((error.byte_offset, error.schema_id, error.field_id), (0, 0, 0));
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn contract_acronym_vector_covers_every_case() {
        let expected = [
            "HTTPServerID", "http_server_id", "httpServerId", "HttpServerId",
            "HTTP_SERVER_ID", "http-server-id",
        ];
        for (case, expected) in ALL_CASES.into_iter().zip(expected) {
            assert_eq!(convert_case("HTTPServerID", case).unwrap(), expected, "{case:?}");
        }
    }

    #[test]
    fn word_boundaries_acronyms_and_digits_have_exact_vectors() {
        let vectors = [
            ("a", ["a", "a", "A", "A", "a"]),
            ("Z", ["z", "z", "Z", "Z", "z"]),
            ("0", ["0", "0", "0", "0", "0"]),
            ("ABCD", ["abcd", "abcd", "Abcd", "ABCD", "abcd"]),
            ("ABc", ["a_bc", "aBc", "ABc", "A_BC", "a-bc"]),
            ("aBCd", ["a_b_cd", "aBCd", "ABCd", "A_B_CD", "a-b-cd"]),
            ("XMLHttpRequest", [
                "xml_http_request", "xmlHttpRequest", "XmlHttpRequest",
                "XML_HTTP_REQUEST", "xml-http-request",
            ]),
            ("foo42Bar7", ["foo42_bar7", "foo42Bar7", "Foo42Bar7", "FOO42_BAR7", "foo42-bar7"]),
            ("HTTP2ServerID", [
                "http2_server_id", "http2ServerId", "Http2ServerId",
                "HTTP2_SERVER_ID", "http2-server-id",
            ]),
            ("version1beta", ["version1beta", "version1beta", "Version1beta", "VERSION1BETA", "version1beta"]),
            ("version_2Beta", ["version_2_beta", "version2Beta", "Version2Beta", "VERSION_2_BETA", "version-2-beta"]),
            ("123ABCdef", ["123_ab_cdef", "123AbCdef", "123AbCdef", "123_AB_CDEF", "123-ab-cdef"]),
            ("A1B2C3", ["a1_b2_c3", "a1B2C3", "A1B2C3", "A1_B2_C3", "a1-b2-c3"]),
            ("x86_64CPU", ["x86_64_cpu", "x8664Cpu", "X8664Cpu", "X86_64_CPU", "x86-64-cpu"]),
            ("123abc", ["123abc", "123abc", "123abc", "123ABC", "123abc"]),
            ("99_BOTTLES", ["99_bottles", "99Bottles", "99Bottles", "99_BOTTLES", "99-bottles"]),
            ("foo_BAR", ["foo_bar", "fooBar", "FooBar", "FOO_BAR", "foo-bar"]),
        ];
        for (input, expected) in vectors {
            for (case, expected) in CONVERTED_CASES.into_iter().zip(expected) {
                assert_eq!(convert_case(input, case).unwrap(), expected, "{input:?}, {case:?}");
            }
        }
    }

    #[test]
    fn repeated_separators_are_ignored_and_reset_word_boundaries() {
        let expected = ["http_server_id", "httpServerId", "HttpServerId", "HTTP_SERVER_ID", "http-server-id"];
        for separator in ['_', '-', ' ', '\t', '\n', '\r', '\x0c'] {
            let input = format!("{separator}{separator}HTTP{separator}{separator}Server{separator}ID{separator}");
            for (case, expected) in CONVERTED_CASES.into_iter().zip(expected) {
                assert_eq!(convert_case(&input, case).unwrap(), expected, "{input:?}, {case:?}");
            }
        }
        for (case, expected) in CONVERTED_CASES.into_iter().zip(expected) {
            assert_eq!(convert_case("_-- \tHTTP_\n-Server\r\x0c_ID--_", case).unwrap(), expected);
        }
    }

    #[test]
    fn empty_inputs_and_separator_only_conversions_are_rejected() {
        for case in ALL_CASES {
            assert_error(convert_case("", case).unwrap_err(), ErrorCode::ConstraintViolation);
        }
        for input in ["_", "__", "-", "--", " ", "\t", "\n", "\r", "\x0c", "_-- \t\n\r\x0c"] {
            assert_eq!(convert_case(input, NameCase::Preserve).unwrap(), input);
            for case in CONVERTED_CASES {
                assert_error(convert_case(input, case).unwrap_err(), ErrorCode::ConstraintViolation);
            }
        }
    }

    #[test]
    fn preserve_keeps_every_utf8_byte_without_normalization() {
        for input in [
            "é", "e\u{301}", "東京", "İ", "ß", "🙂", "a.B !", "_\0-\r\n",
            " ", "__", "\0", "\u{7f}", "\u{a0}", "\u{2003}",
        ] {
            let actual = convert_case(input, NameCase::Preserve).unwrap();
            assert_eq!(actual.as_bytes(), input.as_bytes());
        }
        assert_ne!(
            convert_case("é", NameCase::Preserve).unwrap(),
            convert_case("e\u{301}", NameCase::Preserve).unwrap(),
        );
    }

    #[test]
    fn conversions_reject_unicode_including_non_ascii_whitespace() {
        for input in ["é", "e\u{301}", "東京", "a🙂b", "İ", "ß", "a\u{a0}b", "a\u{2003}b"] {
            for case in CONVERTED_CASES {
                assert_error(convert_case(input, case).unwrap_err(), ErrorCode::ConstraintViolation);
            }
        }
    }

    #[test]
    fn entire_ascii_domain_obeys_conversion_character_rules() {
        for byte in 0_u8..=127 {
            let mut input = String::from("a");
            input.push(char::from(byte));
            input.push('b');
            assert_eq!(convert_case(&input, NameCase::Preserve).unwrap(), input);
            let allowed = matches!(byte,
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
                    | b'_' | b'-' | b' ' | b'\t' | b'\n' | b'\r' | b'\x0c'
            );
            for case in CONVERTED_CASES {
                let actual = convert_case(&input, case);
                if allowed {
                    assert!(!actual.unwrap().is_empty(), "byte {byte}, {case:?}");
                } else {
                    assert_error(actual.unwrap_err(), ErrorCode::ConstraintViolation);
                }
            }
        }
    }

    #[test]
    fn long_names_and_many_words_have_no_artificial_length_limit() {
        let lower = "a".repeat(65_536);
        let mut title = lower.clone();
        title.replace_range(..1, "A");
        for case in ALL_CASES {
            let expected = match case {
                NameCase::Pascal => title.clone(),
                NameCase::ScreamingSnake => "A".repeat(65_536),
                _ => lower.clone(),
            };
            assert_eq!(convert_case(&lower, case).unwrap(), expected);
        }
        let input = "HTTPServerID_".repeat(1_024);
        let mut expected = "http_server_id_".repeat(1_024);
        expected.pop();
        assert_eq!(convert_case(&input, NameCase::Snake).unwrap(), expected);
    }

    #[test]
    fn default_policy_and_empty_lists_preserve_the_contract() {
        assert_eq!(NameCase::default(), NameCase::Preserve);
        assert_eq!(WireNamePolicy::default(), WireNamePolicy {
            case: NameCase::Preserve, case_insensitive: false,
        });
        for case in ALL_CASES {
            for case_insensitive in [false, true] {
                assert_eq!(resolve_wire_names(&[], WireNamePolicy { case, case_insensitive }).unwrap(), vec![]);
            }
        }
        let names = strings(&["HTTPServerID", "é !\0"]);
        let resolved = resolve_wire_names(&names, WireNamePolicy::default()).unwrap();
        for (name, resolved) in names.iter().zip(resolved) {
            assert_eq!(&resolved.source, name);
            assert_eq!(&resolved.wire, name);
        }
    }

    #[test]
    fn resolution_retains_declaration_order_source_and_owned_output() {
        let mut names = strings(&["zLast", "HTTPServerID", "_aFirst", "item42Count"]);
        let original = names.clone();
        let resolved = resolve_wire_names(&names, WireNamePolicy {
            case: NameCase::Snake, case_insensitive: false,
        }).unwrap();
        assert_eq!(names, original);
        assert_eq!(resolved, vec![
            WireName { source: String::from("zLast"), wire: String::from("z_last") },
            WireName { source: String::from("HTTPServerID"), wire: String::from("http_server_id") },
            WireName { source: String::from("_aFirst"), wire: String::from("a_first") },
            WireName { source: String::from("item42Count"), wire: String::from("item42_count") },
        ]);
        names[0].clear();
        names.clear();
        assert_eq!(resolved[0].source, "zLast");
        assert_eq!(resolved[0].wire, "z_last");
        assert_eq!(resolved.len(), original.len());
    }

    #[test]
    fn repeated_sources_collide_under_every_policy() {
        let names = strings(&["HTTPServerID", "other", "HTTPServerID"]);
        let original = names.clone();
        for case in ALL_CASES {
            for case_insensitive in [false, true] {
                assert_error(
                    resolve_wire_names(&names, WireNamePolicy { case, case_insensitive }).unwrap_err(),
                    ErrorCode::DuplicateObjectKey,
                );
                assert_eq!(names, original);
            }
        }
    }

    #[test]
    fn distinct_sources_with_identical_converted_names_collide() {
        for sources in [
            ["HTTPServerID", "http_server_id"],
            ["foo__bar", "foo-bar"],
            [" a b ", "a_b"],
            ["1A", "1_a"],
        ] {
            let names = strings(&sources);
            for case in CONVERTED_CASES {
                for case_insensitive in [false, true] {
                    assert_error(
                        resolve_wire_names(&names, WireNamePolicy { case, case_insensitive }).unwrap_err(),
                        ErrorCode::DuplicateObjectKey,
                    );
                }
            }
            assert_eq!(resolve_wire_names(&names, WireNamePolicy::default()).unwrap().len(), 2);
        }
        // Camel and Pascal can also merge word sequences after discarding separators.
        let names = strings(&["foo1_2", "foo12"]);
        for case in [NameCase::Camel, NameCase::Pascal] {
            assert_error(
                resolve_wire_names(&names, WireNamePolicy { case, case_insensitive: false }).unwrap_err(),
                ErrorCode::DuplicateObjectKey,
            );
        }
    }

    #[test]
    fn case_insensitive_comparison_does_not_recase_emitted_names() {
        let names = strings(&["Name", "name", "NAME"]);
        assert_eq!(resolve_wire_names(&names, WireNamePolicy::default()).unwrap().len(), 3);
        assert_error(
            resolve_wire_names(&names, WireNamePolicy {
                case: NameCase::Preserve, case_insensitive: true,
            }).unwrap_err(),
            ErrorCode::DuplicateObjectKey,
        );
        let names = strings(&["HTTPServerID", "OtherName"]);
        let resolved = resolve_wire_names(&names, WireNamePolicy {
            case: NameCase::Pascal, case_insensitive: true,
        }).unwrap();
        assert_eq!(resolved[0].source, "HTTPServerID");
        assert_eq!(resolved[0].wire, "HttpServerId");
        assert_eq!(resolved[1].wire, "OtherName");
    }

    #[test]
    fn collision_comparison_folds_only_ascii_and_uses_complete_names() {
        let names = strings(&["a", "ab", "a.b", "a-b", "é", "É", "e\u{301}", "İ", "i", "ß", "ss", "\0"]);
        for case_insensitive in [false, true] {
            let resolved = resolve_wire_names(&names, WireNamePolicy {
                case: NameCase::Preserve, case_insensitive,
            }).unwrap();
            assert_eq!(resolved.len(), names.len());
            for (actual, expected) in resolved.iter().zip(&names) {
                assert_eq!(&actual.source, expected);
                assert_eq!(&actual.wire, expected);
            }
        }
        for names in [strings(&["ÉTAG", "Étag"]), strings(&["a\0B", "A\0b"])] {
            assert_error(
                resolve_wire_names(&names, WireNamePolicy {
                    case: NameCase::Preserve, case_insensitive: true,
                }).unwrap_err(),
                ErrorCode::DuplicateObjectKey,
            );
        }
    }

    #[test]
    fn resolution_rejects_invalid_names_without_mutating_sources() {
        for case in ALL_CASES {
            let invalid: &[&str] = if case == NameCase::Preserve {
                &[""]
            } else {
                &["", "__-- \t", "a.b", "a/b", "a\0b", "café", "a\u{a0}b"]
            };
            for name in invalid {
                for case_insensitive in [false, true] {
                    let names = strings(&["valid", name, "another"]);
                    let original = names.clone();
                    assert_error(
                        resolve_wire_names(&names, WireNamePolicy { case, case_insensitive }).unwrap_err(),
                        ErrorCode::ConstraintViolation,
                    );
                    assert_eq!(names, original);
                }
            }
        }
    }

    #[test]
    fn resolution_errors_follow_declaration_order() {
        for case in ALL_CASES {
            let policy = WireNamePolicy { case, case_insensitive: false };
            let duplicate_first = strings(&["valid", "valid", ""]);
            let invalid_first = strings(&["valid", "", "valid"]);
            assert_error(resolve_wire_names(&duplicate_first, policy).unwrap_err(), ErrorCode::DuplicateObjectKey);
            assert_error(resolve_wire_names(&invalid_first, policy).unwrap_err(), ErrorCode::ConstraintViolation);
            assert_eq!(
                resolve_wire_names(&duplicate_first, policy).unwrap_err(),
                resolve_wire_names(&duplicate_first, policy).unwrap_err(),
            );
        }
    }

    #[test]
    fn method_names_accept_identifier_boundaries_without_recasing() {
        let valid = ["a", "Z", "_a", "__", "_0", "a0_", "HTTPStore_ID", "getHTTPStatus"];
        for service in valid {
            for endpoint in valid {
                assert_eq!(method_name(service, endpoint).unwrap(), format!("{service}.{endpoint}"));
            }
        }
        assert_eq!(method_name("HTTPStore_ID", "getHTTPStatus").unwrap(), "HTTPStore_ID.getHTTPStatus");
        assert_ne!(method_name("Store", "Read").unwrap(), method_name("store", "read").unwrap());
        let long = "A".repeat(65_536);
        assert_eq!(method_name(&long, &long).unwrap(), format!("{long}.{long}"));
    }

    #[test]
    fn method_names_reject_invalid_components_in_either_position() {
        for invalid in [
            "", "_", "0", "9bad", ".", "a.b", "a-b", "a b", " a", "a ",
            "a\t", "a\n", "a\r", "a\0b", "a/b", "é", "e\u{301}", "東京", "🙂",
        ] {
            assert_error(method_name(invalid, "valid").unwrap_err(), ErrorCode::ConstraintViolation);
            assert_error(method_name("valid", invalid).unwrap_err(), ErrorCode::ConstraintViolation);
        }
        assert_eq!(method_name("", "_").unwrap_err(), method_name("", "valid").unwrap_err());
    }

    #[test]
    fn method_identifier_grammar_covers_every_ascii_byte() {
        for byte in 0_u8..=127 {
            let first = String::from(char::from(byte));
            let valid_first = matches!(byte, b'A'..=b'Z' | b'a'..=b'z');
            let tail = format!("a{}", char::from(byte));
            let valid_tail = matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_');
            for (component, valid) in [(&first, valid_first), (&tail, valid_tail)] {
                for (service, endpoint) in [(component.as_str(), "Endpoint"), ("Service", component.as_str())] {
                    let actual = method_name(service, endpoint);
                    if valid {
                        assert_eq!(actual.unwrap(), format!("{service}.{endpoint}"));
                    } else {
                        assert_error(actual.unwrap_err(), ErrorCode::ConstraintViolation);
                    }
                }
            }
        }
    }

    #[test]
    fn method_keys_match_registry_lookup_exactly() {
        use crate::{EndpointModel, EndpointSchemaSpec, ServiceSpec};

        let model = EndpointModel {
            services: vec![ServiceSpec {
                id: u16::MAX,
                name: String::from("HTTPStore_ID"),
                endpoints: vec![EndpointSchemaSpec {
                    code: u32::MAX,
                    name: String::from("getHTTPStatus"),
                    service: String::from("HTTPStore_ID"),
                    roles: Vec::new(),
                    params: Vec::new(),
                    response: Vec::new(),
                    stream_response: None,
                    description: String::new(),
                }],
            }],
            ..EndpointModel::default()
        };
        let registry = crate::registry::build_registry(&model).unwrap();
        let endpoint = &model.services[0].endpoints[0];
        let key = method_name(&model.services[0].name, &endpoint.name).unwrap();
        let found = crate::registry::endpoint_by_method(&registry, &key).unwrap();
        assert!(core::ptr::eq(found, endpoint));
        for miss in ["http_store_id.get_http_status", "HTTPStore_ID.GetHTTPStatus", "HTTPStore_ID.getHTTPStatus."] {
            assert!(crate::registry::endpoint_by_method(&registry, miss).is_none());
        }
    }
}
