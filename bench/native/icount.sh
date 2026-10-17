#!/bin/sh
# Instructions and allocations per call for every case and parser.
#   bench/native/icount.sh [ITERS]     (default 200)
# Instructions come from callgrind: (Ir with ITERS calls - Ir with none) / ITERS,
# so setup and the three warmup calls cancel. Allocations are counted in process.
# TSV: case parser instructions/call allocations/call bytes/call
# Darwin appends ns/call cycles/call process_peak_bytes via libSystem counters.
# Counters include loop/snapshot overhead and all process threads.
set -eu
iters=${1:-200}
here=$(cd "$(dirname "$0")" && pwd)
cd "$here"
bun /Users/revenge/code/lanes/bin/mem-cap.ts --rss 8G --name jsonmax -- cargo build --release --bin count -j 4 >&2
exe=target/release/count
if [ "$(uname -s)" = Darwin ]; then
  for case in aggTrade depth_20 depth_1000 trades_100; do
    for parser in "jmax-native" "jmax-native budget" "jmax vm+reparse" "serde_json String" "serde_json fixed" "sonic-rs String" "sonic-rs fixed" "simd-json String" "simd-json fixed"; do
      counts=$(HARDWARE=1 "$exe" "$case" "$parser" "$iters")
      printf '%s\t%s\t%s\n' "$case" "$parser" "$counts"
    done
  done
  exit 0
fi
ir() { valgrind --tool=callgrind --callgrind-out-file=/dev/null "$exe" "$1" "$2" "$3" 2>&1 >/dev/null | awk '/Collected :/ {print $NF}'; }
for case in aggTrade depth_20 depth_1000 trades_100; do
  for parser in "jmax-native" "jmax-native budget" "jmax vm+reparse" "serde_json String" "serde_json fixed" \
                "sonic-rs String" "sonic-rs fixed" "simd-json String" "simd-json fixed"; do
    base=$(ir "$case" "$parser" 0)
    full=$(ir "$case" "$parser" "$iters")
    counts=$("$exe" "$case" "$parser" "$iters")
    printf '%s\t%s\t%s\t%s\n' "$case" "$parser" $(((full - base) / iters)) "$counts"
  done
done
