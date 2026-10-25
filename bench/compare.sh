#!/bin/sh
# Run the cross-parser benchmark suite across N rounds and render a markdown table.
set -eu
rounds=${1:-3}
here=$(cd "$(dirname "$0")" && pwd)
cd "$here/harness"
artifacts=$(mktemp)
trap 'rm -f "$artifacts"' EXIT HUP INT TERM
cargo bench --bench compare --no-run --message-format=json -j 4 > "$artifacts"
exe=$(bun -e 'for (const line of (await Bun.stdin.text()).split("\n")) { try { const a = JSON.parse(line); if (a.reason === "compiler-artifact" && a.target.name === "compare" && a.executable) console.log(a.executable); } catch {} }' < "$artifacts")
[ -x "$exe" ] || exit 1
out=${OUT:-"$here/harness/target/compare-rounds.tsv"}
: > "$out"
r=1
while [ "$r" -le "$rounds" ]; do
  echo "round $r/$rounds" >&2
  "$exe" >> "$out"
  r=$((r + 1))
done
bun "$here/tables.ts" "$out"
