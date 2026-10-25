//! Rewrite src/trades/ from fixtures/trades.ron: `cargo run --release --example regen_trades`.
use jmax_codegen_rust::generate_with_native;
use jmax_native_fixtures::regen::{model, native_options, options, spec, Fixture};

fn main() {
    let fixture = Fixture {
        name: "trades",
        model: include_str!("../fixtures/trades.ron"),
        spec: include_str!("../fixtures/trades.native.ron"),
        roots: &["TradeBatch"],
    };
    let generation =
        generate_with_native(&model(&fixture), &spec(&fixture, true), &options("trades"), &native_options(true))
            .expect("generation succeeds");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for artifact in generation.into_artifacts() {
        let path = root.join(&artifact.path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, artifact.contents).unwrap();
        println!("wrote {}", path.display());
    }
}
