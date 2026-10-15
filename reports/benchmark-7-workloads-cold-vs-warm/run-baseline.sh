#!/bin/sh
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_dir=$(CDPATH= cd -- "$script_dir/../.." && pwd)
bench_dir=$(mktemp -d "${TMPDIR:-/tmp}/jmax-followup-baseline.XXXXXX")
mkdir -p "$bench_dir/src"
cp "$script_dir/baseline.rs" "$bench_dir/src/main.rs"
cp "$script_dir/baseline.Cargo.lock" "$bench_dir/Cargo.lock"
cat > "$bench_dir/Cargo.toml" <<EOF
[package]
name = "jmax-followup-baseline"
version = "0.1.0"
edition = "2024"
[dependencies]
jmax-bench = { path = "$repo_dir/crates/jmax-bench" }
jsonmax = { path = "$repo_dir/crates/jsonmax" }
jmax-encode = { path = "$repo_dir/crates/jmax-encode" }
serde = { version = "1", features = ["derive"] }
serde_json = { version = "1", features = ["float_roundtrip"] }
simd-json = "0.17"
sonic-rs = "0.5"
[profile.release]
debug = 1
EOF
# Output and dependencies stay outside the jsonmax workspace.
RUSTFLAGS="-C target-cpu=native" JMAX_RAW="$bench_dir/samples.csv" \
    cargo run --locked --release --manifest-path "$bench_dir/Cargo.toml" "$@"
printf '\nHarness and raw samples: %s\n' "$bench_dir" >&2
