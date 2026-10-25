use jmax_schema_aot::{BorrowedSlotReader, FieldId, SlotKind, SlotSpec, SlotValue, decode};
use jmax_schema_ir::{ScalarKind, SchemaFieldIr, SchemaId, SchemaIr};
use jmax_schema_vm::{Selection, VmLimits, direct};

#[test]
fn borrowed_and_owned_results_use_the_same_checked_decoder() -> jmax_schema_aot::Result<()> {
    let ir = SchemaIr {
        name: "aot-borrow".into(),
        schema_id: SchemaId(60),
        required_count: 1,
        reject_unknown: true,
        fields: vec![SchemaFieldIr {
            field_id: FieldId(0),
            name: "s".into(),
            required: true,
            scalar_kind: Some(ScalarKind::String),
            format: None,
            min_len: Some(2),
            max_len: Some(2),
            min_i64: None,
            max_i64: None,
        }],
    };
    let program = jmax_schema_aot::compile_schema(&ir)?;
    let raw = br#"{"s":"\u00e9x"}"#;
    let selection = Selection {
        field_ids: vec![],
        strict_validate_unselected: true,
    };
    let numeric = jmax_abi::number::NumericPolicy::strict_default();
    let mut values =
        direct::extract_borrowed(&program, raw, &selection, VmLimits::default(), numeric)?;
    let spec = SlotSpec {
        index: 0,
        field_id: FieldId(0),
        kind: SlotKind::Str,
        required: true,
    };
    let plan = jmax_schema_aot::DecodePlan {
        schema_fingerprint: program.image().header.fingerprint,
        slots: &[spec],
    };
    decode::validate_slots(&values, &plan)?;
    let reader = BorrowedSlotReader::new(&values, plan.schema_fingerprint)?;
    assert_eq!(decode::decode_str(&reader, &spec)?, Some("éx"));
    // Mutable public result fields never authorize unchecked string access.
    let SlotValue::Str(mut reference) = values.slots[0] else {
        panic!("missing string")
    };
    reference.scalar_len += 1;
    values.slots[0] = SlotValue::Str(reference);
    let reader = BorrowedSlotReader::new(&values, plan.schema_fingerprint)?;
    assert!(decode::decode_str(&reader, &spec).is_err());
    let owned = direct::extract_payload_direct(
        &program,
        jmax_abi::buffer::Payload::from_vec(raw.to_vec()),
        &selection,
        VmLimits::default(),
        numeric,
    )?;
    let reader = jmax_schema_aot::SlotReader::new(&owned, plan.schema_fingerprint)?;
    assert_eq!(decode::decode_str(&reader, &spec)?, Some("éx"));
    Ok(())
}
