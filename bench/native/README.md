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
| jmax-native | 191 | 718 | 3036 | 3081 | 0 | 0 |
| jmax-native budget | 190 | 721 | 3036 | 3081 | 0 | 0 |
| jmax vm+reparse | 2765 | 50 | 27116 | 27161 | 1 | 168 |
| serde_json String | 458 | 299 | 6130 | 6130 | 4 | 36 |
| serde_json fixed | 480 | 285 | 6134 | 6134 | 2 | 14 |
| sonic-rs String | 340 | 403 | 4830 | 4830 | 4 | 36 |
| sonic-rs fixed | 352 | 389 | 4948 | 4948 | 2 | 14 |
| simd-json String | 564 | 243 | 9222 | 9222 | 13 | 3430 |
| simd-json fixed | 588 | 233 | 9277 | 9277 | 11 | 3408 |

### depth_20 (1234 bytes)

| parser | median ns | MB/s | instructions | before | allocations | bytes allocated |
|---|---:|---:|---:|---:|---:|---:|
| jmax-native | 1864 | 662 | 26452 | 26514 | 0 | 0 |
| jmax-native budget | 1846 | 669 | 26452 | 26514 | 0 | 0 |
| jmax vm+reparse | 26135 | 47 | 236770 | 236832 | 3 | 1008 |
| serde_json String | 3785 | 326 | 68300 | 68300 | 90 | 6617 |
| serde_json fixed | 4424 | 279 | 56758 | 56758 | 10 | 1937 |
| sonic-rs String | 3338 | 370 | 56250 | 56250 | 90 | 6617 |
| sonic-rs fixed | 3212 | 384 | 45061 | 45060 | 10 | 1937 |
| simd-json String | 3638 | 339 | 62704 | 62704 | 93 | 22817 |
| simd-json fixed | 3643 | 339 | 47104 | 47104 | 13 | 20697 |

### depth_1000 (63602 bytes)

| parser | median ns | MB/s | instructions | before | allocations | bytes allocated |
|---|---:|---:|---:|---:|---:|---:|
| jmax-native | 1447330 | 44 | 11991228 | 11991240 | 3 | 1008 |
| jmax-native budget | 88912 | 715 | 1297467 | 1297477 | 0 | 0 |
| jmax vm+reparse | 1503776 | 42 | 11991213 | 11991223 | 3 | 1008 |
| serde_json String | 181800 | 350 | 3245418 | 3245418 | 4018 | 243784 |
| serde_json fixed | 213302 | 298 | 2622912 | 2622912 | 18 | 65408 |
| sonic-rs String | 156553 | 406 | 2632857 | 2632857 | 4018 | 243784 |
| sonic-rs fixed | 142481 | 446 | 2021518 | 2021518 | 18 | 65408 |
| simd-json String | 174696 | 364 | 2870628 | 2870628 | 4010 | 693036 |
| simd-json fixed | 170494 | 373 | 2227663 | 2227663 | 10 | 581476 |

### trades_100 (14632 bytes)

| parser | median ns | MB/s | instructions | before | allocations | bytes allocated |
|---|---:|---:|---:|---:|---:|---:|
| jmax-native | 21178 | 691 | 295159 | 303063 | 0 | 0 |
| jmax-native budget | 20930 | 699 | 295162 | 303061 | 0 | 0 |
| jmax vm+reparse | 219077 | 67 | 2200602 | 2208503 | 3 | 1008 |
| serde_json String | 51662 | 283 | 730119 | 730119 | 406 | 39858 |
| serde_json fixed | 52681 | 278 | 667799 | 667799 | 206 | 29624 |
| sonic-rs String | 35626 | 411 | 593067 | 593067 | 406 | 39858 |
| sonic-rs fixed | 37270 | 393 | 541618 | 541618 | 206 | 29624 |
| simd-json String | 43450 | 337 | 621980 | 621980 | 410 | 236830 |
| simd-json fixed | 44010 | 332 | 587330 | 587330 | 210 | 231460 |

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

Short strings and keys (an inline scan of up to 32 ASCII bytes before the shared scanner, and
`key_bytes` for the dispatch path) took `aggTrade` from 3,081 to 3,036 and `trades_100` from
303,063 to 295,159 instructions: the strings left after prediction are the symbol and event
type values, which still need a UTF-8 check to become `String`s.
