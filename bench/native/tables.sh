#!/bin/sh
# Markdown tables for the README from three TSVs:
#   tables.sh TIME_MEDIANS ICOUNT [ICOUNT_BEFORE_OR_-]
# TIME_MEDIANS: case parser bytes ns (median of runs); ICOUNT: icount.sh output.
# The "before" column is the instruction count in ICOUNT_BEFORE, when given.
set -eu
awk -F'\t' -v beforefile="${3:--}" '
BEGIN { if (beforefile != "-") while ((getline line < beforefile) > 0) { split(line, f, "\t"); before[f[1] FS f[2]] = f[3] } }
FILENAME == ARGV[1] { t[$1 FS $2] = $4; b[$1] = $3; next }
{ k = $1 FS $2
  if (!($1 in seen)) { seen[$1] = 1
    printf "\n### %s (%d bytes)\n\n| parser | median ns | MB/s | instructions | before | allocations | bytes allocated | ns (counted) | cycles | peak bytes |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n", $1, b[$1] }
  printf "| %s | %.0f | %.0f | %s | %s | %.1f | %.1f | %s | %s | %s |\n", $2, t[k], b[$1] / t[k] * 1000, $3, (k in before ? before[k] : "-"), $4, $5, (NF >= 6 ? $6 : "-"), (NF >= 7 ? $7 : "-"), (NF >= 8 ? $8 : "-") }
' "$1" "$2"
