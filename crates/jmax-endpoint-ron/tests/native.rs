use jmax_endpoint_ron::{ConfigLimits, EndpointModel, ErrorCode, LoadOptions, load_model, load_native_spec};
use jmax_endpoint_model::native::{NativeIdent, NativeSpec, schema_graph_for, validate_native_spec};
use jmax_endpoint_model::registry::build_registry;
use jmax_endpoint_model::SchemaId;
use jmax_schema_compile::CompilerOptions;
use jmax_schema_ir::{ResolveLimits, resolve};

const BINANCE: &str = include_str!("../../../fixtures/native/binance.ron");
const BINANCE_NATIVE: &str = include_str!("../../../fixtures/native/binance.native.ron");
const RPC: &str = include_str!("../../../fixtures/native/rpc.ron");
const RPC_NATIVE: &str = include_str!("../../../fixtures/native/rpc.native.ron");

fn model(source: &str) -> EndpointModel {
    let options = LoadOptions { require_version: false, ..LoadOptions::default() };
    load_model(source, None, &options).unwrap()
}

fn spec(source: &str) -> NativeSpec {
    load_native_spec(source, &ConfigLimits::default()).unwrap()
}

#[test]
fn binance_fixture_loads_and_lowers_to_compilable_programs() {
    let model = model(BINANCE);
    let spec = spec(BINANCE_NATIVE);
    assert_eq!(spec.roots, ["AggTrade", "DepthUpdate", "DepthSnapshot"]);
    assert_eq!(spec.tuples, ["PriceLevel"]);
    assert!(spec.reject_unknown);
    assert_eq!(spec.idents.len(), 19);
    assert_eq!(spec.idents[0], NativeIdent {
        type_name: "AggTrade".into(), wire: "e".into(), ident: "event_type".into(),
    });
    let registry = build_registry(&model).unwrap();
    validate_native_spec(&registry, &spec).unwrap();
    for (index, root) in spec.roots.iter().enumerate() {
        let schema = schema_graph_for(&registry, &spec, root, SchemaId(5000 + index as u32)).unwrap();
        let resolved = resolve::resolve(schema, ResolveLimits::default()).unwrap();
        let program = jmax_schema_compile::compile(&resolved, &CompilerOptions::default()).unwrap();
        assert_eq!(program.image().header.schema_id, 5000 + index as u32, "{root}");
    }
}

#[test]
fn rpc_fixture_loads_and_lowers() {
    let model = model(RPC);
    let spec = spec(RPC_NATIVE);
    let registry = build_registry(&model).unwrap();
    validate_native_spec(&registry, &spec).unwrap();
    let schema = schema_graph_for(&registry, &spec, "RpcRequest", SchemaId(6000)).unwrap();
    let resolved = resolve::resolve(schema, ResolveLimits::default()).unwrap();
    jmax_schema_compile::compile(&resolved, &CompilerOptions::default()).unwrap();
}

#[test]
fn defaults_and_decimal_override() {
    let loaded = spec("NativeSpec(roots: [])");
    assert_eq!(loaded, NativeSpec::default());
    let loaded = spec(
        "(roots: [\"A\"], decimal: Decimal(max_precision: 20, target_scale: 4, tick_step: 5, allow_exponent: true), reject_unknown: false, idents: [(type: \"A\", wire: \"w\", ident: \"x\")])",
    );
    assert_eq!(loaded.decimal_policy.max_precision, 20);
    assert_eq!(loaded.decimal_policy.target_scale, 4);
    assert_eq!(loaded.decimal_policy.tick_step, 5);
    assert!(loaded.decimal_policy.allow_exponent);
    assert!(loaded.decimal_policy.reject_inexact_rescale);
    assert!(!loaded.reject_unknown);
    assert_eq!(loaded.idents[0].ident, "x");
}

fn error(source: &str) -> (ErrorCode, u32) {
    let error = load_native_spec(source, &ConfigLimits::default()).unwrap_err();
    (error.code, error.byte_offset)
}

#[test]
fn malformed_documents_are_rejected_with_offsets() {
    assert_eq!(error("(roots: [\"A\"], extra: 1)").0, ErrorCode::UnknownObjectKey);
    assert_eq!(error("(roots: [\"A\"], roots: [])").0, ErrorCode::DuplicateObjectKey);
    assert_eq!(error("(roots: 5)").0, ErrorCode::TypeMismatch);
    assert_eq!(error("(roots: [5])").0, ErrorCode::TypeMismatch);
    assert_eq!(error("(roots: [\"\"])").0, ErrorCode::ConstraintViolation);
    assert_eq!(error("(reject_unknown: 1)").0, ErrorCode::TypeMismatch);
    assert_eq!(error("(decimal: (max_precision: 300))").0, ErrorCode::NumberOverflow);
    assert_eq!(error("(idents: [Ident(\"a\", \"b\")])").0, ErrorCode::TypeMismatch);
    assert_eq!(error("(idents: [(type: \"a\", wire: \"\", ident: \"x\")])").0, ErrorCode::ConstraintViolation);
    assert_eq!(error("(idents: [(type: \"a\", wire: \"w\")])").0, ErrorCode::MissingRequiredField);
    assert_eq!(error("(roots: [\"A\"]").0, ErrorCode::UnexpectedEof);
    assert_eq!(error("(roots: []) x").0, ErrorCode::TrailingContent);
    assert_eq!(error("Other(roots: [])").0, ErrorCode::TypeMismatch);
    let (_, offset) = error("(roots: [\"A\", 7])");
    assert_eq!(offset, 14);
    let limits = ConfigLimits { max_input_bytes: 4, ..ConfigLimits::default() };
    assert_eq!(load_native_spec("(roots: [])", &limits).unwrap_err().code, ErrorCode::PayloadTooLarge);
}
