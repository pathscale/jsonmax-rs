# Benchmarks and Differential Harness

Standalone benchmark and differential verification suites in `bench/`. Each crate defines its own `[workspace]` table and is excluded from the root workspace so `cargo test --workspace` stays fast and free of third-party parser dev-dependencies.

| Path | Purpose |
|---|---|
| `harness/` | `jmax-harness`: general corpus benchmark suite (`compare`) and deterministic mutation differential runner (`DIFF=N`) |
| `native/` | `jmax-native-bench`: generated native AOT decoders vs. `serde_json`, `sonic-rs`, `simd-json`, `jiter`, and `ps-pikkr` on Binance payloads |
| `icount/` | `jmax-icount`: per-call hardware instruction counters (Darwin `RUSAGE_INFO_V4` or Linux Callgrind) |
| `dcheck/` | Prints the SIMD scanner backend selected by `jmax-dispatch` per input size and detected CPU features |
| `data/` | Vendored small fixtures (`endpoint_batch.json`, `rpc_small.json`, `binance_small.json`) and `fetch.sh` for the five `simdjson-data` corpora |

## Fetching External Corpora

```sh
bench/data/fetch.sh   # downloads twitter, citm_catalog, canada, github_events, apache_builds
```

Downloaded `.json` files in `bench/data/` are SHA-256 checked and git-ignored. Set `JMAX_DATA=<dir>` to override the corpus directory.

## Running the Differential Gate

```sh
cd bench/harness
RUSTFLAGS="-C target-cpu=native" cargo build --release -j 4

# 30,000-mutation strict differential (with duplicate-key rejection)
DIFF=30000 ./target/release/jmax-harness | md5sum
# expected: 6715c04f984ed8838a4f1678ced61116

# 3,000-mutation differential without duplicate-key rejection
NODUP=1 DIFF=3000 ./target/release/jmax-harness | md5sum
# expected: bb107953a10f4344c43003d1964bf341
```

## Running the General Corpus Benchmarks

```sh
bench/compare.sh                         # 1 round (pass N for more), summary then full tables on stdout; DIAG=1 adds memcpy
SAMPLES=201 OUT=run.tsv bench/compare.sh 5 > run.md
```

Environment variables supported by `jmax-harness`:
- `SAMPLES`: individual-call latency sample count (default `101`)
- `CORPORA`: comma-separated subset (`twitter,citm_catalog,canada,github_events,apache_builds,endpoint_batch`, or `small` to include `rpc_small` and `binance_small`)
- `FILTER`: comma-separated substrings matched against `corpus<TAB>op`
- `NODUP=1`: disable duplicate-key rejection in the single-binary harness

## Running the Native Decoder Benchmarks

```sh
cd bench/native
cargo build --release -j 4
./target/release/time > time.tsv
./tables.sh time.tsv
```

See [MEASUREMENT.md](MEASUREMENT.md) for column definitions and [docs/BENCHMARKS.md](../docs/BENCHMARKS.md) for recorded Apple M4 Max tables.

## Trace Marks: Where Time Goes Inside jsonmax

`crates/jmax-trace` holds feature-gated cycle-counter marks (`jmax_trace::span!(SITE)`) scattered over the scanner dispatch, block cursor, strict syntax and `strict_fast`, string and UTF-8 handling, number parsing, DOM tape building (shape and duplicate checks), the schema VM run loop, and the native decoder reader. With the `trace` feature off, the default, every mark expands to a unit value and generates no code: the optimized `jmax-native-bench` binary is instruction-identical for every jmax function.

```sh
cd bench/native   # or bench/harness
RUSTFLAGS="-C target-cpu=native" cargo build --release --features trace --bin time
CASES=aggTrade,general/github_events FILTER="jmax native,jmax vm,jmax dom-borrowed" ./target/release/time > /dev/null
```

After timing each (payload, route), the bench runs the route again for about 20 ms with the marks recording and prints a table on stderr: site, hits per call, self ticks per call, share of the route's total ticks. Rival parsers have no marks and print nothing. Rules for reading it:

- Ticks are the x86_64 TSC or aarch64 `cntvct_el0`, not core cycles. Self ticks exclude time in nested spans, so shares add to 100% (the `unmarked` row is everything outside any span).
- A span costs two counter reads (about 30 ticks here). The bench measures an empty span and subtracts it per hit, but a route with thousands of hits still runs 4x to 10x slower than untraced and loses some out-of-order overlap. Trust the ranking and the big shares; do not read small shares of fine-grained sites as exact. Never compare traced MB/s with untraced MB/s.
- Slots are plain relaxed loads and stores, correct on one thread only. The sites are listed in `crates/jmax-trace/src/lib.rs`; add one there and put `let _t = jmax_trace::span!(NAME);` where the work starts.
