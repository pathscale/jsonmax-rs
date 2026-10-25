import fs from 'fs';

const paths = Bun.argv.slice(2);
const rows = paths.flatMap((p) =>
  fs
    .readFileSync(p, 'utf8')
    .trim()
    .split('\n')
    .filter((l) => l && !l.startsWith('corpus\t'))
    .map((l) => l.split('\t')),
);

for (const row of rows) {
  if (row.length !== 14) {
    throw new Error(`expected 14 columns: ${row.slice(0, 3).join('/')}`);
  }
  for (const i of [3, 4, 5, 6, 9, 10, 11, 12, 13]) {
    if (!Number.isFinite(Number(row[i]))) {
      throw new Error('invalid numeric cell');
    }
  }
  if (+row[13] !== +row[11] + +row[12]) {
    throw new Error('invalid memory sum');
  }
}

const stripDuplicateClass = (cls: string, parser: string): string => {
  const suffix = ` ${cls}`;
  return parser.endsWith(suffix) ? parser.slice(0, -suffix.length) : parser;
};

const routeKey = (r: string[]): string => `${r[1]} / ${stripDuplicateClass(r[1], r[2])}`;

const corpora = [...new Set(rows.map((r) => r[0]))];
const keys = [...new Set(rows.map(routeKey))];


// Summary first: every jmax route against the best rival of its class and payload.
// Diagnostic rows and the memcpy reference are not routes and never rivals.
const isDiagnostic = (parser: string): boolean => parser.includes('(diagnostic)');
const isJmax = (parser: string): boolean => parser === 'jmax' || parser.startsWith('jmax ');
const medianOf = (matches: string[][], i: number): number => {
  const values = matches.map((r) => Number(r[i])).sort((a, b) => a - b);
  return values[Math.floor((values.length - 1) / 2)];
};
const parserOf = (r: string[]): string => stripDuplicateClass(r[1], r[2]);
const classes = [...new Set(rows.filter((r) => r[1] !== 'ref').map((r) => r[1]))];
const summary: string[] = [];
for (const cls of classes) {
  for (const corpus of corpora) {
    const here = rows.filter((r) => r[0] === corpus && r[1] === cls && !isDiagnostic(parserOf(r)));
    const names = [...new Set(here.map(parserOf))];
    const mbs = (name: string) => medianOf(here.filter((r) => parserOf(r) === name), 4);
    const rivals = names.filter((n) => !isJmax(n));
    const routes = names.filter(isJmax);
    if (!routes.length || !rivals.length) continue;
    const best = rivals.reduce((a, b) => (mbs(b) > mbs(a) ? b : a));
    for (const route of routes) {
      const ratio = mbs(route) / mbs(best);
      summary.push(
        `| ${cls} | ${corpus} | ${route} | ${mbs(route).toFixed(0)} | ${best} | ${mbs(best).toFixed(0)} | ${ratio.toFixed(2)} | ${ratio > 1 ? 'WIN' : 'LOSS'} |`,
      );
    }
  }
}
console.log('Summary: jmax route vs best rival of the class (MB/s, median over rounds; ratio > 1 means jmax is faster)');
console.log();
console.log('| Class | Payload | jmax route | jmax MB/s | Best rival | Rival MB/s | jmax/rival | Result |');
console.log('|---|---|---|---:|---|---:|---:|---|');
for (const line of summary) console.log(line);
console.log();
console.log('Full tables: MB/s; p50 ns; p95 ns; instructions; cycles; allocs; requested bytes; peak + retained = total heap bytes');
console.log();
console.log('| Route | ' + corpora.join(' | ') + ' |');
console.log('|---|' + corpora.map(() => '---:|').join(''));

for (const key of keys) {
  const cells = corpora.map((c) => {
    const matches = rows.filter((r) => r[0] === c && routeKey(r) === key);
    if (!matches.length) return 'n/a';
    const median = (i: number) => {
      const values = matches
        .map((r) => r[i])
        .filter((v) => v !== 'n/a')
        .map(Number)
        .sort((a, b) => a - b);
      return values.length
        ? values[Math.floor((values.length - 1) / 2)].toFixed(i === 4 || i === 9 ? 2 : 0)
        : 'n/a';
    };
    const peak = Math.max(...matches.map((r) => +r[11]));
    const retained = Math.max(...matches.map((r) => +r[12]));
    return (
      [4, 5, 6, 7, 8, 9, 10].map(median).join('; ') +
      `; ${peak} + ${retained} = ${peak + retained}`
    );
  });
  console.log('| ' + key + ' | ' + cells.join(' | ') + ' |');
}
