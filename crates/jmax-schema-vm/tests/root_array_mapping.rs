//! Root-array streaming must validate syntax before publishing schema results.

use jmax_abi::number::NumericPolicy;
use jmax_schema_compile::{CompilerOptions, ValidatedProgram};
use jmax_schema_ir::{
    FieldId, ObjectShape, Property, ScalarConstraints, Schema, SchemaId, TypeId, TypeNode,
    ValueKind,
};
use jmax_schema_vm::{ErrorCode, Selection, SlotValue, ValidationScratch, VmLimits, direct};

type Result<T = ()> = jmax_schema_vm::Result<T>;

fn program() -> Result<ValidatedProgram> {
    let mut integer = ScalarConstraints::new(ValueKind::Integer);
    integer.min_i64 = Some(0);
    integer.max_i64 = Some(9);
    let schema = Schema {
        name: "root_array_rows".into(),
        schema_id: SchemaId(7_037),
        root: TypeId(0),
        definitions: vec![],
        nodes: vec![
            TypeNode::Object(ObjectShape {
                nullable: false,
                properties: vec![Property {
                    field_id: FieldId(73),
                    name: "n".into(),
                    ty: TypeId(1),
                    required: true,
                    slot: Some(0),
                }],
                reject_unknown: true,
                min_properties: None,
                max_properties: None,
            }),
            TypeNode::Scalar(integer),
        ],
    };
    let resolved = jmax_schema_ir::resolve::resolve(schema, Default::default())?;
    jmax_schema_compile::compile(&resolved, &CompilerOptions::default())
}

fn selection() -> Selection {
    Selection {
        field_ids: vec![FieldId(73)],
        strict_validate_unselected: true,
    }
}

fn map(raw: &[u8], scratch: &mut ValidationScratch) -> Result<Vec<u64>> {
    direct::map_root_array_borrowed(
        &program()?,
        raw,
        &selection(),
        VmLimits::default(),
        NumericPolicy::strict_default(),
        scratch,
        |_, _, slots| match slots[0] {
            SlotValue::I64(value) => value as u64,
            SlotValue::U64(value) => value,
            _ => u64::MAX,
        },
    )
}

#[test]
fn root_array_maps_rows_and_recovers_after_a_schema_failure() -> Result {
    let mut scratch = ValidationScratch::default();
    assert_eq!(
        map(br#"[{"n":1},{"n":2},{"n":9}]"#, &mut scratch)?,
        vec![1, 2, 9]
    );

    let schema_error = map(br#"[{"n":"wrong"}]"#, &mut scratch).unwrap_err();
    assert_eq!(schema_error.code, ErrorCode::TypeMismatch);
    assert_eq!(schema_error.schema_id, 7_037);
    assert_eq!(schema_error.field_id, 73);

    assert_eq!(map(br#"[{"n":3}]"#, &mut scratch)?, vec![3]);
    Ok(())
}

#[test]
fn malformed_tail_precedes_an_earlier_schema_error() {
    let mut scratch = ValidationScratch::default();
    for raw in [
        br#"[{"n":"wrong"},]"#.as_slice(),
        br#"[{"n":"wrong"},{"n":1} false]"#,
        br#"[{"n":"wrong"},{"n":1"#,
    ] {
        let error = map(raw, &mut scratch).unwrap_err();
        assert_eq!(error.source, jmax_schema_vm::ErrorSource::Syntax, "{raw:?}");
        assert!(
            matches!(
                error.code,
                ErrorCode::InvalidSyntax | ErrorCode::TrailingContent | ErrorCode::UnexpectedEof
            ),
            "{raw:?}: {error:?}"
        );
    }
}

#[test]
fn root_array_limits_and_duplicate_keys_are_enforced_before_mapping() {
    let mut scratch = ValidationScratch::default();
    let error = direct::map_root_array_borrowed(
        &program().unwrap(),
        br#"[{"n":1},{"n":2}]"#,
        &selection(),
        {
            let mut limits = VmLimits::default();
            limits.syntax.max_array_items = 1;
            limits
        },
        NumericPolicy::strict_default(),
        &mut scratch,
        |_, _, _| (),
    )
    .unwrap_err();
    assert_eq!(error.source, jmax_schema_vm::ErrorSource::Syntax);

    let error = direct::map_root_array_borrowed(
        &program().unwrap(),
        br#"[{"n":1,"\u006e":2}]"#,
        &selection(),
        VmLimits::default(),
        NumericPolicy::strict_default(),
        &mut scratch,
        |_, _, _| (),
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::DuplicateObjectKey);
}

#[test]
fn malformed_tail_and_array_limits_do_not_invoke_the_mapping_callback() {
    for raw in [
        br#"[{"n":1},]"#.as_slice(),
        br#"[{"n":1},{"n":2}] trailing"#,
    ] {
        let mut mapped = 0;
        let result = direct::map_root_array_borrowed(
            &program().unwrap(),
            raw,
            &selection(),
            VmLimits::default(),
            NumericPolicy::strict_default(),
            &mut ValidationScratch::default(),
            |_, _, _| {
                mapped += 1;
            },
        );
        assert!(result.is_err());
        assert_eq!(mapped, 0, "syntax must be checked before user callbacks");
    }
}
