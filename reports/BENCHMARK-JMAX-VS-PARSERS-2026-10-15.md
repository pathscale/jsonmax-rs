# jsonmax-rs vs Rust JSON parsers — live benchmark, 2026-10-15

Completed on Apple M4 Max with 64 GiB RAM. **The measured jmax snapshot is slower than serde_json on all six corpora in the main scan, DOM, typed projection, and serialization comparisons.** Geometric-mean speed ratios versus the matching serde_json operation are 0.127× for the strict cursor, 0.356× for DOM parsing, 0.066× for owned typed extraction, and 0.192× for DOM serialization. A ratio below 1 means slower. These are measurements of the specified snapshot and APIs, under recorded machine contention; they are not claims about later workspace edits or all possible parser configurations.

The requested report date and filename are retained. Timing started at **2026-10-16T00:00:26.392974+07:00** and finished at **2026-10-16T00:03:00.142260+07:00**, after local midnight on October 16; the corresponding UTC date remained October 15.

## Measured source and build status

- Initial repository revision: `7e7aafbee8bbeafd1317f3df0f9b4359b5f75052`. The working tree was clean at initial inspection; no `AGENTS.md` was found in the repository ancestry or under `crates/`.
- The complete live workspace also passed a final release build after the concurrent changes had compiled successfully. The initial complete workspace successfully built using `RUSTFLAGS="-C target-cpu=native" cargo +1.98.1 build --release --workspace` after the five-file compile patch described below. The built-in `jmax_bench` completed all 28 combinations of seven deterministic workloads and four modes; all workloads reported `neon` dispatch.
- During this task, unrelated source changes appeared in the shared checkout, including reusable scratch APIs and changes to slot arena ownership. To avoid compiling different versions between timing runs, the measured binary was built from an immutable archive of the initial revision plus this task’s compile fixes, at `/tmp/jmax-vs-parsers/source-snapshot/`. Those concurrent changes are preserved and are **not measured by this report**. No results here should be attributed to the final moving working tree.
- The original external manifest at `/tmp/jmax-vs-parsers/Cargo.toml` contains the requested absolute repository path dependencies. The reproducible run uses `/tmp/jmax-vs-parsers/frozen/Cargo.toml`, with the same dependencies redirected to the frozen snapshot. Only the snapshot’s four existing sibling project paths were made absolute; no parser algorithm was changed for the frozen run.
- The checkout’s `stable` rustup alias lacked a Cargo component. Both workspace and competitor builds used the installed explicit toolchain `+1.98.1`; no global toolchain configuration was changed.
- Compile fixes: explicitly type `Vec<&SlotSpec>` in AOT decode-plan validation; use the public `jmax_encode::JsonWriter` path in the facade; import `FieldId` in the fuzz facade; call `jmax_schema_aot::tables::to_owned`; replace unavailable `Bytes::capacity()` with visible length, account decoded storage using the owning vector’s capacity, and type the sentinel loop index as `usize`. These changes are saved in `compile-fixes.patch`.
- A subsequent `NumericPolicy::default()` call in a concurrently added DOM scratch function was corrected to `strict_default()` in the live checkout; that new scratch API is outside the frozen baseline. No float, Unicode, or large-container correctness failure was observed on the six valid corpora. The default schema VM budget limitation is described below.
- This task added no `serde_json`, `tokio`, `std::thread`, `.expect()`, or `#[cfg(test)]` to jmax source/manifests. The final live-checkout audit found a concurrently created `crates/jsonmax/tests/typed_schema_cache.rs` using `std::thread`; that independent integration test was preserved and is excluded from the measured snapshot. The frozen source passes the requested dependency/thread checks. The frozen workspace lockfile contains no `serde_json` or `tokio` package. Competitors and all comparison code remain external. The ownership lower-bound check cannot observe hidden backing capacity of sliced `Bytes`; visible length is a lower bound, not a total allocation measurement.

## Environment and exact dependencies

- Host: `macOS-26.5-arm64-arm-64bit-Mach-O`; Apple M4 Max, 16 physical cores (12 performance + 4 efficiency), 68,719,476,736 bytes RAM.
- Compiler: `rustc 1.98.1 (48a229cea 2026-09-01)`, target `aarch64-apple-darwin`, LLVM 22.1.8.
- External release profile: `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`; `RUSTFLAGS="-C target-cpu=native"`; standard system allocator; default library features. Built-in benchmark uses the repository’s ordinary release profile instead of the external profile.
- Measured binary SHA-256: `b50ddbd5e0677fbbb7305e67a267a6e2cf6ef1f813c120509a2ba82c38a68e91`. Both Cargo.lock files and build logs are archived.

| Dependency | Resolved version |
|---|---:|
| jsonmax | 0.1.0 |
| serde | 1.0.229 |
| serde_derive | 1.0.229 |
| serde_json | 1.0.154 |
| simd-json | 0.18.1 |
| sonic-rs | 0.5.10 |
| json | 0.12.4 |
| miniserde | 0.1.46 |

All twelve requested jmax packages are direct path dependencies of the external harness. `json` is the `json-rust` crate. sonic-rs built and ran successfully on Apple Silicon. simd-json uses NEON on aarch64, as documented by its [versioned upstream documentation](https://docs.rs/simd-json/0.18.1/simd_json/). API references: [sonic-rs 0.5.10](https://docs.rs/sonic-rs/0.5.10/sonic_rs/), [serde_json source](https://github.com/serde-rs/json), [miniserde source](https://github.com/dtolnay/miniserde), [json-rust source](https://github.com/maciejhirsz/json-rust).

**Contention:** unrelated Rust builds and other applications ran during measurement. Across 75 process snapshots, the sum of CPU usage for observed non-benchmark processes (individual processes ≥5% CPU) had median 884.7% and maximum 1448.7% (100% = one core). Rust compiler processes alone reached 1091.2%. No unrelated process was stopped. There was no CPU affinity pinning, allocator replacement, or thermal isolation. Repetition and shuffled order reduce ordering bias but do not remove contention; p95 and small differences require particular caution. See `system-load.jsonl` for timestamped observations.

## Corpora and provenance

Five fixtures are the unmodified files from [simdjson/simdjson-data](https://github.com/simdjson/simdjson-data/tree/4197c425e857f0ec38e89822fdd0bd9ea21f4daf/jsonexamples), pinned at `4197c425e857f0ec38e89822fdd0bd9ea21f4daf`. They cover the standard Twitter, CITM, Canada, GitHub events, and Apache builds workloads. `endpoint_batch.json` is a deterministic **synthetic realistic workload**, not a canonical benchmark: 175 endpoint request/response records with schema definitions, IDs, arrays, prices, timing values, Unicode, and escaped text. Its generator and exact digest are archived. All inputs were in memory during timing; network and file I/O are excluded.

| Fixture | Exact bytes | Decimal MB | Objects | Arrays | Integer / float values | Max value depth |
|---|---:|---:|---:|---:|---:|---:|
| twitter.json | 631,515 | 0.631515 | 1,264 | 1,050 | 2,108 / 1 | 10 |
| citm_catalog.json | 1,727,204 | 1.727204 | 10,937 | 10,451 | 14,392 / 0 | 7 |
| canada.json | 2,251,051 | 2.251051 | 4 | 56,045 | 46 / 111,080 | 7 |
| github_events.json | 65,132 | 0.065132 | 180 | 19 | 149 / 0 | 6 |
| apache_builds.json | 127,275 | 0.127275 | 884 | 3 | 2 / 0 | 3 |
| endpoint_batch.json | 102,494 | 0.102494 | 1,226 | 701 | 1,052 / 350 | 6 |

| Fixture | SHA-256 |
|---|---|
| twitter.json | `30721e496a8d73cfc50658923c34eb2c0fbe15ee6835005e43ee624d8dedf200` |
| citm_catalog.json | `a73e7a883f6ea8de113dff59702975e60119b4b58d451d518a929f31c92e2059` |
| canada.json | `f83b3b354030d5dd58740c68ac4fecef64cb730a0d12a90362a7f23077f50d78` |
| github_events.json | `c9eebb2cf2d46649059e9d48700919bacb3e8e0fb58452065a1a9de7778fd22e` |
| apache_builds.json | `f8e3422ac7d3c3550674afcb37e979e4e9bbeccffdb66933423495d55b6f5c74` |
| endpoint_batch.json | `1ffaa72a20f83deb87fc9d1d1151edbf1b5080f10ffda87cb63a7c6bc7579b70` |

Fixture downloads, manifest, and hashes are in `/tmp/jmax-bench-data/`. The report archive includes the manifest and generated endpoint fixture, plus pinned URLs for fetching the standard data again.

## Measurement method and scope

Each of three sequential fresh processes benchmarks all 180 operation/corpus pairs. Each operation first passes an untimed correctness invocation, then warms up for at least 50 ms and five complete operations. Warmup estimates calibrate repetitions per sample toward 2 ms (1–65,536 operations). Each process collects 61 batch samples per pair. Job order is deterministically shuffled on every sample round. Results below pool the 183 per-operation batch-average samples from all three runs. Median uses nearest rank 50%, p95 uses nearest rank 95%. Every result and relevant input passes through `std::hint::black_box`; allocation, result destruction, and copy costs required by the tested entry point are included. No disk I/O, fixture generation, schema compilation, semantic comparison, or report formatting is timed.

`median_ns` and `p95_ns` are fractional nanoseconds per complete document operation, derived by dividing measured batch time by repetition count; they are not claimed to have subnanosecond clock accuracy. **The p95 is a percentile of batch averages, not individual-request tail latency.** Per-run medians and every raw sample are available in the archive. No confidence interval or memory-allocation ranking is inferred.

For scan, DOM, typed extraction, and document serialization, rates use the original fixture’s byte length. For escaped-string encoding, rates use the summed decoded UTF-8 bytes of all source object keys and string values. Document serialization is consequently input-normalized throughput, not emitted-byte throughput; output whitespace and numeric formatting differ. Decimal units: `MB/s = bytes × 1000 / median_ns`, `GB/s = bytes / median_ns`. Speed ratio is `matching serde_json median / parser median`; `1.000×` is parity. All speed ratios in a category use the same corpus and denominator. Partial lookup rows have no speed ratio or full-document throughput.

- **A, structural/scan:** jmax consumes every strict cursor event through EOF using its 64-byte NEON classifier; its VM runs a precompiled `Any` schema with empty selections and full traversal. serde_json uses `IgnoredAny`; simd-json builds a tape; sonic-rs parses a whole-document borrowed `LazyValue`. These APIs perform different work and have different Unicode/number-validation guarantees, documented below.
- **B, DOM:** `jsonmax::parse::parse_slice` builds the native flat DOM and owns an input copy; `parse_payload` is reported separately with a pre-owned shared payload. jmax retains exact number spellings and owner-bound string references; the other DOMs generally materialize numbers. The extra `jmax-dom+number-conversion+copy` row reads and black-boxes every number after parsing using Rust’s `u64`/`i64`/`f64` conversion. This adds eager numeric reads but does not replace the native representation or cache numbers, so it is not an identical DOM layout. Native strict numeric policy already checks number validity/finite-range policy during parsing.
- **Copy policy:** every `+copy` row includes input allocation and copying. All simd-json parse/tape/typed rows clone immutable fixture bytes inside the timer because simd-json mutates its input. The separately labeled shared-input jmax DOM row excludes the initial payload construction. No reusable scratch-buffer or parser-only mutable-input results are mixed into the main rows. json-rust/miniserde receive `&str`; the one-time input UTF-8 check needed to form that view occurs outside timing and is disclosed as an API difference.
- **C, typed extraction:** all parsers materialize the same selected scalar fields into the same derived Rust types and owned strings, and scan the remaining document. jmax compiles its native schema once and runs `jmax_schema_vm::execute::validate`, then converts slots into the same owned types. Native graph schemas allow unknown keys; runtime `reject_unknown_keys` is false, matching the derived types’ ignored-field behavior. Duplicate detection and other strict jmax defaults remain enabled. These are metadata projections, not full typed models of every field.
- **VM budget:** the convenience facade’s fixed default 16,000,000-step budget rejects the Canada projection around byte 1,937,565 with `BudgetExhausted`. This is a resource-policy rejection, not invalid JSON. The public VM entry point is used with `max_steps = 128_000_000`, syntax limits otherwise unchanged, for all VM rows. No rejection is counted as a successful fast sample. Applications using `extract_payload` with defaults still have this limit in the measured revision.
- **GitHub typed array:** the VM has fixed slots and cannot accumulate repeated rows directly; writing a captured slot twice is an error. The harness strictly walks the complete root array, finds each event object, then invokes the precompiled event schema on each object and builds `Vec<Github>`. Both passes, per-object copies, and owned outputs are timed. Competitors deserialize the root array directly. This reports the cost of a correct public-API adapter, not a native single-pass repeated-row extraction implementation.
- **D, serialization:** pre-parsed native DOMs are encoded using `jsonmax::encode::document_to_vec` (internally `jmax_encode::JsonWriter`), serde_json/simd-json/sonic-rs `to_vec`, and miniserde `to_string`. Escaped-string rows serialize the same `Vec<String>` containing every key and string value; jmax explicitly writes an array with `JsonWriter::string`. This isolates string escaping from number formatting and DOM traversal. The requested shorthand `jsonmax::parse` / `jsonmax::encode_document` does not exist as a root function in this revision; these are its actual public entry points.

### Exact typed projections

| Fixture | Selected fields / output |
|---|---|
| twitter.json | `search_metadata.{query: String, count: i64, max_id: i64}` |
| citm_catalog.json | `venueNames.PLEYEL_PLEYEL: String` |
| canada.json | root `type: String`; coordinates are traversed but not extracted |
| github_events.json | every root event: `{id: String, type: String, public: bool}` → `Vec<Github>` |
| apache_builds.json | `mode: String, nodeName: String, numExecutors: i64, quietingDown: bool` |
| endpoint_batch.json | `batch_id: String, tenant: String, schema_version: i64, request_count: i64, accepted: bool`; batch records are traversed |

## Main jmax results against serde_json

| Corpus | Cursor MB/s / ratio | DOM MB/s / ratio | Typed MB/s / ratio | Serialize MB/s / ratio |
|---|---:|---:|---:|---:|
| twitter.json | 212.55 / 0.111× | 177.65 / 0.428× | 125.94 / 0.065× | 390.26 / 0.197× |
| citm_catalog.json | 293.11 / 0.155× | 239.78 / 0.337× | 188.64 / 0.101× | 556.71 / 0.141× |
| canada.json | 166.86 / 0.111× | 110.48 / 0.240× | 81.61 / 0.056× | 212.50 / 0.198× |
| github_events.json | 229.69 / 0.119× | 193.33 / 0.355× | 73.40 / 0.041× | 414.13 / 0.252× |
| apache_builds.json | 231.90 / 0.161× | 188.40 / 0.453× | 113.13 / 0.079× | 330.03 / 0.221× |
| endpoint_batch.json | 97.94 / 0.114× | 83.70 / 0.364× | 59.44 / 0.069× | 149.93 / 0.162× |

Across the measured alternatives, sonic-rs has the lowest DOM median on all six corpora (4.678× the serde_json throughput by geometric mean), and the lowest escaped-string median on all six. simd-json tape has the lowest scan-group median on five corpora; serde_json IgnoredAny leads on Canada. Typed projections have mixed winners: simd-json on four, serde_json on Canada, and miniserde on the endpoint batch. sonic-rs leads DOM serialization on five, while serde_json leads on Canada. These are observed medians under the documented scope and contention, not statistically established universal rankings.

The raw-number DOM row is jmax’s normal API result. Use the additional eager-number row below when numeric read work matters. Scan and VM results should be assessed together with their stronger default duplicate/numeric checks and the malformed-input matrix; those differences do not establish the cause or size of the measured performance gap.

## A. Complete-document scan / tape / lazy parsing

| Corpus | Implementation | Median ns | Median µs | P95 ns | P95 µs | MB/s | GB/s | vs serde_json |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| twitter | `jmax-cursor-strict+copy` | 2971125.00 | 2971.125 | 3361625.00 | 3361.625 | 212.55 | 0.212551 | 0.111× |
| twitter | `jmax-vm-any+copy` | 4977083.00 | 4977.083 | 5696667.00 | 5696.667 | 126.88 | 0.126885 | 0.066× |
| twitter | `serde-json-ignored` | 328694.33 | 328.694 | 375034.67 | 375.035 | 1921.28 | 1.921284 | 1.000× |
| twitter | `simd-json-tape+copy` | 253547.71 | 253.548 | 285765.62 | 285.766 | 2490.71 | 2.490715 | 1.296× |
| twitter | `sonic-rs-lazy-document` | 383841.60 | 383.842 | 440183.40 | 440.183 | 1645.25 | 1.645249 | 0.856× |
| citm_catalog | `jmax-cursor-strict+copy` | 5892709.00 | 5892.709 | 7286958.00 | 7286.958 | 293.11 | 0.293109 | 0.155× |
| citm_catalog | `jmax-vm-any+copy` | 9123375.00 | 9123.375 | 11516375.00 | 11516.375 | 189.32 | 0.189316 | 0.100× |
| citm_catalog | `serde-json-ignored` | 914958.50 | 914.958 | 1312097.33 | 1312.097 | 1887.74 | 1.887740 | 1.000× |
| citm_catalog | `simd-json-tape+copy` | 599843.75 | 599.844 | 746906.25 | 746.906 | 2879.42 | 2.879423 | 1.525× |
| citm_catalog | `sonic-rs-lazy-document` | 773972.33 | 773.972 | 898000.00 | 898.000 | 2231.61 | 2.231609 | 1.182× |
| canada | `jmax-cursor-strict+copy` | 13490500.00 | 13490.500 | 32425833.00 | 32425.833 | 166.86 | 0.166862 | 0.111× |
| canada | `jmax-vm-any+copy` | 27457625.00 | 27457.625 | 69390125.00 | 69390.125 | 81.98 | 0.081983 | 0.054× |
| canada | `serde-json-ignored` | 1493895.50 | 1493.896 | 2851687.50 | 2851.688 | 1506.83 | 1.506833 | 1.000× |
| canada | `simd-json-tape+copy` | 2605542.00 | 2605.542 | 4682250.00 | 4682.250 | 863.95 | 0.863947 | 0.573× |
| canada | `sonic-rs-lazy-document` | 1613541.50 | 1613.542 | 2956771.00 | 2956.771 | 1395.10 | 1.395100 | 0.926× |
| github_events | `jmax-cursor-strict+copy` | 283562.50 | 283.562 | 570520.75 | 570.521 | 229.69 | 0.229692 | 0.119× |
| github_events | `jmax-vm-any+copy` | 541010.25 | 541.010 | 1169521.00 | 1169.521 | 120.39 | 0.120390 | 0.063× |
| github_events | `serde-json-ignored` | 33865.75 | 33.866 | 60466.37 | 60.466 | 1923.24 | 1.923241 | 1.000× |
| github_events | `simd-json-tape+copy` | 28212.79 | 28.213 | 55880.56 | 55.881 | 2308.60 | 2.308599 | 1.200× |
| github_events | `sonic-rs-lazy-document` | 40043.20 | 40.043 | 73889.52 | 73.890 | 1626.54 | 1.626543 | 0.846× |
| apache_builds | `jmax-cursor-strict+copy` | 548834.00 | 548.834 | 1027986.00 | 1027.986 | 231.90 | 0.231901 | 0.161× |
| apache_builds | `jmax-vm-any+copy` | 1123083.00 | 1123.083 | 2660791.00 | 2660.791 | 113.33 | 0.113326 | 0.078× |
| apache_builds | `serde-json-ignored` | 88130.93 | 88.131 | 150003.00 | 150.003 | 1444.16 | 1.444158 | 1.000× |
| apache_builds | `simd-json-tape+copy` | 53534.51 | 53.535 | 121870.23 | 121.870 | 2377.44 | 2.377438 | 1.646× |
| apache_builds | `sonic-rs-lazy-document` | 106566.67 | 106.567 | 163463.87 | 163.464 | 1194.32 | 1.194323 | 0.827× |
| endpoint_batch | `jmax-cursor-strict+copy` | 1046444.67 | 1046.445 | 2235236.00 | 2235.236 | 97.94 | 0.097945 | 0.114× |
| endpoint_batch | `jmax-vm-any+copy` | 1731187.50 | 1731.188 | 3717979.00 | 3717.979 | 59.20 | 0.059204 | 0.069× |
| endpoint_batch | `serde-json-ignored` | 118808.82 | 118.809 | 208210.82 | 208.211 | 862.68 | 0.862680 | 1.000× |
| endpoint_batch | `simd-json-tape+copy` | 85916.67 | 85.917 | 164284.71 | 164.285 | 1192.95 | 1.192947 | 1.383× |
| endpoint_batch | `sonic-rs-lazy-document` | 131753.15 | 131.753 | 205948.69 | 205.949 | 777.92 | 0.777924 | 0.902× |

### Partial on-demand lookup — separate workload

`sonic_rs::get_from_slice` looks up `search_metadata`, `venueNames`, `type`, `mode`, or `batch_id`; for GitHub it returns array index 0. It may stop before consuming the tail. **Full-document MB/s, GB/s, and speed ratios are intentionally not reported:** dividing whole input bytes by a partial lookup time would overstate processed-byte throughput. These rows demonstrate the requested on-demand API, not strict validation.

| Corpus | Operation | Median ns | Median µs | P95 ns | P95 µs |
|---|---|---:|---:|---:|---:|
| twitter | `sonic-rs-get-field` | 383358.20 | 383.358 | 435508.20 | 435.508 |
| citm_catalog | `sonic-rs-get-field` | 765944.33 | 765.944 | 946041.67 | 946.042 |
| canada | `sonic-rs-get-field` | 37.75 | 0.038 | 66.38 | 0.066 |
| github_events | `sonic-rs-get-index0` | 856.51 | 0.857 | 2066.21 | 2.066 |
| apache_builds | `sonic-rs-get-field` | 75.23 | 0.075 | 153.83 | 0.154 |
| endpoint_batch | `sonic-rs-get-field` | 36.55 | 0.037 | 74.02 | 0.074 |

## B. Full DOM parsing

| Corpus | Implementation | Median ns | Median µs | P95 ns | P95 µs | MB/s | GB/s | vs serde_json |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| twitter | `jmax-dom+copy` | 3554834.00 | 3554.834 | 4195542.00 | 4195.542 | 177.65 | 0.177650 | 0.428× |
| twitter | `jmax-dom-shared-input` | 3567667.00 | 3567.667 | 4070375.00 | 4070.375 | 177.01 | 0.177011 | 0.427× |
| twitter | `jmax-dom+number-conversion+copy` | 3610750.00 | 3610.750 | 4223666.00 | 4223.666 | 174.90 | 0.174899 | 0.422× |
| twitter | `serde-json-value` | 1522208.00 | 1522.208 | 1759708.50 | 1759.708 | 414.87 | 0.414868 | 1.000× |
| twitter | `simd-json-borrowed+copy` | 513323.00 | 513.323 | 599194.33 | 599.194 | 1230.25 | 1.230249 | 2.965× |
| twitter | `simd-json-owned+copy` | 903152.67 | 903.153 | 1039271.00 | 1039.271 | 699.23 | 0.699234 | 1.685× |
| twitter | `sonic-rs-value` | 276656.25 | 276.656 | 313750.00 | 313.750 | 2282.67 | 2.282670 | 5.502× |
| twitter | `json-rust-value` | 800319.67 | 800.320 | 936819.33 | 936.819 | 789.08 | 0.789078 | 1.902× |
| twitter | `miniserde-value` | 1356708.00 | 1356.708 | 1514041.50 | 1514.042 | 465.48 | 0.465476 | 1.122× |
| citm_catalog | `jmax-dom+copy` | 7203333.00 | 7203.333 | 9154542.00 | 9154.542 | 239.78 | 0.239778 | 0.337× |
| citm_catalog | `jmax-dom-shared-input` | 7201083.00 | 7201.083 | 9167375.00 | 9167.375 | 239.85 | 0.239853 | 0.338× |
| citm_catalog | `jmax-dom+number-conversion+copy` | 7453958.00 | 7453.958 | 9196250.00 | 9196.250 | 231.72 | 0.231716 | 0.326× |
| citm_catalog | `serde-json-value` | 2430834.00 | 2430.834 | 3223917.00 | 3223.917 | 710.54 | 0.710540 | 1.000× |
| citm_catalog | `simd-json-borrowed+copy` | 1400333.00 | 1400.333 | 1854458.00 | 1854.458 | 1233.42 | 1.233424 | 1.736× |
| citm_catalog | `simd-json-owned+copy` | 1869333.50 | 1869.333 | 2507541.00 | 2507.541 | 923.97 | 0.923968 | 1.300× |
| citm_catalog | `sonic-rs-value` | 597760.25 | 597.760 | 820562.50 | 820.562 | 2889.46 | 2.889459 | 4.067× |
| citm_catalog | `json-rust-value` | 1708250.00 | 1708.250 | 2265584.00 | 2265.584 | 1011.10 | 1.011096 | 1.423× |
| citm_catalog | `miniserde-value` | 2758459.00 | 2758.459 | 3660250.00 | 3660.250 | 626.15 | 0.626148 | 0.881× |
| canada | `jmax-dom+copy` | 20375666.00 | 20375.666 | 59759459.00 | 59759.459 | 110.48 | 0.110477 | 0.240× |
| canada | `jmax-dom-shared-input` | 20427750.00 | 20427.750 | 46661083.00 | 46661.083 | 110.20 | 0.110196 | 0.239× |
| canada | `jmax-dom+number-conversion+copy` | 23120917.00 | 23120.917 | 53363833.00 | 53363.833 | 97.36 | 0.097360 | 0.212× |
| canada | `serde-json-value` | 4890459.00 | 4890.459 | 8745542.00 | 8745.542 | 460.29 | 0.460294 | 1.000× |
| canada | `simd-json-borrowed+copy` | 5273750.00 | 5273.750 | 9609417.00 | 9609.417 | 426.84 | 0.426841 | 0.927× |
| canada | `simd-json-owned+copy` | 5371583.00 | 5371.583 | 12126166.00 | 12126.166 | 419.07 | 0.419067 | 0.910× |
| canada | `sonic-rs-value` | 1599687.50 | 1599.688 | 3151958.50 | 3151.959 | 1407.18 | 1.407182 | 3.057× |
| canada | `json-rust-value` | 2937958.00 | 2937.958 | 6154833.00 | 6154.833 | 766.20 | 0.766196 | 1.665× |
| canada | `miniserde-value` | 6275959.00 | 6275.959 | 11011666.00 | 11011.666 | 358.68 | 0.358678 | 0.779× |
| github_events | `jmax-dom+copy` | 336898.86 | 336.899 | 1049069.33 | 1049.069 | 193.33 | 0.193328 | 0.355× |
| github_events | `jmax-dom-shared-input` | 334916.67 | 334.917 | 937555.33 | 937.555 | 194.47 | 0.194472 | 0.357× |
| github_events | `jmax-dom+number-conversion+copy` | 341979.17 | 341.979 | 794708.33 | 794.708 | 190.46 | 0.190456 | 0.350× |
| github_events | `serde-json-value` | 119578.72 | 119.579 | 323449.06 | 323.449 | 544.68 | 0.544679 | 1.000× |
| github_events | `simd-json-borrowed+copy` | 47417.61 | 47.418 | 112812.50 | 112.812 | 1373.58 | 1.373582 | 2.522× |
| github_events | `simd-json-owned+copy` | 86243.58 | 86.244 | 217465.25 | 217.465 | 755.21 | 0.755210 | 1.387× |
| github_events | `sonic-rs-value` | 25169.80 | 25.170 | 53430.22 | 53.430 | 2587.70 | 2.587704 | 4.751× |
| github_events | `json-rust-value` | 96041.70 | 96.042 | 315149.47 | 315.149 | 678.16 | 0.678164 | 1.245× |
| github_events | `miniserde-value` | 138528.69 | 138.529 | 421466.19 | 421.466 | 470.17 | 0.470170 | 0.863× |
| apache_builds | `jmax-dom+copy` | 675541.50 | 675.542 | 1271666.67 | 1271.667 | 188.40 | 0.188404 | 0.453× |
| apache_builds | `jmax-dom-shared-input` | 671944.67 | 671.945 | 1365277.67 | 1365.278 | 189.41 | 0.189413 | 0.455× |
| apache_builds | `jmax-dom+number-conversion+copy` | 678041.00 | 678.041 | 1379375.00 | 1379.375 | 187.71 | 0.187710 | 0.451× |
| apache_builds | `serde-json-value` | 305958.40 | 305.958 | 655035.71 | 655.036 | 415.99 | 0.415988 | 1.000× |
| apache_builds | `simd-json-borrowed+copy` | 106206.25 | 106.206 | 217708.30 | 217.708 | 1198.38 | 1.198376 | 2.881× |
| apache_builds | `simd-json-owned+copy` | 226927.12 | 226.927 | 430010.50 | 430.010 | 560.86 | 0.560863 | 1.348× |
| apache_builds | `sonic-rs-value` | 65471.77 | 65.472 | 131494.61 | 131.495 | 1943.97 | 1.943967 | 4.673× |
| apache_builds | `json-rust-value` | 162532.44 | 162.532 | 312197.75 | 312.198 | 783.07 | 0.783074 | 1.882× |
| apache_builds | `miniserde-value` | 326271.00 | 326.271 | 664200.00 | 664.200 | 390.09 | 0.390090 | 0.938× |
| endpoint_batch | `jmax-dom+copy` | 1224542.00 | 1224.542 | 2565458.00 | 2565.458 | 83.70 | 0.083700 | 0.364× |
| endpoint_batch | `jmax-dom-shared-input` | 1226666.50 | 1226.667 | 2375750.00 | 2375.750 | 83.55 | 0.083555 | 0.363× |
| endpoint_batch | `jmax-dom+number-conversion+copy` | 1247271.00 | 1247.271 | 2554417.00 | 2554.417 | 82.17 | 0.082175 | 0.357× |
| endpoint_batch | `serde-json-value` | 445191.60 | 445.192 | 926058.20 | 926.058 | 230.22 | 0.230224 | 1.000× |
| endpoint_batch | `simd-json-borrowed+copy` | 193973.45 | 193.973 | 375387.85 | 375.388 | 528.39 | 0.528392 | 2.295× |
| endpoint_batch | `simd-json-owned+copy` | 339583.33 | 339.583 | 694208.29 | 694.208 | 301.82 | 0.301823 | 1.311× |
| endpoint_batch | `sonic-rs-value` | 64485.35 | 64.485 | 129831.08 | 129.831 | 1589.42 | 1.589415 | 6.904× |
| endpoint_batch | `json-rust-value` | 256567.62 | 256.568 | 499226.29 | 499.226 | 399.48 | 0.399481 | 1.735× |
| endpoint_batch | `miniserde-value` | 449729.17 | 449.729 | 1376611.17 | 1376.611 | 227.90 | 0.227902 | 0.990× |

## C. Owned typed metadata extraction

| Corpus | Implementation | Median ns | Median µs | P95 ns | P95 µs | MB/s | GB/s | vs serde_json |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| twitter | `jmax-vm-owned-extract` | 5014333.00 | 5014.333 | 5624042.00 | 5624.042 | 125.94 | 0.125942 | 0.065× |
| twitter | `serde-json-typed` | 326642.86 | 326.643 | 375375.00 | 375.375 | 1933.35 | 1.933350 | 1.000× |
| twitter | `simd-json-typed+copy` | 320583.33 | 320.583 | 379982.14 | 379.982 | 1969.89 | 1.969893 | 1.019× |
| twitter | `miniserde-typed` | 389152.67 | 389.153 | 444020.83 | 444.021 | 1622.79 | 1.622795 | 0.839× |
| twitter | `sonic-rs-typed` | 379125.00 | 379.125 | 428908.20 | 428.908 | 1665.72 | 1.665717 | 0.862× |
| citm_catalog | `jmax-vm-owned-extract` | 9156167.00 | 9156.167 | 11147667.00 | 11147.667 | 188.64 | 0.188638 | 0.101× |
| citm_catalog | `serde-json-typed` | 921750.00 | 921.750 | 1179708.50 | 1179.708 | 1873.83 | 1.873831 | 1.000× |
| citm_catalog | `simd-json-typed+copy` | 741389.00 | 741.389 | 1028208.33 | 1028.208 | 2329.69 | 2.329687 | 1.243× |
| citm_catalog | `miniserde-typed` | 984500.00 | 984.500 | 1181541.50 | 1181.542 | 1754.40 | 1.754397 | 0.936× |
| citm_catalog | `sonic-rs-typed` | 770458.33 | 770.458 | 1007444.33 | 1007.444 | 2241.79 | 2.241788 | 1.196× |
| canada | `jmax-vm-owned-extract` | 27584459.00 | 27584.459 | 64692084.00 | 64692.084 | 81.61 | 0.081606 | 0.056× |
| canada | `serde-json-typed` | 1538833.50 | 1538.833 | 3021312.50 | 3021.312 | 1462.83 | 1.462829 | 1.000× |
| canada | `simd-json-typed+copy` | 3128666.00 | 3128.666 | 5685792.00 | 5685.792 | 719.49 | 0.719492 | 0.492× |
| canada | `miniserde-typed` | 2453750.00 | 2453.750 | 5054917.00 | 5054.917 | 917.39 | 0.917392 | 0.627× |
| canada | `sonic-rs-typed` | 1604270.50 | 1604.271 | 2983792.00 | 2983.792 | 1403.16 | 1.403162 | 0.959× |
| github_events | `jmax-vm-owned-extract` | 887375.00 | 887.375 | 2419875.00 | 2419.875 | 73.40 | 0.073399 | 0.041× |
| github_events | `serde-json-typed` | 36782.75 | 36.783 | 105633.93 | 105.634 | 1770.72 | 1.770721 | 1.000× |
| github_events | `simd-json-typed+copy` | 35624.31 | 35.624 | 77881.42 | 77.881 | 1828.30 | 1.828302 | 1.033× |
| github_events | `miniserde-typed` | 48576.22 | 48.576 | 94073.94 | 94.074 | 1340.82 | 1.340821 | 0.757× |
| github_events | `sonic-rs-typed` | 40050.38 | 40.050 | 77338.54 | 77.339 | 1626.25 | 1.626252 | 0.918× |
| apache_builds | `jmax-vm-owned-extract` | 1125041.50 | 1125.042 | 2157062.50 | 2157.062 | 113.13 | 0.113129 | 0.079× |
| apache_builds | `serde-json-typed` | 88339.89 | 88.340 | 124895.79 | 124.896 | 1440.74 | 1.440742 | 1.000× |
| apache_builds | `simd-json-typed+copy` | 69912.11 | 69.912 | 141625.00 | 141.625 | 1820.50 | 1.820500 | 1.264× |
| apache_builds | `miniserde-typed` | 99327.10 | 99.327 | 155737.50 | 155.738 | 1281.37 | 1.281372 | 0.889× |
| apache_builds | `sonic-rs-typed` | 111240.18 | 111.240 | 196436.42 | 196.436 | 1144.15 | 1.144146 | 0.794× |
| endpoint_batch | `jmax-vm-owned-extract` | 1724187.50 | 1724.188 | 3232687.50 | 3232.688 | 59.44 | 0.059445 | 0.069× |
| endpoint_batch | `serde-json-typed` | 119034.29 | 119.034 | 190531.25 | 190.531 | 861.05 | 0.861046 | 1.000× |
| endpoint_batch | `simd-json-typed+copy` | 115908.33 | 115.908 | 208002.80 | 208.003 | 884.27 | 0.884268 | 1.027× |
| endpoint_batch | `miniserde-typed` | 112103.19 | 112.103 | 228125.00 | 228.125 | 914.28 | 0.914283 | 1.062× |
| endpoint_batch | `sonic-rs-typed` | 130475.00 | 130.475 | 219016.67 | 219.017 | 785.55 | 0.785545 | 0.912× |

## D. Document serialization

| Corpus | Implementation | Median ns | Median µs | P95 ns | P95 µs | MB/s | GB/s | vs serde_json |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| twitter | `jmax-dom-writer` | 1618208.50 | 1618.208 | 1847896.00 | 1847.896 | 390.26 | 0.390256 | 0.197× |
| twitter | `serde-json-to-vec` | 318965.33 | 318.965 | 373340.17 | 373.340 | 1979.89 | 1.979886 | 1.000× |
| twitter | `simd-json-to-vec` | 321000.00 | 321.000 | 399541.67 | 399.542 | 1967.34 | 1.967336 | 0.994× |
| twitter | `miniserde-to-string` | 462783.40 | 462.783 | 524441.60 | 524.442 | 1364.60 | 1.364602 | 0.689× |
| twitter | `sonic-rs-to-vec` | 193087.09 | 193.087 | 218401.55 | 218.402 | 3270.62 | 3.270623 | 1.652× |
| citm_catalog | `jmax-dom-writer` | 3102500.00 | 3102.500 | 4069959.00 | 4069.959 | 556.71 | 0.556714 | 0.141× |
| citm_catalog | `serde-json-to-vec` | 436062.50 | 436.062 | 581138.83 | 581.139 | 3960.91 | 3.960909 | 1.000× |
| citm_catalog | `simd-json-to-vec` | 405659.67 | 405.660 | 525716.60 | 525.717 | 4257.77 | 4.257766 | 1.075× |
| citm_catalog | `miniserde-to-string` | 878014.00 | 878.014 | 1190875.00 | 1190.875 | 1967.17 | 1.967171 | 0.497× |
| citm_catalog | `sonic-rs-to-vec` | 402541.60 | 402.542 | 510583.20 | 510.583 | 4290.75 | 4.290747 | 1.083× |
| canada | `jmax-dom-writer` | 10593292.00 | 10593.292 | 21201209.00 | 21201.209 | 212.50 | 0.212498 | 0.198× |
| canada | `serde-json-to-vec` | 2101000.00 | 2101.000 | 7425875.00 | 7425.875 | 1071.42 | 1.071419 | 1.000× |
| canada | `simd-json-to-vec` | 2415625.00 | 2415.625 | 4854458.00 | 4854.458 | 931.87 | 0.931871 | 0.870× |
| canada | `miniserde-to-string` | 4302167.00 | 4302.167 | 10044875.00 | 10044.875 | 523.24 | 0.523237 | 0.488× |
| canada | `sonic-rs-to-vec` | 2155104.50 | 2155.104 | 3910500.00 | 3910.500 | 1044.52 | 1.044521 | 0.975× |
| github_events | `jmax-dom-writer` | 157272.46 | 157.272 | 403342.67 | 403.343 | 414.13 | 0.414135 | 0.252× |
| github_events | `serde-json-to-vec` | 39705.61 | 39.706 | 76001.80 | 76.002 | 1640.37 | 1.640373 | 1.000× |
| github_events | `simd-json-to-vec` | 30661.56 | 30.662 | 46294.88 | 46.295 | 2124.22 | 2.124223 | 1.295× |
| github_events | `miniserde-to-string` | 52503.47 | 52.503 | 117091.12 | 117.091 | 1240.53 | 1.240527 | 0.756× |
| github_events | `sonic-rs-to-vec` | 21034.38 | 21.034 | 45873.44 | 45.873 | 3096.46 | 3.096455 | 1.888× |
| apache_builds | `jmax-dom-writer` | 385645.83 | 385.646 | 800208.33 | 800.208 | 330.03 | 0.330031 | 0.221× |
| apache_builds | `serde-json-to-vec` | 85113.32 | 85.113 | 173229.17 | 173.229 | 1495.36 | 1.495359 | 1.000× |
| apache_builds | `simd-json-to-vec` | 70476.83 | 70.477 | 123196.72 | 123.197 | 1805.91 | 1.805913 | 1.208× |
| apache_builds | `miniserde-to-string` | 121109.38 | 121.109 | 241356.75 | 241.357 | 1050.91 | 1.050910 | 0.703× |
| apache_builds | `sonic-rs-to-vec` | 47698.40 | 47.698 | 98923.80 | 98.924 | 2668.33 | 2.668328 | 1.784× |
| endpoint_batch | `jmax-dom-writer` | 683597.33 | 683.597 | 1224069.33 | 1224.069 | 149.93 | 0.149933 | 0.162× |
| endpoint_batch | `serde-json-to-vec` | 110936.52 | 110.937 | 261878.95 | 261.879 | 923.90 | 0.923898 | 1.000× |
| endpoint_batch | `simd-json-to-vec` | 119888.16 | 119.888 | 182589.89 | 182.590 | 854.91 | 0.854913 | 0.925× |
| endpoint_batch | `miniserde-to-string` | 182493.54 | 182.494 | 365111.08 | 365.111 | 561.63 | 0.561631 | 0.608× |
| endpoint_batch | `sonic-rs-to-vec` | 95134.76 | 95.135 | 236385.87 | 236.386 | 1077.36 | 1.077356 | 1.166× |

## D supplement. Escaped-string array encoding

| Corpus | Implementation | Median ns | Median µs | P95 ns | P95 µs | MB/s | GB/s | vs serde_json |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| twitter | `jmax-string-writer` | 953708.50 | 953.708 | 1111500.00 | 1111.500 | 385.78 | 0.385775 | 0.263× |
| twitter | `serde-json-strings` | 250607.14 | 250.607 | 292875.00 | 292.875 | 1468.10 | 1.468103 | 1.000× |
| twitter | `simd-json-strings` | 210787.11 | 210.787 | 237856.44 | 237.856 | 1745.44 | 1.745444 | 1.189× |
| twitter | `miniserde-strings` | 317708.33 | 317.708 | 362708.33 | 362.708 | 1158.03 | 1.158034 | 0.789× |
| twitter | `sonic-rs-strings` | 77931.17 | 77.931 | 99338.36 | 99.338 | 4721.05 | 4.721050 | 3.216× |
| citm_catalog | `jmax-string-writer` | 1148270.50 | 1148.271 | 1429125.00 | 1429.125 | 192.79 | 0.192793 | 0.198× |
| citm_catalog | `serde-json-strings` | 227637.50 | 227.637 | 323731.44 | 323.731 | 972.51 | 0.972507 | 1.000× |
| citm_catalog | `simd-json-strings` | 244078.67 | 244.079 | 291152.78 | 291.153 | 907.00 | 0.906999 | 0.933× |
| citm_catalog | `miniserde-strings` | 329187.50 | 329.188 | 418583.33 | 418.583 | 672.50 | 0.672501 | 0.692× |
| citm_catalog | `sonic-rs-strings` | 87242.05 | 87.242 | 151319.43 | 151.319 | 2537.53 | 2.537526 | 2.609× |
| canada | `jmax-string-writer` | 735.84 | 0.736 | 1100.43 | 1.100 | 122.31 | 0.122310 | 0.164× |
| canada | `serde-json-strings` | 120.74 | 0.121 | 199.39 | 0.199 | 745.40 | 0.745396 | 1.000× |
| canada | `simd-json-strings` | 132.97 | 0.133 | 235.06 | 0.235 | 676.85 | 0.676850 | 0.908× |
| canada | `miniserde-strings` | 307.69 | 0.308 | 631.17 | 0.631 | 292.50 | 0.292502 | 0.392× |
| canada | `sonic-rs-strings` | 83.64 | 0.084 | 180.81 | 0.181 | 1076.06 | 1.076064 | 1.444× |
| github_events | `jmax-string-writer` | 110625.00 | 110.625 | 222740.78 | 222.741 | 413.81 | 0.413812 | 0.313× |
| github_events | `serde-json-strings` | 34653.04 | 34.653 | 85550.52 | 85.551 | 1321.04 | 1.321039 | 1.000× |
| github_events | `simd-json-strings` | 26683.66 | 26.684 | 45625.00 | 45.625 | 1715.58 | 1.715582 | 1.299× |
| github_events | `miniserde-strings` | 43457.39 | 43.457 | 86662.89 | 86.663 | 1053.40 | 1.053400 | 0.797× |
| github_events | `sonic-rs-strings` | 10894.88 | 10.895 | 31070.26 | 31.070 | 4201.79 | 4.201789 | 3.181× |
| apache_builds | `jmax-string-writer` | 281994.00 | 281.994 | 446944.33 | 446.944 | 272.93 | 0.272928 | 0.262× |
| apache_builds | `serde-json-strings` | 73819.44 | 73.819 | 116733.30 | 116.733 | 1042.60 | 1.042598 | 1.000× |
| apache_builds | `simd-json-strings` | 63534.09 | 63.534 | 100049.23 | 100.049 | 1211.38 | 1.211381 | 1.162× |
| apache_builds | `miniserde-strings` | 98521.00 | 98.521 | 181958.50 | 181.958 | 781.19 | 0.781194 | 0.749× |
| apache_builds | `sonic-rs-strings` | 24211.54 | 24.212 | 67281.08 | 67.281 | 3178.81 | 3.178815 | 3.049× |
| endpoint_batch | `jmax-string-writer` | 412812.50 | 412.812 | 882937.50 | 882.938 | 160.46 | 0.160460 | 0.196× |
| endpoint_batch | `serde-json-strings` | 80983.35 | 80.983 | 165452.05 | 165.452 | 817.95 | 0.817946 | 1.000× |
| endpoint_batch | `simd-json-strings` | 84527.23 | 84.527 | 156567.31 | 156.567 | 783.65 | 0.783653 | 0.958× |
| endpoint_batch | `miniserde-strings` | 110927.10 | 110.927 | 171604.12 | 171.604 | 597.15 | 0.597149 | 0.730× |
| endpoint_batch | `sonic-rs-strings` | 36836.47 | 36.836 | 75166.67 | 75.167 | 1798.22 | 1.798218 | 2.198× |

String benchmark denominators are decoded source key/value bytes; Canada has relatively little string data, so its string workload is small and is calibrated to many repetitions. The encoded JSON array also contains quotes, commas, and escape expansions, which are not included in this denominator.

## Correctness checks and validation differences

All six fixtures passed all 30 operation preflights each (180 total), jmax DOM parse→encode→serde_json exact-value comparison, cross-parser typed-output equality (including every GitHub event), and semantic checks of simd-json, sonic-rs, json-rust, and miniserde encoded DOMs. Competing numeric DOMs are compared with a relative tolerance of `1e-14` because decimal-to-binary64 algorithms can differ; jmax round trips use exact serde_json value equality and retain the original number spelling. These checks are separate from timing and do not establish exhaustive JSON conformance.

Appending a second root (` false`) to each corpus was rejected by the jmax strict cursor, VM, DOM, serde_json IgnoredAny, simd-json tape, and sonic-rs whole-document LazyValue. Eighteen additional positive/negative probes covered literal UTF-8, escaped surrogate pairs, controls, malformed UTF-8, lone surrogates, malformed numbers, trailing commas, missing colons, incomplete nesting, and empty input. The jmax cursor and DOM matched all 18 expected outcomes. Unpaired surrogate escapes are not Unicode scalar values; the following table reports acceptance directly, rather than claiming a complete RFC conformance verdict about all implementations.

| Probe | jmax cursor / DOM | serde IgnoredAny | serde Value | simd tape / borrowed DOM | sonic LazyValue / Value | json-rust | miniserde |
|---|---|---|---|---|---|---|---|
| lone-high-surrogate | reject / reject | accept | reject | accept / accept | accept / reject | reject | reject |
| lone-low-surrogate | reject / reject | accept | reject | reject / reject | accept / reject | reject | reject |
| invalid-utf8 | reject / reject | accept | reject | reject / reject | reject / reject | reject | reject |

These probes explain why `IgnoredAny`, tape, and lazy operations cannot automatically be treated as interchangeable strict Unicode-validation baselines. No competitor source or feature configuration was altered to force identical policy. Every timed fixture itself is valid and accepted.

## Run-to-run stability

Three independent process medians are shown below for the main jmax rows. “Spread” is `max(run median) / min(run median) − 1`; it describes observed variability and is not a confidence interval. All implementations’ run medians and batch counts are in `aggregate.json`.

| Corpus | Operation | Run 1 µs | Run 2 µs | Run 3 µs | Spread |
|---|---|---:|---:|---:|---:|
| twitter | `jmax-cursor-strict+copy` | 2646.625 | 3097.000 | 3103.875 | 17.3% |
| twitter | `jmax-dom+copy` | 3181.750 | 3726.125 | 3702.750 | 17.1% |
| twitter | `jmax-vm-owned-extract` | 4509.291 | 5165.333 | 5198.125 | 15.3% |
| twitter | `jmax-dom-writer` | 1476.812 | 1646.792 | 1682.333 | 13.9% |
| citm_catalog | `jmax-cursor-strict+copy` | 5626.250 | 6394.459 | 5662.291 | 13.7% |
| citm_catalog | `jmax-dom+copy` | 6792.667 | 7771.625 | 6868.875 | 14.4% |
| citm_catalog | `jmax-vm-owned-extract` | 8826.167 | 10112.458 | 8828.458 | 14.6% |
| citm_catalog | `jmax-dom-writer` | 2985.750 | 3389.167 | 2992.625 | 13.5% |
| canada | `jmax-cursor-strict+copy` | 14639.791 | 13623.375 | 11621.000 | 26.0% |
| canada | `jmax-dom+copy` | 20954.917 | 20503.333 | 17971.209 | 16.6% |
| canada | `jmax-vm-owned-extract` | 31826.167 | 27849.666 | 23910.042 | 33.1% |
| canada | `jmax-dom-writer` | 10942.958 | 10995.625 | 9313.333 | 18.1% |
| github_events | `jmax-cursor-strict+copy` | 285.653 | 284.786 | 253.469 | 12.7% |
| github_events | `jmax-dom+copy` | 343.042 | 341.798 | 304.839 | 12.5% |
| github_events | `jmax-vm-owned-extract` | 934.708 | 890.667 | 802.111 | 16.5% |
| github_events | `jmax-dom-writer` | 165.806 | 160.795 | 144.607 | 14.7% |
| apache_builds | `jmax-cursor-strict+copy` | 551.250 | 551.528 | 467.108 | 18.1% |
| apache_builds | `jmax-dom+copy` | 688.083 | 693.597 | 573.833 | 20.9% |
| apache_builds | `jmax-vm-owned-extract` | 1163.209 | 1132.667 | 943.889 | 23.2% |
| apache_builds | `jmax-dom-writer` | 399.514 | 388.444 | 320.161 | 24.8% |
| endpoint_batch | `jmax-cursor-strict+copy` | 1045.729 | 1047.792 | 1046.208 | 0.2% |
| endpoint_batch | `jmax-dom+copy` | 1213.896 | 1238.479 | 1223.292 | 2.0% |
| endpoint_batch | `jmax-vm-owned-extract` | 1710.042 | 1740.083 | 1725.500 | 1.8% |
| endpoint_batch | `jmax-dom-writer` | 674.021 | 690.250 | 674.406 | 2.4% |

## Built-in benchmark output

The following is the complete built-in output for the frozen, compile-fixed revision. Its rows measure complete passes over 128 generated documents rather than one canonical fixture. Its throughput is computed from total input bytes / total elapsed time, while the external tables use median latency; do not compare those aggregation formulas as if identical. Default configuration: 3 warmup passes, 20 measured passes, 32 items/document. The initial live-checkout invocation also completed and is archived separately.

```text
jsonmax deterministic benchmark
Seed: 5354007379338153539 | Documents/workload: 128 | Items/document: 32 | Warmup: 3 | Measured: 20
Times and Payload Bytes describe complete corpus passes; MB/s uses decimal MB.
SIMD dispatch for each workload's first payload:
  SmallObject: neon
  EscapedStrings: neon
  Unicode: neon
  NumberArray: neon
  DepthBook: neon
  AggTrade: neon
  Nested: neon
Extract uses empty selections for NumberArray and Nested; full validation still runs.

| Workload       | Mode      | Payload Bytes | Min (ns) | Median (ns) | P95 (ns) | Throughput (MB/s) | ns/op | Msg/sec |
|----------------|-----------|--------------:|---------:|------------:|---------:|------------------:|------:|--------:|
| SmallObject    | Parse     |         81091 |   848208 |      881542 |   939875 |             91.75 | 6904.57 |  144832 |
| SmallObject    | Validate  |         81091 |  1183750 |     1233583 |  1313458 |             65.38 | 9689.52 |  103204 |
| SmallObject    | Extract   |         81091 |  1208667 |     1211791 |  1279792 |             66.26 | 9561.85 |  104582 |
| SmallObject    | Serialize |         81091 |   377000 |      378542 |   379792 |            214.15 | 2958.27 |  338036 |
| EscapedStrings | Parse     |        363516 |  2793292 |     2878667 |  3020792 |            125.07 | 22707.03 |   44039 |
| EscapedStrings | Validate  |        363516 |  3725000 |     3778792 |  3894958 |             95.54 | 29724.37 |   33642 |
| EscapedStrings | Extract   |        363516 |  3747542 |     3864750 |  3951250 |             93.93 | 30236.13 |   33073 |
| EscapedStrings | Serialize |        363516 |   827875 |      849667 |   875416 |            428.76 | 6623.62 |  150975 |
| Unicode        | Parse     |        232444 |  1416750 |     1437417 |  1469959 |            161.45 | 11247.82 |   88906 |
| Unicode        | Validate  |        232444 |  2232875 |     2299000 |  2347250 |            101.07 | 17967.53 |   55656 |
| Unicode        | Extract   |        232444 |  2266041 |     2324125 |  2365084 |            100.10 | 18141.34 |   55123 |
| Unicode        | Serialize |        232444 |   496000 |      504625 |   521458 |            456.05 | 3981.97 |  251132 |
| NumberArray    | Parse     |         44092 |   422750 |      431167 |   454334 |            101.53 | 3392.77 |  294744 |
| NumberArray    | Validate  |         44092 |   564334 |      574625 |   605541 |             76.30 | 4514.88 |  221490 |
| NumberArray    | Extract   |         44092 |   562250 |      573417 |   607417 |             76.00 | 4532.39 |  220634 |
| NumberArray    | Serialize |         44092 |   153292 |      154458 |   158167 |            284.14 | 1212.30 |  824876 |
| DepthBook      | Parse     |        207024 |  2076333 |     2126375 |  2168625 |             97.09 | 16658.07 |   60031 |
| DepthBook      | Validate  |        207024 |  2681167 |     2796042 |  2869667 |             74.42 | 21734.26 |   46010 |
| DepthBook      | Extract   |        207024 |  2715458 |     2801166 |  2881792 |             73.77 | 21923.70 |   45613 |
| DepthBook      | Serialize |        207024 |  1096708 |     1130334 |  1161750 |            182.34 | 8870.04 |  112739 |
| AggTrade       | Parse     |         19891 |   289000 |      296208 |   298958 |             67.15 | 2314.29 |  432098 |
| AggTrade       | Validate  |         19891 |   402708 |      406667 |   423708 |             48.57 | 3199.15 |  312583 |
| AggTrade       | Extract   |         19891 |   410250 |      416208 |   441208 |             47.19 | 3293.33 |  303644 |
| AggTrade       | Serialize |         19891 |   111250 |      113333 |   127167 |            172.19 | 902.51 | 1108025 |
| Nested         | Parse     |         18884 |   626333 |      642209 |   664708 |             29.21 | 5050.13 |  198015 |
| Nested         | Validate  |         18884 |  1011375 |     1047792 |  1097958 |             17.96 | 8214.45 |  121737 |
| Nested         | Extract   |         18884 |  1010208 |     1027833 |  1096125 |             18.08 | 8159.03 |  122564 |
| Nested         | Serialize |         18884 |   193708 |      196708 |   203041 |             95.53 | 1544.42 |  647493 |
```

## Reproduction and artifacts

The external project remains at `/tmp/jmax-vs-parsers/`; standard fixtures remain at `/tmp/jmax-bench-data/`. The [companion archive](benchmark-jmax-vs-parsers-2026-10-15/reproduction.tar.gz) contains the harness, generator, frozen jmax source (without target directories), lockfiles, source hashes, compile patch, corpus metadata, generated endpoint fixture, preflight/validation results, all three raw JSONL runs, system-load observations, build logs, and report-generation script. The [aggregate CSV](benchmark-jmax-vs-parsers-2026-10-15/aggregate.csv) and [aggregate JSON](benchmark-jmax-vs-parsers-2026-10-15/aggregate.json) preserve unrounded calculations. Standard fixture bytes can be recovered from their pinned manifest URLs and verified against SHA-256.

```sh
# Use the frozen source; the live manifest points at the moving workspace.
RUSTFLAGS="-C target-cpu=native" cargo +1.98.1 build --release \
  --manifest-path /tmp/jmax-vs-parsers/frozen/Cargo.toml
JMAX_CHECK_ONLY=1 /tmp/jmax-vs-parsers/frozen/target/release/jmax-vs-parsers \
  /tmp/jmax-vs-parsers/preflight.jsonl
python3 /tmp/jmax-vs-parsers/run_repeated.py
python3 /tmp/jmax-vs-parsers/write_report.py

RUSTFLAGS="-C target-cpu=native" cargo +1.98.1 run --release \
  --manifest-path /tmp/jmax-vs-parsers/source-snapshot/Cargo.toml \
  -p jmax-bench --bin jmax_bench
```

The snapshot workspace manifest retains absolute declarations for sibling projects (`ps-simdutf8`, `nagoya`, `nago-http`, `nago-wss`), but none is active in this measured dependency graph (`local-dependencies.json` is empty). All active local parser dependencies are included in the frozen source archive. The benchmark executable itself was unchanged throughout the three recorded timing processes. Re-running against the live checkout intentionally measures a different source state. The measurements provide no allocation counts, resident-memory peak, streaming/chunked-input comparison, concurrent service throughput, or isolated attribution of individual hot spots.
