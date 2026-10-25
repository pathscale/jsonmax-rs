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
bench/compare.sh 3                       # 3 rounds, renders markdown table to stdout
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
