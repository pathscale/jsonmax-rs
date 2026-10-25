//! The checked-in generated modules are exactly what the generator produces.

use jmax_native_fixtures::{regen, PREDICT_LAYOUT};
use std::{fs, path::Path};

#[test]
fn checked_in_modules_equal_a_fresh_generation() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let artifacts = regen::generate_all(PREDICT_LAYOUT);
    assert_eq!(artifacts.len(), regen::VARIANTS.len() * 7 + 8);
    let mut paths = std::collections::BTreeSet::new();
    for artifact in artifacts {
        assert!(
            paths.insert(artifact.path.clone()),
            "duplicate generated path: {}",
            artifact.path
        );
        assert!(
            !Path::new(&artifact.path).is_absolute(),
            "generated artifact path is rooted: {}",
            artifact.path
        );
        let on_disk = fs::read_to_string(root.join(&artifact.path))
            .unwrap_or_else(|error| panic!("{}: {error}", artifact.path));
        assert!(
            on_disk == artifact.contents,
            "{} drifted; run `cargo run -p jmax-native-fixtures --example regenerate`",
            artifact.path
        );
    }
}

#[test]
fn fixture_roots_match_their_native_specifications() {
    let mut fixtures = std::collections::BTreeSet::new();
    for (_, fixture, _, _) in regen::VARIANTS {
        if !fixtures.insert(fixture.name) {
            continue;
        }
        let spec = regen::spec(fixture, true);
        let declared: Vec<_> = spec.roots.iter().map(String::as_str).collect();
        assert_eq!(
            declared,
            fixture.roots,
            "{} fixture roots differ from its native specification",
            fixture.name
        );
        regen::model(fixture);
    }
}

#[test]
fn generation_is_deterministic() {
    let first = regen::generate_all(PREDICT_LAYOUT);
    let second = regen::generate_all(PREDICT_LAYOUT);
    assert_eq!(first, second);
}

#[test]
fn the_shared_artifacts_equal_plain_generate() {
    use jmax_codegen_rust::{generate, options_with_native_idents};
    let model = regen::model(&regen::BINANCE);
    let spec = regen::spec(&regen::BINANCE, true);
    let options = options_with_native_idents(&regen::options("binance"), &spec).unwrap();
    let plain = generate(&model, &options).unwrap();
    let native = jmax_codegen_rust::generate_with_native(
        &model,
        &spec,
        &regen::options("binance"),
        &regen::native_options(false),
    )
    .unwrap();
    assert_eq!(native.artifacts().len(), plain.artifacts().len() + 1);
    for artifact in plain.artifacts() {
        let twin = native.artifacts().iter().find(|item| item.path == artifact.path).unwrap();
        if artifact.path.ends_with("/mod.rs") {
            assert_eq!(twin.contents, format!("{}pub mod native;\n", artifact.contents));
        } else {
            assert_eq!(twin, artifact);
        }
    }
}
