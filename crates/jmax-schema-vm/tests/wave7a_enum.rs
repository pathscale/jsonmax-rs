//! Exact enum wire forms through direct, event, and serialized-program paths.
use jmax_abi::number::NumericPolicy;
use jmax_schema_ir::{EnumShape, EnumVariant, ObjectShape, Property, Schema, SchemaId, TypeId, TypeNode, FieldId};
use jmax_schema_vm::{direct, execute, Payload, Selection, SlotValue, VmLimits};

fn program(nullable: bool) -> jmax_schema_compile::ValidatedProgram {
    let schema = Schema {
        name: "enum_wire".into(), schema_id: SchemaId(750), root: TypeId(0), definitions: vec![],
        nodes: vec![
            TypeNode::Object(ObjectShape {
                nullable: false, properties: vec![Property {
                    field_id: FieldId(1), name: "state".into(), ty: TypeId(1), required: true, slot: Some(0),
                }], reject_unknown: true, min_properties: None, max_properties: None,
            }),
            TypeNode::Enum(EnumShape { nullable, variants: vec![
                EnumVariant { name: "Ready".into(), discriminant: 7 },
                EnumVariant { name: "Échec".into(), discriminant: -3 },
                EnumVariant { name: "1".into(), discriminant: i64::MIN },
                EnumVariant { name: "Max".into(), discriminant: i64::MAX },
            ] }),
        ],
    };
    let resolved = jmax_schema_ir::resolve::resolve(schema, Default::default()).unwrap();
    let options = jmax_schema_compile::CompilerOptions::default();
    let p = jmax_schema_compile::compile(&resolved, &options).unwrap();
    let bytes = jmax_schema_compile::image::encode(&p, options.max_image_bytes).unwrap();
    jmax_schema_compile::image::decode(&bytes, &options).unwrap()
}

fn both(raw: &[u8], nullable: bool) -> (jmax_schema_vm::Result<Vec<SlotValue>>, jmax_schema_vm::Result<Vec<SlotValue>>) {
    let program = program(nullable);
    let selection = Selection { field_ids: vec![FieldId(1)], strict_validate_unselected: true };
    let limits = VmLimits::default();
    let numeric = NumericPolicy::strict_default();
    let direct = direct::extract_borrowed(&program, raw, &selection, limits, numeric).map(|v| v.slots);
    let input = execute::CursorInput::new(Payload::from_slice(raw), limits.syntax, numeric).unwrap();
    let events = execute::validate(&program, input, &selection, limits).map(|v| v.slots);
    (direct, events)
}

#[test]
fn names_escaped_names_and_integer_discriminants_publish_the_same_value() {
    for (token, want) in [
        (r#""Ready""#, 7), (r#""Re\u0061dy""#, 7), ("7", 7),
        (r#""Échec""#, -3), (r#""\u00c9chec""#, -3), ("-3", -3),
        (r#""1""#, i64::MIN), ("-9223372036854775808", i64::MIN),
        ("9223372036854775807", i64::MAX),
    ] {
        let raw = format!("{{\"state\":{token}}}");
        let (a, b) = both(raw.as_bytes(), false);
        assert_eq!(a.unwrap(), vec![SlotValue::I64(want)], "{raw}");
        assert_eq!(b.unwrap(), vec![SlotValue::I64(want)], "{raw}");
    }
}

#[test]
fn unknown_and_noninteger_enum_forms_are_rejected_by_both_engines() {
    for token in [r#""Unknown""#, r#""07""#, "8", "1", "7.0", "7e0", "true", "null", "[]", "{}", "9223372036854775808", "-9223372036854775809"] {
        let raw = format!("{{\"state\":{token}}}");
        let (a, b) = both(raw.as_bytes(), false);
        assert!(a.is_err(), "direct accepted {raw}");
        assert!(b.is_err(), "events accepted {raw}");
    }
    let (a, b) = both(br#"{"state":null}"#, true);
    assert!(a.is_ok()); assert!(b.is_ok());
}
