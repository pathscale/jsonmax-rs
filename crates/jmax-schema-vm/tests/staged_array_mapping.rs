//! Single-pass root-array mapping publishes no callback until strict EOF.

use jmax_abi::number::NumericPolicy;
use jmax_schema_compile::{CompilerOptions, ValidatedProgram};
use jmax_schema_ir::{
    FieldId, ObjectShape, Property, ScalarConstraints, Schema, SchemaId, TypeId, TypeNode,
    ValueKind,
};
use jmax_schema_vm::{
    ErrorCode, ErrorSource, Selection, SlotValue, ValidationScratch, VmLimits, direct,
};

type Result<T = ()> = jmax_schema_vm::Result<T>;

fn program() -> Result<ValidatedProgram> {
    let mut integer = ScalarConstraints::new(ValueKind::Integer);
    integer.min_i64 = Some(0);
    integer.max_i64 = Some(9);
    let schema = Schema {
        name: "staged_array_rows".into(),
        schema_id: SchemaId(7_047),
        root: TypeId(0),
        definitions: vec![],
        nodes: vec![
            TypeNode::Object(ObjectShape {
                nullable: false,
                properties: vec![Property {
                    field_id: FieldId(74),
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
        field_ids: vec![FieldId(74)],
        strict_validate_unselected: true,
    }
}

fn run(raw: &[u8], limits: VmLimits, mapped: &mut usize) -> Result<Vec<u64>> {
    direct::map_root_array_borrowed(
        &program()?,
        raw,
        &selection(),
        limits,
        NumericPolicy::strict_default(),
        &mut ValidationScratch::default(),
        |_, _, slots| {
            *mapped += 1;
            match slots[0] {
                SlotValue::I64(value) => value as u64,
                SlotValue::U64(value) => value,
                _ => u64::MAX,
            }
        },
    )
}

#[test]
fn invalid_later_element_discards_all_staged_callbacks() {
    let mut mapped = 0;
    let error = run(
        br#"[{"n":1},{"n":"bad"}]"#,
        VmLimits::default(),
        &mut mapped,
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::TypeMismatch);
    assert_eq!(error.schema_id, 7_047);
    assert_eq!(mapped, 0);
}

#[test]
fn malformed_suffix_and_later_syntax_beat_earlier_schema_failure() {
    for raw in [
        br#"[{"n":1}] trailing"#.as_slice(),
        br#"[{"n":"bad"},{"n":2} false]"#,
        br#"[{"n":"bad"},]"#,
    ] {
        let mut mapped = 0;
        let error = run(raw, VmLimits::default(), &mut mapped).unwrap_err();
        assert_eq!(error.source, ErrorSource::Syntax, "{raw:?}");
        assert_eq!(mapped, 0, "{raw:?}");
    }
}

#[test]
fn array_limit_fails_before_callbacks() {
    let mut limits = VmLimits::default();
    limits.syntax.max_array_items = 1;
    let mut mapped = 0;
    let error = run(br#"[{"n":1},{"n":2}]"#, limits, &mut mapped).unwrap_err();
    assert_eq!(error.source, ErrorSource::Syntax);
    assert_eq!(mapped, 0);
}

#[test]
fn staged_output_respects_scratch_limit() {
    let mut limits = VmLimits::default();
    limits.max_scratch_bytes = 1;
    let mut mapped = 0;
    let error = run(br#"[{"n":1}]"#, limits, &mut mapped).unwrap_err();
    assert_eq!(error.source, ErrorSource::SchemaVm);
    assert_eq!(mapped, 0);
}

#[test]
fn enclosing_array_counts_toward_known_object_depth() {
    let mut limits = VmLimits::default();
    limits.syntax.max_depth = 1;
    let mut mapped = 0;
    let error = run(br#"[{"n":1}]"#, limits, &mut mapped).unwrap_err();
    assert_eq!(error.code, ErrorCode::DepthLimitExceeded);
    assert_eq!(mapped, 0);
}

#[test]
fn escaped_selected_strings_keep_one_cumulative_arena() -> Result<()> {
    let schema = Schema {
        name: "staged_array_strings".into(), schema_id: SchemaId(7_048), root: TypeId(0),
        definitions: vec![], nodes: vec![
            TypeNode::Object(ObjectShape {
                nullable: false, reject_unknown: true, min_properties: None,
                max_properties: None, properties: vec![Property {
                    field_id: FieldId(74), name: "n".into(), ty: TypeId(1),
                    required: true, slot: Some(0),
                }],
            }),
            TypeNode::Scalar(ScalarConstraints::new(ValueKind::String)),
        ],
    };
    let resolved = jmax_schema_ir::resolve::resolve(schema, Default::default())?;
    let program = jmax_schema_compile::compile(&resolved, &CompilerOptions::default())?;
    let raw = br#"[{"n":"a\nb"},{"n":"c\nd"}]"#;
    let mut scratch = ValidationScratch::default();
    let values = direct::map_root_array_borrowed(
        &program, raw, &selection(), VmLimits::default(), NumericPolicy::strict_default(),
        &mut scratch, |raw, decoded, slots| {
            let SlotValue::Str(reference) = slots[0] else { panic!("string slot") };
            let bytes = match reference.source {
                jmax_abi::span::SpanSource::RawPayload => raw,
                jmax_abi::span::SpanSource::DecodedArena => decoded,
            };
            reference.span.slice(bytes).unwrap().to_vec()
        },
    )?;
    assert_eq!(values, vec![b"a\nb".to_vec(), b"c\nd".to_vec()]);
    let mut limits = VmLimits::default();
    limits.syntax.max_decoded_bytes = 5;
    let mut mapped = 0;
    let error = direct::map_root_array_borrowed(
        &program, raw, &selection(), limits, NumericPolicy::strict_default(),
        &mut scratch, |_, _, _| { mapped += 1; },
    ).unwrap_err();
    assert_eq!(mapped, 0);
    assert_eq!(error.source, ErrorSource::Syntax);
    Ok(())
}
