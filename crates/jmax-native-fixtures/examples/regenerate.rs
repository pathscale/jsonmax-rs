//! Rewrite the checked-in generated modules under `src/`.
//!
//! `cargo run -p jmax-native-fixtures --example regenerate`

use std::{fs, path::Path};

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for artifact in jmax_native_fixtures::regen::generate_all(jmax_native_fixtures::PREDICT_LAYOUT) {
        let path = root.join(&artifact.path);
        fs::create_dir_all(path.parent().expect("artifact has a directory")).expect("create directory");
        fs::write(&path, artifact.contents).expect("write artifact");
        println!("wrote {}", path.display());
    }
}
