//! ident_overrides, the Wave 5 emitter and generate_with_native.

use jmax_codegen_rust::{
    generate, generate_with_native, native::generate_native, ArtifactKind, CodegenOptions, ErrorCode,
    IdentOverride, NativeOptions, NativeSpec,
};
use jmax_endpoint_model::EndpointModel;
use jmax_endpoint_ron::{load_model, load_native_spec, ConfigLimits, LoadOptions};

const BINANCE: &str = include_str!("../../../fixtures/native/binance.ron");
const BINANCE_SPEC: &str = include_str!("../../../fixtures/native/binance.native.ron");
const RPC: &str = include_str!("../../../fixtures/native/rpc.ron");
const RPC_SPEC: &str = include_str!("../../../fixtures/native/rpc.native.ron");

fn model(source: &str) -> EndpointModel {
    load_model(source, None, &LoadOptions { require_version: false, ..LoadOptions::default() }).unwrap()
}

fn spec(source: &str) -> NativeSpec {
    load_native_spec(source, &ConfigLimits::default()).unwrap()
}

fn options() -> CodegenOptions {
    CodegenOptions { emit_schema_tables: false, ..CodegenOptions::default() }
}

fn code<T: std::fmt::Debug>(result: jmax_codegen_rust::Result<T>) -> ErrorCode {
    result.unwrap_err().code
}

#[test]
fn colliding_fields_need_an_override() {
    let model = model(BINANCE);
    assert_eq!(code(generate(&model, &options())), ErrorCode::ConstraintViolation);
    let mut with = options();
    for (wire, ident) in [("e", "event_type"), ("E", "event_time_ms")] {
        with.ident_overrides.push(IdentOverride {
            type_name: "AggTrade".into(),
            wire: wire.into(),
            ident: ident.into(),
        });
    }
    // DepthUpdate still collides: overrides are per type.
    assert_eq!(code(generate(&model, &with)), ErrorCode::ConstraintViolation);
}

#[test]
fn overrides_are_validated() {
    let model = model(BINANCE);
    let spec = spec(BINANCE_SPEC);
    let native = NativeOptions::default();
    let attempt = |item: IdentOverride| {
        let mut options = options();
        options.ident_overrides.push(item);
        generate_with_native(&model, &spec, &options, &native)
    };
    let item = |type_name: &str, wire: &str, ident: &str| IdentOverride {
        type_name: type_name.into(),
        wire: wire.into(),
        ident: ident.into(),
    };
    // Names no type, names no field, repeats a spec ident, keyword, not snake_case.
    assert_eq!(code(attempt(item("Nope", "e", "x"))), ErrorCode::ConstraintViolation);
    assert_eq!(code(attempt(item("AggTrade", "zz", "x"))), ErrorCode::ConstraintViolation);
    assert_eq!(code(attempt(item("AggTrade", "e", "other"))), ErrorCode::ConstraintViolation);
    assert_eq!(code(attempt(item("PriceLevel", "price", "match"))), ErrorCode::ConstraintViolation);
    assert_eq!(code(attempt(item("PriceLevel", "price", "Price"))), ErrorCode::ConstraintViolation);
    // A legal override of a field the spec leaves alone is used.
    let generation = attempt(item("PriceLevel", "price", "bid_price")).unwrap();
    let models = generation.artifacts().iter().find(|a| a.kind == ArtifactKind::Models).unwrap();
    assert!(models.contents.contains("pub struct PriceLevel {\n    pub bid_price: "));
    // Two overrides of one field.
    let mut twice = options();
    twice.ident_overrides.push(item("PriceLevel", "price", "a"));
    twice.ident_overrides.push(item("PriceLevel", "price", "b"));
    assert_eq!(code(generate_with_native(&model, &spec, &twice, &native)), ErrorCode::ConstraintViolation);
}

#[test]
fn spec_idents_become_field_names() {
    let generation = generate_with_native(&model(BINANCE), &spec(BINANCE_SPEC), &options(), &NativeOptions::default()).unwrap();
    let kinds: Vec<_> = generation.artifacts().iter().map(|a| a.kind).collect();
    assert_eq!(kinds.len(), 7);
    assert!(kinds.contains(&ArtifactKind::Native));
    let models = generation.artifacts().iter().find(|a| a.kind == ArtifactKind::Models).unwrap();
    for name in ["event_type", "event_time_ms", "first_update_id", "final_update_id", "last_update_id"] {
        assert!(models.contents.contains(&format!("pub {name}: ")), "{name}");
    }
    let paths: Vec<_> = generation.artifacts().iter().map(|a| a.path.as_str()).collect();
    assert!(paths.windows(2).all(|pair| pair[0] < pair[1]), "bytewise path order: {paths:?}");
    let root = generation.artifacts().iter().find(|a| a.kind == ArtifactKind::ModuleRoot).unwrap();
    assert!(root.contents.ends_with("pub mod native;\n"));
}

#[test]
fn plain_generate_has_no_native_artifact() {
    let mut options = options();
    options.ident_overrides = vec![];
    let model = model(RPC);
    let plain = generate(&model, &options).unwrap();
    assert_eq!(plain.artifacts().len(), 6);
    assert!(!plain.artifacts().iter().any(|a| a.kind == ArtifactKind::Native));
    assert!(!plain.artifacts().iter().any(|a| a.contents.contains("pub mod native;")));
    assert!(ArtifactKind::Compatibility < ArtifactKind::Native);
}

#[test]
fn the_rpc_fixture_needs_format_validators() {
    // UUID is a format, and jmax-native has no format validators yet.
    let result = generate_with_native(&model(RPC), &spec(RPC_SPEC), &options(), &NativeOptions::default());
    assert_eq!(code(result), ErrorCode::UnsupportedSchemaFeature);
}

fn unsupported(definitions: &str, native: &str) -> ErrorCode {
    let source = format!("Config(version: \"1.0.0\", definitions: [{definitions}])");
    let model = model(&source);
    let spec = spec(native);
    code(generate_native(&model, &spec, &options(), &NativeOptions::default()))
}

#[test]
fn shapes_without_a_native_form_are_unsupported() {
    let wrap = |ty: &str| {
        format!("Definition(name: \"Root\", ty: Struct(name: \"Root\", fields: [Field(name: \"a\", ty: {ty})]))")
    };
    let spec = "NativeSpec(roots: [\"Root\"], reject_unknown: true)";
    for ty in [
        "Object",
        "UUID",
        "Bytea",
        "IpAddr",
        "BlockchainAddress",
        "NanoId(len: 21)",
        "Vec(Optional(Boolean))",
        "Optional(Optional(Boolean))",
        "Enum(name: \"E\", variants: [EnumVariant(name: \"A\", value: 1)])",
    ] {
        assert_eq!(unsupported(&wrap(ty), spec), ErrorCode::UnsupportedSchemaFeature, "{ty}");
    }
    // Recursion.
    let recursive = "Definition(name: \"Root\", ty: Struct(name: \"Root\", fields: [Field(name: \"next\", ty: Optional(StructRef(\"Root\")))]))";
    assert_eq!(unsupported(recursive, spec), ErrorCode::UnsupportedSchemaFeature);
    // Heterogeneous tuple.
    let tuple = "Definition(name: \"Pair\", ty: Struct(name: \"Pair\", fields: [Field(name: \"a\", ty: Int64), Field(name: \"b\", ty: String)])), \
        Definition(name: \"Root\", ty: Struct(name: \"Root\", fields: [Field(name: \"p\", ty: Vec(StructRef(\"Pair\")))]))";
    let spec_tuple = "NativeSpec(roots: [\"Root\"], tuples: [\"Pair\"], reject_unknown: true)";
    assert_eq!(unsupported(tuple, spec_tuple), ErrorCode::UnsupportedSchemaFeature);
}

#[test]
fn the_emitter_validates_its_inputs() {
    let model = model(BINANCE);
    let spec = spec(BINANCE_SPEC);
    let mut bad = NativeOptions::default();
    bad.runtime_path = "not a path".into();
    assert!(generate_native(&model, &spec, &options(), &bad).is_err());
    let mut empty = spec.clone();
    empty.roots.clear();
    assert!(generate_native(&model, &empty, &options(), &NativeOptions::default()).is_err());
    let mut unknown = spec.clone();
    unknown.roots.push("Nope".into());
    assert!(generate_native(&model, &unknown, &options(), &NativeOptions::default()).is_err());
    assert_eq!(NativeOptions::default().runtime_path, "::jmax_native");
    assert!(!NativeOptions::default().predict_layout);
}

#[test]
fn generation_is_deterministic_and_uses_the_configured_runtime_path() {
    let model = model(BINANCE);
    let spec = spec(BINANCE_SPEC);
    let native = NativeOptions { runtime_path: "::my::runtime".into(), predict_layout: false };
    let a = generate_native(&model, &spec, &options(), &native).unwrap();
    let b = generate_native(&model, &spec, &options(), &native).unwrap();
    assert_eq!(a, b);
    assert!(a.contents.contains("use ::my::runtime as __native;"));
    assert!(a.contents.starts_with(jmax_codegen_rust::GENERATED_NOTICE));
    assert!(a.contents.ends_with("}\n"));
    assert_eq!(a.path, "generated/native.rs");
    assert!(!a.contents.contains("std::"));
}

#[test]
fn layout_prediction_is_opt_in_and_only_touches_object_dispatch() {
    let model = model(BINANCE);
    let spec = spec(BINANCE_SPEC);
    let plain = generate_native(&model, &spec, &options(), &NativeOptions::default()).unwrap();
    let predicted = generate_native(
        &model,
        &spec,
        &options(),
        &NativeOptions { predict_layout: true, ..NativeOptions::default() },
    )
    .unwrap();
    assert!(!plain.contents.contains("match_key"));
    assert!(predicted.contents.contains("r.match_key(ctx, KEYS[next])"));
    assert!(plain.contents.contains("r.key_bytes(ctx)?"));
    assert!(predicted.contents.contains("const KEYS: [&[u8]; 11] = [b\"\\\"e\\\":\", b\"\\\"E\\\":\""));
    assert_ne!(plain.contents, predicted.contents);
    // The embedded programs are the same bytes either way.
    let programs = |text: &str| text.split("impl ").next().unwrap().to_owned();
    assert_eq!(programs(&plain.contents), programs(&predicted.contents));
}

#[test]
fn schema_tables_are_emitted_for_struct_models() {
    // Regression: every compiled program carries a schema capsule after its key
    // names, which the structure verifier used to reject as an unowned suffix, so
    // generate() with its default options failed on any model with a struct.
    let source = r#"Config(version: "1.0.0", definitions: [
        Definition(name: "Quote", ty: Struct(name: "Quote", fields: [
            Field(name: "symbol", ty: String), Field(name: "bid", ty: Int64), Field(name: "ask", ty: Int64)]))])"#;
    let model = model(source);
    let generation = generate(&model, &CodegenOptions::default()).unwrap();
    let tables = generation.artifacts().iter().find(|a| a.kind == ArtifactKind::SchemaTables).unwrap();
    assert!(tables.contents.contains("SCHEMA_COUNT: usize = 1"), "{}", &tables.contents[..200.min(tables.contents.len())]);
}
