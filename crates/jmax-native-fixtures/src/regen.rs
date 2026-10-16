//! The generation recipe for the checked-in modules, shared by the
//! `regenerate` example and the drift test.

use jmax_codegen_rust::{
    generate_with_native, CodegenOptions, GeneratedArtifact, NativeOptions, NativeSpec,
};
use jmax_endpoint_model::EndpointModel;
use jmax_endpoint_ron::{load_model, load_native_spec, ConfigLimits, LoadOptions};

/// A model and the native specification for it.
#[derive(Clone, Copy, Debug)]
pub struct Fixture {
    pub name: &'static str,
    pub model: &'static str,
    pub spec: &'static str,
    /// Specification roots, in order.
    pub roots: &'static [&'static str],
}

pub const BINANCE: Fixture = Fixture {
    name: "binance",
    model: include_str!("../../../fixtures/native/binance.ron"),
    spec: include_str!("../../../fixtures/native/binance.native.ron"),
    roots: &["AggTrade", "DepthUpdate", "DepthSnapshot"],
};

pub const SHAPES: Fixture = Fixture {
    name: "shapes",
    model: include_str!("../../../fixtures/native/shapes.ron"),
    spec: include_str!("../../../fixtures/native/shapes.native.ron"),
    roots: &["Shape"],
};

/// Generated module name, fixture, and whether the specification rejects
/// unknown keys.
pub const VARIANTS: &[(&str, Fixture, bool)] = &[
    ("binance", BINANCE, true),
    ("binance_open", BINANCE, false),
    ("shapes", SHAPES, true),
    ("shapes_open", SHAPES, false),
];

pub fn model(fixture: &Fixture) -> EndpointModel {
    let options = LoadOptions { require_version: false, ..LoadOptions::default() };
    load_model(fixture.model, None, &options).expect("fixture model loads")
}

/// The specification, with unknown keys rejected or accepted as asked.
pub fn spec(fixture: &Fixture, reject_unknown: bool) -> NativeSpec {
    let mut spec = load_native_spec(fixture.spec, &ConfigLimits::default()).expect("fixture spec loads");
    spec.reject_unknown = reject_unknown;
    spec
}

pub fn options(module: &str) -> CodegenOptions {
    CodegenOptions {
        module_name: module.into(),
        emit_schema_tables: false,
        ..CodegenOptions::default()
    }
}

pub fn native_options(predict_layout: bool) -> NativeOptions {
    NativeOptions { predict_layout, ..NativeOptions::default() }
}

/// Every checked-in file, with its path relative to `src/`.
pub fn generate_all(predict_layout: bool) -> Vec<GeneratedArtifact> {
    let mut files = Vec::new();
    for (module, fixture, reject_unknown) in VARIANTS {
        let generation = generate_with_native(
            &model(fixture),
            &spec(fixture, *reject_unknown),
            &options(module),
            &native_options(predict_layout),
        )
        .expect("generation succeeds");
        files.extend(generation.into_artifacts());
    }
    files
}
