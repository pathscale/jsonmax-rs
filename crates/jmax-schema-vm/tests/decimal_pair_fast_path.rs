use jmax_abi::number::{DecimalPolicy, NumericPolicy};
use jmax_schema_compile::{CompilerOptions, ValidatedProgram};
use jmax_schema_ir::{
    ArrayShape, DecimalConstraints, FieldId, ObjectShape, Property, ScalarConstraints, Schema,
    SchemaId, TypeId, TypeNode, ValueKind,
};
use jmax_schema_vm::{ErrorCode, Selection, SlotValue, VmLimits, direct};

type Result<T = ()> = jmax_schema_vm::Result<T>;

fn program() -> Result<ValidatedProgram> {
    program_inner(false)
}

fn program_inner(bounded: bool) -> Result<ValidatedProgram> {
    let mut decimal = ScalarConstraints::new(ValueKind::Decimal);
    decimal.decimal = Some(DecimalConstraints {
        policy: DecimalPolicy::spot_8(),
        minimum: bounded.then(|| jmax_abi::number::Decimal128::new(0, 0)),
        maximum: None,
    });
    let schema = Schema {
        name: "decimal_price_levels".into(),
        schema_id: SchemaId(7_057),
        root: TypeId(0),
        definitions: vec![],
        nodes: vec![
            TypeNode::Object(ObjectShape {
                nullable: false,
                properties: vec![Property {
                    field_id: FieldId(75),
                    name: "levels".into(),
                    ty: TypeId(1),
                    required: true,
                    slot: Some(0),
                }],
                reject_unknown: true,
                min_properties: None,
                max_properties: None,
            }),
            TypeNode::Array(ArrayShape {
                nullable: false,
                items: TypeId(2),
                min_items: None,
                max_items: None,
            }),
            TypeNode::Array(ArrayShape {
                nullable: false,
                items: TypeId(3),
                min_items: Some(2),
                max_items: Some(2),
            }),
            TypeNode::Scalar(decimal),
        ],
    };
    let resolved = jmax_schema_ir::resolve::resolve(schema, Default::default())?;
    jmax_schema_compile::compile(&resolved, &CompilerOptions::default())
}

fn selection() -> Selection {
    Selection {
        field_ids: vec![FieldId(75)],
        strict_validate_unselected: true,
    }
}

#[test]
fn fixed_decimal_pairs_validate_and_capture_only_the_outer_span() -> Result {
    let program = program()?;
    let raw = br#"{"levels":[["10.25","0.1"],["2.0","3.5"]]}"#;
    let values = direct::extract_borrowed(
        &program,
        raw,
        &selection(),
        VmLimits::default(),
        NumericPolicy::strict_default(),
    )?;
    let SlotValue::RawSpan(span) = values.slots[0] else {
        panic!("array capture must stay a raw span")
    };
    assert_eq!(
        span.slice(raw),
        Some(&raw[b"{\"levels\":".len()..raw.len() - 1])
    );
    assert!(values.decoded.is_empty());
    Ok(())
}

#[test]
fn escaped_and_exponent_decimal_strings_fall_back_to_scalar_validation() -> Result {
    let program = program()?;
    for raw in [
        br#"{"levels":[["1\u002e5","2.0"]]}"#.as_slice(),
        br#"{"levels":[["1e0","2.0"]]}"#,
    ] {
        let result = direct::validate_borrowed(
            &program,
            raw,
            VmLimits::default(),
            NumericPolicy::strict_default(),
        );
        if raw.contains(&b'\\') {
            assert!(result.is_ok());
        } else {
            assert_eq!(result.unwrap_err().code, ErrorCode::InvalidNumber);
        }
    }
    Ok(())
}

#[test]
fn pair_shape_and_depth_limits_use_vm_diagnostics() -> Result {
    let program = program()?;
    let raw = br#"{"levels":[["1.0"]]}"#;
    let error = direct::validate_borrowed(
        &program,
        raw,
        VmLimits::default(),
        NumericPolicy::strict_default(),
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::ConstraintViolation);

    let mut limits = VmLimits::default();
    limits.syntax.max_depth = 2;
    let error = direct::validate_borrowed(
        &program,
        br#"{"levels":[["1.0","2.0"]]}"#,
        limits,
        NumericPolicy::strict_default(),
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::DepthLimitExceeded);
    Ok(())
}

#[test]
fn warmed_decimal_pairs_match_reference_budget_and_storage() -> Result {
    let fast = program_inner(false)?;
    // A harmless minimum forces the established decimal/array engine. Its
    // prepaid scalar work is identical for these positive values.
    let reference = program_inner(true)?;
    let raw = br#"{"levels":[["10.25000000","0.10000000"],["2.00000000","3.50000000"]]}"#;
    let mut fast_scratch = jmax_schema_vm::ValidationScratch::default();
    let mut exact_scratch = jmax_schema_vm::ValidationScratch::default();
    let numeric = NumericPolicy::strict_default();
    direct::validate_borrowed_into(&fast, raw, VmLimits::default(), numeric, &mut fast_scratch)?;
    direct::validate_borrowed_into(
        &reference,
        raw,
        VmLimits::default(),
        numeric,
        &mut exact_scratch,
    )?;
    for max_steps in (100..12000).step_by(41) {
        let mut limits = VmLimits::default();
        limits.max_steps = max_steps;
        let actual = direct::validate_borrowed_into(&fast, raw, limits, numeric, &mut fast_scratch);
        let expected =
            direct::validate_borrowed_into(&reference, raw, limits, numeric, &mut exact_scratch);
        assert_eq!(
            actual.as_ref().err(),
            expected.as_ref().err(),
            "steps {max_steps}"
        );
        assert_eq!(actual.is_ok(), expected.is_ok(), "steps {max_steps}");
    }
    let selected = selection();
    for _ in 0..2 {
        let actual = direct::extract_borrowed_with_scratch(
            &fast,
            raw,
            &selected,
            VmLimits::default(),
            numeric,
            &mut fast_scratch,
        )?;
        let expected = direct::extract_borrowed_with_scratch(
            &reference,
            raw,
            &selected,
            VmLimits::default(),
            numeric,
            &mut exact_scratch,
        )?;
        assert_eq!(actual.slots, expected.slots);
        assert_eq!(actual.decoded, expected.decoded);
        assert!(actual.decoded.is_empty());
    }
    Ok(())
}
