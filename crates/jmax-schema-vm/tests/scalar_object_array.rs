use jmax_abi::number::{DecimalPolicy, NumericPolicy};
use jmax_schema_compile::{CompilerOptions, ValidatedProgram};
use jmax_schema_ir::{
    ArrayShape, DecimalConstraints, FieldId, ObjectShape, Property, ScalarConstraints, Schema,
    SchemaId, TypeId, TypeNode, ValueKind,
};
use jmax_schema_vm::{ErrorCode, VmLimits, direct};

type Result<T = ()> = jmax_schema_vm::Result<T>;

fn trade_rows(reference: bool) -> Result<ValidatedProgram> {
    let mut decimal = ScalarConstraints::new(ValueKind::Decimal);
    decimal.decimal = Some(DecimalConstraints {
        policy: DecimalPolicy {
            max_precision: 38,
            target_scale: 2,
            allow_exponent: false,
            reject_inexact_rescale: true,
            tick_step: 1,
        },
        minimum: None,
        maximum: None,
    });
    let schema = Schema {
        name: "scalar_trade_rows".into(),
        schema_id: SchemaId(7_070),
        root: TypeId(0),
        definitions: vec![],
        nodes: vec![
            TypeNode::Array(ArrayShape {
                nullable: false,
                items: TypeId(1),
                min_items: None,
                max_items: None,
            }),
            TypeNode::Object(ObjectShape {
                nullable: false,
                properties: vec![
                    Property {
                        field_id: FieldId(701),
                        name: "symbol".into(),
                        ty: TypeId(2),
                        required: true,
                        slot: None,
                    },
                    Property {
                        field_id: FieldId(702),
                        name: "price".into(),
                        ty: TypeId(3),
                        required: true,
                        slot: None,
                    },
                    Property {
                        field_id: FieldId(703),
                        name: "venue".into(),
                        ty: TypeId(2),
                        required: true,
                        slot: None,
                    },
                ],
                reject_unknown: true,
                min_properties: reference.then_some(0),
                max_properties: None,
            }),
            TypeNode::Scalar(ScalarConstraints::new(ValueKind::String)),
            TypeNode::Scalar(decimal),
        ],
    };
    let resolved = jmax_schema_ir::resolve::resolve(schema, Default::default())?;
    jmax_schema_compile::compile(&resolved, &CompilerOptions::default())
}

#[test]
fn scalar_object_array_accepts_reordered_escaped_decimal_and_string_fields() -> Result {
    let program = trade_rows(false)?;
    let input = br#"[{"venue":"X\u002dNAS","pr\u0069ce":"12\u002e34","symbol":"BTC\u002dUSD"},{"symbol":"ETH","venue":"XNAS","price":"0.01"}]"#;
    direct::validate_borrowed(
        &program,
        input,
        VmLimits::default(),
        NumericPolicy::strict_default(),
    )?;
    Ok(())
}

#[test]
fn scalar_object_array_fallback_keeps_error_precedence() -> Result {
    let fast = trade_rows(false)?;
    let reference = trade_rows(true)?;
    let numeric = NumericPolicy::strict_default();
    for (input, expected) in [
        (
            br#"[{"symbol":"BTC","price":"1.00","price":"2.00","venue":"X"}]"#.as_slice(),
            ErrorCode::DuplicateObjectKey,
        ),
        (
            br#"[{"symbol":"BTC","price":"1.0x","venue":"X"}]"#.as_slice(),
            ErrorCode::InvalidNumber,
        ),
        (
            br#"[{"symbol":"BTC","price":"1.00","venue":"X","extra":true}]"#.as_slice(),
            ErrorCode::UnknownObjectKey,
        ),
        (
            br#"[{"symbol":"BTC","price":"1.00","venue":"X",}]"#.as_slice(),
            ErrorCode::InvalidSyntax,
        ),
    ] {
        let actual =
            direct::validate_borrowed(&fast, input, VmLimits::default(), numeric).unwrap_err();
        let expected_error =
            direct::validate_borrowed(&reference, input, VmLimits::default(), numeric).unwrap_err();
        assert_eq!(actual.code, expected);
        assert_eq!(actual.code, expected_error.code);
        assert_eq!(actual.byte_offset, expected_error.byte_offset);
    }
    Ok(())
}

#[test]
fn warmed_scalar_object_array_matches_reference_work_and_scratch_budgets() -> Result {
    let fast = trade_rows(false)?;
    let reference = trade_rows(true)?;
    let input = br#"[{"venue":"X\u002dNAS","pr\u0069ce":"12\u002e34","symbol":"BTC\u002dUSD"},{"symbol":"ETH","venue":"XNAS","price":"0.01"}]"#;
    let numeric = NumericPolicy::strict_default();
    let mut fast_scratch = jmax_schema_vm::ValidationScratch::default();
    let mut reference_scratch = jmax_schema_vm::ValidationScratch::default();
    for _ in 0..2 {
        direct::validate_borrowed_into(
            &fast,
            input,
            VmLimits::default(),
            numeric,
            &mut fast_scratch,
        )?;
        direct::validate_borrowed_into(
            &reference,
            input,
            VmLimits::default(),
            numeric,
            &mut reference_scratch,
        )?;
    }
    for max_steps in (100..8_000).step_by(37) {
        let limits = VmLimits {
            max_steps,
            ..VmLimits::default()
        };
        let actual =
            direct::validate_borrowed_into(&fast, input, limits, numeric, &mut fast_scratch);
        let expected = direct::validate_borrowed_into(
            &reference,
            input,
            limits,
            numeric,
            &mut reference_scratch,
        );
        assert_eq!(
            actual.as_ref().err(),
            expected.as_ref().err(),
            "steps {max_steps}"
        );
        assert_eq!(actual.is_ok(), expected.is_ok(), "steps {max_steps}");
    }
    for max_scratch_bytes in (0..8_000).step_by(29) {
        let limits = VmLimits {
            max_scratch_bytes,
            ..VmLimits::default()
        };
        let actual =
            direct::validate_borrowed_into(&fast, input, limits, numeric, &mut fast_scratch);
        let expected = direct::validate_borrowed_into(
            &reference,
            input,
            limits,
            numeric,
            &mut reference_scratch,
        );
        assert_eq!(
            actual.as_ref().err(),
            expected.as_ref().err(),
            "scratch {max_scratch_bytes}"
        );
        assert_eq!(
            actual.is_ok(),
            expected.is_ok(),
            "scratch {max_scratch_bytes}"
        );
    }
    Ok(())
}

#[test]
fn fixed_scalar_rows_match_exact_budget_threshold_and_diagnostics() -> Result {
    let fast = trade_rows(false)?;
    let reference = trade_rows(true)?;
    let numeric = NumericPolicy::strict_default();
    for input in [
        br#"[{"symbol":"BTC","price":"12.34","venue":"XNAS"},{"symbol":"ETH","price":"0.01","venue":"XNAS"}]"#.as_slice(),
        br#"[{"price":"12.34","venue":"X","symbol":"BTC"},{"symbol":"ETH","price":"0.01","venue":"X"}]"#,
        br#"[{"symbol":"BTC","price":"12.3","venue":"X"},{"symbol":"ETH","price":"12","venue":"X"}]"#,
        br#"[{"symbol":"BTC","price":"12.340","venue":"X"},{"symbol":"ETH","price":"0.01","venue":"X"}]"#,
    ] {
        let mut a = jmax_schema_vm::ValidationScratch::default();
        let mut b = jmax_schema_vm::ValidationScratch::default();
        direct::validate_borrowed_into(&fast, input, VmLimits::default(), numeric, &mut a)?;
        direct::validate_borrowed_into(&reference, input, VmLimits::default(), numeric, &mut b)?;
        for max_steps in 1..7_000 {
            let limits = VmLimits { max_steps, ..VmLimits::default() };
            let actual = direct::validate_borrowed_into(&fast, input, limits, numeric, &mut a);
            let expected = direct::validate_borrowed_into(&reference, input, limits, numeric, &mut b);
            assert_eq!(actual.as_ref().err(), expected.as_ref().err(), "steps {max_steps}");
        }
    }
    Ok(())
}

#[test]
fn scalar_rows_with_generated_signed_bounds_keep_range_and_work_checks() -> Result {
    fn program(reference: bool) -> Result<ValidatedProgram> {
        let mut integer = ScalarConstraints::new(ValueKind::Integer);
        integer.min_i64 = Some(i64::MIN);
        integer.max_i64 = Some(i64::MAX);
        let schema = Schema {
            name: "signed_rows".into(),
            schema_id: SchemaId(7_071),
            root: TypeId(0),
            definitions: vec![],
            nodes: vec![
                TypeNode::Array(ArrayShape {
                    nullable: false,
                    items: TypeId(1),
                    min_items: None,
                    max_items: None,
                }),
                TypeNode::Object(ObjectShape {
                    nullable: false,
                    reject_unknown: true,
                    min_properties: reference.then_some(0),
                    max_properties: None,
                    properties: vec![Property {
                        field_id: FieldId(1),
                        name: "id".into(),
                        ty: TypeId(2),
                        required: true,
                        slot: None,
                    }],
                }),
                TypeNode::Scalar(integer),
            ],
        };
        let resolved = jmax_schema_ir::resolve::resolve(schema, Default::default())?;
        jmax_schema_compile::compile(&resolved, &CompilerOptions::default())
    }
    let (fast, reference) = (program(false)?, program(true)?);
    for input in [
        br#"[{"id":-9223372036854775808},{"id":9223372036854775807}]"#.as_slice(),
        br#"[{"id":9223372036854775808}]"#,
        br#"[{"id":-9223372036854775809}]"#,
        br#"[{"id":-0}]"#,
        br#"[{"id":1.0}]"#,
    ] {
        let (mut a, mut b) = (
            jmax_schema_vm::ValidationScratch::default(),
            jmax_schema_vm::ValidationScratch::default(),
        );
        for max_steps in [u64::MAX, u64::MAX, 0, 50, 100, 150, 200, 300, 500, 1_000] {
            let limits = VmLimits {
                max_steps,
                ..VmLimits::default()
            };
            let actual = direct::validate_borrowed_into(
                &fast,
                input,
                limits,
                NumericPolicy::strict_default(),
                &mut a,
            );
            let expected = direct::validate_borrowed_into(
                &reference,
                input,
                limits,
                NumericPolicy::strict_default(),
                &mut b,
            );
            assert_eq!(
                actual.as_ref().err(),
                expected.as_ref().err(),
                "{input:?}/{max_steps}"
            );
        }
    }
    Ok(())
}
