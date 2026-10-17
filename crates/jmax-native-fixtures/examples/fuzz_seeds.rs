//! Write the `native_vs_vm` seed corpus: `cargo run -p jmax-native-fixtures --example fuzz_seeds -- DIR`.
fn main() {
    let dir = std::env::args().nth(1).expect("usage: fuzz_seeds DIR");
    std::fs::create_dir_all(&dir).expect("create directory");
    for (index, seed) in jmax_native_fixtures::fuzzing::seed_corpus().iter().enumerate() {
        std::fs::write(format!("{dir}/seed-{index:03}"), seed).expect("write seed");
    }
}
