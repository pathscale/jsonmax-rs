use jmax_abi::schema::SchemaFieldIr;
use jsonmax::{
    CompileLimits, ErrorCode, ErrorSource, FieldId, ParseOptions, Payload, SchemaId, SchemaIr,
    Selection, SlotValue, endpoint, parse,
};

fn schema() -> jsonmax::Result<endpoint::CompiledSchema> {
    let number = SchemaFieldIr {
        field_id: FieldId(1),
        name: "value".into(),
        required: true,
        scalar_kind: Some(jmax_abi::syntax::ScalarKind::Number),
        format: None,
        min_len: None,
        max_len: None,
        min_i64: Some(0),
        max_i64: Some(10),
    };
    endpoint::compile_schema(
        &SchemaIr {
            name: "validation-reuse".into(),
            schema_id: SchemaId(7),
            required_count: 1,
            reject_unknown: true,
            fields: vec![number],
        },
        CompileLimits::default_limits(),
    )
}

#[test]
fn warm_syntax_validation_reuses_state_and_keeps_diagnostics_exact() -> jsonmax::Result<()> {
    let options = ParseOptions::default();
    for input in [br#"{"a":"\u00e9","b":[1,true]}"#.as_slice(), b"null"] {
        parse::validate_syntax(input, options)?;
        parse::validate_syntax(input, options)?;
    }

    for input in [br#"{"a":1,"a":2}"#.as_slice(), b"[1,]"] {
        let expected = parse::validate_borrowed(input, options, &mut jsonmax::Scratch::default())
            .expect_err("fixture must be invalid");
        for _ in 0..2 {
            let actual = parse::validate_syntax(input, options).expect_err("fixture must fail");
            assert_eq!(actual, expected);
        }
    }
    Ok(())
}

#[test]
fn strict_payload_extraction_repeats_and_recovers_after_failure() -> jsonmax::Result<()> {
    let schema = schema()?;
    let selection = Selection {
        field_ids: vec![FieldId(1)],
        strict_validate_unselected: true,
    };
    let options = ParseOptions::default();
    for (raw, expected) in [(br#"{"value":7}"#.as_slice(), 7), (b"{\"value\":3}", 3)] {
        let slots =
            endpoint::extract_payload(Payload::from_slice(raw), &schema, &selection, options)?;
        assert_eq!(slots.slots, vec![SlotValue::U64(expected)]);
    }

    let invalid = Payload::from_slice(br#"{"value":11}"#);
    let expected = endpoint::extract_payload_into(
        &invalid,
        &schema,
        &selection,
        options,
        &mut endpoint::ValidationScratch::default(),
    )
    .expect_err("reference extraction must reject out-of-range value");
    let error = endpoint::extract_payload(invalid, &schema, &selection, options)
        .expect_err("out-of-range value must fail");
    assert_eq!(error, expected);
    assert_eq!(error.code, ErrorCode::ConstraintViolation);
    assert_eq!(error.source, ErrorSource::SchemaVm);
    assert_eq!(error.schema_id, 7);

    let slots = endpoint::extract_payload(
        Payload::from_slice(br#"{"value":4}"#),
        &schema,
        &selection,
        options,
    )?;
    assert_eq!(slots.slots, vec![SlotValue::U64(4)]);
    Ok(())
}
