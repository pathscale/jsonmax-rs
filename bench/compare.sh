#!/bin/sh
# Cross-parser comparison driver.
#   bench/compare.sh [ROUNDS]            (default 5)
# Runs the `compare` bench ROUNDS times (each run interleaves every job in random order,
# SAMPLES samples each), takes the median of the per-run medians, prints markdown tables:
# MB/s per corpus per parser per class, then geomean relative to jmax (= 1.0).
# Raw per-round TSV is kept in $OUT (default: bench/harness/target/compare-rounds.tsv).
set -eu
rounds=${1:-5}
here=$(cd "$(dirname "$0")" && pwd)
cd "$here/harness"
artifacts=$(mktemp)
raw=$(mktemp)
trap 'rm -f "$artifacts" "$raw"' EXIT HUP INT TERM
bun /Users/revenge/code/lanes/bin/mem-cap.ts --rss 8G --name jsonmax -- cargo bench --bench compare --no-run --message-format=json -j 4 > "$artifacts"
exe=$(bun -e 'for (const line of (await Bun.stdin.text()).split("\n")) { try { const a = JSON.parse(line); if (a.reason === "compiler-artifact" && a.target.name === "compare" && a.executable) console.log(a.executable); } catch {} }' < "$artifacts")
[ -x "$exe" ] || { echo "cannot find bench executable" >&2; exit 1; }
out=${OUT:-"$here/harness/target/compare-rounds.tsv"}
: > "$out"
r=1
while [ "$r" -le "$rounds" ]; do
  echo "round $r/$rounds" >&2
  "$exe" > "$raw"
  awk -v r="$r" '{ print r "\t" $0 }' "$raw" >> "$out"
  r=$((r + 1))
done
awk -F'\t' '
function med(key,   n, i, j, t, a) {
  n = cnt[key]; for (i = 1; i <= n; i++) a[i] = val[key, i]
  for (i = 2; i <= n; i++) { t = a[i]; for (j = i - 1; j >= 1 && a[j] > t; j--) a[j+1] = a[j]; a[j+1] = t }
  return a[int((n + 1) / 2)]
}
{ k = $2 SUBSEP $3 SUBSEP $4; cnt[k]++; val[k, cnt[k]] = $6; bytes[$2] = $5
  if (!(k in seen)) { seen[k] = 1; keys[++nk] = k }
  if (!($2 in cs)) { cs[$2] = 1; corp[++nc] = $2 }
  ck = $3 SUBSEP $4; if (!(ck in ps)) { ps[ck] = 1; pl[$3, ++np[$3]] = $4 }
  if (!($3 in cl)) { cl[$3] = 1; cls[++ncl] = $3 } }
END {
  for (i = 1; i <= nk; i++) { k = keys[i]; split(k, p, SUBSEP); ns[k] = med(k); mbs[k] = bytes[p[1]] * 1000 / ns[k] }
  nord = split("validate dom-owned dom-borrowed typed serialize ref", order, " ")
  for (o = 1; o <= nord; o++) { c = order[o]; if (!(c in cl)) continue
    printf "\n### %s (MB/s, median of %d rounds; last column: geomean vs jmax = 1.0)\n\n| parser |", c, '"$rounds"'
    for (i = 1; i <= nc; i++) printf " %s |", corp[i]
    printf " geomean x jmax |\n|---|"; for (i = 1; i <= nc + 1; i++) printf "---:|"; printf "\n"
    for (q = 1; q <= np[c]; q++) { pr = pl[c, q]; printf "| %s |", pr; s = 0; n = 0
      for (i = 1; i <= nc; i++) { k = corp[i] SUBSEP c SUBSEP pr; b = corp[i] SUBSEP c SUBSEP "jmax"
        if (k in ns) { printf " %.0f |", mbs[k]; if (b in ns) { s += log(mbs[k] / mbs[b]); n++; ov[pr, c] = 1; sum[pr, c] += log(mbs[k] / mbs[b]); num[pr, c]++; osum[pr] += log(mbs[k] / mbs[b]); onum[pr]++ } } else printf " n/a |" }
      if (c == "ref" || n == 0) printf " - |\n"; else printf " %.2f |\n", exp(s / n) }
  }
  print "\n### Summary: geomean MB/s relative to jmax (jmax = 1.0) over the corpora\n"
  print "| class | parser | geomean x jmax |\n|---|---|---:|"
  for (o = 1; o <= nord; o++) { c = order[o]; if (c == "ref" || !(c in cl)) continue
    for (q = 1; q <= np[c]; q++) { pr = pl[c, q]; if (pr == "jmax") continue
      printf "| %s | %s | %.2f |\n", c, pr, exp(sum[pr, c] / num[pr, c])
      split(pr, w, " "); fam = w[1]; if (!(fam in fs)) { fs[fam] = 1; fl[++nf] = fam }
      fsum[fam] += sum[pr, c]; fnum[fam] += num[pr, c] } }
  print "\n### Overall per library: geomean over every (class, variant, corpus) cell it has\n"
  print "| library | geomean x jmax | cells |\n|---|---:|---:|"
  for (x = 1; x <= nf; x++) printf "| %s | %.2f | %d |\n", fl[x], exp(fsum[fl[x]] / fnum[fl[x]]), fnum[fl[x]]
}' "$out"
