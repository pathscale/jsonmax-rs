#!/bin/zsh
set -eu
cargo +1.98.1 --version
"$RUSTC" -vV
"$RUSTDOC" --version
uname -a
sw_vers
sysctl -n machdep.cpu.brand_string hw.memsize hw.ncpu
"$RUSTC" -C target-cpu=native --print cfg
rustup target list --installed --toolchain 1.98.1
