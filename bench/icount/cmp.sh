#!/bin/sh
# Compare jmax and, when present in both files, jmax-native rows against BASE.tsv.
# Historical jmax baselines use the best non-native rival for the class.
set -eu
[ "$#" -eq 2 ] || exit 2
awk -F '\t' -v gate="${GATE_PCT:-1}" '
function invalid(message) { print "ERROR " message > "/dev/stderr"; bad = 1 }
function valid() { return NF == 4 && $1 != "" && $2 != "" && $3 != "" && $4 ~ /^[0-9]+([.][0-9]+)?$/ && $4 + 0 > 0 }
FILENAME == ARGV[1] {
  if (!valid()) { invalid("invalid baseline row " NR); next }
  key = $1 SUBSEP $2 SUBSEP $3
  if (key in base_row) { invalid("duplicate baseline row " $1 " " $2 " " $3); next }
  base_row[key] = $4 + 0
  cell = $1 SUBSEP $2
  if ($3 == "jmax") base[cell] = $4 + 0
  else if ($3 == "jmax-native") native_base[cell] = $4 + 0
  else if (!(cell in best) || $4 < best[cell]) best[cell] = $4 + 0
  next
}
{
  if (!valid()) { invalid("invalid new row " NR); next }
  key = $1 SUBSEP $2 SUBSEP $3
  if (key in seen_row) { invalid("duplicate new row " $1 " " $2 " " $3); next }
  seen_row[key] = $4 + 0
  if ($3 == "jmax") {
    cell = $1 SUBSEP $2
    if (!(cell in base)) { invalid("no baseline jmax cell " $1 " " $2); next }
    seen[cell] = 1
    delta = ($4 / base[cell] - 1) * 100
    if (!count++ || delta > worst) worst = delta
    if (cell in best) { sum[$2] += log(($4 + 0) / best[cell]); cells[$2]++ }
    printf "%-14s %-12s %7.2f -> %7.2f (%+.2f%%)\n", $1, $2, base[cell]/1e6, $4/1e6, delta
  }
  if ($3 == "jmax-native") {
    cell = $1 SUBSEP $2
    native_seen[cell] = $4 + 0
    if (cell in native_base) {
      delta = ($4 / native_base[cell] - 1) * 100
      if (!native_count++ || delta > native_worst) native_worst = delta
      printf "%-14s %-12s native %7.2f -> %7.2f (%+.2f%%)\n", $1, $2, native_base[cell]/1e6, $4/1e6, delta
    }
  }
}
END {
  for (cell in base) if (!(cell in seen)) invalid("missing new jmax cell " cell)
  for (cell in native_base) if (!(cell in native_seen)) invalid("missing new jmax-native cell " cell)
  if (!count) invalid("no comparable jmax cells")
  for (cl in cells) printf "GEO %-12s %.3f\n", cl, exp(sum[cl]/cells[cl])
  if (count) printf "WORST jmax %+.2f%%\n", worst
  if (native_count) printf "WORST jmax-native %+.2f%%\n", native_worst
  if (gate !~ /^[0-9]+([.][0-9]+)?$/ || worst > gate + 0 + 1e-9 || (native_count && native_worst > gate + 0 + 1e-9)) bad = 1
  exit bad
}' "$1" "$2"
