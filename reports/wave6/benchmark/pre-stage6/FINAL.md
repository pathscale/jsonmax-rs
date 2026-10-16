# Wave 6 sequential benchmark and final report

Completed 2026-10-16T02:36:40.363016+07:00. Workspace `/Users/revenge/code/jsonmax-rs`.

## Executive result

Wave 6's native tape, borrowed input, direct schema execution, fused classifiers and allocator integration are implemented and pass the recorded correctness gates. The 56 matched canonical CLI rows show **2.20× geometric-mean observed throughput for Wave 6/System versus archived Wave 5/System** (per-row range 0.96–7.18×). This is a historical observation under different toolchains, API internals and host contention; it is not an isolated architecture speedup. The matched Wave 6 allocator ablation gives **1.123× RustyAlloc/System** across these rows, range 0.757–1.724×. Allocator effects cannot be subtracted from the historical comparison.

All **28 canonical warm owned cases** (7 workloads × Parse, strict schema Validate, strict Extract, direct Serialize) allocate **0 times / 0 requested bytes per document** after warming. The stress suite exposes exceptions: sparse-subtree strict schema validation/extraction each allocate 4 times / 720 bytes per document. There is no universal zero-allocation claim. On all six corpora, jmax borrowed DOM remains slower than serde_json, simd-json (+copy), and sonic-rs in this RustyAlloc suite. Geometric-mean speed ratios against serde_json are **0.513× for borrowed DOM**, **0.164× for owned typed projection**, and **0.249× for serialization** (below 1 means slower). Native ps-pikkr is faster on its narrower query-only task. These semantic differences and all values remain explicit below.

## Implemented architecture and integration

| Owner | Delivered behavior |
| --- | --- |
| C0 | Frozen ABI masks, tape and ownership contracts; checked owner subviews/capabilities; integrated callers and verification repairs. |
| S1 | Four-load NEON / two-load AVX2 fused classification, direct full blocks, word backslash parity, independently gated PMULL/PCLMUL, bulk UTF-8 and escape helpers. |
| S2 | Persistent BlockCursor with cached dispatch, one checked grammar transition, borrowed core, bulk strings, transactional escaped output, separate strict/query/live-owner skips. |
| S3 | Private immutable `TapeDocument` / `BorrowedDocument`, variable-width `Vec<u64>` records, parent/subtree navigation, explicit legacy conversion. |
| S4 | Direct schema consumer, prevalidated dispatch, explicit duplicate owner, proof-only validation and strict capture; query result types carry no validation proof. |
| S5 | Native tape/borrowed encoding, cached block escape masks and maximal slice writes; direct streaming separated from staged publication. |
| S6 | Owned/borrowed facade and scratch APIs, native DOM, strict/query extraction, optional allocator export. Generic `from_borrowed<T: JsonDecode>` remains a documented one-input-copy adapter. |
| S7 | Real nagoya sync primitives for bounded job/completion rings, admission/cancellation/order, private canonical validator capability, owner-preserving transport seams. |
| S8 | Durable ps-pikkr fork with provenance, crate-name/cfg repairs, NEON+PMULL, portable and x86-gated paths, bounded tails and quote carry. |
| L1 | RustyAlloc in all four executable roots; counted System feature for benchmark ablation; no library global allocator. |
| L2 / Stage 5 | Native and external harnesses; two seven-workload generators plus seven stress inputs, borrowed/scratch/query/syntax modes, per-operation allocation accounting, prepared simd kernels, reproducible snapshots. |

Details: [contracts](CONTRACTS.md), [S1](S1.md), [S2](S2.md), [S3](S3.md), [S4](S4.md), [S5](S5.md), [S6](S6.md), [S7](S7.md), [S8](S8.md), [L1](L1.md), [L2](L2.md). Handoff reports' initially unexecuted tests are superseded by Stage 4's final verification ledger.

## Verification and correctness gates

[VERIFICATION.md](verification/VERIFICATION.md) is the authoritative gate ledger: native workspace **121 passed, 0 failed** (118 unit/integration + 3 compile-fail doctests); targeted native feature run **32 passed, 0 failed, 1 ignored**; ps-pikkr native **29 passed**, portable **27 passed**, zero failures. Counts overlap and must not be summed. Workspace/all-target/bin checks, jsonmax default/no-default alloc-only/rusty-alloc configurations, transport no-default/all-feature checks and the standalone single-global-allocator consumer pass. x86 runtime is **UNEXECUTED**; S8's 1.98.0 cross-check is not native x86 execution or a 1.98.1 x86 check. No physical ARM machine without PMULL was available. Broad `cargo fmt --all -- --check` retains pre-existing differences; scoped integration formatting passes.

Stage 5 built both harnesses in both allocator configurations before any timing. Both new supplemental preflights pass all exposed operations; external preflights pass **247 operations per allocator** plus 18 conformance probes and semantic/typed equality. Native harness compilation and `cargo +1.98.1 test -p jmax-bench --offline` pass (three zero-test targets; these are not extra correctness tests). All timing processes exit 0. The new allocation, pointer, nonempty selection, scratch growth and reactive lease assertions pass. Sonic LazyValue's invalid-UTF-8 panic remains separately recorded in conformance output; it is not counted as an ordinary rejection. Root-array GitHub schema projection and pikkr array-index capability rows are explicitly N/A where unsupported.

## Build, environment, sequencing and measurement boundaries

Apple M4 Max, 16 logical CPUs, 64 GiB RAM, macOS 26.5, AArch64. rustc/cargo **1.98.1**, LLVM 22.1.8; native cfg includes `neon` and `aes` (PMULL). Every Cargo invocation is offline with `RUSTFLAGS="-C target-cpu=native"`. Requested `export PATH="/Users/revenge/.cargo/bin:$PATH"` is applied; that directory lacks a cargo proxy on this host, so the previously established `/Users/revenge/code/ps-pikkr/.tooling/bin/cargo` rustup proxy and explicitly pinned RUSTC/RUSTDOC are also used. No global toolchain settings changed. Exact commands, environment, source/manifest/binary/corpus hashes and process observations are archived in [benchmark/](benchmark/).

The task's jobs run strictly sequentially: one child build/test/benchmark at a time, all timed release builds/preflights and the bench test completed before timing, with no task-owned Cargo/rustc build during timing. All four timed harness variants are invoked from saved allocator-specific binaries; no `cargo run` appears in timing. Unrelated user processes were left running. Across 68 timing observations, one-minute load was **20.12–77.79**, median **30.03**. Other processes individually at ≥5% CPU summed to median **966.5%**, maximum **1398.3%** (100%=one core). Unrelated rustc reached **1140.2%**; unrelated benchmark applications were visible in 28 observations. Exclusive access applies to this task's jobs, not host isolation. `pmset -g therm` reports no recorded thermal/performance/CPU-power warning; direct temperature/frequency telemetry is unavailable, so thermal headroom is unknown. No CPU affinity or power control was applied.

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
| NagoyaAuth | Parse | Cold-Alloc | 47.84; 1663.7 / 2195.6 | 115.56; 699.9 / 1309.2 | 139.44; 526.0 / 1125.0 |
| NagoyaAuth | Parse | Warm-Scratch | 57.51; 1124.0 / 2436.8 | 144.11; 575.2 / 848.6 | 169.88; 487.3 / 515.3 |
| NagoyaAuth | Validate | Cold-Alloc | 38.03; 2101.6 / 2609.4 | 93.93; 750.0 / 936.9 | 136.54; 586.9 / 772.1 |
| NagoyaAuth | Validate | Warm-Scratch | 53.47; 1566.7 / 1572.6 | 136.50; 627.3 / 697.6 | 155.21; 534.5 / 561.5 |
| NagoyaAuth | Extract | Cold-Alloc | 34.21; 2200.5 / 3814.8 | 90.46; 920.6 / 1136.1 | 116.99; 691.7 / 721.7 |
| NagoyaAuth | Extract | Warm-Scratch | 52.65; 1477.9 / 2619.5 | 89.99; 693.7 / 1927.7 | 125.67; 593.4 / 1345.1 |
| NagoyaAuth | Serialize | Cold-Alloc | 117.56; 715.5 / 721.4 | 132.44; 648.8 / 730.5 | 126.82; 406.9 / 1150.1 |
| NagoyaAuth | Serialize | Warm-Scratch | 173.37; 483.7 / 487.0 | 214.18; 413.4 / 442.7 | 194.22; 364.3 / 663.1 |
| ExchangeOrder | Parse | Cold-Alloc | 44.02; 1925.1 / 1961.9 | 93.49; 874.0 / 1627.0 | 122.98; 596.7 / 1367.2 |
| ExchangeOrder | Parse | Warm-Scratch | 75.76; 1120.8 / 1124.7 | 135.22; 644.9 / 715.5 | 153.63; 546.9 / 575.5 |
| ExchangeOrder | Validate | Cold-Alloc | 36.28; 2335.3 / 2376.6 | 102.53; 861.7 / 939.8 | 114.20; 681.3 / 840.8 |
| ExchangeOrder | Validate | Warm-Scratch | 48.23; 1761.1 / 1766.3 | 100.66; 737.0 / 1147.1 | 79.98; 618.8 / 2425.1 |
| ExchangeOrder | Extract | Cold-Alloc | 35.21; 2415.4 / 2426.8 | 86.92; 1019.2 / 1122.7 | 100.10; 783.5 / 1120.8 |
| ExchangeOrder | Extract | Warm-Scratch | 49.71; 1654.6 / 1785.5 | 107.13; 807.6 / 868.8 | 121.94; 690.1 / 736.0 |
| ExchangeOrder | Serialize | Cold-Alloc | 107.05; 792.6 / 800.1 | 126.10; 712.2 / 765.6 | 152.94; 510.4 / 1009.8 |
| ExchangeOrder | Serialize | Warm-Scratch | 149.50; 568.4 / 570.0 | 143.29; 534.5 / 1083.0 | 143.05; 457.0 / 845.1 |
| RpcEnvelope | Parse | Cold-Alloc | 43.90; 2308.9 / 2501.6 | 104.09; 990.2 / 1592.1 | 142.57; 719.1 / 787.1 |
| RpcEnvelope | Parse | Warm-Scratch | 63.75; 1633.5 / 1638.3 | 134.77; 795.6 / 877.3 | 151.39; 686.5 / 712.9 |
| RpcEnvelope | Validate | Cold-Alloc | 32.03; 3090.5 / 3621.4 | 84.75; 1270.8 / 1351.6 | 112.31; 905.3 / 1015.6 |
| RpcEnvelope | Validate | Warm-Scratch | 46.29; 2283.5 / 2314.8 | 114.38; 950.9 / 1020.5 | 124.96; 811.2 / 836.9 |
| RpcEnvelope | Extract | Cold-Alloc | 34.73; 2914.7 / 3041.7 | 75.66; 1423.5 / 1509.4 | 102.46; 1010.1 / 1057.9 |
| RpcEnvelope | Extract | Warm-Scratch | 36.44; 2340.8 / 4662.8 | 70.55; 1010.4 / 2876.3 | 121.59; 849.3 / 893.2 |
| RpcEnvelope | Serialize | Cold-Alloc | 101.32; 1028.3 / 1033.9 | 117.36; 880.2 / 1353.2 | 159.60; 655.9 / 659.2 |
| RpcEnvelope | Serialize | Warm-Scratch | 87.17; 1012.4 / 1927.1 | 163.41; 649.4 / 708.3 | 177.34; 581.1 / 593.1 |
| LargeCatalog | Parse | Cold-Alloc | 59.14; 578212.2 / 633959.0 | 119.05; 279314.8 / 381564.5 | 138.91; 245238.0 / 279413.1 |
| LargeCatalog | Parse | Warm-Scratch | 71.57; 478702.1 / 518883.8 | 153.24; 217727.2 / 274432.0 | 135.78; 228141.6 / 361986.3 |
| LargeCatalog | Validate | Cold-Alloc | 33.78; 1048394.9 / 1312421.6 | 129.17; 265372.4 / 322856.1 | 104.01; 265098.6 / 719231.4 |
| LargeCatalog | Validate | Warm-Scratch | 20.53; 1616407.2 / 2788929.7 | 129.82; 261320.0 / 349160.8 | 114.18; 287466.8 / 437754.6 |
| LargeCatalog | Extract | Cold-Alloc | 19.18; 1548263.3 / 3222377.6 | 137.67; 248980.1 / 287258.5 | 104.22; 293626.3 / 493691.1 |
| LargeCatalog | Extract | Warm-Scratch | 22.99; 1456694.3 / 2029194.0 | 120.50; 271252.6 / 384090.5 | 107.66; 304719.4 / 417417.6 |
| LargeCatalog | Serialize | Cold-Alloc | 114.69; 281719.7 / 340639.3 | 168.43; 209147.8 / 284057.9 | 164.24; 212486.0 / 236712.6 |
| LargeCatalog | Serialize | Warm-Scratch | 72.88; 339629.2 / 895857.1 | 162.77; 203436.2 / 356502.6 | 171.30; 206863.9 / 226427.1 |
| VectorCoordinates | Parse | Cold-Alloc | 61.05; 14082.7 / 15206.4 | 101.26; 8007.5 / 11290.4 | 111.69; 7450.9 / 9202.8 |
| VectorCoordinates | Parse | Warm-Scratch | 54.08; 14930.3 / 23457.0 | 96.87; 8111.3 / 14533.5 | 114.61; 7467.1 / 9107.4 |
| VectorCoordinates | Validate | Cold-Alloc | 49.68; 17417.0 / 19631.8 | 89.97; 9431.6 / 15369.8 | 101.59; 8562.2 / 9793.6 |
| VectorCoordinates | Validate | Warm-Scratch | 44.33; 18385.4 / 25564.8 | 89.92; 9324.9 / 15931.3 | 104.16; 8378.3 / 9646.5 |
| VectorCoordinates | Extract | Cold-Alloc | 45.08; 18240.6 / 25819.3 | 72.83; 10113.9 / 23259.1 | 100.98; 8797.9 / 9579.1 |
| VectorCoordinates | Extract | Warm-Scratch | 38.82; 18429.7 / 38033.5 | 85.99; 9209.6 / 14026.0 | 104.09; 8696.3 / 9484.4 |
| VectorCoordinates | Serialize | Cold-Alloc | 135.69; 6072.9 / 6593.1 | 214.00; 4159.2 / 5341.8 | 246.23; 3607.1 / 3852.9 |
| VectorCoordinates | Serialize | Warm-Scratch | 79.19; 8856.4 / 19823.2 | 249.19; 3432.3 / 5085.0 | 251.94; 3316.4 / 3943.4 |
| DeepNesting | Parse | Cold-Alloc | 11.30; 12057.6 / 17146.8 | 54.93; 2671.5 / 3227.5 | 72.75; 1998.0 / 2083.0 |
| DeepNesting | Parse | Warm-Scratch | 27.80; 4430.3 / 8347.3 | 79.39; 1804.4 / 2163.1 | 82.06; 1793.6 / 1834.0 |
| DeepNesting | Validate | Cold-Alloc | 9.88; 12948.6 / 22320.0 | 21.00; 6583.3 / 10336.9 | 32.23; 4533.5 / 4835.9 |
| DeepNesting | Validate | Warm-Scratch | 16.02; 6640.3 / 16447.3 | 53.61; 2675.1 / 3274.7 | 54.61; 2675.8 / 2760.7 |
| DeepNesting | Extract | Cold-Alloc | 8.49; 13684.9 / 29811.2 | 20.52; 6822.3 / 10134.4 | 31.32; 4722.7 / 4767.6 |
| DeepNesting | Extract | Warm-Scratch | 11.84; 9349.6 / 17621.1 | 54.16; 2718.1 / 3268.6 | 54.00; 2726.6 / 2882.5 |
| DeepNesting | Serialize | Cold-Alloc | 53.73; 2657.2 / 2691.1 | 62.37; 2388.0 / 2573.9 | 69.43; 2123.0 / 2164.4 |
| DeepNesting | Serialize | Warm-Scratch | 41.02; 3292.0 / 4804.4 | 78.04; 1898.1 / 2031.6 | 75.99; 1926.8 / 2012.4 |
| EscapedUnicode | Parse | Cold-Alloc | 99.13; 17000.7 / 31295.2 | 137.73; 15117.2 / 17344.1 | 140.93; 14660.8 / 15034.5 |
| EscapedUnicode | Parse | Warm-Scratch | 97.08; 16579.4 / 28517.2 | 141.11; 14322.9 / 17269.9 | 144.72; 14220.1 / 14745.1 |
| EscapedUnicode | Validate | Cold-Alloc | 76.01; 23818.4 / 36835.3 | 195.01; 10595.1 / 12306.3 | 205.22; 10040.7 / 10338.2 |
| EscapedUnicode | Validate | Warm-Scratch | 78.06; 24908.5 / 30635.1 | 173.07; 9664.1 / 14892.3 | 210.83; 9293.0 / 10244.1 |
| EscapedUnicode | Extract | Cold-Alloc | 43.17; 31918.0 / 115208.7 | 170.78; 10606.4 / 17922.2 | 207.38; 9538.1 / 10530.0 |
| EscapedUnicode | Extract | Warm-Scratch | 84.45; 22346.4 / 32007.8 | 189.28; 9863.3 / 18236.3 | 213.33; 9366.9 / 10123.4 |
| EscapedUnicode | Serialize | Cold-Alloc | 400.25; 3883.5 / 8783.9 | 725.24; 2810.2 / 3213.9 | 756.76; 2688.8 / 2846.0 |
| EscapedUnicode | Serialize | Warm-Scratch | 551.89; 3652.0 / 3755.2 | 776.25; 2548.2 / 3238.9 | 763.48; 2643.2 / 2874.3 |

Borrowed DOM, warm borrowed DOM, strict syntax-only, schema Validate, strict/query Extract and staged/direct serialization have distinct rows in the [complete native-regime table](benchmark/tables/native-regimes.md) ([CSV](benchmark/tables/native-regimes.csv), [JSON](benchmark/tables/native-regimes.json)). Every measured row has MB/s and p50/p95. Historical borrowed/query rows without an equivalent operation are N/A; no values were invented. The [six-corpus before/after table](benchmark/tables/historical-six-corpus.md) additionally compares the earlier scan/DOM/typed/serialize results, with copy/API transitions labeled.

### Supplemental canonical regimes

| Workload | Mode | Regime | C: System | D: RustyAlloc | D allocs / bytes per op |
| --- | --- | --- | --- | --- | --- |
| NagoyaAuth | Parse | Cold-Borrowed | 143.63; 585.3 / 589.2 | 168.31; 499.0 / 504.6 | 5 / 528 |
| NagoyaAuth | Parse | Warm-Borrowed | 194.85; 431.6 / 434.6 | 197.25; 426.4 / 428.4 | 0 / 0 |
| NagoyaAuth | Validate-Syntax | Warm-Borrowed | 110.09; 729.8 / 979.8 | 121.40; 695.0 / 698.2 | 3 / 464 |
| NagoyaAuth | Validate-Schema | Warm-Borrowed | 137.00; 613.9 / 617.8 | 136.60; 615.9 / 618.2 | 0 / 0 |
| NagoyaAuth | Extract-Strict | Warm-Borrowed | 121.42; 667.3 / 673.2 | 125.81; 668.6 / 673.5 | 0 / 0 |
| NagoyaAuth | Extract-Query-Schema | Cold-Borrowed | 73.51; 997.1 / 1582.7 | 99.58; 844.7 / 848.6 | 12 / 508 |
| NagoyaAuth | Extract-Query-Path | Cold-Borrowed | 71.23; 1158.9 / 1254.2 | 82.25; 1022.8 / 1026.4 | 12 / 1180 |
| ExchangeOrder | Parse | Cold-Borrowed | 121.58; 696.0 / 704.4 | 149.42; 561.2 / 570.0 | 6 / 784 |
| ExchangeOrder | Parse | Warm-Borrowed | 112.44; 505.5 / 1403.3 | 167.68; 506.2 / 509.8 | 0 / 0 |
| ExchangeOrder | Validate-Syntax | Warm-Borrowed | 95.12; 875.7 / 949.9 | 101.08; 826.5 / 925.8 | 4 / 976 |
| ExchangeOrder | Validate-Schema | Warm-Borrowed | 118.02; 719.7 / 724.6 | 118.61; 718.1 / 721.4 | 0 / 0 |
| ExchangeOrder | Extract-Strict | Warm-Borrowed | 109.65; 774.4 / 778.6 | 109.92; 772.8 / 776.0 | 0 / 0 |
| ExchangeOrder | Extract-Query-Schema | Cold-Borrowed | 80.83; 1052.1 / 1056.0 | 88.68; 950.2 / 1004.9 | 11 / 543 |
| ExchangeOrder | Extract-Query-Path | Cold-Borrowed | 61.82; 1356.8 / 1499.0 | 70.92; 1199.2 / 1201.8 | 13 / 1694 |
| RpcEnvelope | Parse | Cold-Borrowed | 127.42; 797.9 / 899.4 | 152.53; 683.3 / 685.2 | 6 / 784 |
| RpcEnvelope | Parse | Warm-Borrowed | 168.63; 617.2 / 619.5 | 166.27; 626.3 / 628.9 | 0 / 0 |
| RpcEnvelope | Validate-Syntax | Warm-Borrowed | 94.34; 1084.6 / 1235.4 | 97.73; 1044.3 / 1164.4 | 4 / 720 |
| RpcEnvelope | Validate-Schema | Warm-Borrowed | 105.44; 931.6 / 1106.1 | 114.51; 906.2 / 936.5 | 0 / 0 |
| RpcEnvelope | Extract-Strict | Warm-Borrowed | 105.02; 977.2 / 1064.8 | 105.19; 979.8 / 1035.5 | 0 / 0 |
| RpcEnvelope | Extract-Query-Schema | Cold-Borrowed | 60.79; 1478.2 / 2781.9 | 83.24; 1239.3 / 1341.8 | 16 / 997 |
| RpcEnvelope | Extract-Query-Path | Cold-Borrowed | 91.27; 1108.4 / 1206.1 | 98.04; 1049.8 / 1158.5 | 13 / 1191 |
| LargeCatalog | Parse | Cold-Borrowed | 168.34; 196861.3 / 234106.1 | 162.87; 212725.3 / 221544.3 | 23 / 392976 |
| LargeCatalog | Parse | Warm-Borrowed | 102.53; 262445.0 / 577849.6 | 171.43; 173544.9 / 413883.8 | 0 / 0 |
| LargeCatalog | Validate-Syntax | Warm-Borrowed | 83.03; 385390.3 / 562454.4 | 58.34; 506707.0 / 910561.5 | 4 / 720 |
| LargeCatalog | Validate-Schema | Warm-Borrowed | 46.06; 623136.4 / 1509153.6 | 111.62; 282665.7 / 499532.9 | 0 / 0 |
| LargeCatalog | Extract-Strict | Warm-Borrowed | 61.41; 436173.8 / 923304.4 | 165.15; 210346.0 / 212482.1 | 0 / 0 |
| LargeCatalog | Extract-Query-Schema | Cold-Borrowed | 65.32; 563937.5 / 841078.8 | 166.25; 208766.3 / 210540.7 | 17 / 1438 |
| LargeCatalog | Extract-Query-Path | Cold-Borrowed | 348.20; 30727.2 / 362696.3 | 1530.57; 22695.0 / 22789.1 | 13 / 1191 |
| VectorCoordinates | Parse | Cold-Borrowed | 119.97; 7072.3 / 7755.2 | 167.64; 5244.1 / 5298.8 | 9 / 4368 |
| VectorCoordinates | Parse | Warm-Borrowed | 77.05; 6889.0 / 23002.6 | 166.71; 5264.0 / 5385.7 | 0 / 0 |
| VectorCoordinates | Validate-Syntax | Warm-Borrowed | 91.94; 9244.1 / 10126.3 | 119.09; 7398.8 / 7529.6 | 3 / 464 |
| VectorCoordinates | Validate-Schema | Warm-Borrowed | 78.96; 10052.4 / 17050.5 | 114.99; 7599.9 / 8018.9 | 0 / 0 |
| VectorCoordinates | Extract-Strict | Warm-Borrowed | 92.99; 9430.3 / 9852.2 | 120.35; 7281.9 / 7670.2 | 0 / 0 |
| VectorCoordinates | Extract-Query-Schema | Cold-Borrowed | 148.41; 5348.0 / 7417.0 | 242.53; 3595.7 / 3776.7 | 11 / 627 |
| VectorCoordinates | Extract-Query-Path | Cold-Borrowed | 79.22; 10905.6 / 13203.1 | 113.40; 7789.4 / 7889.6 | 14 / 1395 |
| DeepNesting | Parse | Cold-Borrowed | 55.77; 2349.9 / 3314.8 | 89.94; 1587.2 / 1602.2 | 14 / 5296 |
| DeepNesting | Parse | Warm-Borrowed | 86.73; 1648.1 / 1653.0 | 106.70; 1340.8 / 1342.8 | 0 / 0 |
| DeepNesting | Validate-Syntax | Warm-Borrowed | 33.69; 4085.9 / 4189.1 | 51.66; 2752.0 / 2853.5 | 27 / 10800 |
| DeepNesting | Validate-Schema | Warm-Borrowed | 42.32; 3137.4 / 4689.5 | 61.78; 2301.4 / 2423.5 | 0 / 0 |
| DeepNesting | Extract-Strict | Warm-Borrowed | 41.30; 3251.6 / 4102.2 | 60.60; 2326.8 / 2457.4 | 0 / 0 |
| DeepNesting | Extract-Query-Schema | Cold-Borrowed | 128.02; 973.3 / 1593.8 | 241.54; 591.1 / 599.6 | 14 / 714 |
| DeepNesting | Extract-Query-Path | Cold-Borrowed | 34.59; 4121.8 / 4185.5 | 49.37; 2874.0 / 2985.0 | 36 / 8396 |
| EscapedUnicode | Parse | Cold-Borrowed | 177.78; 11090.5 / 12601.2 | 234.45; 8593.4 / 8789.7 | 14 / 4616 |
| EscapedUnicode | Parse | Warm-Borrowed | 184.55; 10548.5 / 12951.2 | 243.14; 8238.9 / 8416.0 | 0 / 0 |
| EscapedUnicode | Validate-Syntax | Warm-Borrowed | 184.60; 10523.8 / 13427.1 | 251.73; 7969.1 / 8201.5 | 3 / 464 |
| EscapedUnicode | Validate-Schema | Warm-Borrowed | 144.81; 11396.8 / 19890.3 | 238.87; 8295.2 / 8737.6 | 0 / 0 |
| EscapedUnicode | Extract-Strict | Warm-Borrowed | 145.43; 12666.3 / 18160.2 | 236.49; 8464.5 / 8828.5 | 0 / 0 |
| EscapedUnicode | Extract-Query-Schema | Cold-Borrowed | 150.13; 12526.4 / 22927.4 | 225.41; 8905.9 / 9161.5 | 20 / 4475 |
| EscapedUnicode | Extract-Query-Path | Cold-Borrowed | 147.42; 12716.1 / 22613.9 | 219.73; 9122.4 / 9406.9 | 21 / 5266 |

### Stress cases

These valid deterministic inputs cover long ASCII/Unicode, dense escapes, long backslashes, 1,024-field objects, reordered fields and a sparse selected field over an 89,045-byte document. Full regimes and allocation rows are in [stress.md](benchmark/tables/stress.md). The table below selects cold borrowed Parse, warm strict schema validation, and query schema extraction; A/B are N/A.

| Stress | Operation | C: System | D: RustyAlloc | D allocs / bytes per op |
| --- | --- | --- | --- | --- |
| long-unescaped-ascii | Parse | 1009.47; 1865.9 / 2290.0 | 1408.65; 1458.7 / 1466.8 | 4 / 400 |
| long-unescaped-ascii | Validate-Schema | 1079.21; 1904.0 / 1907.5 | 1323.62; 1504.2 / 1653.3 | 0 / 0 |
| long-unescaped-ascii | Extract-Query-Schema | 920.74; 2174.8 / 2599.3 | 1252.68; 1625.3 / 1641.0 | 10 / 322 |
| long-unescaped-unicode | Parse | 832.32; 8430.0 / 13529.0 | 1329.79; 5758.8 / 5920.6 | 4 / 400 |
| long-unescaped-unicode | Validate-Schema | 850.80; 7326.8 / 11586.9 | 1307.82; 5779.9 / 6435.9 | 0 / 0 |
| long-unescaped-unicode | Extract-Query-Schema | 925.11; 8308.6 / 8322.9 | 1238.12; 6182.3 / 6353.2 | 10 / 322 |
| escape-dense | Parse | 243.70; 28343.1 / 34838.5 | 306.37; 23484.0 / 23821.9 | 14 / 8584 |
| escape-dense | Validate-Schema | 259.18; 27580.7 / 28728.5 | 248.34; 26151.0 / 38085.9 | 0 / 0 |
| escape-dense | Extract-Query-Schema | 247.34; 28894.5 / 29945.0 | 224.21; 31444.7 / 36834.6 | 20 / 8506 |
| long-backslash-runs | Parse | 223.92; 18288.1 / 18335.9 | 208.29; 18052.7 / 22022.1 | 13 / 4488 |
| long-backslash-runs | Validate-Schema | 229.85; 17794.3 / 18008.1 | 210.41; 17793.9 / 25221.0 | 0 / 0 |
| long-backslash-runs | Extract-Query-Schema | 194.08; 20600.9 / 23339.2 | 222.69; 18307.3 / 18613.6 | 19 / 4410 |
| wide-object | Parse | 155.48; 103602.5 / 108679.7 | 157.72; 102168.0 / 106166.3 | 20 / 130832 |
| wide-object | Validate-Schema | 121.52; 133357.4 / 134738.3 | 111.77; 142775.1 / 152672.2 | 0 / 0 |
| wide-object | Extract-Query-Schema | 148.09; 110929.0 / 113654.3 | 148.11; 110495.8 / 111014.0 | 19 / 131138 |
| reordered-fields | Parse | 79.35; 723.3 / 766.3 | 97.20; 589.5 / 635.1 | 6 / 784 |
| reordered-fields | Validate-Schema | 53.10; 1086.6 / 1104.8 | 58.38; 993.2 / 996.1 | 3 / 464 |
| reordered-fields | Extract-Query-Schema | 68.55; 827.8 / 991.9 | 84.31; 687.5 / 691.4 | 13 / 594 |
| sparse-large-subtree | Parse | 157.16; 566350.3 / 639157.6 | 171.28; 525635.1 / 618658.9 | 27 / 1572624 |
| sparse-large-subtree | Validate-Schema | 100.45; 860856.4 / 1037195.0 | 131.76; 676112.3 / 679568.4 | 4 / 720 |
| sparse-large-subtree | Extract-Query-Schema | 1280.01; 68731.4 / 71661.1 | 1346.31; 63698.9 / 74206.1 | 15 / 1050 |

## Head-to-head competitors

All implementations in each process use the same counting allocator backend. The main table below is **RustyAlloc**; the complete [competitor table](benchmark/tables/competitors.md) ([CSV](benchmark/tables/competitors.csv), [JSON](benchmark/tables/competitors.json)) includes System, all scan/typed/DOM/string jobs and prepared simd kernels. Main dependency versions: serde/serde_derive 1.0.229, serde_json 1.0.154, simd-json 0.18.1, sonic-rs 0.5.10, ps-pikkr 0.16.1; json-rust/miniserde are retained as additional rows. The external profile is opt-level=3, thin LTO, one codegen unit; native CLI uses the ordinary workspace release profile.

Strict jmax schema checks, syntax-only scans, DOM construction, typed projections and query selection are different workloads. serde IgnoredAny, simd tape and sonic LazyValue have different Unicode/surrogate/error policies, as recorded by the conformance matrix. jmax's strict APIs check syntax/UTF-8/escapes/numeric/duplicates/limits; schema APIs additionally enforce the compiled shape. No other parser row is labeled equivalent strict schema validation merely because it accepts these valid fixtures.

`simd-json ... +copy` includes cloning immutable input to mutable storage inside timing. `... prepared` excludes input refresh and input-buffer destruction; result allocation/destruction remains included. jmax borrowed DOM excludes copying input, while `jmax-dom+copy` and typed adapters include their explicit copy. DOMs differ in number representation: jmax retains spelling with strict numeric-policy checks, while materialized competitors generally convert numbers. Typed rows produce the same owned scalar projections and strings; GitHub's jmax adapter builds a native borrowed DOM then extracts each event object, unlike competitors' direct array deserialization.

ps-pikkr is a **query-only raw-value selector**, with no full validation, schema proof, DOM or serialization. Trained setup is outside measurement; untrained uses `usize::MAX`. Captured values are compared against the reference. It retains bitmap/per-depth indexes and learned state. Unsupported DOM Parse / Full Validate / Serialize are **N/A on all six corpora**, and GitHub's array-index query is **N/A**. Success does not establish validity of unselected JSON, UTF-8, escapes, numbers, duplicate policy or trailing bytes. Object-order deoptimization correctness is tested in S8; the six-corpus timing reuses each fixed corpus and does not measure a reordered-stream deoptimization distribution. Sonic's partial lookup sometimes returns a parent object (search_metadata/venueNames) while the jmax/pikkr path returns its leaf; those partial-query jobs are separately labeled in the full table.

| Corpus | Operation | Implementation | MB/s; p50 / p95 ns/doc | allocs/op |
| --- | --- | --- | --- | --- |
| twitter | DOM Parse | jmax-dom-borrowed | 274.55; 2300209.0 / 4462542.0 | 44.0 |
| twitter | DOM Parse | serde-json-value | 465.12; 1357750.0 / 3093354.5 | 20845.0 |
| twitter | DOM Parse | simd-json-borrowed+copy | 1203.10; 524908.4 / 1097375.0 | 3889.0 |
| twitter | DOM Parse | simd-json-borrowed-prepared | 1262.37; 500260.5 / 1062474.0 | 3888.0 |
| twitter | DOM Parse | sonic-rs-value | 1897.36; 332839.3 / 555809.6 | 13.0 |
| twitter | Typed strict projection | jmax-vm-owned-extract | 277.65; 2274500.0 / 4590042.0 | 44.0 |
| twitter | Typed strict projection | serde-json-typed | 1571.91; 401750.0 / 569300.0 | 2.0 |
| twitter | Typed strict projection | simd-json-typed+copy | 1627.94; 387923.7 / 652007.0 | 8.0 |
| twitter | Typed strict projection | sonic-rs-typed | 1363.77; 463066.8 / 874883.4 | 1.0 |
| twitter | Serialize | jmax-native-tape-encode-sink | 399.34; 1581396.0 / 3298292.0 | 20.0 |
| twitter | Serialize | serde-json-to-vec | 1635.94; 386025.0 / 850138.8 | 13.0 |
| twitter | Serialize | simd-json-to-vec | 1541.22; 409750.0 / 626666.6 | 11.0 |
| twitter | Serialize | sonic-rs-to-vec | 2341.59; 269694.5 / 436798.7 | 12.0 |
| twitter | Query only | jmax-query-only-extract-borrowed | 1167.87; 540741.8 / 1047083.4 | 20.0 |
| twitter | Query only | ps-pikkr-trained-query | 2893.54; 218250.0 / 393783.3 | 1.0 |
| twitter | Query only | ps-pikkr-untrained-query | 2901.07; 217683.4 / 425629.7 | 1.0 |
| citm_catalog | DOM Parse | jmax-dom-borrowed | 366.08; 4718042.0 / 10310292.0 | 33.0 |
| citm_catalog | DOM Parse | serde-json-value | 752.02; 2296750.0 / 4656125.0 | 41010.0 |
| citm_catalog | DOM Parse | simd-json-borrowed+copy | 1123.99; 1536666.0 / 2888791.0 | 34086.0 |
| citm_catalog | DOM Parse | simd-json-borrowed-prepared | 1319.69; 1308791.8 / 2273203.1 | 34085.0 |
| citm_catalog | DOM Parse | sonic-rs-value | 2329.73; 741375.0 / 1499542.0 | 15.0 |
| citm_catalog | Typed strict projection | jmax-vm-owned-extract | 364.02; 4744834.0 / 8720667.0 | 32.0 |
| citm_catalog | Typed strict projection | serde-json-typed | 1494.34; 1155833.0 / 1750771.0 | 2.0 |
| citm_catalog | Typed strict projection | simd-json-typed+copy | 1880.38; 918541.5 / 1524722.3 | 8.0 |
| citm_catalog | Typed strict projection | sonic-rs-typed | 1829.61; 944027.7 / 1918264.0 | 1.0 |
| citm_catalog | Serialize | jmax-native-tape-encode-sink | 591.26; 2921250.0 / 4423792.0 | 19.0 |
| citm_catalog | Serialize | serde-json-to-vec | 3398.54; 508218.8 / 995791.8 | 13.0 |
| citm_catalog | Serialize | simd-json-to-vec | 3362.50; 513666.8 / 744843.8 | 11.0 |
| citm_catalog | Serialize | sonic-rs-to-vec | 3261.13; 529633.4 / 929541.8 | 13.0 |
| citm_catalog | Query only | jmax-query-only-extract-borrowed | 1133.06; 1524375.0 / 2824250.0 | 28.0 |
| citm_catalog | Query only | ps-pikkr-trained-query | 2782.87; 620656.2 / 1163041.7 | 1.0 |
| citm_catalog | Query only | ps-pikkr-untrained-query | 2731.70; 632281.2 / 1036062.5 | 1.0 |
| canada | DOM Parse | jmax-dom-borrowed | 161.98; 13896750.0 / 30485292.0 | 21.0 |
| canada | DOM Parse | serde-json-value | 435.31; 5171125.0 / 19902208.0 | 57683.0 |
| canada | DOM Parse | simd-json-borrowed+copy | 522.27; 4310167.0 / 11679000.0 | 112106.0 |
| canada | DOM Parse | simd-json-borrowed-prepared | 454.39; 4954057.2 / 9838911.5 | 112105.0 |
| canada | DOM Parse | sonic-rs-value | 1283.56; 1753750.0 / 4251458.0 | 17.0 |
| canada | Typed strict projection | jmax-vm-owned-extract | 151.08; 14900208.0 / 39331667.0 | 16.0 |
| canada | Typed strict projection | serde-json-typed | 1325.15; 1698708.0 / 3297167.0 | 2.0 |
| canada | Typed strict projection | simd-json-typed+copy | 711.19; 3165209.0 / 7504208.0 | 9.0 |
| canada | Typed strict projection | sonic-rs-typed | 1204.31; 1869166.0 / 3799208.0 | 1.0 |
| canada | Serialize | jmax-native-tape-encode-sink | 355.60; 6330208.0 / 16392708.0 | 21.0 |
| canada | Serialize | serde-json-to-vec | 945.29; 2381334.0 / 4726458.0 | 15.0 |
| canada | Serialize | simd-json-to-vec | 795.72; 2828958.0 / 6836375.0 | 13.0 |
| canada | Serialize | sonic-rs-to-vec | 936.09; 2404750.0 / 4762541.0 | 15.0 |
| canada | Query only | jmax-query-only-extract-borrowed | 1086.83; 2071209.0 / 4892291.0 | 13.0 |
| canada | Query only | ps-pikkr-trained-query | 3075.32; 731972.3 / 1358375.0 | 1.0 |
| canada | Query only | ps-pikkr-untrained-query | 3020.19; 745333.3 / 1579250.0 | 1.0 |
| github_events | DOM Parse | jmax-dom-borrowed | 370.70; 175700.0 / 236070.8 | 30.0 |
| github_events | DOM Parse | serde-json-value | 647.16; 100643.6 / 160608.8 | 2137.0 |
| github_events | DOM Parse | simd-json-borrowed+copy | 1389.73; 46866.7 / 75602.4 | 403.0 |
| github_events | DOM Parse | simd-json-borrowed-prepared | 1604.90; 40583.2 / 48677.1 | 402.0 |
| github_events | DOM Parse | sonic-rs-value | 2438.77; 26706.9 / 38508.0 | 9.0 |
| github_events | Typed strict projection | jmax-vm-owned-extract | 162.52; 400756.8 / 617632.0 | 578.0 |
| github_events | Typed strict projection | serde-json-typed | 1656.37; 39322.1 / 54894.1 | 65.0 |
| github_events | Typed strict projection | simd-json-typed+copy | 1737.15; 37493.6 / 55234.5 | 69.0 |
| github_events | Typed strict projection | sonic-rs-typed | 1493.48; 43610.8 / 59712.8 | 64.0 |
| github_events | Serialize | jmax-native-tape-encode-sink | 434.97; 149738.1 / 205464.7 | 16.0 |
| github_events | Serialize | serde-json-to-vec | 1492.56; 43637.7 / 59360.5 | 10.0 |
| github_events | Serialize | simd-json-to-vec | 1868.54; 34857.1 / 53381.9 | 8.0 |
| github_events | Serialize | sonic-rs-to-vec | 2564.19; 25400.6 / 41956.4 | 10.0 |
| github_events | Query only | jmax-query-only-extract-borrowed | 1106.44; 58866.4 / 85593.8 | 46.0 |
| github_events | Query only | ps-pikkr-trained-query | N/A | N/A |
| github_events | Query only | ps-pikkr-untrained-query | N/A | N/A |
| apache_builds | DOM Parse | jmax-dom-borrowed | 369.35; 344593.8 / 600468.8 | 31.0 |
| apache_builds | DOM Parse | serde-json-value | 575.99; 220966.7 / 334525.0 | 6189.0 |
| apache_builds | DOM Parse | simd-json-borrowed+copy | 1314.04; 96858.0 / 146298.1 | 1779.0 |
| apache_builds | DOM Parse | simd-json-borrowed-prepared | 1293.16; 98421.9 / 129406.2 | 1778.0 |
| apache_builds | DOM Parse | sonic-rs-value | 1874.88; 67884.4 / 130410.1 | 10.0 |
| apache_builds | Typed strict projection | jmax-vm-owned-extract | 324.08; 392722.2 / 782591.6 | 16.0 |
| apache_builds | Typed strict projection | serde-json-typed | 1393.85; 91311.6 / 157154.8 | 2.0 |
| apache_builds | Typed strict projection | simd-json-typed+copy | 1727.72; 73666.6 / 105313.3 | 9.0 |
| apache_builds | Typed strict projection | sonic-rs-typed | 1124.05; 113229.1 / 158581.2 | 1.0 |
| apache_builds | Serialize | jmax-native-tape-encode-sink | 352.64; 360923.7 / 457052.0 | 15.0 |
| apache_builds | Serialize | serde-json-to-vec | 1377.68; 92383.4 / 127265.0 | 11.0 |
| apache_builds | Serialize | simd-json-to-vec | 1631.45; 78013.4 / 97906.2 | 9.0 |
| apache_builds | Serialize | sonic-rs-to-vec | 2202.53; 57785.9 / 93121.4 | 9.0 |
| apache_builds | Query only | jmax-query-only-extract-borrowed | 1124.95; 113138.2 / 149596.5 | 27.0 |
| apache_builds | Query only | ps-pikkr-trained-query | 3021.49; 42123.3 / 62670.7 | 1.0 |
| apache_builds | Query only | ps-pikkr-untrained-query | 3010.79; 42273.0 / 69240.9 | 1.0 |
| endpoint_batch | DOM Parse | jmax-dom-borrowed | 143.81; 712680.3 / 2566500.0 | 37.0 |
| endpoint_batch | DOM Parse | serde-json-value | 309.57; 331083.3 / 1100750.0 | 9119.0 |
| endpoint_batch | DOM Parse | simd-json-borrowed+copy | 595.62; 172078.8 / 343743.0 | 3688.0 |
| endpoint_batch | DOM Parse | simd-json-borrowed-prepared | 620.98; 165052.0 / 216770.8 | 3687.0 |
| endpoint_batch | DOM Parse | sonic-rs-value | 1450.10; 70680.5 / 194680.5 | 11.0 |
| endpoint_batch | Typed strict projection | jmax-vm-owned-extract | 138.43; 740416.7 / 1832895.5 | 20.0 |
| endpoint_batch | Typed strict projection | serde-json-typed | 790.38; 129677.0 / 165875.0 | 3.0 |
| endpoint_batch | Typed strict projection | simd-json-typed+copy | 816.19; 125576.4 / 701348.1 | 11.0 |
| endpoint_batch | Typed strict projection | sonic-rs-typed | 708.89; 144583.3 / 189592.6 | 2.0 |
| endpoint_batch | Serialize | jmax-native-tape-encode-sink | 166.86; 614250.0 / 1272333.0 | 17.0 |
| endpoint_batch | Serialize | serde-json-to-vec | 826.70; 123979.0 / 292958.2 | 11.0 |
| endpoint_batch | Serialize | simd-json-to-vec | 757.17; 135364.5 / 344412.9 | 9.0 |
| endpoint_batch | Serialize | sonic-rs-to-vec | 936.65; 109425.9 / 210495.9 | 11.0 |
| endpoint_batch | Query only | jmax-query-only-extract-borrowed | 1110.52; 92293.4 / 447431.1 | 14.0 |
| endpoint_batch | Query only | ps-pikkr-trained-query | 2835.61; 36145.4 / 75594.6 | 1.0 |
| endpoint_batch | Query only | ps-pikkr-untrained-query | 2829.98; 36217.2 / 92243.4 | 1.0 |

## Allocation, copying and retained memory

The [complete native-regime allocation table](benchmark/tables/native-regimes.md) and [competitor table](benchmark/tables/competitors.md) report allocs/op and requested bytes/op separately from timing. Cold owned native Parse adopts/clones prepared Payload owners; it does **not** include an input copy. Borrowed Parse/schema/query input paths copy **0 input bytes**. External `jmax-dom+copy`, `jmax-dom+number-conversion+copy`, VM-any+copy and owned typed adapters copy the fixture once per operation. simd +copy rows copy the full input once inside timing; prepared kernels copy it outside timing. These are explicit API-path accounts and pointer/invariant checks, not a hardware memmove profiler. Escaped decoded output and final serialization bytes remain necessary writes.

For the first document in each workload, native live tape is exactly active u64 words ×8, plus decoded bytes. Warm tape/decoded capacities are measured after recycling into DomScratch. Legacy figures below come from explicit conversion of the **same document** into the six Vec representation, including NodeMeta, spans, containers and ConvertedNumber storage. They are representation comparisons, not a historical RSS measurement. Raw input backing is shared and excluded from both representation totals. Scratch stack and duplicate slot capacities are recorded separately in JSON; their private entry byte layouts, VM capacities, allocator pools and externally retained indexes are not included in this table. The `u64` tape is variable-width records, not one word per arbitrary node.

| Workload (first doc) | Input bytes | Tape live | Decoded live | Warm tape capacity | Legacy 6 Vec live |
| --- | --- | --- | --- | --- | --- |
| NagoyaAuth | 83 | 120 | 0 | 128 | 436 |
| ExchangeOrder | 85 | 144 | 0 | 256 | 520 |
| RpcEnvelope | 103 | 200 | 0 | 256 | 704 |
| LargeCatalog | 34116 | 61520 | 0 | 65536 | 223516 |
| VectorCoordinates | 881 | 1592 | 0 | 2048 | 8264 |
| DeepNesting | 143 | 944 | 0 | 1024 | 3384 |
| EscapedUnicode | 2012 | 88 | 1637 | 128 | 1905 |
| small_object-seed-5354007379338153539-docs-128-items-32 | 567 | 792 | 0 | 1024 | 2788 |
| escaped_strings-seed-5354007379338153539-docs-128-items-32 | 2807 | 1304 | 1750 | 2048 | 4538 |
| unicode-seed-5354007379338153539-docs-128-items-32 | 1783 | 1304 | 0 | 2048 | 2788 |
| number_array-seed-5354007379338153539-docs-128-items-32 | 343 | 536 | 0 | 1024 | 2788 |
| depth_book-seed-5354007379338153539-docs-128-items-32 | 1608 | 3792 | 0 | 4096 | 17872 |
| agg_trade-seed-5354007379338153539-docs-128-items-32 | 149 | 288 | 0 | 512 | 1024 |
| nested-seed-5354007379338153539-docs-128-items-32 | 147 | 912 | 0 | 1024 | 3284 |
| long-unescaped-ascii | 2056 | 48 | 0 | 64 | 184 |
| long-unescaped-unicode | 7688 | 64 | 0 | 64 | 184 |
| escape-dense | 7176 | 64 | 2560 | 64 | 2744 |
| long-backslash-runs | 4104 | 64 | 2048 | 64 | 2232 |
| wide-object | 16213 | 24600 | 0 | 32768 | 86116 |
| reordered-fields | 58 | 232 | 0 | 256 | 888 |
| sparse-large-subtree | 89045 | 262280 | 0 | 524288 | 1098196 |

Full decoded/tape capacities and legacy capacity totals: [memory.md](benchmark/tables/memory.md). Retained input ownership is in addition to these totals. Bytes slices expose only a lower bound on pinned allocation size; Arc views report the whole backing length.

Pointer/range assertions preserve **Bytes, Arc<[u8]>, and generic Arc<T>** through checked subviews and owned native Parse. Each measured subview operation allocates 0 times / 0 bytes. That Bytes fixture uses static backing. A separate heap-Bytes probe finds the first clone allocates **one 24-byte shared-owner wrapper** while preserving its byte pointer; subsequent clones allocate zero. Native corpus owners are promoted during untimed preflight/warmup, so this once-per-owner cost is excluded from warmed per-document rows and is counted here explicitly. The eight-byte JSON view retains a 12-byte Arc backing (exact); Bytes reports the visible 8-byte lower bound. Stage 4 additionally verifies Vec adoption, nested ranges, lifetime after caller owner drop, actual Arc retain/release, borrowed lifetimes and invalid ranges. A generic typed borrowed compatibility decoder still copies once; it is excluded from zero-copy claims.

Warm reuse is capacity-bounded: a larger ASCII string can need no new tape/decoded storage. Growing a wide object from scale 1 to 32 incurs 10 additional allocation/reallocation calls and 126,976 requested bytes; the next equal-size and subsequent smaller parse each allocate zero. Escape-dense growth incurs 5 calls / 7,936 requested bytes, then zero for equal/smaller reuse. These are untimed source-copy probes. They exclude corpus generation and retain the expanded scratch capacities. Vec adoption and generic Arc<T> owner adaptation independently measure zero allocations/bytes and preserve the input pointer. Untimed growth probes and scan counts are archived separately from timings. “Zero buffers” excludes intermediate whole-input/token/index buffers on the native path; final tape, selected slots, duplicate/nesting state, escaped decoded output and final encoded bytes remain real storage.

## Reactive handoff

| Operation | C: System MB/s; p50/p95 ns/doc | D: RustyAlloc MB/s; p50/p95 ns/doc | D allocs / bytes per op |
| --- | --- | --- | --- |
| NagoyaAuth parse+strict capture+admission+rings | 53.50; 1437.8 / 1969.1 | 57.99; 1451.5 / 1454.1 | 9 / 808 |

The measured lane uses real HostedPool/HostedWorker futures and nagoya locks/notifications, cooperatively polled in-process with one request in flight. It includes schema validation, capture, admission, completion ownership, pointer and lease assertions. All **3,072 requests per allocator** (warmup, measurement and allocation pass) submit and complete in order; final jobs, retained-byte and scratch-byte counters are zero after result drop. No payload byte copy occurs. This is synchronous lane handoff throughput with pass-average latency; it is not socket throughput, runtime scheduling under contention, or concurrent service request tails. Stage 4 separately executes rejection, cancellation, completion backpressure, shutdown and exactly-once release tests. No external live service was launched.

## Remaining bottlenecks and limitations

- Historical speedups include API/layout/toolchain/instrumentation changes and different host contention. Only C/D are matched source/build allocator ablations. Fixed allocator order can interact with drift; per-process medians/spread and all chronological samples are retained. Close rankings and p95 differences are not statistically isolated.
- Sparse strict discard still allocates temporary bounded consumer state (4 allocations/720 requested bytes per document here). Query paths and cold result storage can allocate; warm canonical zeros do not generalize to new shapes, larger inputs or user decoders. Materialized ps-pikkr bitmap/depth/training retention and process RSS are not exposed by this harness; N/A does not mean zero.
- `query::execute_borrowed` rebuilds its SelectorTrie from the retained path list on each invocation; these per-call setup allocations are included in the query-path rows. Compiling the public path list outside the timer does not remove this internal work.
- DOM duplicate tracking retains parsed keys for the parse, not just currently open depth. Arbitrary caller-created NodeId resolution checks record membership by walking earlier records; genuine-handle iteration/navigation remains O(1) per step. A large tape and duplicate table still cost memory/CPU even with borrowed raw strings.
- Strict processing must inspect unselected syntax, escapes, numeric/duplicate policies and applicable schema constraints. Query-only results intentionally do less and never gain a validation proof. Long skip cancellation is checked before/after synchronous VM turns rather than via an instruction-level external callback.
- Generic `JsonDecode` has an owned-slot ABI, so its borrowed typed compatibility facade retains a one-copy adapter. Borrowed DOM and borrowed strict slot extraction do preserve input. Arbitrarily fragmented incremental input, WebSocket unmasking/decompression, assembly, TLS/kernel copying and partial transport writes are outside the complete-input zero-copy measurements.
- Native Apple Silicon and scalar portable paths execute; real x86 AVX2/PCLMUL/AVX-512 performance and correctness remain unexecuted. The [generated-code inspection](benchmark/generated-code.md) confirms four input vector loads in the release NEON raw classifier and an emitted PMULL instruction; callee-save register spills and returned mask stores remain. It does not prove whole-parser register residency. Physical no-PMULL ARM hardware was unavailable.
- These runs do not establish an attribution profile for every remaining CPU hotspot or a universal 5×–15× improvement. Warm CPU work and serialization/escape/duplicate/schema costs remain relevant even when allocator calls are zero. The separately instrumented scan probe records classification work and skip spans without contaminating timing. On the first 2,012-byte EscapedUnicode document, cursor classification covers 2,012 bytes and ordinary-run escape helpers additionally classify 8,154 bytes in 129 helper blocks: 4.05× extra classified bytes beyond the cursor pass. This identifies repeated helper discovery work; it is not a measured percentage of total CPU time. Long backslash-only runs have zero such helper-block calls in this probe.

## Reproduction and preservation

[run-stage5.py](benchmark/run-stage5.py) builds and preflights sequentially; [run-timings.py](benchmark/run-timings.py) runs saved binaries sequentially; [write-report.py](benchmark/write-report.py) derives these tables from raw samples. The original temporary external project was absent at Stage 5 entry and was restored byte-for-byte from the verified snapshot before the documented counter/kernel additions. The [final harness snapshot](benchmark/harness-snapshot/) contains the exact external Cargo.toml, Cargo.lock and src used. [jmax-bench-snapshot](benchmark/jmax-bench-snapshot/), workspace manifest/lockfile copies and [source-snapshot.tar.gz](benchmark/source-snapshot.tar.gz) preserve native and local dependency sources. Local sibling dependency declarations remain path-based; restore the archived sibling layout or adjust those paths to reproduce elsewhere. The saved binaries and hashes are host-specific.

Raw requested logs: [jmax RustyAlloc](benchmark/jmax-bench-rusty-alloc.txt), [jmax System](benchmark/jmax-bench-system-alloc.txt), [competitors RustyAlloc](benchmark/jmax-vs-parsers-rusty-alloc.txt), [competitors System](benchmark/jmax-vs-parsers-system-alloc.txt). Original JSONL pass/batch samples, chronological external samples, capability rows, conformance matrices, environment/load observations and command logs remain under benchmark/. All machine-readable and Markdown tables are under [tables/](benchmark/tables/).

Final audit: **281 timed source/manifest files unchanged during measurement**, **9 protected historical files preserved**, exact external snapshot/lockfile match. HEAD remains `c3b97ce31eb93725c8494f258be7c6803752e558`; the measured workspace contains uncommitted Wave 6 changes, so the final source inventory digest, not HEAD alone, identifies the measured source: `f4b97ced14199b149cf8fc22c641c0befb37a72e9b29091c3030336aacbc4326`. No historical report was overwritten and no unrelated process was stopped.

Untimed scan and growth evidence: [scan-invariants.md](benchmark/tables/scan-invariants.md), [JSON](benchmark/tables/scan-invariants.json), [instrumentation patch](benchmark/invariant-instrumentation.patch), [probe source](benchmark/invariant-probe/src/main.rs). Skip-byte counters sum successful skip-call lengths; they are not a deduplicated coverage proof. Only the source copy contains these counters.
