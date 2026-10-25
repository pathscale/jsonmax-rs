#!/bin/sh
# All available native/general rows, discovered from the benchmark itself.
# Darwin TSV: case parser process-instructions allocs requested_B p50_ns cycles peak_B retained_B p95_ns MB/s.
# Other platforms: case parser callgrind-instructions allocs requested_B.
set -eu
iters=${1:-200}
general_corpora=${GENERAL_CORPORA:-endpoint_batch}
here=$(cd "$(dirname "$0")" && pwd)
cd "$here"
cargo build --release --bin count -j 4 >&2
exe=target/release/count
os=$(uname -s)
ir() { valgrind --tool=callgrind --callgrind-out-file=/dev/null "$exe" "$1" "$2" "$3" 2>&1 >/dev/null | awk '/Collected :/ {print $NF}'; }
cases='aggTrade depth_20 depth_1000 trades_100'
for corpus in $general_corpora; do cases="$cases general/$corpus"; done
for case_name in $cases; do
  "$exe" --list "$case_name" | while IFS= read -r parser; do
    if [ "$os" = Darwin ]; then
      counts=$(HARDWARE=1 "$exe" "$case_name" "$parser" "$iters")
      printf '%s\t%s\t%s\n' "$case_name" "$parser" "$counts"
    else
      base=$(ir "$case_name" "$parser" 0)
      full=$(ir "$case_name" "$parser" "$iters")
      counts=$("$exe" "$case_name" "$parser" "$iters")
      printf '%s\t%s\t%s\t%s\n' "$case_name" "$parser" $(((full - base) / iters)) "$counts"
    fi
  done
done
