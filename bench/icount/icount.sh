#!/bin/sh
# Instruction counts per call for the general classes.
#   bench/icount/icount.sh OUT.tsv [N]
# For every corpus, class and parser, measures 0 and N iterations and writes the difference / N
# as `corpus class parser instructions`. Linux uses callgrind; Darwin uses process counters.
# PARSERS=jmax limits
# the run to jmax's rows (about 4x faster); compare those with a full baseline via cmp.sh.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=$1
os=$(uname -s)
if [ "$os" = Darwin ]; then n=${ITERS:-${2:-128}}; else n=${ITERS:-${2:-4}}; fi
case "$n" in ''|*[!0-9]*) echo 'ITERS must be a positive integer' >&2; exit 2;; esac
[ "$n" -gt 0 ] || exit 2
(cd "$here" && cargo build --release --offline -q -j 4)
exe=$here/target/release/jmax-icount
: > "$out"
ir() {
  if [ "$os" = Darwin ]; then "$exe" "$@"; else
    valgrind --tool=callgrind --callgrind-out-file=/dev/null "$exe" "$@" 2>&1 >/dev/null | awk '/Collected :/ {print $NF}'
  fi
}
rows="validate:jmax validate:serde_json validate:sonic-rs-ignored validate:jiter
 typed:jmax typed:jmax-native typed:serde_json typed:simd-json typed:sonic-rs
 dom-owned:jmax dom-owned:serde_json dom-owned:sonic-rs dom-owned:simd-json dom-owned:jiter dom-owned:json-rust
 dom-borrowed:jmax dom-borrowed:simd-json-tape dom-borrowed:jiter
 serialize:jmax serialize:serde_json serialize:sonic-rs serialize:simd-json"
for c in twitter citm_catalog canada github_events apache_builds endpoint_batch; do
  for cp in $rows; do
    cl=${cp%%:*}; pr=${cp#*:}
    if [ "${PARSERS:-}" = jmax ] && [ "$pr" != jmax ]; then continue; fi
    a=$(ir "$c" "$cl" "$pr" 0); b=$(ir "$c" "$cl" "$pr" "$n")
    awk -v a="$a" -v b="$b" -v n="$n" -v c="$c" -v cl="$cl" -v pr="$pr" 'BEGIN {
      if (a !~ /^[0-9]+$/ || b !~ /^[0-9]+$/ || b <= a) exit 1
      printf "%s\t%s\t%s\t%.1f\n", c, cl, pr, (b-a)/n
    }' >> "$out"
  done
done
