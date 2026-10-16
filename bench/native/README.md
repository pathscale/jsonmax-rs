# native bench

Side-by-side typed decoding of Binance documents: the generated native decoders
(`jmax-native`) against serde_json, sonic-rs and simd-json. Out of the workspace like
`../harness` (own `[workspace]`, listed in the root `exclude`); needs `../ps-rusty_alloc`.

| path | what |
|---|---|
| `src/lib.rs` | the four cases, the competitor structs, and the rows; every row is checked against the others (a sum of all integers, counts and decimals scaled to 1e8) before it is timed |
| `src/bin/time.rs` | median ns per call, all rows interleaved in random order (`SAMPLES`, `TARGET_NS`, `CASES`, `FILTER`); TSV `case parser bytes ns` |
| `src/bin/count.rs` | `count CASE PARSER ITERS`: allocations and bytes requested per call, in process |
| `tables.sh` | the tables below from the time medians and the `icount.sh` output; the `before` column is the instruction count of the previous published run |
| `icount.sh` | instructions per call from callgrind, `(Ir with N calls - Ir with none) / N`, plus the allocation counts; TSV |
| `fixtures/trades.*` + `src/trades/` | the `TradeBatch` model (a native root is an object, so the REST trade list is wrapped as `{"trades":[...]}`) and its checked-in generated decoder; `cargo run --release --example regen_trades` rewrites it |

```sh
cd bench/native
cargo build -j 1 --release
./target/release/time > run.tsv      # repeat and take the median of the medians
./icount.sh 100 > icount.tsv
```

## Cases and rows

`aggTrade` is the single event in `fixtures/binance_agg_trade.json`; `depth_20` is a
`depthUpdate` with 20 bids and 20 asks; `depth_1000` is a REST snapshot with 1000 and 1000;
`trades_100` is 100 aggTrade records. The depth documents use the harness generators, so the
numbers repeat.

| row | what it does |
|---|---|
| jmax-native | `decode_into` with default limits and a reused context and output |
| jmax-native budget | the same with `max_steps` raised to 2^28 so `budget_safe` vouches for the larger inputs |
| jmax vm+reparse | the VM-first route: the schema VM validates the whole input, then the native decoder reads it again |
| X String | the serde struct with decimals kept as `String` (no decimal parsing) |
| X fixed | decimals parsed during deserialization into an i64 scaled by 1e8 |

Differences that matter when reading the numbers. jmax-native builds exact `Decimal128`
values with the schema's range and scale checks, rejects unknown and duplicate keys, and
validates the whole document to the schema, so it does more than the serde rows, which
ignore unknown keys and truncate past 8 fractional digits. jmax-native reuses its output (and
the boxed levels inside it) across calls; the competitors return a fresh value per call, which
is where their allocations come from. `depth_1000` is 63,602 bytes, over the 27.6 KB the default
`max_steps` can vouch for, so the plain jmax-native row takes the VM-first route there; the
budget row shows the native route. simd-json copies the input into a scratch buffer before each
call, as it must, and that copy is inside its timing.

Numbers below: x86_64, 4 cores shared with other builds (load average about 3), median of
three runs of 15 samples. Treat differences under 10% as noise.

### aggTrade (137 bytes)

| parser | median ns | MB/s | instructions | before | allocations | bytes allocated |
|---|---:|---:|---:|---:|---:|---:|
| jmax-native | 246 | 558 | 3081 | 6329 | 0 | 0 |
| jmax-native budget | 215 | 636 | 3081 | 6329 | 0 | 0 |
| jmax vm+reparse | 3027 | 45 | 27161 | 30409 | 1 | 168 |
| serde_json String | 559 | 245 | 6130 | 6130 | 4 | 36 |
| serde_json fixed | 488 | 281 | 6134 | 6134 | 2 | 14 |
| sonic-rs String | 365 | 376 | 4830 | 4830 | 4 | 36 |
| sonic-rs fixed | 413 | 332 | 4948 | 4948 | 2 | 14 |
| simd-json String | 614 | 223 | 9222 | 9222 | 13 | 3430 |
| simd-json fixed | 725 | 189 | 9277 | 9277 | 11 | 3408 |

### depth_20 (1234 bytes)

| parser | median ns | MB/s | instructions | before | allocations | bytes allocated |
|---|---:|---:|---:|---:|---:|---:|
| jmax-native | 3212 | 384 | 26514 | 157732 | 0 | 0 |
| jmax-native budget | 3233 | 382 | 26514 | 157732 | 0 | 0 |
| jmax vm+reparse | 40158 | 31 | 236832 | 368050 | 3 | 1008 |
| serde_json String | 6905 | 179 | 68300 | 68300 | 90 | 6617 |
| serde_json fixed | 7923 | 156 | 56758 | 56758 | 10 | 1937 |
| sonic-rs String | 5360 | 230 | 56250 | 56250 | 90 | 6617 |
| sonic-rs fixed | 5953 | 207 | 45060 | 45061 | 10 | 1937 |
| simd-json String | 5630 | 219 | 62704 | 62704 | 93 | 22817 |
| simd-json fixed | 6364 | 194 | 47104 | 47104 | 13 | 20697 |

### depth_1000 (63602 bytes)

| parser | median ns | MB/s | instructions | before | allocations | bytes allocated |
|---|---:|---:|---:|---:|---:|---:|
| jmax-native | 2259110 | 28 | 11991240 | 19395302 | 3 | 1008 |
| jmax-native budget | 171036 | 372 | 1297477 | 8701539 | 0 | 0 |
| jmax vm+reparse | 2258200 | 28 | 11991223 | 19395285 | 3 | 1008 |
| serde_json String | 345371 | 184 | 3245418 | 3245418 | 4018 | 243784 |
| serde_json fixed | 401626 | 158 | 2622912 | 2622912 | 18 | 65408 |
| sonic-rs String | 256797 | 248 | 2632857 | 2632857 | 4018 | 243784 |
| sonic-rs fixed | 296052 | 215 | 2021518 | 2021518 | 18 | 65408 |
| simd-json String | 276503 | 230 | 2870628 | 2870628 | 4010 | 693036 |
| simd-json fixed | 326231 | 195 | 2227663 | 2227663 | 10 | 581476 |

### trades_100 (14632 bytes)

| parser | median ns | MB/s | instructions | before | allocations | bytes allocated |
|---|---:|---:|---:|---:|---:|---:|
| jmax-native | 47744 | 306 | 303063 | 644863 | 0 | 0 |
| jmax-native budget | 47782 | 306 | 303061 | 644863 | 0 | 0 |
| jmax vm+reparse | 431478 | 34 | 2208503 | 2550239 | 3 | 1008 |
| serde_json String | 108667 | 135 | 730119 | 730119 | 406 | 39858 |
| serde_json fixed | 109684 | 133 | 667799 | 667799 | 206 | 29624 |
| sonic-rs String | 78807 | 186 | 593067 | 593067 | 406 | 39858 |
| sonic-rs fixed | 83574 | 175 | 541618 | 541618 | 206 | 29624 |
| simd-json String | 86454 | 169 | 621980 | 621980 | 410 | 236830 |
| simd-json fixed | 89265 | 164 | 587330 | 587330 | 210 | 231460 |

## Reading the result

Instructions per call is the number to trust; the nanosecond column was taken while other
builds were running on the same cores (the serde_json row is 25 to 80% above its quiet-machine
value), so only its ordering is meaningful. The `before` column is the previous published run.

The native route is allocation-free (0 allocations on `aggTrade`, `depth_20` and `trades_100`;
the serde rows make 10 to 4018) and, since the decimal fast path, executes fewer instructions
than every competitor row on all four documents (the budget row on `depth_1000` is 1.30M
against 2.02M for the best competitor, sonic-rs fixed). The default-limits `depth_1000` row is the VM-first route
(11.99M) because the document is over the 27.6 KB `budget_safe` length; raising `max_steps`
is what the budget row shows. The VM-first route costs about 2x the native route on the small
documents and about 9x on `depth_1000`, the price paid for inputs `budget_safe` cannot vouch for.

Changes so far, in instructions per call (aggTrade / depth_20 / trades_100 / depth_1000 budget):
8,504 / 159,270 / 917,754 / 8,702,259 at the start; 6,329 / 157,732 / 644,863 / 8,701,539 with layout
prediction (the decoders here are generated with it: a predicted key is one byte compare instead of
a string scan and a dispatch); 3,081 / 26,514 / 303,063 / 1,297,477 with the inline decimal path
(`Reader::decimal` reads a canonical decimal straight from the input and declines to the general
jmax-decimal path for anything else, where `coefficient_precision`'s division loop ran three times
per decimal).
