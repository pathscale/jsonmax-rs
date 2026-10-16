# Wave 6 sequential benchmark and final report

Completed 2026-10-16T03:33:06.723711+07:00. Workspace `/Users/revenge/code/jsonmax-rs`.

## Executive result

Wave 6's native tape, borrowed input, direct schema execution, fused classifiers and allocator integration are implemented and pass the recorded correctness gates. The 56 matched canonical CLI rows show **2.05× geometric-mean observed throughput for Wave 6/System versus archived Wave 5/System** (per-row range 0.76–3.96×). This is a historical observation under different toolchains, API internals and host contention; it is not an isolated architecture speedup. The matched Wave 6 allocator ablation gives **1.403× RustyAlloc/System** across these rows, range 0.815–2.928×. Allocator effects cannot be subtracted from the historical comparison.

All **28 canonical warm owned cases** (7 workloads × Parse, strict schema Validate, strict Extract, direct Serialize) allocate **0 times / 0 requested bytes per document** after warming. The sparse-subtree strict schema validation/extraction results are remeasured below; retained scratch now covers discarded-subtree state. There is no universal zero-allocation claim. Geometric-mean speed ratios against serde_json are **0.743× for borrowed DOM**, **0.185× for owned typed projection**, and **0.381× for serialization** (below 1 means slower). The ps-pikkr rows measure a narrower query-only task. These semantic differences and all values remain explicit below.

## Post-Stage-6 bottleneck verification

P1/P2/P3 changes are integrated and all requested correctness gates pass. The pre-pass Stage 5 report, raw samples, binaries, tables and source snapshot are preserved in [pre-stage6/](benchmark/pre-stage6/). Current top-level benchmark files and tables describe the rebuilt post-Stage-6 run. [Every matched comparison](benchmark/tables/post-stage6-comparison.md) includes allocation/byte counts and observed throughput ratios; JSON/CSV include both allocators.

| EscapedUnicode first document | Stage 5 | Post-Stage-6 |
| --- | --- | --- |
| Input bytes | 2012 | 2012 |
| Cursor classified bytes | 2012 | 2012 |
| Escape helper blocks | 129 | 32 |
| Escape helper classified bytes | 8154 | 1989 |
| Helper bytes / input | 4.053× | 0.989× |

Helper block classifications fell **75.2%** and helper-classified bytes fell **75.6%**. One additional bounded escape-helper traversal remains; the original cursor pass also remains. This removes the measured 4.05× redundant helper work rather than eliminating every additional pass. Counters are inserted only in the source copy at the current fixed-block discovery site, with scalar tails counted too.

Sparse-large-subtree strict schema **validation and extraction each fell from 4 allocations / 720 requested bytes to 0 / 0 per call**, for warm owned and borrowed execution under both RustyAlloc and System. Cold/new shapes can still grow retained skip storage.

| Query mode | Scratch/result reuse | Calls | Trie builds | Total allocs | Total requested bytes |
| --- | --- | --- | --- | --- | --- |
| Query | False | 32 | 0 | 224 | 32000 |
| Query | True | 32 | 0 | 32 | 256 |
| Strict | False | 32 | 0 | 288 | 48128 |
| Strict | True | 32 | 0 | 0 | 0 |

The plan counter records one build during compilation and zero during either execution API. Cold query execution still allocates traversal/results. On this nested fixture, warmed query-only skipping retains one 8-byte delimiter-stack allocation per call; strict execution with recycled scratch/results allocates zero. Overlapping parent/descendant selectors still use the event traversal and are outside this zero-allocation probe. An initial probe asserted all warmed query modes were allocation-free; it was corrected to record the observed query-only count. The failed assertion and superseding successful run are retained in the command ledger and [initial probe log](benchmark/invariants-initial.log).

DOM storage now reuses per-depth duplicate tables; [measured slot/tape capacities](benchmark/tables/post-stage6-storage.md) show both reductions and cases with increased depth/tape slack. Native serialization now emits trusted unescaped quoted tokens directly. Numeric lexemes already used the trusted direct path before P2 and retain it; this pass does not establish a new number-only optimization. The following cold borrowed DOM and direct native serialization results measure the combined P1/P2/P3 changes, with setup outside timing. They do not isolate individual fast paths.

| Corpus | Operation | Stage 5 MB/s | Post-Stage-6 MB/s | Observed ratio | Allocs before → after | Bytes before → after |
| --- | --- | --- | --- | --- | --- | --- |
| twitter | jmax-dom-borrowed | 274.55 | 418.63 | 1.52× | 44 → 41 | 2.26103e+06 → 815614 |
| twitter | jmax-native-tape-encode-sink | 399.34 | 718.89 | 1.80× | 20 → 2 | 1.11839e+06 → 631595 |
| citm_catalog | jmax-dom-borrowed | 366.08 | 530.91 | 1.45× | 33 → 19 | 4.19424e+06 → 1.75516e+06 |
| citm_catalog | jmax-native-tape-encode-sink | 591.26 | 909.85 | 1.54× | 19 → 2 | 1.04866e+06 → 1.72727e+06 |
| canada | jmax-dom-borrowed | 161.98 | 245.28 | 1.51× | 21 → 8 | 8.38898e+06 → 6.29291e+06 |
| canada | jmax-native-tape-encode-sink | 355.60 | 400.93 | 1.13× | 21 → 2 | 7.07796e+06 → 2.25111e+06 |
| github_events | jmax-dom-borrowed | 370.70 | 645.77 | 1.74× | 30 → 22 | 214824 → 92176 |
| github_events | jmax-native-tape-encode-sink | 434.97 | 954.55 | 2.19× | 16 → 2 | 131160 → 65180 |
| apache_builds | jmax-dom-borrowed | 369.35 | 616.29 | 1.67× | 31 → 14 | 525064 → 130416 |
| apache_builds | jmax-native-tape-encode-sink | 352.64 | 758.59 | 2.15× | 15 → 2 | 296690 → 127299 |
| endpoint_batch | jmax-dom-borrowed | 143.81 | 274.56 | 1.91× | 37 → 21 | 1.08321e+06 → 344790 |
| endpoint_batch | jmax-native-tape-encode-sink | 166.86 | 327.33 | 1.96× | 17 → 2 | 286805 → 102542 |

The targeted unescaped-string rows below preserve the corresponding escaped workload as well, so allocation reductions are not treated as a universal timing gain. Native number emission predates this pass.

| Workload | Serializer regime | Before MB/s | After MB/s | Observed ratio | Allocs before → after | Bytes before → after |
| --- | --- | --- | --- | --- | --- | --- |
| EscapedUnicode | Serialize-Staged-Sink/Cold-Owned | 823.76 | 729.68 | 0.89× | 5 → 3 | 6741.56 → 3778.28 |
| EscapedUnicode | Serialize-Native-Direct/Warm-Owned | 841.64 | 798.18 | 0.95× | 0 → 0 | 0 → 0 |
| long-unescaped-ascii | Serialize-Staged-Sink/Cold-Owned | 3800.95 | 9075.40 | 2.39× | 5 → 3 | 8261 → 4120 |
| long-unescaped-ascii | Serialize-Native-Direct/Warm-Owned | 4750.15 | 17010.68 | 3.58× | 0 → 0 | 0 → 0 |
| long-unescaped-unicode | Serialize-Staged-Sink/Cold-Owned | 4805.24 | 24024.74 | 5.00× | 5 → 3 | 30789 → 15384 |
| long-unescaped-unicode | Serialize-Native-Direct/Warm-Owned | 5547.02 | 33790.16 | 6.09× | 0 → 0 | 0 → 0 |

Across 56 matched canonical CLI rows, observed post-Stage-6/Stage-5 geometric-mean throughput is **1.164× for RustyAlloc** and **0.932× for System**. Runs occur at different times and host loads; these are combined-change observations, not controlled attribution. All 21 generated corpus hashes match the preserved run. Full hashes confirm all four protected sibling repositories unchanged.

## Implemented architecture and integration

| Owner | Delivered behavior |
| --- | --- |
| C0 | Frozen ABI masks, tape and ownership contracts; checked owner subviews/capabilities; integrated callers and verification repairs. |
| S1 | Four-load NEON / two-load AVX2 fused classification, direct full blocks, word backslash parity, independently gated PMULL/PCLMUL, bulk UTF-8 and escape helpers. |
| S2 | Persistent BlockCursor with cached dispatch, one checked grammar transition, borrowed core, bulk strings, transactional escaped output, separate strict/query/live-owner skips. |
| S3 | Private immutable `TapeDocument` / `BorrowedDocument`, variable-width `Vec<u64>` records, parent/subtree navigation, explicit legacy conversion. |
| S4 | Direct schema consumer, prevalidated dispatch, explicit duplicate owner, proof-only validation and strict capture; query result types carry no validation proof. |
| S5 | Native tape/borrowed encoding, cached block escape masks and maximal slice writes; direct streaming separated from staged publication. |
| S6 | Owned/borrowed facade and scratch APIs, native DOM, strict/query extraction, optional allocator export. Derived borrowed typed decoding uses `JsonDecodeBorrowed` over the original slice; caller-compiled scratch variants retain VM storage. |
| S7 | Real nagoya sync primitives for bounded job/completion rings, admission/cancellation/order, private canonical validator capability, owner-preserving transport seams. |
| S8 | Durable ps-pikkr fork with provenance, crate-name/cfg repairs, NEON+PMULL, portable and x86-gated paths, bounded tails and quote carry. |
| L1 | RustyAlloc in all four executable roots; counted System feature for benchmark ablation; no library global allocator. |
| L2 / Stage 5 | Native and external harnesses; two seven-workload generators plus seven stress inputs, borrowed/scratch/query/syntax modes, per-operation allocation accounting, prepared simd kernels, reproducible snapshots. |

Details: [contracts](CONTRACTS.md), [S1](S1.md), [S2](S2.md), [S3](S3.md), [S4](S4.md), [S5](S5.md), [S6](S6.md), [S7](S7.md), [S8](S8.md), [L1](L1.md), [L2](L2.md). P1/P2/P3 handoff tests now execute in the post-Stage-6 correctness ledger; older stage results remain preserved.

## Verification and correctness gates

[VERIFICATION.md](verification/VERIFICATION.md) is the authoritative gate ledger: post-Stage-6 native workspace **148 passed, 0 failed** (145 unit/integration + 3 compile-fail doctests); targeted native feature run **35 passed, 0 failed, 1 ignored**. Earlier Stage 4 ps-pikkr native **29 passed**, portable **27 passed** remain historical verification, not rerun counts. Counts overlap and must not be summed. Workspace/all-target/bin checks, jsonmax default/no-default alloc-only/rusty-alloc configurations, transport no-default/all-feature checks and the standalone single-global-allocator consumer pass. x86 runtime is **UNEXECUTED**; S8's 1.98.0 cross-check is not native x86 execution or a 1.98.1 x86 check. No physical ARM machine without PMULL was available. Broad `cargo fmt --all -- --check` retains pre-existing differences; scoped integration formatting passes.

Post-Stage-6 verification rebuilt both harnesses in both allocator configurations before any timing. Both new supplemental preflights pass all exposed operations; external preflights pass **247 operations per allocator** plus 18 conformance probes and semantic/typed equality. Native harness compilation and `cargo +1.98.1 test -p jmax-bench --offline` pass (three zero-test targets; these are not extra correctness tests). All timing processes exit 0. The new allocation, pointer, nonempty selection, scratch growth and reactive lease assertions pass. Sonic LazyValue's invalid-UTF-8 panic remains separately recorded in conformance output; it is not counted as an ordinary rejection. Root-array GitHub schema projection and pikkr array-index capability rows are explicitly N/A where unsupported.

## Build, environment, sequencing and measurement boundaries

Apple M4 Max, 16 logical CPUs, 64 GiB RAM, macOS 26.5, AArch64. rustc/cargo **1.98.1**, LLVM 22.1.8; native cfg includes `neon` and `aes` (PMULL). Every Cargo invocation is offline; release builds and native tests use `RUSTFLAGS="-C target-cpu=native"`, while the requested plain checks omit it. The exact requested `export PATH="/Users/revenge/code/ps-pikkr/.tooling/bin:/Users/revenge/.cargo/bin:$PATH"` and pinned RUSTC/RUSTDOC are applied. Builds and tests run with one Cargo job and one Rust test thread. No global toolchain settings changed. Exact commands, environment, source/manifest/binary/corpus hashes and process observations are archived in [benchmark/](benchmark/).

The task's jobs run strictly sequentially: one child build/test/benchmark at a time, all timed release builds/preflights and the bench test completed before timing, with no task-owned Cargo/rustc build during timing. All four timed harness variants are invoked from saved allocator-specific binaries; no `cargo run` appears in timing. Unrelated user processes were left running. Across 66 timing observations, one-minute load was **12.62–45.31**, median **26.22**. Other processes individually at ≥5% CPU summed to median **902.2%**, maximum **1400.7%** (100%=one core). Unrelated rustc reached **898.0%**; unrelated benchmark applications were visible in 0 observations. Exclusive access applies to this task's jobs, not host isolation. `pmset -g therm` reports no recorded thermal/performance/CPU-power warning; direct temperature/frequency telemetry is unavailable, so thermal headroom is unknown. No CPU affinity or power control was applied.

- Canonical CLI: unchanged follow-up generators, seed 5354007379338153539, 128 docs, items=32, 3 warmups, 20 pass samples in each of 3 fresh processes per allocator. Pooled p50/p95 divide pass times by 128; MB/s uses total bytes/total elapsed time. CLI observation/digest work is timed. Each raw pass is saved, including digests. “Cold” means fresh result/scratch allocation, with prepared immutable input/schema owners, not cold CPU caches.
- Supplemental operation-only loop: the seven follow-up workloads, seven original generators (including Unicode), and seven stress cases at scale=32; 128 docs, 3 warmups, 20 measured passes, one process per allocator. Results are black-boxed and dropped/recycled inside timing. Schema compilation, input generation, serializer input DOMs, preflight comparisons and output I/O are untimed. Its lighter observation boundary differs from CLI; do not splice its values into historical CLI ratios. Stress repetitions share one immutable payload owner.
- External suite: same six byte-identical historical corpora, 3 fresh processes ×61 calibrated samples per allocator. Minimum 50ms/5 operations of warmup per job, sample target 2ms, deterministic shuffled job order within each corpus. Each pooled row has 183 batch averages. Prepared simd rows use 8 fresh mutable buffers/sample refreshed outside each timer, 3 warmup samples; parse/result allocation and destruction remain timed. These are prepared-input kernels, with different cache/working-set conditions, not end-to-end timings.
- Allocations and requested bytes are separate untimed operation passes after capacities warm (128 docs supplemental; 5 repetitions per external job). Realloc counts as one call and charges the full new requested size. Deallocations do not add requested bytes. Counters remain active in timings for both allocator builds; archived harness instrumentation differs. Their atomic overhead can disproportionately affect allocation-heavy cases; no uninstrumented timing rerun was performed. Allocator metadata, TLS pools, malloc usable sizes and peak RSS are not measured.
- **p95 is a percentile of corpus/batch averages, not individual-request tail latency.** All rates are decimal MB/s. Serialization rates use original JSON input bytes, not output bytes; escaped-string-array rates use decoded source string/key bytes. Query rates are input-normalized selection rates, not evidence that every byte was fully validated.

## Historical A/B/C/D comparison

A = archived Wave 5/System; B = Wave 5/RustyAlloc **not measured**; C = Wave 6/System; D = Wave 6/RustyAlloc. A's seven-workload report used rustc 1.99.0 and significant contention; its six-corpus report used 1.98.1 and different profiles/earlier source. No new Wave 5 rebuild was attempted: the preserved `run-baseline.sh` points at moving APIs and a different simd-json version. Native payload pass sizes match A exactly for all 56 rows, and all seven generated follow-up JSONL SHA-256 hashes match the archived Wave 5 corpus manifest byte-for-byte. The [generated corpus manifest](benchmark/generated-corpus-manifest.json) also saves per-document hashes for both seven-workload generators and the seven stress cases. Old schema Validate captured slots; Wave 6 Validate is proof-only. Native DOM/layout/digest and encoder internals also changed. These changes are disclosed rather than treated as allocator-only differences. B remains N/A, not an inferred estimate.

Each cell below is **MB/s; p50 / p95 ns/doc**. Complete machine-readable values and per-process variability: [before-after CSV](benchmark/tables/before-after.csv), [native CLI JSON](benchmark/tables/native-cli.json).

| Workload | Mode | Regime | A: Wave 5 System | C: Wave 6 System | D: Wave 6 RustyAlloc |
| --- | --- | --- | --- | --- | --- |
| NagoyaAuth | Parse | Cold-Alloc | 47.84; 1663.7 / 2195.6 | 118.95; 612.6 / 1146.2 | 100.24; 541.3 / 933.6 |
| NagoyaAuth | Parse | Warm-Scratch | 57.51; 1124.0 / 2436.8 | 162.70; 501.6 / 516.3 | 148.38; 509.1 / 837.9 |
| NagoyaAuth | Validate | Cold-Alloc | 38.03; 2101.6 / 2609.4 | 133.79; 609.7 / 779.3 | 118.63; 599.3 / 1218.4 |
| NagoyaAuth | Validate | Warm-Scratch | 53.47; 1566.7 / 1572.6 | 141.39; 577.1 / 592.8 | 139.94; 572.6 / 886.1 |
| NagoyaAuth | Extract | Cold-Alloc | 34.21; 2200.5 / 3814.8 | 115.79; 701.8 / 899.4 | 123.30; 672.9 / 702.1 |
| NagoyaAuth | Extract | Warm-Scratch | 52.65; 1477.9 / 2619.5 | 88.71; 667.0 / 1598.6 | 105.00; 622.1 / 1183.3 |
| NagoyaAuth | Serialize | Cold-Alloc | 117.56; 715.5 / 721.4 | 208.43; 338.9 / 804.4 | 243.21; 293.9 / 518.2 |
| NagoyaAuth | Serialize | Warm-Scratch | 173.37; 483.7 / 487.0 | 278.49; 269.9 / 363.6 | 308.71; 276.4 / 283.5 |
| ExchangeOrder | Parse | Cold-Alloc | 44.02; 1925.1 / 1961.9 | 94.58; 665.0 / 1546.5 | 137.58; 575.5 / 1078.1 |
| ExchangeOrder | Parse | Warm-Scratch | 75.76; 1120.8 / 1124.7 | 139.60; 531.6 / 924.8 | 156.54; 528.3 / 550.5 |
| ExchangeOrder | Validate | Cold-Alloc | 36.28; 2335.3 / 2376.6 | 107.98; 691.4 / 1483.7 | 125.74; 677.1 / 690.1 |
| ExchangeOrder | Validate | Warm-Scratch | 48.23; 1761.1 / 1766.3 | 65.49; 1278.0 / 1696.6 | 129.77; 662.1 / 668.6 |
| ExchangeOrder | Extract | Cold-Alloc | 35.21; 2415.4 / 2426.8 | 80.55; 857.4 / 1677.1 | 108.57; 759.1 / 988.9 |
| ExchangeOrder | Extract | Warm-Scratch | 49.71; 1654.6 / 1785.5 | 112.50; 763.3 / 779.3 | 122.03; 710.3 / 716.5 |
| ExchangeOrder | Serialize | Cold-Alloc | 107.05; 792.6 / 800.1 | 200.65; 414.7 / 448.2 | 251.78; 340.2 / 353.2 |
| ExchangeOrder | Serialize | Warm-Scratch | 149.50; 568.4 / 570.0 | 226.22; 350.6 / 361.7 | 262.40; 325.5 / 335.3 |
| RpcEnvelope | Parse | Cold-Alloc | 43.90; 2308.9 / 2501.6 | 92.04; 974.0 / 1738.6 | 142.82; 742.8 / 751.3 |
| RpcEnvelope | Parse | Warm-Scratch | 63.75; 1633.5 / 1638.3 | 129.34; 726.2 / 1158.5 | 154.36; 684.9 / 693.0 |
| RpcEnvelope | Validate | Cold-Alloc | 32.03; 3090.5 / 3621.4 | 84.74; 994.5 / 1912.1 | 122.56; 852.9 / 888.7 |
| RpcEnvelope | Validate | Warm-Scratch | 46.29; 2283.5 / 2314.8 | 122.95; 825.2 / 893.2 | 126.63; 829.8 / 861.3 |
| RpcEnvelope | Extract | Cold-Alloc | 34.73; 2914.7 / 3041.7 | 89.79; 1019.2 / 1955.1 | 113.02; 929.4 / 957.7 |
| RpcEnvelope | Extract | Warm-Scratch | 36.44; 2340.8 / 4662.8 | 113.21; 938.8 / 950.5 | 121.47; 871.4 / 884.4 |
| RpcEnvelope | Serialize | Cold-Alloc | 101.32; 1028.3 / 1033.9 | 191.51; 541.0 / 652.7 | 235.51; 440.8 / 454.4 |
| RpcEnvelope | Serialize | Warm-Scratch | 87.17; 1012.4 / 1927.1 | 188.85; 471.0 / 803.1 | 244.15; 426.8 / 440.4 |
| LargeCatalog | Parse | Cold-Alloc | 59.14; 578212.2 / 633959.0 | 69.74; 393445.0 / 899893.9 | 159.85; 193120.8 / 351072.3 |
| LargeCatalog | Parse | Warm-Scratch | 71.57; 478702.1 / 518883.8 | 93.48; 323307.6 / 698313.1 | 140.79; 201619.5 / 513340.8 |
| LargeCatalog | Validate | Cold-Alloc | 33.78; 1048394.9 / 1312421.6 | 46.67; 675152.7 / 1268971.0 | 123.64; 244800.5 / 450469.1 |
| LargeCatalog | Validate | Warm-Scratch | 20.53; 1616407.2 / 2788929.7 | 70.95; 443342.8 / 837922.2 | 121.32; 233901.0 / 552629.2 |
| LargeCatalog | Extract | Cold-Alloc | 19.18; 1548263.3 / 3222377.6 | 64.15; 444306.0 / 1095668.9 | 130.95; 233134.1 / 476094.4 |
| LargeCatalog | Extract | Warm-Scratch | 22.99; 1456694.3 / 2029194.0 | 85.84; 331215.5 / 727564.8 | 118.09; 242198.2 / 553277.0 |
| LargeCatalog | Serialize | Cold-Alloc | 114.69; 281719.7 / 340639.3 | 168.10; 175752.3 / 369145.2 | 260.57; 129758.8 / 150538.1 |
| LargeCatalog | Serialize | Warm-Scratch | 72.88; 339629.2 / 895857.1 | 177.55; 174509.4 / 329034.5 | 215.07; 118808.6 / 434718.4 |
| VectorCoordinates | Parse | Cold-Alloc | 61.05; 14082.7 / 15206.4 | 104.74; 7050.1 / 11963.5 | 150.20; 5724.9 / 6388.4 |
| VectorCoordinates | Parse | Warm-Scratch | 54.08; 14930.3 / 23457.0 | 102.96; 6639.0 / 11816.7 | 152.53; 5687.2 / 6148.4 |
| VectorCoordinates | Validate | Cold-Alloc | 49.68; 17417.0 / 19631.8 | 70.61; 8492.5 / 18093.4 | 121.13; 7143.9 / 7683.9 |
| VectorCoordinates | Validate | Warm-Scratch | 44.33; 18385.4 / 25564.8 | 53.83; 8513.3 / 38008.8 | 124.80; 6905.6 / 8043.0 |
| VectorCoordinates | Extract | Cold-Alloc | 45.08; 18240.6 / 25819.3 | 50.36; 10181.3 / 22849.9 | 122.46; 7196.6 / 7504.2 |
| VectorCoordinates | Extract | Warm-Scratch | 38.82; 18429.7 / 38033.5 | 58.23; 8251.0 / 34608.4 | 121.54; 7088.2 / 8250.0 |
| VectorCoordinates | Serialize | Cold-Alloc | 135.69; 6072.9 / 6593.1 | 127.31; 3779.0 / 10712.6 | 110.31; 2960.6 / 24088.5 |
| VectorCoordinates | Serialize | Warm-Scratch | 79.19; 8856.4 / 19823.2 | 205.48; 3690.1 / 7839.5 | 167.39; 2958.3 / 14902.7 |
| DeepNesting | Parse | Cold-Alloc | 11.30; 12057.6 / 17146.8 | 40.12; 2887.7 / 6140.3 | 49.77; 1766.9 / 6614.3 |
| DeepNesting | Parse | Warm-Scratch | 27.80; 4430.3 / 8347.3 | 90.90; 1434.6 / 2488.0 | 115.64; 1223.3 / 1370.4 |
| DeepNesting | Validate | Cold-Alloc | 9.88; 12948.6 / 22320.0 | 16.36; 4563.2 / 16937.8 | 47.92; 2902.7 / 3292.0 |
| DeepNesting | Validate | Warm-Scratch | 16.02; 6640.3 / 16447.3 | 56.89; 2476.9 / 2545.2 | 66.20; 2091.8 / 2345.4 |
| DeepNesting | Extract | Cold-Alloc | 8.49; 13684.9 / 29811.2 | 16.18; 4699.5 / 10586.3 | 46.65; 2988.3 / 3346.0 |
| DeepNesting | Extract | Warm-Scratch | 11.84; 9349.6 / 17621.1 | 23.33; 2518.6 / 6354.2 | 66.82; 2091.5 / 2340.5 |
| DeepNesting | Serialize | Cold-Alloc | 53.73; 2657.2 / 2691.1 | 40.66; 1750.3 / 5212.2 | 95.81; 1398.8 / 1640.3 |
| DeepNesting | Serialize | Warm-Scratch | 41.02; 3292.0 / 4804.4 | 78.67; 1742.8 / 2890.6 | 90.11; 1397.5 / 2351.6 |
| EscapedUnicode | Parse | Cold-Alloc | 99.13; 17000.7 / 31295.2 | 160.64; 11596.4 / 17293.0 | 213.19; 9256.8 / 10671.9 |
| EscapedUnicode | Parse | Warm-Scratch | 97.08; 16579.4 / 28517.2 | 99.20; 11226.2 / 54291.7 | 131.38; 9002.6 / 80557.6 |
| EscapedUnicode | Validate | Cold-Alloc | 76.01; 23818.4 / 36835.3 | 244.27; 6688.8 / 17000.3 | 293.39; 5083.0 / 15869.5 |
| EscapedUnicode | Validate | Warm-Scratch | 78.06; 24908.5 / 30635.1 | 198.44; 6276.0 / 28298.8 | 357.13; 5049.5 / 10163.1 |
| EscapedUnicode | Extract | Cold-Alloc | 43.17; 31918.0 / 115208.7 | 170.88; 7140.3 / 15746.4 | 366.22; 5301.8 / 6574.2 |
| EscapedUnicode | Extract | Warm-Scratch | 84.45; 22346.4 / 32007.8 | 273.08; 6080.7 / 11389.6 | 337.80; 4943.7 / 11209.3 |
| EscapedUnicode | Serialize | Cold-Alloc | 400.25; 3883.5 / 8783.9 | 766.85; 2617.8 / 2828.8 | 758.63; 2288.7 / 3939.5 |
| EscapedUnicode | Serialize | Warm-Scratch | 551.89; 3652.0 / 3755.2 | 727.97; 2738.9 / 2779.0 | 792.13; 2270.5 / 3959.3 |

Borrowed DOM, warm borrowed DOM, strict syntax-only, schema Validate, strict/query Extract and staged/direct serialization have distinct rows in the [complete native-regime table](benchmark/tables/native-regimes.md) ([CSV](benchmark/tables/native-regimes.csv), [JSON](benchmark/tables/native-regimes.json)). Every measured row has MB/s and p50/p95. Historical borrowed/query rows without an equivalent operation are N/A; no values were invented. The [six-corpus before/after table](benchmark/tables/historical-six-corpus.md) additionally compares the earlier scan/DOM/typed/serialize results, with copy/API transitions labeled.

### Supplemental canonical regimes

| Workload | Mode | Regime | C: System | D: RustyAlloc | D allocs / bytes per op |
| --- | --- | --- | --- | --- | --- |
| NagoyaAuth | Parse | Cold-Borrowed | 164.05; 511.4 / 532.2 | 253.64; 326.8 / 337.2 | 4 / 720 |
| NagoyaAuth | Parse | Warm-Borrowed | 114.06; 666.3 / 1050.1 | 284.01; 284.5 / 383.1 | 0 / 0 |
| NagoyaAuth | Validate-Syntax | Warm-Borrowed | 76.14; 1241.2 / 1383.8 | 150.83; 557.6 / 562.5 | 3 / 512 |
| NagoyaAuth | Validate-Schema | Warm-Borrowed | 149.06; 564.5 / 567.1 | 184.40; 454.1 / 456.4 | 0 / 0 |
| NagoyaAuth | Extract-Strict | Warm-Borrowed | 134.39; 607.8 / 654.0 | 172.98; 485.7 / 489.9 | 0 / 0 |
| NagoyaAuth | Extract-Query-Schema | Cold-Borrowed | 97.15; 864.9 / 871.1 | 144.27; 583.3 / 585.3 | 8 / 420 |
| NagoyaAuth | Extract-Query-Path | Cold-Borrowed | 145.89; 576.8 / 578.8 | 226.91; 369.8 / 373.0 | 5 / 736 |
| ExchangeOrder | Parse | Cold-Borrowed | 142.05; 599.0 / 608.4 | 246.33; 345.4 / 350.9 | 5 / 976 |
| ExchangeOrder | Parse | Warm-Borrowed | 206.59; 412.1 / 414.4 | 269.67; 301.8 / 357.1 | 0 / 0 |
| ExchangeOrder | Validate-Syntax | Warm-Borrowed | 89.45; 947.6 / 957.0 | 126.89; 659.2 / 698.9 | 4 / 1024 |
| ExchangeOrder | Validate-Schema | Warm-Borrowed | 119.76; 709.3 / 715.2 | 162.72; 522.5 / 524.7 | 0 / 0 |
| ExchangeOrder | Extract-Strict | Warm-Borrowed | 112.07; 757.8 / 762.0 | 150.75; 560.5 / 584.0 | 0 / 0 |
| ExchangeOrder | Extract-Query-Schema | Cold-Borrowed | 87.82; 964.5 / 981.1 | 126.23; 673.8 / 675.1 | 7 / 455 |
| ExchangeOrder | Extract-Query-Path | Cold-Borrowed | 127.83; 663.1 / 670.9 | 201.76; 420.9 / 423.5 | 6 / 1248 |
| RpcEnvelope | Parse | Cold-Borrowed | 127.03; 819.7 / 829.8 | 218.94; 469.1 / 490.6 | 6 / 1360 |
| RpcEnvelope | Parse | Warm-Borrowed | 183.08; 567.7 / 574.2 | 238.46; 430.7 / 491.2 | 0 / 0 |
| RpcEnvelope | Validate-Syntax | Warm-Borrowed | 13.77; 1164.7 / 1186.9 | 123.72; 837.9 / 858.4 | 4 / 768 |
| RpcEnvelope | Validate-Schema | Warm-Borrowed | 57.21; 1625.3 / 1866.5 | 158.74; 655.9 / 658.9 | 0 / 0 |
| RpcEnvelope | Extract-Strict | Warm-Borrowed | 121.17; 859.7 / 862.0 | 149.92; 686.5 / 705.1 | 0 / 0 |
| RpcEnvelope | Extract-Query-Schema | Cold-Borrowed | 87.74; 1199.5 / 1203.5 | 126.54; 825.5 / 838.5 | 9 / 757 |
| RpcEnvelope | Extract-Query-Path | Cold-Borrowed | 184.58; 564.1 / 567.7 | 275.48; 376.0 / 378.9 | 6 / 744 |
| LargeCatalog | Parse | Cold-Borrowed | 182.84; 175385.1 / 236908.9 | 270.35; 129089.2 / 132643.2 | 6 / 105064 |
| LargeCatalog | Parse | Warm-Borrowed | 162.06; 162957.7 / 449125.0 | 272.54; 126601.6 / 130239.6 | 0 / 0 |
| LargeCatalog | Validate-Syntax | Warm-Borrowed | 79.98; 379112.6 / 754257.5 | 128.81; 268325.2 / 277846.0 | 4 / 768 |
| LargeCatalog | Validate-Schema | Warm-Borrowed | 90.33; 373721.0 / 520452.1 | 113.00; 255205.1 / 567319.3 | 0 / 0 |
| LargeCatalog | Extract-Strict | Warm-Borrowed | 69.78; 478038.7 / 729710.9 | 140.61; 238967.8 / 269730.8 | 0 / 0 |
| LargeCatalog | Extract-Query-Schema | Cold-Borrowed | 63.27; 351458.3 / 1316778.3 | 134.72; 256025.1 / 297195.6 | 10 / 1158 |
| LargeCatalog | Extract-Query-Path | Cold-Borrowed | 471.54; 30227.2 / 189549.8 | 1015.31; 31035.8 / 48968.4 | 6 / 744 |
| VectorCoordinates | Parse | Cold-Borrowed | 19.04; 5778.0 / 74590.8 | 178.84; 4922.2 / 4947.6 | 5 / 3232 |
| VectorCoordinates | Parse | Warm-Borrowed | 89.79; 5740.2 / 18991.5 | 181.73; 4835.0 / 4905.3 | 0 / 0 |
| VectorCoordinates | Validate-Syntax | Warm-Borrowed | 86.52; 8593.4 / 14772.1 | 110.84; 7946.3 / 7993.5 | 3 / 512 |
| VectorCoordinates | Validate-Schema | Warm-Borrowed | 20.46; 7795.2 / 143268.6 | 109.64; 7857.4 / 8550.8 | 0 / 0 |
| VectorCoordinates | Extract-Strict | Warm-Borrowed | 110.57; 7966.8 / 7988.3 | 109.11; 7599.0 / 10903.0 | 0 / 0 |
| VectorCoordinates | Extract-Query-Schema | Cold-Borrowed | 110.95; 4581.1 / 18561.2 | 243.20; 3622.4 / 3644.5 | 7 / 571 |
| VectorCoordinates | Extract-Query-Path | Cold-Borrowed | 70.34; 8632.2 / 34736.7 | 110.98; 7942.7 / 7992.2 | 7 / 992 |
| DeepNesting | Parse | Cold-Borrowed | 33.26; 3280.6 / 4766.0 | 82.73; 1696.0 / 1924.5 | 31 / 15016 |
| DeepNesting | Parse | Warm-Borrowed | 71.25; 1365.9 / 2725.3 | 133.44; 1070.3 / 1089.5 | 0 / 0 |
| DeepNesting | Validate-Syntax | Warm-Borrowed | 7.01; 4199.5 / 53965.8 | 42.49; 3194.0 / 4511.1 | 27 / 12288 |
| DeepNesting | Validate-Schema | Warm-Borrowed | 48.40; 2329.4 / 4579.8 | 61.77; 2316.1 / 2322.9 | 0 / 0 |
| DeepNesting | Extract-Strict | Warm-Borrowed | 4.01; 8849.6 / 140266.9 | 61.91; 2307.6 / 2315.1 | 0 / 0 |
| DeepNesting | Extract-Query-Schema | Cold-Borrowed | 67.01; 1292.0 / 7085.0 | 230.88; 619.1 / 622.7 | 10 / 658 |
| DeepNesting | Extract-Query-Path | Cold-Borrowed | 27.79; 3820.0 / 15460.6 | 45.61; 3134.8 / 3140.6 | 29 / 8672 |
| EscapedUnicode | Parse | Cold-Borrowed | 330.79; 6256.5 / 6311.9 | 356.51; 5583.3 / 6002.0 | 13 / 6688 |
| EscapedUnicode | Parse | Warm-Borrowed | 80.18; 7758.1 / 30216.1 | 375.67; 5360.4 / 5385.7 | 0 / 0 |
| EscapedUnicode | Validate-Syntax | Warm-Borrowed | 272.76; 5258.8 / 10144.2 | 413.48; 4865.2 / 4889.6 | 3 / 512 |
| EscapedUnicode | Validate-Schema | Warm-Borrowed | 222.77; 6101.6 / 15106.4 | 363.34; 5533.2 / 5566.7 | 0 / 0 |
| EscapedUnicode | Extract-Strict | Warm-Borrowed | 340.91; 6072.3 / 6083.7 | 357.74; 5620.4 / 5642.3 | 0 / 0 |
| EscapedUnicode | Extract-Query-Schema | Cold-Borrowed | 139.07; 7131.8 / 13877.6 | 345.13; 5828.5 / 5861.7 | 16 / 4387 |
| EscapedUnicode | Extract-Query-Path | Cold-Borrowed | 934.71; 1819.0 / 3612.6 | 1178.03; 1708.0 / 1714.8 | 5 / 736 |

### Stress cases

These valid deterministic inputs cover long ASCII/Unicode, dense escapes, long backslashes, 1,024-field objects, reordered fields and a sparse selected field over an 89,045-byte document. Full regimes and allocation rows are in [stress.md](benchmark/tables/stress.md). The table below selects cold borrowed Parse, warm strict schema validation, and query schema extraction; A/B are N/A.

| Stress | Operation | C: System | D: RustyAlloc | D allocs / bytes per op |
| --- | --- | --- | --- | --- |
| long-unescaped-ascii | Parse | 161.76; 1877.3 / 10598.0 | 1228.05; 1672.2 / 1683.9 | 4 / 2648 |
| long-unescaped-ascii | Validate-Schema | 545.17; 1888.0 / 12343.8 | 1008.25; 1755.2 / 3429.0 | 0 / 0 |
| long-unescaped-ascii | Extract-Query-Schema | 1043.43; 1939.8 / 2091.8 | 1105.44; 1857.7 / 1874.7 | 7 / 298 |
| long-unescaped-unicode | Parse | 66.43; 41097.0 / 322236.7 | 1151.43; 6659.8 / 6726.6 | 4 / 8280 |
| long-unescaped-unicode | Validate-Schema | 208.35; 7300.1 / 189140.0 | 1002.60; 6809.9 / 10253.3 | 0 / 0 |
| long-unescaped-unicode | Extract-Query-Schema | 1021.30; 7525.1 / 7551.1 | 1119.35; 6864.9 / 6890.3 | 7 / 298 |
| escape-dense | Parse | 85.85; 42662.8 / 306853.5 | 253.40; 22713.5 / 45711.3 | 14 / 15952 |
| escape-dense | Validate-Schema | 129.72; 25466.1 / 173802.7 | 275.72; 24908.5 / 29747.1 | 0 / 0 |
| escape-dense | Extract-Query-Schema | 181.26; 26245.1 / 80481.1 | 312.07; 22442.4 / 24543.3 | 17 / 8482 |
| long-backslash-runs | Parse | 116.29; 22719.4 / 101742.2 | 267.12; 14174.8 / 18429.0 | 13 / 8784 |
| long-backslash-runs | Validate-Schema | 126.21; 14955.1 / 73385.4 | 265.82; 13869.5 / 21078.8 | 0 / 0 |
| long-backslash-runs | Extract-Query-Schema | 197.07; 15717.8 / 23276.7 | 294.48; 14140.3 / 14164.1 | 16 / 4386 |
| wide-object | Parse | 102.39; 136684.6 / 237555.3 | 199.46; 80211.9 / 87141.6 | 12 / 146752 |
| wide-object | Validate-Schema | 69.53; 200656.2 / 456682.9 | 114.52; 133423.8 / 183505.5 | 0 / 0 |
| wide-object | Extract-Query-Schema | 54.30; 179965.2 / 827993.8 | 149.07; 107432.6 / 121452.5 | 16 / 131114 |
| reordered-fields | Parse | 75.20; 769.5 / 780.9 | 91.07; 619.1 / 641.6 | 6 / 1360 |
| reordered-fields | Validate-Schema | 30.17; 1979.2 / 2427.7 | 45.48; 1085.6 / 1892.6 | 0 / 0 |
| reordered-fields | Extract-Query-Schema | 61.59; 939.5 / 946.6 | 69.50; 833.7 / 838.5 | 10 / 570 |
| sparse-large-subtree | Parse | 171.28; 504684.2 / 619859.7 | 112.32; 722456.7 / 1113418.3 | 8 / 624640 |
| sparse-large-subtree | Validate-Schema | 65.17; 1194515.0 / 2228999.4 | 43.15; 2080694.0 / 2599601.9 | 0 / 0 |
| sparse-large-subtree | Extract-Query-Schema | 363.70; 84761.1 / 780926.1 | 777.01; 102000.7 / 180329.4 | 12 / 1074 |

## Head-to-head competitors

All implementations in each process use the same counting allocator backend. The main table below is **RustyAlloc**; the complete [competitor table](benchmark/tables/competitors.md) ([CSV](benchmark/tables/competitors.csv), [JSON](benchmark/tables/competitors.json)) includes System, all scan/typed/DOM/string jobs and prepared simd kernels. Main dependency versions: serde/serde_derive 1.0.229, serde_json 1.0.154, simd-json 0.18.1, sonic-rs 0.5.10, ps-pikkr 0.16.1; json-rust/miniserde are retained as additional rows. The external profile is opt-level=3, thin LTO, one codegen unit; native CLI uses the ordinary workspace release profile.

Strict jmax schema checks, syntax-only scans, DOM construction, typed projections and query selection are different workloads. serde IgnoredAny, simd tape and sonic LazyValue have different Unicode/surrogate/error policies, as recorded by the conformance matrix. jmax's strict APIs check syntax/UTF-8/escapes/numeric/duplicates/limits; schema APIs additionally enforce the compiled shape. No other parser row is labeled equivalent strict schema validation merely because it accepts these valid fixtures.

`simd-json ... +copy` includes cloning immutable input to mutable storage inside timing. `... prepared` excludes input refresh and input-buffer destruction; result allocation/destruction remains included. jmax borrowed DOM excludes copying input, while `jmax-dom+copy` and typed adapters include their explicit copy. DOMs differ in number representation: jmax retains spelling with strict numeric-policy checks, while materialized competitors generally convert numbers. Typed rows produce the same owned scalar projections and strings; GitHub's jmax adapter builds a native borrowed DOM then extracts each event object, unlike competitors' direct array deserialization.

ps-pikkr is a **query-only raw-value selector**, with no full validation, schema proof, DOM or serialization. Trained setup is outside measurement; untrained uses `usize::MAX`. Captured values are compared against the reference. It retains bitmap/per-depth indexes and learned state. Unsupported DOM Parse / Full Validate / Serialize are **N/A on all six corpora**, and GitHub's array-index query is **N/A**. Success does not establish validity of unselected JSON, UTF-8, escapes, numbers, duplicate policy or trailing bytes. Object-order deoptimization correctness is tested in S8; the six-corpus timing reuses each fixed corpus and does not measure a reordered-stream deoptimization distribution. Sonic's partial lookup sometimes returns a parent object (search_metadata/venueNames) while the jmax/pikkr path returns its leaf; those partial-query jobs are separately labeled in the full table.

| Corpus | Operation | Implementation | MB/s; p50 / p95 ns/doc | allocs/op |
| --- | --- | --- | --- | --- |
| twitter | DOM Parse | jmax-dom-borrowed | 418.63; 1508542.0 / 5906125.0 | 41.0 |
| twitter | DOM Parse | serde-json-value | 446.39; 1414708.0 / 11045917.0 | 20845.0 |
| twitter | DOM Parse | simd-json-borrowed+copy | 1211.35; 521333.0 / 1702083.5 | 3889.0 |
| twitter | DOM Parse | simd-json-borrowed-prepared | 1407.34; 448729.1 / 3008635.4 | 3888.0 |
| twitter | DOM Parse | sonic-rs-value | 1940.49; 325441.6 / 2135000.0 | 13.0 |
| twitter | Typed strict projection | jmax-vm-owned-extract | 330.38; 1911459.0 / 14744104.0 | 38.0 |
| twitter | Typed strict projection | serde-json-typed | 1718.70; 367437.5 / 1678965.3 | 2.0 |
| twitter | Typed strict projection | simd-json-typed+copy | 1740.31; 362875.0 / 2663694.3 | 8.0 |
| twitter | Typed strict projection | sonic-rs-typed | 1376.65; 458733.2 / 3859718.8 | 1.0 |
| twitter | Serialize | jmax-native-tape-encode-sink | 718.89; 878459.0 / 7603597.0 | 2.0 |
| twitter | Serialize | serde-json-to-vec | 1723.00; 366521.0 / 1712896.0 | 13.0 |
| twitter | Serialize | simd-json-to-vec | 1513.82; 417167.0 / 3560975.0 | 11.0 |
| twitter | Serialize | sonic-rs-to-vec | 2585.03; 244296.9 / 1608343.8 | 12.0 |
| twitter | Query only | jmax-query-only-extract-borrowed | 1518.19; 415965.2 / 2333583.0 | 10.0 |
| twitter | Query only | ps-pikkr-trained-query | 3089.78; 204388.7 / 1332666.7 | 1.0 |
| twitter | Query only | ps-pikkr-untrained-query | 3097.40; 203885.5 / 1106025.0 | 1.0 |
| citm_catalog | DOM Parse | jmax-dom-borrowed | 530.91; 3253291.0 / 12027666.0 | 19.0 |
| citm_catalog | DOM Parse | serde-json-value | 820.15; 2105959.0 / 11372208.0 | 41010.0 |
| citm_catalog | DOM Parse | simd-json-borrowed+copy | 1400.34; 1233417.0 / 3787500.0 | 34086.0 |
| citm_catalog | DOM Parse | simd-json-borrowed-prepared | 1527.82; 1130505.2 / 2140338.6 | 34085.0 |
| citm_catalog | DOM Parse | sonic-rs-value | 2585.85; 667944.3 / 1786834.0 | 15.0 |
| citm_catalog | Typed strict projection | jmax-vm-owned-extract | 446.75; 3866167.0 / 16521792.0 | 26.0 |
| citm_catalog | Typed strict projection | serde-json-typed | 1710.74; 1009625.0 / 2564319.7 | 2.0 |
| citm_catalog | Typed strict projection | simd-json-typed+copy | 2062.95; 837250.0 / 2162333.0 | 8.0 |
| citm_catalog | Typed strict projection | sonic-rs-typed | 2022.73; 853896.0 / 2339062.5 | 1.0 |
| citm_catalog | Serialize | jmax-native-tape-encode-sink | 909.85; 1898333.0 / 6775000.0 | 2.0 |
| citm_catalog | Serialize | serde-json-to-vec | 3448.95; 500791.5 / 3373792.0 | 13.0 |
| citm_catalog | Serialize | simd-json-to-vec | 3617.97; 477396.0 / 2695604.0 | 11.0 |
| citm_catalog | Serialize | sonic-rs-to-vec | 3686.67; 468500.0 / 1013937.5 | 13.0 |
| citm_catalog | Query only | jmax-query-only-extract-borrowed | 1465.47; 1178597.3 / 4865854.0 | 18.0 |
| citm_catalog | Query only | ps-pikkr-trained-query | 3087.74; 559375.0 / 2519000.0 | 1.0 |
| citm_catalog | Query only | ps-pikkr-untrained-query | 3089.58; 559041.7 / 1739000.0 | 1.0 |
| canada | DOM Parse | jmax-dom-borrowed | 245.28; 9177542.0 / 25550125.0 | 8.0 |
| canada | DOM Parse | serde-json-value | 490.52; 4589083.0 / 20445292.0 | 57683.0 |
| canada | DOM Parse | simd-json-borrowed+copy | 465.95; 4831084.0 / 25405334.0 | 112106.0 |
| canada | DOM Parse | simd-json-borrowed-prepared | 462.22; 4870109.4 / 23590552.0 | 112105.0 |
| canada | DOM Parse | sonic-rs-value | 1466.09; 1535416.0 / 4956417.0 | 17.0 |
| canada | Typed strict projection | jmax-vm-owned-extract | 188.45; 11944791.0 / 40108666.0 | 12.0 |
| canada | Typed strict projection | serde-json-typed | 1530.91; 1470396.0 / 3227042.0 | 2.0 |
| canada | Typed strict projection | simd-json-typed+copy | 819.42; 2747125.0 / 7326584.0 | 9.0 |
| canada | Typed strict projection | sonic-rs-typed | 1392.91; 1616083.0 / 4122041.5 | 1.0 |
| canada | Serialize | jmax-native-tape-encode-sink | 400.93; 5614583.0 / 21920792.0 | 2.0 |
| canada | Serialize | serde-json-to-vec | 1092.85; 2059792.0 / 6730084.0 | 15.0 |
| canada | Serialize | simd-json-to-vec | 948.84; 2372417.0 / 5576666.0 | 13.0 |
| canada | Serialize | sonic-rs-to-vec | 1079.68; 2084917.0 / 4707083.0 | 15.0 |
| canada | Query only | jmax-query-only-extract-borrowed | 1372.71; 1639854.0 / 7121125.0 | 6.0 |
| canada | Query only | ps-pikkr-trained-query | 3538.85; 636097.3 / 1812472.0 | 1.0 |
| canada | Query only | ps-pikkr-untrained-query | 3508.02; 641687.5 / 1831604.2 | 1.0 |
| github_events | DOM Parse | jmax-dom-borrowed | 645.77; 100859.6 / 276669.6 | 22.0 |
| github_events | DOM Parse | serde-json-value | 731.87; 88994.1 / 361318.5 | 2137.0 |
| github_events | DOM Parse | simd-json-borrowed+copy | 1688.48; 38574.3 / 163497.8 | 403.0 |
| github_events | DOM Parse | simd-json-borrowed-prepared | 1898.77; 34302.1 / 100849.0 | 402.0 |
| github_events | DOM Parse | sonic-rs-value | 2915.49; 22340.0 / 63880.2 | 9.0 |
| github_events | Typed strict projection | jmax-vm-owned-extract | 243.80; 267156.2 / 1278434.6 | 450.0 |
| github_events | Typed strict projection | serde-json-typed | 1937.96; 33608.5 / 150514.9 | 65.0 |
| github_events | Typed strict projection | simd-json-typed+copy | 2085.04; 31237.7 / 81297.8 | 69.0 |
| github_events | Typed strict projection | sonic-rs-typed | 1773.06; 36734.3 / 260638.9 | 64.0 |
| github_events | Serialize | jmax-native-tape-encode-sink | 954.55; 68233.3 / 191492.0 | 2.0 |
| github_events | Serialize | serde-json-to-vec | 1825.51; 35678.9 / 203784.8 | 10.0 |
| github_events | Serialize | simd-json-to-vec | 2340.17; 27832.2 / 78900.7 | 8.0 |
| github_events | Serialize | sonic-rs-to-vec | 3144.57; 20712.6 / 165723.7 | 10.0 |
| github_events | Query only | jmax-query-only-extract-borrowed | 1652.35; 39417.7 / 324413.1 | 38.0 |
| github_events | Query only | ps-pikkr-trained-query | N/A | N/A |
| github_events | Query only | ps-pikkr-untrained-query | N/A | N/A |
| apache_builds | DOM Parse | jmax-dom-borrowed | 616.29; 206516.7 / 531270.9 | 14.0 |
| apache_builds | DOM Parse | serde-json-value | 719.69; 176847.2 / 554617.5 | 6189.0 |
| apache_builds | DOM Parse | simd-json-borrowed+copy | 1635.29; 77830.2 / 200992.6 | 1779.0 |
| apache_builds | DOM Parse | simd-json-borrowed-prepared | 1697.23; 74989.6 / 270333.2 | 1778.0 |
| apache_builds | DOM Parse | sonic-rs-value | 2263.95; 56218.2 / 167027.8 | 10.0 |
| apache_builds | Typed strict projection | jmax-vm-owned-extract | 422.98; 300898.9 / 817555.3 | 12.0 |
| apache_builds | Typed strict projection | serde-json-typed | 1665.28; 76428.6 / 202948.5 | 2.0 |
| apache_builds | Typed strict projection | simd-json-typed+copy | 2145.67; 59317.1 / 148787.9 | 9.0 |
| apache_builds | Typed strict projection | sonic-rs-typed | 1339.00; 95052.6 / 188614.6 | 1.0 |
| apache_builds | Serialize | jmax-native-tape-encode-sink | 758.59; 167778.8 / 463279.7 | 2.0 |
| apache_builds | Serialize | serde-json-to-vec | 1704.28; 74679.6 / 309701.1 | 11.0 |
| apache_builds | Serialize | simd-json-to-vec | 2016.55; 63115.2 / 162976.9 | 9.0 |
| apache_builds | Serialize | sonic-rs-to-vec | 2724.08; 46722.2 / 124581.2 | 9.0 |
| apache_builds | Query only | jmax-query-only-extract-borrowed | 1657.15; 76803.6 / 192064.1 | 13.0 |
| apache_builds | Query only | ps-pikkr-trained-query | 3658.82; 34785.8 / 73052.4 | 1.0 |
| apache_builds | Query only | ps-pikkr-untrained-query | 3644.73; 34920.3 / 80375.0 | 1.0 |
| endpoint_batch | DOM Parse | jmax-dom-borrowed | 274.56; 373303.6 / 924917.0 | 21.0 |
| endpoint_batch | DOM Parse | serde-json-value | 373.27; 274583.4 / 789146.0 | 9119.0 |
| endpoint_batch | DOM Parse | simd-json-borrowed+copy | 721.68; 142020.8 / 364138.7 | 3688.0 |
| endpoint_batch | DOM Parse | simd-json-borrowed-prepared | 706.17; 145140.6 / 656515.6 | 3687.0 |
| endpoint_batch | DOM Parse | sonic-rs-value | 1743.77; 58777.4 / 121369.1 | 11.0 |
| endpoint_batch | Typed strict projection | jmax-vm-owned-extract | 189.40; 541158.4 / 1203958.0 | 16.0 |
| endpoint_batch | Typed strict projection | serde-json-typed | 938.16; 109250.0 / 208625.0 | 3.0 |
| endpoint_batch | Typed strict projection | simd-json-typed+copy | 988.49; 103687.5 / 282031.2 | 11.0 |
| endpoint_batch | Typed strict projection | sonic-rs-typed | 868.81; 117969.9 / 283589.3 | 2.0 |
| endpoint_batch | Serialize | jmax-native-tape-encode-sink | 327.33; 313125.0 / 691479.0 | 2.0 |
| endpoint_batch | Serialize | serde-json-to-vec | 1007.69; 101712.1 / 373382.0 | 11.0 |
| endpoint_batch | Serialize | simd-json-to-vec | 928.25; 110416.7 / 183601.3 | 9.0 |
| endpoint_batch | Serialize | sonic-rs-to-vec | 1135.82; 90237.8 / 220125.0 | 11.0 |
| endpoint_batch | Query only | jmax-query-only-extract-borrowed | 1538.17; 66633.6 / 156271.0 | 7.0 |
| endpoint_batch | Query only | ps-pikkr-trained-query | 3393.09; 30206.7 / 60496.6 | 1.0 |
| endpoint_batch | Query only | ps-pikkr-untrained-query | 3426.24; 29914.4 / 64784.7 | 1.0 |

## Allocation, copying and retained memory

The [complete native-regime allocation table](benchmark/tables/native-regimes.md) and [competitor table](benchmark/tables/competitors.md) report allocs/op and requested bytes/op separately from timing. Cold owned native Parse adopts/clones prepared Payload owners; it does **not** include an input copy. Borrowed Parse/schema/query input paths copy **0 input bytes**. External `jmax-dom+copy`, `jmax-dom+number-conversion+copy`, VM-any+copy and owned typed adapters copy the fixture once per operation. simd +copy rows copy the full input once inside timing; prepared kernels copy it outside timing. These are explicit API-path accounts and pointer/invariant checks, not a hardware memmove profiler. Escaped decoded output and final serialization bytes remain necessary writes.

For the first document in each workload, native live tape is exactly active u64 words ×8, plus decoded bytes. Warm tape/decoded capacities are measured after recycling into DomScratch. Legacy figures below come from explicit conversion of the **same document** into the six Vec representation, including NodeMeta, spans, containers and ConvertedNumber storage. They are representation comparisons, not a historical RSS measurement. Raw input backing is shared and excluded from both representation totals. Scratch stack and duplicate slot capacities are recorded separately in JSON; their private entry byte layouts, VM capacities, allocator pools and externally retained indexes are not included in this table. The `u64` tape is variable-width records, not one word per arbitrary node.

| Workload (first doc) | Input bytes | Tape live | Decoded live | Warm tape capacity | Legacy 6 Vec live |
| --- | --- | --- | --- | --- | --- |
| NagoyaAuth | 83 | 120 | 0 | 128 | 436 |
| ExchangeOrder | 85 | 144 | 0 | 256 | 520 |
| RpcEnvelope | 103 | 200 | 0 | 256 | 704 |
| LargeCatalog | 34116 | 61520 | 0 | 68224 | 223516 |
| VectorCoordinates | 881 | 1592 | 0 | 1760 | 8264 |
| DeepNesting | 143 | 944 | 0 | 1088 | 3384 |
| EscapedUnicode | 2012 | 88 | 1637 | 2008 | 1905 |
| small_object-seed-5354007379338153539-docs-128-items-32 | 567 | 792 | 0 | 1120 | 2788 |
| escaped_strings-seed-5354007379338153539-docs-128-items-32 | 2807 | 1304 | 1750 | 2800 | 4538 |
| unicode-seed-5354007379338153539-docs-128-items-32 | 1783 | 1304 | 0 | 1776 | 2788 |
| number_array-seed-5354007379338153539-docs-128-items-32 | 343 | 536 | 0 | 672 | 2788 |
| depth_book-seed-5354007379338153539-docs-128-items-32 | 1608 | 3792 | 0 | 6432 | 17872 |
| agg_trade-seed-5354007379338153539-docs-128-items-32 | 149 | 288 | 0 | 288 | 1024 |
| nested-seed-5354007379338153539-docs-128-items-32 | 147 | 912 | 0 | 1152 | 3284 |
| long-unescaped-ascii | 2056 | 48 | 0 | 2056 | 184 |
| long-unescaped-unicode | 7688 | 64 | 0 | 7688 | 184 |
| escape-dense | 7176 | 64 | 2560 | 7176 | 2744 |
| long-backslash-runs | 4104 | 64 | 2048 | 4104 | 2232 |
| wide-object | 16213 | 24600 | 0 | 32416 | 86116 |
| reordered-fields | 58 | 232 | 0 | 256 | 888 |
| sparse-large-subtree | 89045 | 262280 | 0 | 356160 | 1098196 |

Full decoded/tape capacities and legacy capacity totals: [memory.md](benchmark/tables/memory.md). Retained input ownership is in addition to these totals. Bytes slices expose only a lower bound on pinned allocation size; Arc views report the whole backing length.

Pointer/range assertions preserve **Bytes, Arc<[u8]>, and generic Arc<T>** through checked subviews and owned native Parse. Each measured subview operation allocates 0 times / 0 bytes. That Bytes fixture uses static backing. A separate heap-Bytes probe finds the first clone allocates **one 24-byte shared-owner wrapper** while preserving its byte pointer; subsequent clones allocate zero. Native corpus owners are promoted during untimed preflight/warmup, so this once-per-owner cost is excluded from warmed per-document rows and is counted here explicitly. The eight-byte JSON view retains a 12-byte Arc backing (exact); Bytes reports the visible 8-byte lower bound. Stage 4 additionally verifies Vec adoption, nested ranges, lifetime after caller owner drop, actual Arc retain/release, borrowed lifetimes and invalid ranges. Derived borrowed typed decoders now read slots tied to the original slice. A custom decoder can still choose owned copies; owned String fields still allocate their contents.

Warm reuse is capacity-bounded: a larger ASCII string can need no new tape/decoded storage. The updated scan-invariants JSON records exact cold/growth/equal/smaller allocation counts for wide-object and escape-dense inputs; equal/smaller reuse remains checked separately. These are untimed source-copy probes. They exclude corpus generation and retain the expanded scratch capacities. Vec adoption and generic Arc<T> owner adaptation independently measure zero allocations/bytes and preserve the input pointer. Untimed growth probes and scan counts are archived separately from timings. “Zero buffers” excludes intermediate whole-input/token/index buffers on the native path; final tape, selected slots, duplicate/nesting state, escaped decoded output and final encoded bytes remain real storage.

## Reactive handoff

| Operation | C: System MB/s; p50/p95 ns/doc | D: RustyAlloc MB/s; p50/p95 ns/doc | D allocs / bytes per op |
| --- | --- | --- | --- |
| NagoyaAuth parse+strict capture+admission+rings | 65.83; 1277.7 / 1280.3 | 14.08; 1308.9 / 3127.6 | 5 / 720 |

The measured lane uses real HostedPool/HostedWorker futures and nagoya locks/notifications, cooperatively polled in-process with one request in flight. It includes schema validation, capture, admission, completion ownership, pointer and lease assertions. All **3,072 requests per allocator** (warmup, measurement and allocation pass) submit and complete in order; final jobs, retained-byte and scratch-byte counters are zero after result drop. No payload byte copy occurs. This is synchronous lane handoff throughput with pass-average latency; it is not socket throughput, runtime scheduling under contention, or concurrent service request tails. Stage 4 separately executes rejection, cancellation, completion backpressure, shutdown and exactly-once release tests. No external live service was launched.

## Remaining bottlenecks and limitations

- Historical speedups include API/layout/toolchain/instrumentation changes and different host contention. Only C/D are matched source/build allocator ablations. Fixed allocator order can interact with drift; per-process medians/spread and all chronological samples are retained. Close rankings and p95 differences are not statistically isolated.
- Strict-discard scratch is now retained across calls; the updated sparse-subtree rows record the observed warmed counts. Query paths and cold result storage can allocate; warm canonical zeros do not generalize to new shapes, larger inputs or user decoders. Materialized ps-pikkr bitmap/depth/training retention and process RSS are not exposed by this harness; N/A does not mean zero.
- `CompiledQuery` retains its SelectorTrie. The source-copy counter checks zero trie builds during repeated execution, including `execute_borrowed`; cold traversal/result storage can still allocate. The added Warm-Borrowed path-query row recycles results and retains traversal storage.
- DOM duplicate tables now reuse generation-tagged storage per active depth; raw/decoded key identity and parent duplicate rejection remain tested. Arbitrary caller-created NodeId resolution checks record membership by walking earlier records; genuine-handle iteration/navigation remains O(1) per step. A large tape and duplicate table still cost memory/CPU even with borrowed raw strings.
- Strict processing must inspect unselected syntax, escapes, numeric/duplicate policies and applicable schema constraints. Query-only results intentionally do less and never gain a validation proof. Long skip cancellation is checked before/after synchronous VM turns rather than via an instruction-level external callback.
- Borrowed typed decoding now requires the additive `JsonDecodeBorrowed` trait, generated by derives. Handwritten owned-only implementations must add it to use borrowed typed entry points. Borrowed DOM and borrowed strict slot extraction preserve input. Arbitrarily fragmented incremental input, WebSocket unmasking/decompression, assembly, TLS/kernel copying and partial transport writes are outside the complete-input zero-copy measurements.
- Native Apple Silicon and scalar portable paths execute; real x86 AVX2/PCLMUL/AVX-512 performance and correctness remain unexecuted. The earlier Stage 5 [generated-code inspection](benchmark/pre-stage6/generated-code.md) confirmed four input vector loads in the release NEON raw classifier and an emitted PMULL instruction; callee-save register spills and returned mask stores remain. It does not prove whole-parser register residency. Physical no-PMULL ARM hardware was unavailable.
- These runs do not establish an attribution profile for every remaining CPU hotspot or a universal 5×–15× improvement. Warm CPU work and serialization/escape/duplicate/schema costs remain relevant even when allocator calls are zero. The separately instrumented scan probe records classification work and skip spans without contaminating timing. The updated EscapedUnicode counts and their Stage 5 comparison are reported in the post-Stage-6 table. Instrumentation now counts actual fixed-block EscapeMaskCursor classifications, including bounded scalar tails; it no longer targets the removed stateless helper. This measures classification work, not a percentage of total CPU time.

## Reproduction and preservation

[run-stage5.py](benchmark/run-stage5.py) builds and preflights sequentially; [run-timings.py](benchmark/run-timings.py) runs saved binaries sequentially; [write-report.py](benchmark/write-report.py) derives these tables from raw samples. The original temporary external project was absent at Stage 5 entry and was restored byte-for-byte from the verified snapshot before the documented counter/kernel additions. The [final harness snapshot](benchmark/harness-snapshot/) contains the exact external Cargo.toml, Cargo.lock and src used. [jmax-bench-snapshot](benchmark/jmax-bench-snapshot/), workspace manifest/lockfile copies and [source-snapshot.tar.gz](benchmark/source-snapshot.tar.gz) preserve native and local dependency sources. Local sibling dependency declarations remain path-based; restore the archived sibling layout or adjust those paths to reproduce elsewhere. The saved binaries and hashes are host-specific.

Raw requested logs: [jmax RustyAlloc](benchmark/jmax-bench-rusty-alloc.txt), [jmax System](benchmark/jmax-bench-system-alloc.txt), [competitors RustyAlloc](benchmark/jmax-vs-parsers-rusty-alloc.txt), [competitors System](benchmark/jmax-vs-parsers-system-alloc.txt). Original JSONL pass/batch samples, chronological external samples, capability rows, conformance matrices, environment/load observations and command logs remain under benchmark/. All machine-readable and Markdown tables are under [tables/](benchmark/tables/).

Final audit: **282 timed source/manifest files unchanged during measurement**, **9 protected historical files preserved**, exact external snapshot/lockfile match. HEAD remains `c3b97ce31eb93725c8494f258be7c6803752e558`; the measured workspace contains uncommitted Wave 6 changes, so the final source inventory digest, not HEAD alone, identifies the measured source: `045cbe7425a936dc17ac5f02fbb97bea826a089a76bce534bb6cd920a799aae4`. Protected older reports are unchanged; Stage 5 outputs were archived before their top-level replacements. No unrelated process was stopped.

Untimed scan and growth evidence: [scan-invariants.md](benchmark/tables/scan-invariants.md), [JSON](benchmark/tables/scan-invariants.json), [instrumentation patch](benchmark/invariant-instrumentation.patch), [probe source](benchmark/invariant-probe/src/main.rs). Skip-byte counters sum successful skip-call lengths; they are not a deduplicated coverage proof. Only the source copy contains these counters.
