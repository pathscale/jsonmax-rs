use jmax_schema_aot::{compile_graph, StaticSchemaImage};
use jmax_schema_ir::{EnumShape, EnumVariant, Schema, SchemaId, TypeId, TypeNode};

#[test]
fn enum_capsule_survives_static_and_full_image_round_trips() {
    let schema = Schema { name: "side".into(), schema_id: SchemaId(750), root: TypeId(0), definitions: vec![], nodes: vec![TypeNode::Enum(EnumShape {
        nullable: false, variants: vec![EnumVariant { name: "Bid".into(), discriminant: -2 }, EnumVariant { name: "Ask".into(), discriminant: 7 }],
    })] };
    let program = compile_graph(&schema).unwrap();
    let static_image = StaticSchemaImage::from(program.image());
    let owned = jmax_schema_aot::tables::to_owned(static_image).unwrap();
    let options = jmax_schema_compile::CompilerOptions::default();
    let loaded = jmax_schema_compile::image::load(owned, &options).unwrap();
    assert_eq!(program.program().states, loaded.program().states);
    let bytes = jmax_schema_aot::dump_program(&program).unwrap();
    let loaded = jmax_schema_aot::load_program(&bytes).unwrap();
    assert_eq!(program.program().states, loaded.program().states);
    for valid in [br#""Bid""#.as_slice(), br#""\u0041sk""#.as_slice(), b"-2", b"7"] {
        jmax_schema_vm::validate_borrowed(&loaded, valid, Default::default(), jmax_abi::number::NumericPolicy::strict_default()).unwrap();
    }
    for invalid in [br#""bid""#.as_slice(), b"2", b"7.0", b"null"] {
        assert!(jmax_schema_vm::validate_borrowed(&loaded, invalid, Default::default(), jmax_abi::number::NumericPolicy::strict_default()).is_err());
    }
}
