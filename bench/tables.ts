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
