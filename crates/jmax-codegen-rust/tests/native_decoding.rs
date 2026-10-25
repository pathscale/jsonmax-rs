use jmax_codegen_rust::native::generate_native;
use jmax_codegen_rust::{ CodegenOptions, NativeOptions, NativeSpec};
use jmax_endpoint_model::EndpointModel;
use jmax_endpoint_ron::{load_model, load_native_spec, ConfigLimits, LoadOptions};

fn model(source: &str) -> EndpointModel {
    load_model(source, None, &LoadOptions { require_version: false, ..LoadOptions::default() }).unwrap()
}

fn spec(source: &str) -> NativeSpec {
    load_native_spec(source, &ConfigLimits::default()).unwrap()
}

#[test]
fn native_enum_decoder_accepts_variant_names_and_integer_discriminants() {
    let model = model(r#"Config(version: "1.0.0", definitions: [
        Definition(name: "Side", ty: Enum(name: "Side", variants: [
            EnumVariant(name: "Bid", value: -2, description: ""),
            EnumVariant(name: "Ask", value: 7, description: "")
        ])),
        Definition(name: "Root", ty: Struct(name: "Root", fields: [
            Field(name: "side", ty: EnumRef(name: "Side", prefixed_name: false), description: "")
        ]))
    ])"#);
    let generated = generate_native(
        &model,
        &spec("NativeSpec(roots: [\"Root\"], reject_unknown: true)"),
        &CodegenOptions::default(),
        &NativeOptions::default(),
    ).unwrap();

    assert!(generated.contents.contains("if r.peek() == Some(b'\"')"));
    assert!(generated.contents.contains("\"Bid\" => super::models::Side::Bid"));
    assert!(generated.contents.contains("\"Ask\" => super::models::Side::Ask"));
    assert!(generated.contents.contains("-2 => super::models::Side::Bid"));
    assert!(generated.contents.contains("7 => super::models::Side::Ask"));
    assert!(generated.contents.contains("impl ::core::default::Default for super::models::Side"));
}

#[test]
fn native_unit_and_empty_struct_values_have_decoders() {
    let model = model(r#"Config(version: "1.0.0", definitions: [
        Definition(name: "Empty", ty: Struct(name: "Empty", fields: [])),
        Definition(name: "Root", ty: Struct(name: "Root", fields: [
            Field(name: "nothing", ty: Unit, description: ""),
            Field(name: "empty", ty: StructRef("Empty"), description: "")
        ]))
    ])"#);
    let generated = generate_native(
        &model,
        &spec("NativeSpec(roots: [\"Root\"], reject_unknown: true)"),
        &CodegenOptions::default(),
        &NativeOptions { predict_layout: true, ..NativeOptions::default() },
    ).unwrap();

    assert!(generated.contents.contains("if !r.null() { return Err(__native::Fail); }"));
    assert!(generated.contents.contains("fn __decode_empty("));
    assert!(generated.contents.contains("r.expect(b'{')?;"));
    assert!(generated.contents.contains("r.expect(b'}')?;"));
    let empty_decoder = generated.contents.split("fn __decode_empty(").nth(1).unwrap().split("\n}\n").next().unwrap();
    assert!(!empty_decoder.contains("KEYS[next]"));
}
