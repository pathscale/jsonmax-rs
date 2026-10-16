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
| jmax-native | 174 | 785 | 2807 | 2802 | 0 | 0 |
| jmax-native budget | 175 | 783 | 2807 | 2802 | 0 | 0 |
| jmax vm+reparse | 2701 | 51 | 26883 | 26882 | 1 | 168 |
| serde_json String | 453 | 303 | 6130 | 6130 | 4 | 36 |
| serde_json fixed | 478 | 287 | 6134 | 6134 | 2 | 14 |
| sonic-rs String | 353 | 388 | 4830 | 4830 | 4 | 36 |
| sonic-rs fixed | 345 | 397 | 4948 | 4948 | 2 | 14 |
| simd-json String | 573 | 239 | 9222 | 9222 | 13 | 3430 |
| simd-json fixed | 584 | 235 | 9277 | 9277 | 11 | 3408 |

### depth_20 (1234 bytes)

| parser | median ns | MB/s | instructions | before | allocations | bytes allocated |
|---|---:|---:|---:|---:|---:|---:|
| jmax-native | 1769 | 698 | 26446 | 26363 | 0 | 0 |
| jmax-native budget | 1772 | 696 | 26446 | 26363 | 0 | 0 |
| jmax vm+reparse | 25265 | 49 | 236761 | 236681 | 3 | 1008 |
| serde_json String | 3652 | 338 | 68300 | 68300 | 90 | 6617 |
| serde_json fixed | 4220 | 292 | 56758 | 56758 | 10 | 1937 |
| sonic-rs String | 3128 | 395 | 56250 | 56250 | 90 | 6617 |
| sonic-rs fixed | 3142 | 393 | 45061 | 45061 | 10 | 1937 |
| simd-json String | 3550 | 348 | 62704 | 62704 | 93 | 22817 |
| simd-json fixed | 3514 | 351 | 47104 | 47104 | 13 | 20697 |

### depth_1000 (63602 bytes)

| parser | median ns | MB/s | instructions | before | allocations | bytes allocated |
|---|---:|---:|---:|---:|---:|---:|
| jmax-native | 87613 | 726 | 1301719 | 11991195 | 0 | 0 |
| jmax-native budget | 87492 | 727 | 1301437 | 1297434 | 0 | 0 |
| jmax vm+reparse | 1413512 | 45 | 11995214 | 11991180 | 3 | 1008 |
| serde_json String | 170640 | 373 | 3358223 | 3245418 | 4018 | 243784 |
| serde_json fixed | 197148 | 323 | 2623035 | 2622912 | 18 | 65408 |
| sonic-rs String | 146281 | 435 | 2716067 | 2632857 | 4018 | 243784 |
| sonic-rs fixed | 141319 | 450 | 2021560 | 2021518 | 18 | 65408 |
| simd-json String | 170978 | 372 | 2870662 | 2870628 | 4010 | 693036 |
| simd-json fixed | 165856 | 383 | 2228122 | 2227663 | 10 | 581476 |

### trades_100 (14632 bytes)

| parser | median ns | MB/s | instructions | before | allocations | bytes allocated |
|---|---:|---:|---:|---:|---:|---:|
| jmax-native | 19175 | 763 | 276566 | 276359 | 0 | 0 |
| jmax-native budget | 19004 | 770 | 276565 | 276362 | 0 | 0 |
| jmax vm+reparse | 212922 | 69 | 2182002 | 2181802 | 3 | 1008 |
| serde_json String | 50922 | 287 | 730119 | 730119 | 406 | 39858 |
| serde_json fixed | 51347 | 285 | 667799 | 667799 | 206 | 29624 |
| sonic-rs String | 36031 | 406 | 593067 | 593067 | 406 | 39858 |
| sonic-rs fixed | 36424 | 402 | 541618 | 541618 | 206 | 29624 |
| simd-json String | 42271 | 346 | 621980 | 621980 | 410 | 236830 |
| simd-json fixed | 41667 | 351 | 587330 | 587330 | 210 | 231460 |

## Reading the result

Instructions per call is the number to trust; the nanosecond column was taken while other
builds were running on the same cores (the serde_json row is 25 to 80% above its quiet-machine
value), so only its ordering is meaningful. The `before` column is the previous published run.

The native route is allocation-free (0 allocations on `aggTrade`, `depth_20` and `trades_100`;
the serde rows make 10 to 4018) and, since the decimal fast path, executes fewer instructions
than every competitor row on all four documents (the budget row on `depth_1000` is 1.30M
against 2.02M for the best competitor, sonic-rs fixed). Every case now takes the native route under the default
limits: `depth_1000` (63.6 KB) used to run the VM first (11.99M instructions, 3 allocations)
because the length-only `budget_safe` bound assumes the densest possible decimals and cannot vouch
beyond 27.6 KB; the decoder now counts the decimals it reads and asks `budget_safe_counted`, so the
"budget" rows (which raise `max_steps` to get the same effect) are now identical to the default
rows. The `vm+reparse` row is the route that remains for inputs the counted bound cannot vouch for
(and for decoders that accept unknown keys): about 9x the native route on the small documents and
`depth_1000`.

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

Inlining (`#[inline]` on the generated object, tuple and array functions and on the Reader's
scalar methods) took `aggTrade` from 3,036 to 2,802, `trades_100` from 295,159 to 276,359 and
left the depth documents near 26.4k and 1.30M: their cost is the 258 instructions each decimal
still takes (digit loops and the checks around them), not call overhead.

Tried and dropped: reading digit runs eight bytes at a time (SWAR) in `uint`, `int` and the
decimal fast path made every case slower (`aggTrade` 2,802 to 2,951, `depth_20` 26,363 to
28,645, `trades_100` 276,359 to 296,243 instructions), and using it only for the integers still
lost (`aggTrade` 2,976, `trades_100` 290,276). The numbers here are mostly 1 to 6 digits long, so
the chunk test and the fallback to the byte loop cost more than they save; the scalar loops stay.

Counting decimals for the budget bound (`depth_1000`, default limits) took that case from 11,995,195
to 1,301,437 instructions and from 3 allocations to none. The VM bills each decimal a flat 2,048
steps whatever its length, so the bound `len * per_byte + decimals * 2,096 + fixed` is tight once the
count is known: the worst measured/bound ratio over books of up to 3,000 levels, one-digit decimals
and long, escaped and exponent decimals is 0.84.
