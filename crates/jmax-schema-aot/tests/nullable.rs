//! A nullable node accepts a JSON null in place of its value, whatever its kind,
//! and nothing else changes: the same node without the flag still rejects null.

use jmax_abi::number::NumericPolicy;
use jmax_schema_aot::{compile_graph, dump_program, load_program, ValidatedProgram};
use jmax_schema_ir::{
    ArrayShape, ObjectShape, Property, ScalarConstraints, Schema, SchemaId, TypeId, TypeNode, ValueKind,
};
use jmax_schema_vm::{ErrorCode, ValidationScratch, VmLimits};

fn scalar(kind: ValueKind, nullable: bool) -> TypeNode {
    let mut constraints = ScalarConstraints::new(kind);
    constraints.nullable = nullable;
    if kind == ValueKind::String {
        constraints.min_length = Some(2);
    }
    TypeNode::Scalar(constraints)
}

/// {n_int, n_str, n_obj{x}, n_arr[int], int, obj{x}} with the `n_` ones nullable and optional.
fn program(nullable: bool) -> ValidatedProgram {
    let property = |id: u32, name: &str, ty: u32, required: bool| Property {
        field_id: jmax_abi::schema::FieldId(id),
        name: name.into(),
        ty: TypeId(ty),
        required,
        slot: None,
    };
    let nodes = vec![
        TypeNode::Object(ObjectShape {
            nullable: false,
            properties: vec![
                property(1, "n_int", 1, false),
                property(2, "n_str", 2, false),
                property(3, "n_obj", 3, false),
                property(4, "n_arr", 5, false),
                property(5, "int", 6, false),
                property(6, "obj", 7, false),
            ],
            reject_unknown: true,
            min_properties: None,
            max_properties: None,
        }),
        scalar(ValueKind::Integer, nullable),
        scalar(ValueKind::String, nullable),
        TypeNode::Object(ObjectShape {
            nullable,
            properties: vec![property(7, "x", 4, true)],
            reject_unknown: true,
            min_properties: None,
            max_properties: None,
        }),
        scalar(ValueKind::Integer, false),
        TypeNode::Array(ArrayShape { nullable, items: TypeId(4), min_items: None, max_items: Some(3) }),
        scalar(ValueKind::Integer, false),
        TypeNode::Object(ObjectShape {
            nullable: false,
            properties: vec![property(8, "x", 4, true)],
            reject_unknown: true,
            min_properties: None,
            max_properties: None,
        }),
    ];
    compile_graph(&Schema {
        name: "nullable".into(),
        schema_id: SchemaId(31),
        root: TypeId(0),
        nodes,
        definitions: vec![],
    })
    .unwrap()
}

fn run(program: &ValidatedProgram, input: &str) -> Option<ErrorCode> {
    match jmax_schema_vm::direct::validate_borrowed_into(
        program,
        input.as_bytes(),
        VmLimits::default(),
        NumericPolicy::strict_default(),
        &mut ValidationScratch::default(),
    ) {
        Ok(_) => None,
        Err(error) => Some(error.code),
    }
}

#[test]
fn null_is_accepted_exactly_where_the_node_is_nullable() {
    let nullable = program(true);
    let strict = program(false);
    // (document, accepted by the nullable program, accepted by the strict one)
    let cases: &[(&str, bool, bool)] = &[
        ("{}", true, true),
        (r#"{"n_int":null}"#, true, false),
        (r#"{"n_int":5}"#, true, true),
        (r#"{"n_int":"5"}"#, false, false),
        (r#"{"n_int":nul}"#, false, false),
        (r#"{"n_int":Null}"#, false, false),
        (r#"{"n_str":null}"#, true, false),
        (r#"{"n_str":"ab"}"#, true, true),
        (r#"{"n_str":"a"}"#, false, false),
        (r#"{"n_str":3}"#, false, false),
        (r#"{"n_obj":null}"#, true, false),
        (r#"{"n_obj":{"x":1}}"#, true, true),
        (r#"{"n_obj":{}}"#, false, false),
        (r#"{"n_obj":{"x":null}}"#, false, false),
        (r#"{"n_obj":[]}"#, false, false),
        (r#"{"n_arr":null}"#, true, false),
        (r#"{"n_arr":[1,2,3]}"#, true, true),
        (r#"{"n_arr":[1,2,3,4]}"#, false, false),
        (r#"{"n_arr":[null]}"#, false, false),
        (r#"{"n_arr":{}}"#, false, false),
        (r#"{"int":null}"#, false, false),
        (r#"{"obj":null}"#, false, false),
        (r#"{"n_int":null,"n_str":null,"n_obj":null,"n_arr":null}"#, true, false),
        (r#"{"n_int":null,"n_int":1}"#, false, false),
        ("null", false, false),
        (r#"{"n_int":null,}"#, false, false),
    ];
    for (document, expect_nullable, expect_strict) in cases {
        assert_eq!(run(&nullable, document).is_none(), *expect_nullable, "nullable: {document}");
        assert_eq!(run(&strict, document).is_none(), *expect_strict, "strict: {document}");
    }
    // The programs differ only by the flag, so a null on a strict node is a type error.
    assert_eq!(run(&strict, r#"{"n_obj":null}"#), Some(ErrorCode::TypeMismatch));
}

#[test]
fn nullable_survives_the_program_image_and_changes_the_fingerprint() {
    let nullable = program(true);
    let strict = program(false);
    assert_ne!(nullable.image().header.fingerprint, strict.image().header.fingerprint);
    for original in [&nullable, &strict] {
        let restored = load_program(&dump_program(original).unwrap()).unwrap();
        assert_eq!(restored.image().header, original.image().header);
        assert_eq!(restored.program().canonical_schema, original.program().canonical_schema);
        assert_eq!(restored.program().states, original.program().states);
    }
    // A schema without nullable nodes keeps tags 1 to 3 in its canonical bytes.
    let strict_bytes = &strict.program().canonical_schema;
    let nullable_bytes = &nullable.program().canonical_schema;
    assert!(strict_bytes.len() == nullable_bytes.len());
    assert!(strict_bytes.iter().zip(nullable_bytes.iter()).filter(|(a, b)| a != b).count() >= 3);
}
