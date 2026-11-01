use jsonmax::schema_aot::{compile_graph, StaticSchemaImage};
use jsonmax::schema_ir::{EnumShape, EnumVariant, Schema, SchemaId, TypeId, TypeNode};

#[test]
fn enum_capsule_survives_static_and_full_image_round_trips() {
    let schema = Schema { name: "side".into(), schema_id: SchemaId(750), root: TypeId(0), definitions: vec![], nodes: vec![TypeNode::Enum(EnumShape {
        nullable: false, variants: vec![EnumVariant { name: "Bid".into(), discriminant: -2 }, EnumVariant { name: "Ask".into(), discriminant: 7 }],
    })] };
    let program = compile_graph(&schema).unwrap();
    let static_image = StaticSchemaImage::from(program.image());
    let owned = jsonmax::schema_aot::tables::to_owned(static_image).unwrap();
    let options = jsonmax::schema_compile::CompilerOptions::default();
    let loaded = jsonmax::schema_compile::image::load(owned, &options).unwrap();
    assert_eq!(program.program().states, loaded.program().states);
    let bytes = jsonmax::schema_aot::dump_program(&program).unwrap();
    let loaded = jsonmax::schema_aot::load_program(&bytes).unwrap();
    assert_eq!(program.program().states, loaded.program().states);
    for valid in [br#""Bid""#.as_slice(), br#""\u0041sk""#.as_slice(), b"-2", b"7"] {
        jsonmax::schema_vm::validate_borrowed(&loaded, valid, Default::default(), jsonmax::abi::number::NumericPolicy::strict_default()).unwrap();
    }
    for invalid in [br#""bid""#.as_slice(), b"2", b"7.0", b"null"] {
        assert!(jsonmax::schema_vm::validate_borrowed(&loaded, invalid, Default::default(), jsonmax::abi::number::NumericPolicy::strict_default()).is_err());
    }
}
