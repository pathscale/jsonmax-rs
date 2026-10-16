#!/bin/sh
# Instructions and allocations per call for every case and parser.
#   bench/native/icount.sh [ITERS]     (default 200)
# Instructions come from callgrind: (Ir with ITERS calls - Ir with none) / ITERS,
# so setup and the three warmup calls cancel. Allocations are counted in process.
# TSV: case  parser  instructions_per_call  allocations_per_call  bytes_per_call
set -eu
iters=${1:-200}
here=$(cd "$(dirname "$0")" && pwd)
cd "$here"
cargo build -j 1 --release --bin count >&2
exe=target/release/count
ir() { valgrind --tool=callgrind --callgrind-out-file=/dev/null "$exe" "$1" "$2" "$3" 2>&1 >/dev/null | sed -n 's/.*Collected : \([0-9]*\).*/\1/p'; }
for case in aggTrade depth_20 depth_1000 trades_100; do
  for parser in "jmax-native" "jmax-native budget" "jmax vm+reparse" "serde_json String" "serde_json fixed" \
                "sonic-rs String" "sonic-rs fixed" "simd-json String" "simd-json fixed"; do
    base=$(ir "$case" "$parser" 0)
    full=$(ir "$case" "$parser" "$iters")
    counts=$("$exe" "$case" "$parser" "$iters")
    printf '%s\t%s\t%s\t%s\n' "$case" "$parser" $(((full - base) / iters)) "$counts"
  done
done
