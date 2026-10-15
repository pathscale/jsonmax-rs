//! Exact-path extraction and borrowed views into the result's own storage.
//!
//! Spans describe raw JSON tokens, including string quotes and both container
//! delimiters. Cooked strings resolve through the payload or decoded arena
//! retained by `SelectedValues`. Neither spans nor results are schema proofs.

use super::*;

/// Execute all compiled paths in one cursor traversal.
///
/// Slots retain input path order, including repeated paths. Each slot receives
/// its first complete match; missing paths remain absent, while JSON null is a
/// present value. Numeric spelling and whitespace inside containers are kept.
///
/// Strict mode validates through EOF. Query mode may return before validating
/// the suffix, so callers must inspect `SelectedValues::fully_validated` when
/// full syntax validation matters. The result retains the entire supplied
/// payload in either mode. Cursor errors propagate without modification.
pub fn execute(
    payload: Payload,
    paths: &CompiledPaths,
    mode: SkipMode,
    limits: Limits,
) -> Result<SelectedValues> {
    jmax_cursor::select::extract(payload, paths.plan(), mode, limits)
}

/// Return the selected raw token span, or `None` for a missing or invalid slot.
///
/// The offsets refer to `results.payload()` only. A copied span does not carry
/// its owner and does not establish validity for bytes from another payload.
pub fn span(results: &SelectedValues, slot: u32) -> Option<Span32> {
    let slot = usize::try_from(slot).ok()?;
    results.slots().get(slot)?.as_ref().map(|value| value.span)
}

/// Borrow a complete raw token from its result owner using checked bounds.
///
/// Strings include quotes; containers include their delimiters and internal
/// whitespace. Missing slots, out-of-range slots, and invalid spans yield `None`.
pub fn raw(results: &SelectedValues, slot: u32) -> Option<&[u8]> {
    results.raw(slot)
}

/// Borrow a selected string's decoded UTF-8 content from its result owner.
///
/// Resolution uses the string reference's tagged raw or decoded storage.
/// Missing or out-of-range slots, non-string values, invalid references, and
/// invalid UTF-8 yield `None`. An empty string yields `Some("")`.
pub fn string(results: &SelectedValues, slot: u32) -> Option<&str> {
    results.string(slot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn compiled(inputs: &[&str]) -> CompiledPaths {
        super::super::compile::compile_strings(inputs, PathLimits::default()).unwrap()
    }

    fn strict(input: &[u8], inputs: &[&str]) -> SelectedValues {
        execute(Payload::from_slice(input), &compiled(inputs), SkipMode::Strict,
            Limits::strict_default()).unwrap()
    }

    fn assert_value(results: &SelectedValues, slot: u32, kind: ValueKind, bytes: &[u8]) {
        let selected = results.slots()[slot as usize].as_ref().unwrap();
        assert_eq!(selected.kind, kind);
        assert_eq!(span(results, slot), Some(selected.span));
        assert_eq!(raw(results, slot), Some(bytes));
        assert_eq!(selected.span.slice(results.payload().as_slice()), Some(bytes));
        // Raw views must borrow the owner's bytes, including nonzero offsets.
        let offset = selected.span.offset as usize;
        assert!(core::ptr::eq(raw(results, slot).unwrap().as_ptr(),
            results.payload().as_slice()[offset..].as_ptr()));
        assert_eq!(string(results, slot), results.string(slot));
        if kind != ValueKind::String {
            assert_eq!(string(results, slot), None);
        }
    }

    fn assert_absent(results: &SelectedValues, slot: u32) {
        assert_eq!(span(results, slot), None);
        assert_eq!(raw(results, slot), None);
        assert_eq!(string(results, slot), None);
    }

    // Compare the complete public result or error, including decoded references,
    // syntax validation state, untouched owner bytes, and error diagnostics.
    fn assert_cursor_parity(
        input: &[u8],
        paths: &CompiledPaths,
        mode: SkipMode,
        limits: Limits,
    ) -> Result<SelectedValues> {
        let expected = jmax_cursor::select::extract(
            Payload::from_slice(input), paths.plan(), mode, limits);
        let actual = execute(Payload::from_slice(input), paths, mode, limits);
        match (actual, expected) {
            (Ok(actual), Ok(expected)) => {
                assert_eq!(actual.payload().as_slice(), input);
                assert_eq!(actual.payload().as_slice(), expected.payload().as_slice());
                assert_eq!(actual.decoded_bytes(), expected.decoded_bytes());
                assert_eq!(actual.slots(), expected.slots());
                assert_eq!(actual.fully_validated(), expected.fully_validated());
                assert_eq!(actual.slots().len(), paths.plan().slot_count() as usize);
                for slot in 0..paths.plan().slot_count() {
                    assert_eq!(span(&actual, slot),
                        expected.slots()[slot as usize].map(|value| value.span));
                    assert_eq!(raw(&actual, slot), expected.raw(slot));
                    assert_eq!(string(&actual, slot), expected.string(slot));
                }
                assert_absent(&actual, paths.plan().slot_count());
                assert_absent(&actual, u32::MAX);
                Ok(actual)
            }
            (Err(actual), Err(expected)) => {
                assert_eq!(actual, expected);
                Err(actual)
            }
            (actual, expected) => panic!("cursor delegation differs: {actual:?}, {expected:?}"),
        }
    }

    #[test]
    fn root_spans_cover_every_value_kind_without_outer_whitespace() {
        let cases: &[(&[u8], ValueKind)] = &[
            (b"null", ValueKind::Null), (b"true", ValueKind::Bool),
            (b"false", ValueKind::Bool), (b"-12.30e+4", ValueKind::Number),
            (br#""text""#, ValueKind::String), (br#""""#, ValueKind::String),
            (b"{}", ValueKind::Object), (b"[]", ValueKind::Array),
            (br#"{ "a" : [ null, {"b":false} ] }"#, ValueKind::Object),
            (br#"[ 1, { "a": "x" }, [] ]"#, ValueKind::Array),
        ];
        for &(token, kind) in cases {
            let mut input = Vec::from(&b" \n\t"[..]);
            input.extend_from_slice(token);
            input.extend_from_slice(b" \r\n");
            let results = strict(&input, &["$", ""]);
            assert!(results.fully_validated());
            assert_eq!(results.payload().as_slice(), input.as_slice());
            for slot in 0..2 {
                assert_value(&results, slot, kind, token);
                assert_eq!(span(&results, slot), Some(Span32::new(3, token.len() as u32)));
            }
            assert_eq!(results.slots()[0], results.slots()[1]);
        }
    }

    #[test]
    fn slot_order_prefixes_descendants_and_repeated_paths_are_preserved() {
        let input = br#"{"z":9,"a":{"b":[null,"x",{"n":2}]}}"#;
        let results = strict(input, &["a.b[2].n", "z", "$", "a", "a.b[0]", "z", "a.b"]);
        let expected: &[(ValueKind, &[u8])] = &[
            (ValueKind::Number, b"2"), (ValueKind::Number, b"9"),
            (ValueKind::Object, input),
            (ValueKind::Object, br#"{"b":[null,"x",{"n":2}]}"#),
            (ValueKind::Null, b"null"), (ValueKind::Number, b"9"),
            (ValueKind::Array, br#"[null,"x",{"n":2}]"#),
        ];
        assert_eq!(results.slots().len(), expected.len());
        for (slot, &(kind, bytes)) in expected.iter().enumerate() {
            assert_value(&results, slot as u32, kind, bytes);
        }
        assert_eq!(results.slots()[1], results.slots()[5]);
        assert!(span(&results, 3).unwrap().offset < span(&results, 0).unwrap().offset);
    }

    #[test]
    fn missing_null_empty_and_type_mismatches_stay_distinct() {
        let results = strict(br#"{"nil":null,"empty":"","array":[],"object":{},"n":7}"#,
            &["missing", "nil", "empty", "array[0]", "object.x", "n.x", "nil[0]", "n[0]"]);
        assert_eq!(results.slots().len(), 8);
        assert_value(&results, 1, ValueKind::Null, b"null");
        assert_value(&results, 2, ValueKind::String, br#""""#);
        assert_eq!(string(&results, 2), Some(""));
        for slot in [0, 3, 4, 5, 6, 7, 8, u32::MAX] {
            assert_absent(&results, slot);
        }
    }

    #[test]
    fn object_numeric_keys_and_array_indices_are_not_coerced() {
        let paths = [r#"["0"]"#, "[0]", "[1]", "[4294967295]"];
        let object = strict(br#"{"0":"member"}"#, &paths);
        assert_eq!(string(&object, 0), Some("member"));
        for slot in 1..4 { assert_absent(&object, slot); }
        let array = strict(br#"["element"]"#, &paths);
        assert_absent(&array, 0);
        assert_eq!(string(&array, 1), Some("element"));
        assert_absent(&array, 2);
        assert_absent(&array, 3);
        for input in [b"null".as_slice(), b"7", br#""text""#] {
            let scalar = strict(input, &paths);
            for slot in 0..4 { assert_absent(&scalar, slot); }
        }
    }

    #[test]
    fn duplicate_members_fill_each_slot_from_its_first_complete_match() {
        let input = br#"{"a":{},"a":{"b":1},"a":{"b":2},"x":null,"x":9}"#;
        let paths = compiled(&["a", "a.b", "a.b", "x", "a.missing"]);
        let limits = Limits { reject_duplicate_keys: false, ..Limits::strict_default() };
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let results = assert_cursor_parity(input, &paths, mode, limits).unwrap();
            assert_value(&results, 0, ValueKind::Object, b"{}");
            assert_value(&results, 1, ValueKind::Number, b"1");
            assert_eq!(results.slots()[1], results.slots()[2]);
            assert_value(&results, 3, ValueKind::Null, b"null");
            assert_absent(&results, 4);
            // Absence of a.missing requires traversal through EOF in query mode.
            assert!(results.fully_validated());
        }
    }

    #[test]
    fn container_spans_keep_delimiters_lexical_whitespace_and_escaped_punctuation() {
        let token = br#"[  { "s" : "a\"}][b", "n" : -0.00e+02 }, [ ]  ]"#;
        let mut input = Vec::from(&br#" { "value" : "#[..]);
        input.extend_from_slice(token);
        input.extend_from_slice(b" , \"tail\" : 1 } \n");
        let results = strict(&input, &["value", "value[0].s", "value[1]"]);
        assert_value(&results, 0, ValueKind::Array, token);
        assert_value(&results, 2, ValueKind::Array, b"[ ]");
        assert_eq!(string(&results, 1), Some("a\"}][b"));
        assert_eq!(span(&results, 0).unwrap().len as usize, token.len());
    }

    #[test]
    fn cooked_strings_resolve_raw_and_decoded_storage_without_losing_quotes() {
        let cases: &[(&[u8], &str, SpanSource)] = &[
            (br#""""#, "", SpanSource::RawPayload),
            ("\"é界😀\"".as_bytes(), "é界😀", SpanSource::RawPayload),
            (br#""a\n\uD83D\uDE00""#, "a\n😀", SpanSource::DecodedArena),
            (br#""\"\\\/\b\f\n\r\t""#, "\"\\/\u{8}\u{c}\n\r\t", SpanSource::DecodedArena),
            (br#""\u0000\u007f\u0080""#, "\0\u{7f}\u{80}", SpanSource::DecodedArena),
        ];
        for &(input, cooked, source) in cases {
            let results = strict(input, &["$"]);
            assert_value(&results, 0, ValueKind::String, input);
            assert_eq!(string(&results, 0), Some(cooked));
            let reference = results.slots()[0].unwrap().decoded.unwrap();
            assert_eq!(reference.source, source);
            let owner = match source {
                SpanSource::RawPayload => results.payload().as_slice(),
                SpanSource::DecodedArena => results.decoded_bytes(),
            };
            assert_eq!(reference.span.slice(owner), Some(cooked.as_bytes()));
            assert!(core::ptr::eq(string(&results, 0).unwrap().as_ptr(),
                owner[reference.span.offset as usize..].as_ptr()));
        }
    }

    #[test]
    fn decoded_keys_match_exactly_without_unicode_normalization() {
        let input = "{\"a\\u002eb\":1,\"\\u00e9\":2,\"e\\u0301\":3,\"\\uD83D\\uDE00\":4,\"\":5}";
        let results = strict(input.as_bytes(),
            &[r"a\.b", r#"["a.b"]"#, "é", "e\u{0301}", "😀", r#"[""]"#, "a.b"]);
        for (slot, expected) in [b"1", b"1", b"2", b"3", b"4", b"5"].iter().enumerate() {
            assert_value(&results, slot as u32, ValueKind::Number, &expected[..]);
        }
        assert_absent(&results, 6);
    }

    #[test]
    fn programmatic_keys_remain_literal_even_when_they_resemble_extensions() {
        let keys = ["*", "?", "a.b[0]", "\0\n", "$"];
        let paths: Vec<Path> = keys.iter().map(|key| Path::from_segments(
            vec![Segment::Key(String::from(*key))], PathLimits::default()).unwrap()).collect();
        let paths = super::super::compile::compile(&paths, PathLimits::default()).unwrap();
        let input = br#"{"*":0,"?":1,"a.b[0]":2,"\u0000\n":3,"$":4}"#;
        let results = execute(Payload::from_slice(input), &paths, SkipMode::Strict,
            Limits::strict_default()).unwrap();
        for (slot, expected) in [b"0", b"1", b"2", b"3", b"4"].iter().enumerate() {
            assert_value(&results, slot as u32, ValueKind::Number, &expected[..]);
        }
    }

    #[test]
    fn numbers_keep_exact_lexemes_without_conversion_or_rounding() {
        for token in ["0", "-0", "1.0", "1e0", "-0.000e+02", "18446744073709551616",
            "9007199254740993", "1e9999", "0.123456789012345678901234567890"] {
            let results = strict(token.as_bytes(), &["$"]);
            assert_value(&results, 0, ValueKind::Number, token.as_bytes());
            assert_eq!(span(&results, 0), Some(Span32::new(0, token.len() as u32)));
        }
    }

    #[test]
    fn results_and_clones_retain_payload_and_arena_after_input_and_plan_drop() {
        let results = {
            let mut input = Vec::from(&br#"{"s":"a\n\u00e9","raw":"plain"}"#[..]);
            let paths = compiled(&["s", "raw"]);
            let payload = Payload::from_slice(&input);
            input.fill(b'!');
            execute(payload, &paths, SkipMode::Strict, Limits::strict_default()).unwrap()
        };
        let copy = results.clone();
        drop(results);
        assert_eq!(copy.payload().as_slice(), br#"{"s":"a\n\u00e9","raw":"plain"}"#);
        assert_value(&copy, 0, ValueKind::String, br#""a\n\u00e9""#);
        assert_eq!(string(&copy, 0), Some("a\né"));
        assert_eq!(string(&copy, 1), Some("plain"));
        assert!(copy.fully_validated());
    }

    #[test]
    fn both_payload_backings_preserve_all_owner_bytes_in_both_modes() {
        let input = br#" {"s":"\u0061","tail":[1,2,3]}  "#;
        let paths = compiled(&["s"]);
        for mode in [SkipMode::Strict, SkipMode::Query] {
            for payload in [Payload::from_slice(input), Payload::from_vec(Vec::from(&input[..]))] {
                let results = execute(payload, &paths, mode, Limits::strict_default()).unwrap();
                assert_eq!(results.payload().as_slice(), input);
                assert_eq!(string(&results, 0), Some("a"));
                assert_value(&results, 0, ValueKind::String, br#""\u0061""#);
                if mode == SkipMode::Strict { assert!(results.fully_validated()); }
            }
        }
    }

    #[test]
    fn empty_selection_still_validates_strict_input_and_has_no_accessible_slots() {
        let paths = compiled(&[]);
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let results = assert_cursor_parity(b" [null] \n", &paths, mode,
                Limits::strict_default()).unwrap();
            assert!(results.slots().is_empty());
            assert_absent(&results, 0);
            if mode == SkipMode::Strict { assert!(results.fully_validated()); }
        }
        for input in [b"".as_slice(), b"[", b"null false", br#"{"x":"\q"}"#] {
            assert!(assert_cursor_parity(input, &paths, SkipMode::Strict,
                Limits::strict_default()).is_err());
        }
    }

    #[test]
    fn query_suffix_and_validation_flag_follow_the_cursor_policy_exactly() {
        let paths = compiled(&["hit", "hit"]);
        for input in [br#"{"hit":"\n","tail":[1,2]}"#.as_slice(),
            br#"{"hit":"\n","tail":["\q"]}"#,
            br#"{"hit":"\n"} trailing"#] {
            let outcome = assert_cursor_parity(input, &paths, SkipMode::Query,
                Limits::strict_default());
            if let Ok(results) = outcome {
                assert_eq!(results.payload().as_slice(), input);
                assert_eq!(string(&results, 0), Some("\n"));
                assert_eq!(results.slots()[0], results.slots()[1]);
                if input != br#"{"hit":"\n","tail":[1,2]}"# {
                    assert!(!results.fully_validated());
                }
            }
        }
        // Query may stop early or finish validation; delegation preserves both
        // legal policies instead of inventing a validation flag in this layer.
    }

    #[test]
    fn query_missing_paths_require_eof_and_surface_invalid_suffixes() {
        let paths = compiled(&["hit", "missing"]);
        let results = assert_cursor_parity(br#"{"hit":null}"#, &paths, SkipMode::Query,
            Limits::strict_default()).unwrap();
        assert_value(&results, 0, ValueKind::Null, b"null");
        assert_absent(&results, 1);
        assert!(results.fully_validated());
        for input in [br#"{"hit":null,"tail":["\q"]}"#.as_slice(),
            br#"{"hit":null} false"#, br#"{"hit":null,"tail":["#] {
            assert!(assert_cursor_parity(input, &paths, SkipMode::Query,
                Limits::strict_default()).is_err());
        }
    }

    #[test]
    fn strict_errors_propagate_unchanged_even_after_a_slot_is_filled() {
        let paths = compiled(&["hit"]);
        let invalid: &[&[u8]] = &[
            b"", b" \t\r\n", b"[", b"{", b"[1,]", b"{\"x\":}", b"null true",
            br#"{"hit":0,"x":"\q"}"#, br#"{"hit":0,"x":"\uD800"}"#,
            br#"{"hit":0,"x":01}"#, br#"{"hit":0,"x":1e}"#,
            b"{\"hit\":0,\"x\":\"\xff\"}", b"{\"hit\":0,\"x\":\"\n\"}",
            br#"{"hit":0,"x":[1,2}"#, br#"{"hit":0} trailing"#,
        ];
        for &input in invalid {
            let error = assert_cursor_parity(input, &paths, SkipMode::Strict,
                Limits::strict_default()).unwrap_err();
            assert!(!error.detail.is_empty());
            assert!(error.byte_offset as usize <= input.len());
            assert_eq!((error.schema_id, error.field_id), (0, 0));
        }
    }

    #[test]
    fn duplicate_key_rejection_and_diagnostics_are_not_overridden() {
        let paths = compiled(&["a"]);
        for input in [br#"{"a":1,"a":2}"#.as_slice(), br#"{"a":1,"\u0061":2}"#] {
            let error = assert_cursor_parity(input, &paths, SkipMode::Strict,
                Limits::strict_default()).unwrap_err();
            assert_eq!(error.code, ErrorCode::DuplicateObjectKey);
        }
    }

    #[test]
    fn payload_limit_is_inclusive_including_unconsumed_query_suffix_bytes() {
        let input = br#"{"hit":0,"tail":[1,2,3]}"#;
        let paths = compiled(&["hit"]);
        let limits = Limits { max_payload_bytes: input.len() as u32, ..Limits::strict_default() };
        for mode in [SkipMode::Strict, SkipMode::Query] {
            assert!(assert_cursor_parity(input, &paths, mode, limits).is_ok());
            let error = assert_cursor_parity(input, &paths, mode,
                Limits { max_payload_bytes: input.len() as u32 - 1, ..limits }).unwrap_err();
            assert_eq!(error.code, ErrorCode::PayloadTooLarge);
            let error = assert_cursor_parity(input, &paths, mode,
                Limits { max_payload_bytes: 0, ..limits }).unwrap_err();
            assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        }
    }

    #[test]
    fn syntax_resource_limits_accept_the_boundary_and_reject_the_next_unit() {
        let defaults = Limits::strict_default();
        let cases: &[(Limits, &[u8], &[u8], ErrorCode)] = &[
            (Limits { max_depth: 2, ..defaults }, b"[[]]", b"[[[]]]", ErrorCode::DepthLimitExceeded),
            (Limits { max_array_items: 1, ..defaults }, b"[0]", b"[0,1]", ErrorCode::ConstraintViolation),
            (Limits { max_array_items: 0, ..defaults }, b"[]", b"[0]", ErrorCode::ConstraintViolation),
            (Limits { max_fields_per_object: 1, ..defaults }, br#"{"a":0}"#,
                br#"{"a":0,"b":1}"#, ErrorCode::ConstraintViolation),
            (Limits { max_fields_per_object: 0, ..defaults }, b"{}",
                br#"{"a":0}"#, ErrorCode::ConstraintViolation),
            (Limits { max_string_bytes: 1, ..defaults }, br#"["a"]"#,
                br#"["ab"]"#, ErrorCode::ConstraintViolation),
            (Limits { max_string_bytes: 0, ..defaults }, br#"[""]"#,
                br#"["a"]"#, ErrorCode::ConstraintViolation),
            (Limits { max_number_digits: 2, ..defaults }, b"[1e2]", b"[1e23]", ErrorCode::ConstraintViolation),
            (Limits { max_decoded_bytes: 1, ..defaults }, br#"["\n"]"#,
                br#"["\n","\t"]"#, ErrorCode::DecodedArenaOverflow),
        ];
        let paths = compiled(&["$"]);
        for &(limits, allowed, denied, code) in cases {
            let results = assert_cursor_parity(allowed, &paths, SkipMode::Strict, limits).unwrap();
            assert_eq!(raw(&results, 0), Some(allowed));
            assert!(results.fully_validated());
            let error = assert_cursor_parity(denied, &paths, SkipMode::Strict, limits).unwrap_err();
            assert_eq!(error.code, code);
        }
    }

    #[test]
    fn zero_resource_limits_still_allow_scalars_that_use_none_of_the_resource() {
        let limits = Limits {
            max_payload_bytes: 4, max_decoded_bytes: 0, max_depth: 0,
            max_fields_per_object: 0, max_string_bytes: 0, max_number_digits: 0,
            max_array_items: 0, reject_duplicate_keys: true, reject_unknown_keys: true,
        };
        let paths = compiled(&["$"]);
        for input in [b"null".as_slice(), b"true", br#""""#] {
            let results = assert_cursor_parity(input, &paths, SkipMode::Strict, limits).unwrap();
            assert_eq!(raw(&results, 0), Some(input));
        }
        for input in [b"[]".as_slice(), b"{}", b"0", br#""a""#] {
            assert!(assert_cursor_parity(input, &paths, SkipMode::Strict, limits).is_err());
        }
    }

    #[test]
    fn many_repeated_slots_and_extreme_slot_arguments_remain_range_checked() {
        let inputs = vec!["$"; 257];
        let paths = compiled(&inputs);
        let results = execute(Payload::from_slice(br#""\u00e9""#), &paths,
            SkipMode::Strict, Limits::strict_default()).unwrap();
        assert_eq!(results.slots().len(), 257);
        for slot in 0..257 {
            assert_eq!(span(&results, slot), Some(Span32::new(0, 8)));
            assert_eq!(raw(&results, slot), Some(&br#""\u00e9""#[..]));
            assert_eq!(string(&results, slot), Some("é"));
        }
        for slot in [257, 258, 65_535, 65_536, u32::MAX - 1, u32::MAX] {
            assert_absent(&results, slot);
        }
    }
}
