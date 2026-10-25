use jmax_abi::number::{Decimal128, DecimalPolicy, NumericPolicy};
use jmax_schema_compile::{CompilerOptions, ValidatedProgram};
use jmax_schema_ir::{
    ArrayShape, DecimalConstraints, FieldId, ObjectShape, Property, ScalarConstraints, Schema,
    SchemaId, TypeId, TypeNode, ValueKind,
};
use jmax_schema_vm::{Selection, SlotValue, VmLimits, direct};

type Result<T = ()> = jmax_schema_vm::Result<T>;

fn program() -> Result<ValidatedProgram> {
    let mut decimal = ScalarConstraints::new(ValueKind::Decimal);
    decimal.decimal = Some(DecimalConstraints {
        policy: DecimalPolicy {
            max_precision: 38,
            target_scale: 0,
            allow_exponent: false,
            reject_inexact_rescale: true,
            tick_step: 1,
        },
        minimum: None,
        maximum: None,
    });
    let schema = Schema {
        name: "decimal_pairs".into(),
        schema_id: SchemaId(7_070),
        root: TypeId(0),
        definitions: vec![],
        nodes: vec![
            TypeNode::Object(ObjectShape {
                nullable: false,
                properties: vec![
                    Property {
                        field_id: FieldId(701),
                        name: "amount".into(),
                        ty: TypeId(1),
                        required: true,
                        slot: Some(0),
                    },
                    Property {
                        field_id: FieldId(702),
                        name: "quantity".into(),
                        ty: TypeId(1),
                        required: true,
                        slot: Some(1),
                    },
                ],
                reject_unknown: true,
                min_properties: None,
                max_properties: None,
            }),
            TypeNode::Scalar(decimal),
        ],
    };
    let resolved = jmax_schema_ir::resolve::resolve(schema, Default::default())?;
    jmax_schema_compile::compile(&resolved, &CompilerOptions::default())
}

#[test]
fn paired_decimal_digits_preserve_signed_wide_values_and_escaped_reordered_fields() -> Result {
    let program = program()?;
    let input =
        br#"{"quantity":"1\u0032","am\u006funt":"-17014118346046923173168730371588410572"}"#;
    let output = direct::extract_borrowed(
        &program,
        input,
        &Selection {
            field_ids: vec![],
            strict_validate_unselected: true,
        },
        VmLimits::default(),
        NumericPolicy::strict_default(),
    )?;
    assert_eq!(
        output.slots,
        vec![
            SlotValue::Decimal(Decimal128::new(i128::MIN / 10, 0)),
            SlotValue::Decimal(Decimal128::new(12, 0)),
        ]
    );
    Ok(())
}

#[test]
fn paired_decimal_accumulation_rejects_overflow() -> Result {
    let program = program()?;
    let input = br#"{"amount":"-170141183460469231731687303715884105729","quantity":"0"}"#;
    let error = direct::validate_borrowed(
        &program,
        input,
        VmLimits::default(),
        NumericPolicy::strict_default(),
    )
    .unwrap_err();
    assert_eq!(error.code, jmax_schema_vm::ErrorCode::NumberOverflow);
    assert_eq!(error.field_id, 701);
    Ok(())
}

fn trades_program() -> Result<ValidatedProgram> {
    trades_program_inner(false)
}

fn trades_program_inner(reference: bool) -> Result<ValidatedProgram> {
    let schema = Schema {
        name: "decimal_trades".into(),
        schema_id: SchemaId(7_071),
        root: TypeId(0),
        definitions: vec![],
        nodes: vec![
            TypeNode::Array(ArrayShape {
                nullable: false,
                items: TypeId(1),
                min_items: Some(1),
                max_items: None,
            }),
            TypeNode::Object(ObjectShape {
                nullable: false,
                properties: vec![
                    Property {
                        field_id: FieldId(711),
                        name: "symbol".into(),
                        ty: TypeId(2),
                        required: true,
                        slot: None,
                    },
                    Property {
                        field_id: FieldId(712),
                        name: "quantity".into(),
                        ty: TypeId(3),
                        required: true,
                        slot: None,
                    },
                ],
                reject_unknown: true,
                min_properties: reference.then_some(0),
                max_properties: None,
            }),
            TypeNode::Scalar(ScalarConstraints::new(ValueKind::String)),
            TypeNode::Scalar(ScalarConstraints::new(ValueKind::Integer)),
        ],
    };
    let resolved = jmax_schema_ir::resolve::resolve(schema, Default::default())?;
    jmax_schema_compile::compile(&resolved, &CompilerOptions::default())
}

#[test]
fn struct_array_fast_path_accepts_reordered_scalar_properties() -> Result {
    let program = trades_program()?;
    direct::validate_borrowed(
        &program,
        br#"[{"quantity":2,"symbol":"BTC"},{"symbol":"ETH","quantity":3}]"#,
        VmLimits::default(),
        NumericPolicy::strict_default(),
    )?;
    Ok(())
}

#[test]
fn struct_array_fallback_preserves_required_duplicate_and_unknown_errors() -> Result {
    let program = trades_program()?;
    let limits = VmLimits::default();
    let numeric = NumericPolicy::strict_default();
    let duplicate = direct::validate_borrowed(
        &program,
        br#"[{"symbol":"BTC","quantity":2,"quantity":3}]"#,
        limits,
        numeric,
    )
    .unwrap_err();
    assert_eq!(
        duplicate.code,
        jmax_schema_vm::ErrorCode::DuplicateObjectKey
    );
    let missing =
        direct::validate_borrowed(&program, br#"[{"symbol":"BTC"}]"#, limits, numeric).unwrap_err();
    assert_eq!(
        missing.code,
        jmax_schema_vm::ErrorCode::MissingRequiredField
    );
    let unknown = direct::validate_borrowed(
        &program,
        br#"[{"symbol":"BTC","quantity":2,"extra":true}]"#,
        limits,
        numeric,
    )
    .unwrap_err();
    assert_eq!(unknown.code, jmax_schema_vm::ErrorCode::UnknownObjectKey);
    Ok(())
}

#[test]
fn warmed_struct_arrays_match_reference_resource_limits() -> Result {
    let fast = trades_program_inner(false)?;
    let exact = trades_program_inner(true)?;
    let raw = br#"[{"symbol":"BTC","quantity":2},{"quantity":3,"symbol":"ETH"}]"#;
    let numeric = NumericPolicy::strict_default();
    let mut fast_scratch = jmax_schema_vm::ValidationScratch::default();
    let mut exact_scratch = jmax_schema_vm::ValidationScratch::default();
    for _ in 0..2 {
        direct::validate_borrowed_into(
            &fast,
            raw,
            VmLimits::default(),
            numeric,
            &mut fast_scratch,
        )?;
        direct::validate_borrowed_into(
            &exact,
            raw,
            VmLimits::default(),
            numeric,
            &mut exact_scratch,
        )?;
    }
    for max_steps in (100..4000).step_by(7) {
        let limits = VmLimits {
            max_steps,
            ..VmLimits::default()
        };
        let a = direct::validate_borrowed_into(&fast, raw, limits, numeric, &mut fast_scratch);
        let b = direct::validate_borrowed_into(&exact, raw, limits, numeric, &mut exact_scratch);
        assert_eq!(a.as_ref().err(), b.as_ref().err(), "steps {max_steps}");
        assert_eq!(a.is_ok(), b.is_ok(), "steps {max_steps}");
    }
    for max_scratch_bytes in (0..4000).step_by(13) {
        let limits = VmLimits {
            max_scratch_bytes,
            ..VmLimits::default()
        };
        let a = direct::validate_borrowed_into(&fast, raw, limits, numeric, &mut fast_scratch);
        let b = direct::validate_borrowed_into(&exact, raw, limits, numeric, &mut exact_scratch);
        assert_eq!(
            a.as_ref().err(),
            b.as_ref().err(),
            "scratch {max_scratch_bytes}"
        );
        assert_eq!(a.is_ok(), b.is_ok(), "scratch {max_scratch_bytes}");
    }
    Ok(())
}
