# native bench

Side-by-side typed decoding of Binance documents: the generated native decoders
(`jmax-native`) against serde_json, sonic-rs and simd-json. Out of the workspace like
`../harness` (own `[workspace]`, listed in the root `exclude`); needs `../ps-rusty_alloc`.

| path | what |
|---|---|
| `src/lib.rs` | the four cases, the competitor structs, and the rows; every row is checked against the others (a sum of all integers, counts and decimals scaled to 1e8) before it is timed |
| `src/bin/time.rs` | median ns per call, all rows interleaved in random order (`SAMPLES`, `TARGET_NS`, `CASES`, `FILTER`); TSV `case parser bytes ns` |
| `src/bin/count.rs` | `count CASE PARSER ITERS`: allocations and bytes requested per call, in process |
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

| parser | median ns | MB/s | instructions | allocations | bytes allocated |
|---|---:|---:|---:|---:|---:|
| jmax-native | 662 | 207 | 8504 | 0 | 0 |
| jmax-native budget | 668 | 205 | 8504 | 0 | 0 |
| jmax vm+reparse | 2600 | 53 | 32584 | 1 | 168 |
| serde_json String | 431 | 318 | 6130 | 4 | 36 |
| serde_json fixed | 440 | 311 | 6134 | 2 | 14 |
| sonic-rs String | 279 | 491 | 4830 | 4 | 36 |
| sonic-rs fixed | 276 | 496 | 4948 | 2 | 14 |
| simd-json String | 515 | 266 | 9222 | 13 | 3430 |
| simd-json fixed | 507 | 270 | 9277 | 11 | 3408 |

### depth_20 (1234 bytes)

| parser | median ns | MB/s | instructions | allocations | bytes allocated |
|---|---:|---:|---:|---:|---:|
| jmax-native | 14515 | 85 | 159270 | 0 | 0 |
| jmax-native budget | 14998 | 82 | 159270 | 0 | 0 |
| jmax vm+reparse | 32920 | 37 | 369588 | 3 | 1008 |
| serde_json String | 3344 | 369 | 68300 | 90 | 6617 |
| serde_json fixed | 3691 | 334 | 56758 | 10 | 1937 |
| sonic-rs String | 2805 | 440 | 56250 | 90 | 6617 |
| sonic-rs fixed | 2365 | 522 | 45061 | 10 | 1937 |
| simd-json String | 3214 | 384 | 62704 | 93 | 22817 |
| simd-json fixed | 2898 | 426 | 47104 | 13 | 20697 |

### depth_1000 (63602 bytes)

| parser | median ns | MB/s | instructions | allocations | bytes allocated |
|---|---:|---:|---:|---:|---:|
| jmax-native | 1921504 | 33 | 19396022 | 3 | 1008 |
| jmax-native budget | 901221 | 71 | 8702259 | 0 | 0 |
| jmax vm+reparse | 1956097 | 33 | 19396005 | 3 | 1008 |
| serde_json String | 159552 | 399 | 3245418 | 4018 | 243784 |
| serde_json fixed | 182621 | 348 | 2622912 | 18 | 65408 |
| sonic-rs String | 132523 | 480 | 2632857 | 4018 | 243784 |
| sonic-rs fixed | 107068 | 594 | 2021518 | 18 | 65408 |
| simd-json String | 158985 | 400 | 2870628 | 4010 | 693036 |
| simd-json fixed | 138842 | 458 | 2227663 | 10 | 581476 |

### trades_100 (14632 bytes)

| parser | median ns | MB/s | instructions | allocations | bytes allocated |
|---|---:|---:|---:|---:|---:|
| jmax-native | 68134 | 215 | 917754 | 0 | 0 |
| jmax-native budget | 69742 | 210 | 917754 | 0 | 0 |
| jmax vm+reparse | 198218 | 74 | 2823186 | 3 | 1008 |
| serde_json String | 42728 | 342 | 730119 | 406 | 39858 |
| serde_json fixed | 42679 | 343 | 667799 | 206 | 29624 |
| sonic-rs String | 25843 | 566 | 593067 | 406 | 39858 |
| sonic-rs fixed | 27102 | 540 | 541618 | 206 | 29624 |
| simd-json String | 38446 | 381 | 621980 | 410 | 236830 |
| simd-json fixed | 38064 | 384 | 587330 | 210 | 231460 |

## Reading the result

jmax-native does not beat the serde-based decoders in this build. It is allocation-free on
the native route (0 allocations on `aggTrade`, `depth_20` and `trades_100`; the serde rows make
10 to 4018), but it executes more instructions: about 1.4x serde_json on `aggTrade`, 2.3x on
`depth_20`, and 2.7x on `depth_1000` (budget row) against the String rows. The callgrind
profile of `depth_20` puts roughly 60% of the instructions in the decimal path
(`Reader::decimal`, `jmax_decimal::bounds::{validate,rescale}`, i128 arithmetic and a
`from_utf8` per token), over 1,000 instructions per decimal token. The fixed-point rows do the same
work with an i64 and no range or scale checks. The VM-first route costs about 2x the native route, which is the
price paid for inputs `budget_safe` cannot vouch for.
