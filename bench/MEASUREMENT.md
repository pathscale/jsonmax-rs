# Measurement Methodology

Both `bench/compare.sh` (six general corpora) and `bench/native/target/release/time` (four Binance payloads) emit 14-column TSV output. Pass one or more TSV files to `bun bench/tables.ts FILE.tsv...` to render markdown comparison tables.

## Column Definitions

1. **Throughput (`MB/s`)**: Decimal input bytes divided by mean execution time across 15 batched samples.
2. **Latency (`p50 ns`, `p95 ns`)**: Nearest-rank percentiles across 101 individually timed single-call invocations (configurable via `SAMPLES`).
3. **Hardware Counters (`instructions/call`, `cycles/call`)**: On macOS (`aarch64`/`x86_64`), deltas from `proc_pid_rusage(RUSAGE_INFO_V4)` over at least 128 warmed calls. On Linux, `icount.sh` uses Valgrind Callgrind.
4. **Heap Metrics (`allocs/call`, `req B/call`, `peak live B + retained B = max overhead B`)**:
   - `CountingPeakAlloc` wraps `RustyAlloc` (or `System` when built with `--features system-alloc`).
   - Timing and hardware-counter passes disable allocation-event hooks; a dedicated memory pass resets counters before each call.
   - `allocs/call` counts allocations, zeroed allocations, and reallocations; `req B/call` sums requested byte sizes.
   - `peak live B` is the maximum live heap increase above the pre-call baseline during a single operation.
   - `retained B` records warm scratch, index, or output buffer capacity retained across calls (including `simd-json`'s mutable input scratch buffer and `Pikkr::retained_heap_bytes()`).

## Workload Classes

- **`validate`**: Full syntactic RFC 8259 + UTF-8 validation (`jsonmax::parse::validate_borrowed` with reused `Scratch`, `serde_json` / `sonic-rs` `IgnoredAny`, `jiter` `next_skip`).
- **`lazy-skip`**: `sonic-rs` lazy root boundary scan (`get` with empty path, which skips unvisited interiors without validating every number or string escape).
- **`query-only`**: Single-field raw slice lookup (`ps-pikkr` speculative SIMD parser and `jsonmax` query mode) without full-document validation.
- **`strict-extract`**: Single-field extraction combined with full-document strict syntax validation (`jsonmax` `QueryMode::Strict`).
- **`typed`**: Two jmax engines are always shown, `jmax native` (generated AOT decoder) and `jmax vm` (schema bytecode VM). Schema-driven field projection on the six general corpora, and full typed struct decoding (including exact `Decimal128` parsing in `jmax native` vs. `String` or fixed-point `i64` in `serde_json`, `sonic-rs`, and `simd-json`) on the four Binance payloads.
- **`dom-borrowed` / `dom-owned`**: Full DOM document construction (`BorrowedDocument` / `TapeDocument` in `jsonmax`, `Value` in `serde_json` and `sonic-rs`, borrowed/owned/tape views in `simd-json`, `JsonValue` in `jiter`, and `json-rust`).
- **`serialize`**: Compact JSON serialization of a pre-parsed document or typed model.
