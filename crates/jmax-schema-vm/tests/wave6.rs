use jmax_abi::number::NumericPolicy;
use jmax_schema_compile::{CompilerOptions, ValidatedProgram};
use jmax_schema_ir::{
    ArrayShape, FieldId, ObjectShape, Property, ScalarConstraints, Schema, SchemaId, TypeId,
    TypeNode, ValueKind,
};
use jmax_schema_vm::{
    ErrorCode, Payload, Selection, SlotValue, ValidationScratch, VmLimits, direct,
};

type Result<T = ()> = jmax_schema_vm::Result<T>;

fn program() -> Result<ValidatedProgram> {
    let property = |id, name: &str, ty, slot, required| Property {
        field_id: FieldId(id),
        name: name.into(),
        ty: TypeId(ty),
        required,
        slot,
    };
    let mut integer = ScalarConstraints::new(ValueKind::Integer);
    integer.min_i64 = Some(0);
    integer.max_i64 = Some(10);
    let mut string = ScalarConstraints::new(ValueKind::String);
    string.min_length = Some(2);
    string.max_length = Some(3);
    let schema = Schema {
        name: "wave6".into(),
        schema_id: SchemaId(46),
        root: TypeId(0),
        definitions: vec![],
        nodes: vec![
            TypeNode::Object(ObjectShape { nullable: false,
                properties: vec![
                    property(1, "n", 1, Some(0), true),
                    property(2, "s", 2, Some(1), false),
                    property(3, "free", 3, Some(2), false),
                    property(4, "list", 4, None, false),
                ],
                reject_unknown: false,
                min_properties: None,
                max_properties: None,
            }),
            TypeNode::Scalar(integer),
            TypeNode::Scalar(string),
            TypeNode::Any,
            TypeNode::Array(ArrayShape { nullable: false,
                items: TypeId(2),
                min_items: Some(1),
                max_items: Some(2),
            }),
        ],
    };
    let resolved = jmax_schema_ir::resolve::resolve(schema, Default::default())?;
    jmax_schema_compile::compile(&resolved, &CompilerOptions::default())
}
fn limits() -> VmLimits {
    let mut limits = VmLimits::default();
    limits.syntax.reject_unknown_keys = false;
    limits
}
fn selected(strict: bool) -> Selection {
    Selection {
        field_ids: vec![FieldId(1)],
        strict_validate_unselected: strict,
    }
}
fn validate(raw: &[u8]) -> Result {
    direct::validate_borrowed(&program()?, raw, limits(), NumericPolicy::strict_default())
        .map(|_| ())
}

#[test]
fn borrowed_extraction_preserves_input_and_scalar_metadata() -> Result {
    let program = program()?;
    let raw = "{\"n\":7,\"s\":\"é😀\"}".as_bytes();
    let values = direct::extract_borrowed(
        &program,
        raw,
        &Selection {
            field_ids: vec![],
            strict_validate_unselected: true,
        },
        limits(),
        NumericPolicy::strict_default(),
    )?;
    assert_eq!(values.raw.as_ptr(), raw.as_ptr());
    assert!(values.decoded.is_empty());
    assert_eq!(values.slots[0], SlotValue::U64(7));
    let SlotValue::Str(reference) = values.slots[1] else {
        panic!("missing string")
    };
    assert_eq!(reference.scalar_len, 2);
    assert_eq!(reference.span.slice(raw), Some("é😀".as_bytes()));
    assert!(values.capability.matches_slice(
        raw,
        values.decoded.len(),
        program.image().header.fingerprint,
        direct::policy_fingerprint(limits(), NumericPolicy::strict_default())
    ));
    Ok(())
}

#[test]
fn escaped_known_and_unknown_duplicate_keys_are_rejected() -> Result {
    for raw in [
        br#"{"n":1,"\u006e":2}"#.as_slice(),
        br#"{"n":1,"x":0,"\u0078":1}"#,
        br#"{"n":1,"free":{"x":0,"\u0078":1}}"#,
        br#"{"n":1,"unknown":[{"x":0,"\u0078":1}]}"#,
    ] {
        assert_eq!(
            validate(raw).unwrap_err().code,
            ErrorCode::DuplicateObjectKey,
            "{raw:?}"
        );
    }
    Ok(())
}

#[test]
fn strict_skip_rejects_invalid_interior_and_numeric_policy() -> Result {
    for raw in [
        br#"{"n":1,"free":[1,]}"#.as_slice(),
        br#"{"n":1,"unknown":{"bad":"\uD800"}}"#,
        br#"{"n":1,"free":[01]}"#,
        br#"{"n":1,"unknown":[-0]}"#,
        br#"{"n":1,"free":[1e999]}"#,
        b"{\"n\":1,\"unknown\":\"\xff\"}",
        br#"{"n":1,"free":{]}"#,
    ] {
        assert!(validate(raw).is_err(), "accepted {raw:?}");
    }
    Ok(())
}

#[test]
fn unselected_constraints_required_unknown_and_eof_remain_strict() -> Result {
    let program = program()?;
    for raw in [
        br#"{"n":1,"s":"x"}"#.as_slice(),
        br#"{"s":"ok"}"#,
        br#"{"n":11}"#,
        br#"{"n":1,"list":[]}"#,
        br#"{"n":1} false"#,
        br#"{"n":1,}"#,
        br#"{"n":1 "s":"ok"}"#,
    ] {
        assert!(
            direct::extract_borrowed(
                &program,
                raw,
                &selected(true),
                limits(),
                NumericPolicy::strict_default()
            )
            .is_err(),
            "accepted {raw:?}"
        );
    }
    let mut reject = limits();
    reject.syntax.reject_unknown_keys = true;
    assert_eq!(
        direct::validate_borrowed(
            &program,
            br#"{"n":1,"x":0}"#,
            reject,
            NumericPolicy::strict_default()
        )
        .unwrap_err()
        .code,
        ErrorCode::UnknownObjectKey
    );
    Ok(())
}

#[test]
fn query_skips_unselected_schema_and_lexical_interiors() -> Result {
    let program = program()?;
    let raw = br#"{"n":2,"s":"x","free":{"bad":"\q","atom":garbage},"list":[]}"#;
    assert!(
        direct::extract_borrowed(
            &program,
            raw,
            &selected(true),
            limits(),
            NumericPolicy::strict_default()
        )
        .is_err()
    );
    let query = direct::extract_query_borrowed(
        &program,
        raw,
        &selected(false),
        limits(),
        NumericPolicy::strict_default(),
    )?;
    assert_eq!(query.raw.as_ptr(), raw.as_ptr());
    assert_eq!(
        query.slots,
        vec![SlotValue::U64(2), SlotValue::Empty, SlotValue::Empty]
    );
    assert!(query.decoded.is_empty());
    Ok(())
}

#[test]
fn warm_validation_and_extraction_recover_after_failure() -> Result {
    let program = program()?;
    let mut scratch = ValidationScratch::default();
    let numeric = NumericPolicy::strict_default();
    for _ in 0..3 {
        let output = direct::extract_borrowed_with_scratch(
            &program,
            br#"{"n":3,"s":"\u00e9x","free":{"k":"\n"}}"#,
            &Selection {
                field_ids: vec![],
                strict_validate_unselected: true,
            },
            limits(),
            numeric,
            &mut scratch,
        )?;
        assert_eq!(output.slots[0], SlotValue::U64(3));
        assert!(
            direct::validate_borrowed_into(
                &program,
                br#"{"n":3,"free":[1,]}"#,
                limits(),
                numeric,
                &mut scratch
            )
            .is_err()
        );
        direct::validate_borrowed_into(&program, br#"{"n":4}"#, limits(), numeric, &mut scratch)?;
        assert!(scratch.slots().is_none());
    }
    Ok(())
}

#[test]
fn limits_apply_inside_skipped_subtrees_and_across_siblings() -> Result {
    let program = program()?;
    let numeric = NumericPolicy::strict_default();
    let cases: &[(&[u8], fn(&mut VmLimits))] = &[
        (br#"{"n":1,"free":[[[]]]}"#, |l| l.syntax.max_depth = 3),
        (br#"{"n":1,"free":[0,1]}"#, |l| l.syntax.max_array_items = 1),
        (br#"{"n":1,"free":{"a":0,"b":1}}"#, |l| {
            l.syntax.max_fields_per_object = 1
        }),
        (br#"{"n":1,"free":"abcd"}"#, |l| {
            l.syntax.max_string_bytes = 3
        }),
        (br#"{"n":1,"free":1234}"#, |l| {
            l.syntax.max_number_digits = 3
        }),
        (br#"{"n":1,"x":"\n\n","y":"\n\n"}"#, |l| {
            l.syntax.max_decoded_bytes = 3
        }),
    ];
    for (raw, change) in cases {
        let mut bounded = limits();
        change(&mut bounded);
        assert!(
            direct::validate_borrowed(&program, raw, bounded, numeric).is_err(),
            "accepted {raw:?}"
        );
    }
    Ok(())
}

#[test]
fn duplicate_disabled_allows_uncaptured_unknown_duplicates() -> Result {
    let program = program()?;
    let mut permissive = limits();
    permissive.syntax.reject_duplicate_keys = false;
    direct::validate_borrowed(
        &program,
        br#"{"n":1,"n":2,"x":0,"x":1,"free":{"x":0,"x":1}}"#,
        permissive,
        NumericPolicy::strict_default(),
    )?;
    Ok(())
}

#[test]
fn owned_direct_results_preserve_payload_owner() -> Result {
    let program = program()?;
    let payload = Payload::from_vec(br#"{"n":5}"#.to_vec());
    let ptr = payload.as_slice().as_ptr();
    let output = direct::extract_payload_direct(
        &program,
        payload,
        &selected(true),
        limits(),
        NumericPolicy::strict_default(),
    )?;
    assert_eq!(output.payload.as_slice().as_ptr(), ptr);
    assert_eq!(output.slots[0], SlotValue::U64(5));
    Ok(())
}

#[test]
fn live_guard_reextraction_rejects_policy_substitution_and_keeps_original_borrow() -> Result {
    let program = program()?;
    let numeric = NumericPolicy::strict_default();
    let raw = br#"{"n":1,"free":{"x":"\n"}}"#;
    let mut source = direct::extract_borrowed(&program, raw, &selected(true), limits(), numeric)?;
    source.raw = b"invalid";
    let output =
        direct::extract_validated_borrowed(&program, &source, &selected(true), limits(), numeric)?;
    assert_eq!(output.raw.as_ptr(), raw.as_ptr());
    let mut changed = limits();
    changed.syntax.max_depth = 1;
    assert!(
        direct::extract_validated_borrowed(&program, &source, &selected(true), changed, numeric)
            .is_err()
    );
    Ok(())
}

#[test]
fn compiled_pattern_and_decimal_constraints_apply_without_capture() -> Result {
    let mut string = ScalarConstraints::new(ValueKind::String);
    string.pattern = Some("^a+$".into());
    let schema = Schema {
        name: "pattern".into(),
        schema_id: SchemaId(65),
        root: TypeId(0),
        definitions: vec![],
        nodes: vec![TypeNode::Scalar(string)],
    };
    let resolved = jmax_schema_ir::resolve::resolve(schema, Default::default())?;
    let program = jmax_schema_compile::compile(&resolved, &CompilerOptions::default())?;
    direct::validate_borrowed(
        &program,
        br#""aaa""#,
        limits(),
        NumericPolicy::strict_default(),
    )?;
    assert_eq!(
        direct::validate_borrowed(
            &program,
            br#""aab""#,
            limits(),
            NumericPolicy::strict_default()
        )
        .unwrap_err()
        .code,
        ErrorCode::PatternMismatch
    );
    let mut decimal = ScalarConstraints::new(ValueKind::Decimal);
    decimal.decimal = Some(jmax_schema_ir::DecimalConstraints {
        policy: jmax_abi::number::DecimalPolicy::spot_8(),
        minimum: Some(jmax_abi::number::Decimal128::new(100_000_000, 8)),
        maximum: Some(jmax_abi::number::Decimal128::new(200_000_000, 8)),
    });
    let schema = Schema {
        name: "decimal".into(),
        schema_id: SchemaId(66),
        root: TypeId(0),
        definitions: vec![],
        nodes: vec![TypeNode::Scalar(decimal)],
    };
    let resolved = jmax_schema_ir::resolve::resolve(schema, Default::default())?;
    let program = jmax_schema_compile::compile(&resolved, &CompilerOptions::default())?;
    direct::validate_borrowed(
        &program,
        br#""1.5""#,
        limits(),
        NumericPolicy::strict_default(),
    )?;
    assert!(
        direct::validate_borrowed(
            &program,
            br#""2.5""#,
            limits(),
            NumericPolicy::strict_default()
        )
        .is_err()
    );
    let mut bounded = limits();
    bounded.syntax.max_number_digits = 1;
    assert!(
        direct::validate_borrowed(
            &program,
            br#""1.5""#,
            bounded,
            NumericPolicy::strict_default()
        )
        .is_err()
    );
    Ok(())
}
