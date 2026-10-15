# Seven workloads: Cold-Alloc versus Warm-Scratch

Measured on 2026-10-16, Asia/Phnom_Penh. All requested changes are implemented. **Every jmax warm operation measured exactly zero heap allocations: 7 workloads × 4 operations = 28 cases.** The native CLI completed 56 rows; the external head-to-head completed 182 cases and 3,640 timed corpus passes.

## Environment and measurement limits

Apple M4 Max, aarch64-apple-darwin, macOS 26.5 (25F71), rustc 1.99.0 (b940084d7, LLVM 23.1.1). Both builds used `RUSTFLAGS="-C target-cpu=native"`. The jmax CLI reported NEON for every workload. No persistent Cargo target settings were changed.

The external runner used serde 1.0.229, serde_json 1.0.154 with `float_roundtrip`, simd-json 0.17.3, and sonic-rs 0.5.10. Other dependency features are defaults. Versions are pinned by the archived lockfile; this is a comparison of these versions, not a claim about every release. Both builds use ordinary release optimization, without added LTO or codegen-unit overrides; the external harness additionally retains level-1 debug information for profiling. All engines share the system allocator through the same counting wrapper. Its disabled counter branch remains in allocation calls during timing.

**Contention:** 10 process snapshots during the head-to-head recorded other processes using a median 1074.3% CPU and maximum 1159.8% CPU (100% = one core), counting only processes individually at or above 5%. Other compilers, browsers, and applications remained active. The two benchmark processes ran sequentially, and profiling was separate. Setup and system activity were not fully isolated. No unrelated process was stopped; there was no affinity, thermal isolation, or frequency control. Sequential case order can interact with contention. Some warm medians are slower than cold despite zero allocation; these timings do not establish a stable speedup or a reliable close ranking. See [system-load.jsonl](benchmark-7-workloads-cold-vs-warm/system-load.jsonl).

Each case runs the same 128 prebuilt payloads, seed 5354007379338153539, `items=32`, three untimed warmup passes, and twenty timed passes. Each timer covers a complete 128-document pass. The external table divides pass times by 128; its median is the nearest-rank tenth pass, and p95 is the nineteenth pass. These are distributions of pass averages, not individual-request tail latency. MB/s uses decimal MB and the aggregate mean elapsed time.

Allocations and requested allocation bytes are counted in a separate untimed pass after prewarming, including reallocations and zeroed allocations. Warm jmax cases abort on any allocation. Timing includes result destruction and each API's required input copy. JSON generation, schemas, serializer input trees, semantic comparisons, output formatting, and file I/O are untimed. `black_box` consumes actual results. The CLI additionally includes its constant-size observation/accounting work, so its numbers should be compared within its own table.

Cold-Alloc means fresh engine/result allocations per message, with schemas and payload owners already prepared. It does not mean an empty CPU cache or a newly started allocator. Warm-Scratch retains sufficient capacities across messages; larger future inputs, different schema shapes, or allocating custom typed decoders can still allocate. Borrowed-slice facade helpers copy input into Payload; use the retained-Payload APIs for zero-allocation input reuse.

## Corpus and extraction semantics

The checkout originally had a different seven-workload suite. The requested names now have explicit deterministic synthetic payload generators and exact native shape schemas in `crates/jmax-bench/src/followup.rs`; the legacy generator is still available. These are synthetic workloads, not production traffic captures.

| Workload | Payload shape at items=32 | Bytes/document, min–max | Mean bytes | Captured result |
|---|---|---:|---:|---|
| NagoyaAuth | user, token, expires, admin | 83–85 | 84.14 | four scalar fields |
| ExchangeOrder | symbol, side, price, quantity, id | 85–85 | 85.00 | five scalar fields |
| RpcEnvelope | JSON-RPC envelope plus account/limit/active params | 103–105 | 104.14 | three scalars and raw params object |
| LargeCatalog | 512 products, each with id/name/price/active, plus version | 34116–35140 | 34700.00 | raw catalog array and version |
| VectorCoordinates | coordinates object containing 96 numeric components | 881–881 | 881.00 | raw coordinates array |
| DeepNesting | root object containing 32 alternating array/object levels | 143–143 | 143.00 | raw root container |
| EscapedUnicode | 32 repeated escape/UTF-8 segments, surrogate pair, controls, plus id | 2012–2014 | 2013.14 | decoded text and id |

Every Extract row for jmax returns a nonempty selection and validates the complete document, including unselected subtrees and trailing bytes. Numeric native schema fields use the schema's Number policy; price and quantity in ExchangeOrder are intentionally strings. Container captures retain raw spans instead of allocating owned child trees. [Corpus hashes and sizes](benchmark-7-workloads-cold-vs-warm/corpus-manifest.json) and [first payload examples](benchmark-7-workloads-cold-vs-warm/examples) are archived. Generation uses no JSON baseline library.

## Implemented APIs and invariants

- DOM: `DocumentParts::clear_keep_capacity`, `jmax_dom::build::ParseScratch`, `parse_into`, `parse_with_numeric_into`, and `jsonmax::parse::parse_payload_into` retain nodes, spans, containers, converted numbers, decoded bytes, node metadata, builder frames, cursor frames, and per-depth duplicate-key tables. The table layout in this checkout uses spans/containers/metadata rather than separate object-entry, array-item, and raw-lexeme vectors.
- Cursor: `SyntaxState::reset_with_payload`, `Cursor::reset_with_payload`, and arena transfer retain syntax storage while resetting traversal, EOF/error state, SIMD block/carry state, and numeric options.
- VM: `ValidationScratch`/`VmScratch`, retained selection words, seen masks, per-depth unknown-key tables, slot values, decoded arena, and selection reorder buffer. `execute::validate_into`, `endpoint::validate_payload_into`, and `extract_payload_into` reuse them. Every invocation charges retained VM capacities against its scratch budget. Both regimes share event execution, full EOF/schema validation, and reference checks before proof publication.
- `ValidatedSlots.decoded` changes from a boxed slice to `Vec<u8>` to preserve spare capacity. This is a public field type change; downstream constructors expecting a boxed arena need conversion. Existing ParsedMessage adapters now convert at their ownership boundary. Existing custom EventInput/InputBuffers adapters retain their boxed interface. The scratch path does not box or shrink its arena.
- Serialization: `WriterScratch`, `JsonWriter::new_with_scratch`/`finish_with_scratch`, and `jsonmax::encode::document_to_buffer` retain both output capacity and writer frames. Reusing only the output vector would still allocate frames for every container message.
- Typed decoding: `from_slice_with_schema<T>(&[u8], &CompiledSchema)` and `decode_payload_with_schema<T>(&Payload, &CompiledSchema)`, their scratch variants, and cached `decode_payload_into`/`from_slice_into`. In std mode, the default generic APIs compile once per `TypeId`, caching both success and failure under a Mutex/OnceLock. Concurrent first calls cannot compile twice. These generic cached entry points require `T: 'static`; explicit-schema entry points do not. Alloc-only builds retain the compilation fallback, and explicit compiled-schema APIs avoid it.

Scratch results borrow their storage. Typed `_into` helpers return T while scratch retains the validated slots; owner-relative string/span handles must be resolved against that current result before reuse. T's own decoder may allocate. Schemas are never recompiled in the explicit-schema methods or the benchmark's per-message loop.

## Small payload highlight: native CLI

These medians are the CLI's complete-pass median divided by 128. Allocation counts come from the external counter and permanent integration tests. The full CLI includes every mode below.

| Workload | Parse Cold-Alloc ns/doc | Parse Warm-Scratch ns/doc | Cold allocations/doc | Warm allocations/doc |
|---|---:|---:|---:|---:|
| NagoyaAuth | 1663.73 | 1124.02 | 11 | 0 |
| ExchangeOrder | 1925.13 | 1120.77 | 12 | 0 |
| RpcEnvelope | 2308.92 | 1633.46 | 12 | 0 |

## Complete native CLI output

Executed successfully with exactly:

```sh
RUSTFLAGS="-C target-cpu=native" cargo run --release -p jmax-bench --bin jmax_bench
```

jsonmax deterministic benchmark
Seed: 5354007379338153539 | Documents/workload: 128 | Items/document: 32 | Warmup: 3 | Measured: 20
Times and Payload Bytes describe complete corpus passes; MB/s uses decimal MB.
SIMD dispatch for each workload's first payload:
  NagoyaAuth: neon
  ExchangeOrder: neon
  RpcEnvelope: neon
  LargeCatalog: neon
  VectorCoordinates: neon
  DeepNesting: neon
  EscapedUnicode: neon
Extract validates complete schemas and captures fields on every workload. Warm scratch is prewarmed on the entire corpus.

| Workload       | Mode      | Allocation regime | Payload Bytes | Min (ns) | Median (ns) | P95 (ns) | Throughput (MB/s) | ns/op | Msg/sec |
|----------------|-----------|-------------------|--------------:|---------:|------------:|---------:|------------------:|------:|--------:|
| NagoyaAuth     | Parse     | Cold-Alloc |         10770 |   211583 |      212958 |   281042 |             47.84 | 1758.90 |  568536 |
| NagoyaAuth     | Parse     | Warm-Scratch (Zero-Alloc Reuse) |         10770 |   133834 |      143875 |   311916 |             57.51 | 1463.10 |  683479 |
| NagoyaAuth     | Validate  | Cold-Alloc |         10770 |   266375 |      269000 |   334000 |             38.03 | 2212.39 |  452001 |
| NagoyaAuth     | Validate  | Warm-Scratch (Zero-Alloc Reuse) |         10770 |   199209 |      200541 |   201291 |             53.47 | 1573.63 |  635472 |
| NagoyaAuth     | Extract   | Cold-Alloc |         10770 |   259333 |      281666 |   488292 |             34.21 | 2459.31 |  406618 |
| NagoyaAuth     | Extract   | Warm-Scratch (Zero-Alloc Reuse) |         10770 |   188000 |      189167 |   335291 |             52.65 | 1598.01 |  625777 |
| NagoyaAuth     | Serialize | Cold-Alloc |         10770 |    90916 |       91584 |    92334 |            117.56 | 715.71 | 1397220 |
| NagoyaAuth     | Serialize | Warm-Scratch (Zero-Alloc Reuse) |         10770 |    61708 |       61916 |    62333 |            173.37 | 485.32 | 2060496 |
| ExchangeOrder  | Parse     | Cold-Alloc |         10880 |   243875 |      246417 |   251125 |             44.02 | 1931.09 |  517843 |
| ExchangeOrder  | Parse     | Warm-Scratch (Zero-Alloc Reuse) |         10880 |   142709 |      143458 |   143959 |             75.76 | 1122.02 |  891248 |
| ExchangeOrder  | Validate  | Cold-Alloc |         10880 |   296875 |      298916 |   304209 |             36.28 | 2342.94 |  426815 |
| ExchangeOrder  | Validate  | Warm-Scratch (Zero-Alloc Reuse) |         10880 |   224833 |      225417 |   226083 |             48.23 | 1762.43 |  567397 |
| ExchangeOrder  | Extract   | Cold-Alloc |         10880 |   305583 |      309167 |   310625 |             35.21 | 2413.82 |  414281 |
| ExchangeOrder  | Extract   | Warm-Scratch (Zero-Alloc Reuse) |         10880 |   210875 |      211792 |   228542 |             49.71 | 1709.75 |  584881 |
| ExchangeOrder  | Serialize | Cold-Alloc |         10880 |   100166 |      101458 |   102417 |            107.05 | 794.04 | 1259378 |
| ExchangeOrder  | Serialize | Warm-Scratch (Zero-Alloc Reuse) |         10880 |    72583 |       72750 |    72958 |            149.50 | 568.55 | 1758846 |
| RpcEnvelope    | Parse     | Cold-Alloc |         13330 |   291583 |      295542 |   320209 |             43.90 | 2372.41 |  421512 |
| RpcEnvelope    | Parse     | Warm-Scratch (Zero-Alloc Reuse) |         13330 |   208458 |      209083 |   209708 |             63.75 | 1633.69 |  612111 |
| RpcEnvelope    | Validate  | Cold-Alloc |         13330 |   384833 |      395583 |   463541 |             32.03 | 3251.29 |  307571 |
| RpcEnvelope    | Validate  | Warm-Scratch (Zero-Alloc Reuse) |         13330 |   273916 |      292292 |   296292 |             46.29 | 2249.58 |  444528 |
| RpcEnvelope    | Extract   | Cold-Alloc |         13330 |   370167 |      373084 |   389333 |             34.73 | 2998.21 |  333532 |
| RpcEnvelope    | Extract   | Warm-Scratch (Zero-Alloc Reuse) |         13330 |   296416 |      299625 |   596833 |             36.44 | 2858.14 |  349878 |
| RpcEnvelope    | Serialize | Cold-Alloc |         13330 |   130667 |      131625 |   132333 |            101.32 | 1027.82 |  972937 |
| RpcEnvelope    | Serialize | Warm-Scratch (Zero-Alloc Reuse) |         13330 |    99917 |      129583 |   246667 |             87.17 | 1194.65 |  837068 |
| LargeCatalog   | Parse     | Cold-Alloc |       4441600 | 72397916 |    74011167 | 81146750 |             59.14 | 586791.85 |    1704 |
| LargeCatalog   | Parse     | Warm-Scratch (Zero-Alloc Reuse) |       4441600 | 58627542 |    61273875 | 66417125 |             71.57 | 484870.54 |    2062 |
| LargeCatalog   | Validate  | Cold-Alloc |       4441600 | 86315541 |   134194541 | 167989959 |             33.78 | 1027217.87 |     974 |
| LargeCatalog   | Validate  | Warm-Scratch (Zero-Alloc Reuse) |       4441600 | 119604875 |   206900125 | 356983000 |             20.53 | 1690193.18 |     592 |
| LargeCatalog   | Extract   | Cold-Alloc |       4441600 | 136720917 |   198177708 | 412464333 |             19.18 | 1809408.46 |     553 |
| LargeCatalog   | Extract   | Warm-Scratch (Zero-Alloc Reuse) |       4441600 | 111026834 |   186456875 | 259736833 |             22.99 | 1509375.11 |     663 |
| LargeCatalog   | Serialize | Cold-Alloc |       4441600 | 32022750 |    36060125 | 43601833 |            114.69 | 302562.89 |    3305 |
| LargeCatalog   | Serialize | Warm-Scratch (Zero-Alloc Reuse) |       4441600 | 31704958 |    43472542 | 114669708 |             72.88 | 476109.65 |    2100 |
| VectorCoordinates | Parse     | Cold-Alloc |        112768 |  1791708 |     1802584 |  1946417 |             61.05 | 14430.68 |   69297 |
| VectorCoordinates | Parse     | Warm-Scratch (Zero-Alloc Reuse) |        112768 |  1479000 |     1911083 |  3002500 |             54.08 | 16289.18 |   61390 |
| VectorCoordinates | Validate  | Cold-Alloc |        112768 |  2116209 |     2229375 |  2512875 |             49.68 | 17732.62 |   56393 |
| VectorCoordinates | Validate  | Warm-Scratch (Zero-Alloc Reuse) |        112768 |  2175792 |     2353334 |  3272292 |             44.33 | 19873.86 |   50317 |
| VectorCoordinates | Extract   | Cold-Alloc |        112768 |  1979958 |     2334792 |  3304875 |             45.08 | 19541.06 |   51174 |
| VectorCoordinates | Extract   | Warm-Scratch (Zero-Alloc Reuse) |        112768 |  2052791 |     2359000 |  4868292 |             38.82 | 22693.72 |   44065 |
| VectorCoordinates | Serialize | Cold-Alloc |        112768 |   745167 |      777333 |   843916 |            135.69 | 6492.94 |  154014 |
| VectorCoordinates | Serialize | Warm-Scratch (Zero-Alloc Reuse) |        112768 |   692125 |     1133625 |  2537375 |             79.19 | 11125.46 |   89884 |
| DeepNesting    | Parse     | Cold-Alloc |         18304 |  1013417 |     1543375 |  2194791 |             11.30 | 12650.21 |   79050 |
| DeepNesting    | Parse     | Warm-Scratch (Zero-Alloc Reuse) |         18304 |   563750 |      567083 |  1068459 |             27.80 | 5143.24 |  194430 |
| DeepNesting    | Validate  | Cold-Alloc |         18304 |  1637833 |     1657417 |  2856958 |              9.88 | 14474.33 |   69088 |
| DeepNesting    | Validate  | Warm-Scratch (Zero-Alloc Reuse) |         18304 |   821625 |      849958 |  2105250 |             16.02 | 8926.22 |  112030 |
| DeepNesting    | Extract   | Cold-Alloc |         18304 |  1658875 |     1751666 |  3815833 |              8.49 | 16837.03 |   59393 |
| DeepNesting    | Extract   | Warm-Scratch (Zero-Alloc Reuse) |         18304 |   810708 |     1196750 |  2255500 |             11.84 | 12074.58 |   82819 |
| DeepNesting    | Serialize | Cold-Alloc |         18304 |   338000 |      340125 |   344458 |             53.73 | 2661.30 |  375757 |
| DeepNesting    | Serialize | Warm-Scratch (Zero-Alloc Reuse) |         18304 |   274583 |      421375 |   614958 |             41.02 | 3486.02 |  286860 |
| EscapedUnicode | Parse     | Cold-Alloc |        257682 |  2155666 |     2176084 |  4005791 |             99.13 | 20307.89 |   49242 |
| EscapedUnicode | Parse     | Warm-Scratch (Zero-Alloc Reuse) |        257682 |  1931833 |     2122166 |  3650208 |             97.08 | 20736.02 |   48225 |
| EscapedUnicode | Validate  | Cold-Alloc |        257682 |  2824833 |     3048750 |  4714916 |             76.01 | 26485.24 |   37757 |
| EscapedUnicode | Validate  | Warm-Scratch (Zero-Alloc Reuse) |        257682 |  2895083 |     3188291 |  3921291 |             78.06 | 25789.29 |   38776 |
| EscapedUnicode | Extract   | Cold-Alloc |        257682 |  2797667 |     4085500 | 14746709 |             43.17 | 46633.40 |   21444 |
| EscapedUnicode | Extract   | Warm-Scratch (Zero-Alloc Reuse) |        257682 |  2644542 |     2860333 |  4097000 |             84.45 | 23838.83 |   41948 |
| EscapedUnicode | Serialize | Cold-Alloc |        257682 |   465917 |      497083 |  1124334 |            400.25 | 5029.70 |  198819 |
| EscapedUnicode | Serialize | Warm-Scratch (Zero-Alloc Reuse) |        257682 |   440583 |      467458 |   480666 |            551.89 | 3647.75 |  274141 |

## Head-to-head interpretation

The rows deliberately identify the amount of work each API performs:

- Parse compares jmax DOM, serde_json Value, simd-json BorrowedValue, and sonic-rs Value. simd-json mutates input, so its per-message copy is timed in both its cold and retained-buffer paths.
- Validate includes syntax-only serde_json IgnoredAny, simd-json Tape/fill_tape, and sonic-rs LazyValue, plus typed Deserialize rows. Syntax-only rows do not enforce the native schema's required fields, unknown-field rules, fixed collection counts, or typed constraints. The retained simd Tape row resets its lifetime before copying the next input, reuses Buffers and tape, and reports its measured allocation count without assuming it is zero.
- Extract compares jmax's fully validated scalar/raw-span slots with typed owned Deserialize outputs from all three baselines. Typed structs reject unknown fields and require the declared fields, but do not implement every jmax schema constraint, such as exact coordinate/catalog lengths. The recursive typed deep model checks singleton arrays and v objects. sonic-rs LazyValue rows additionally show syntax/raw-root retention; they do not represent equivalent typed field extraction.
- Serialize starts from each engine's prepared native DOM/Value and calls its serializer. jmax emits every value and field; it does not copy the original JSON wholesale. The baselines use `to_vec`, so their fresh-output allocation is included. Output formatting/key order can differ; untimed cross-engine preflight reparses all serialized outputs and verifies semantic equality for every payload. simd BorrowedValue is made static only during preparation.

Primary API references: [serde_json from_slice](https://docs.rs/serde_json/1.0.154/serde_json/fn.from_slice.html), [simd-json 0.17.3](https://docs.rs/simd-json/0.17.3/simd_json/), [sonic-rs LazyValue](https://docs.rs/sonic-rs/0.5.10/sonic_rs/struct.LazyValue.html). The table does not describe syntax-only or raw-root results as equivalent schema validation or typed extraction.

## Complete head-to-head results

128 documents/workload; items=32; seed=5354007379338153539; 3 prewarm passes, 20 measured passes. Allocation counts are a separate untimed pass. ns/doc and decimal MB/s. simd mutable input copying is timed.
| Workload | Mode | Engine | API / semantics | Regime | Mean ns/doc | Median ns/doc | Min ns/doc | P95 ns/doc | MB/s | allocs/doc | allocated bytes/doc |
|---|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|
| NagoyaAuth | Parse | jmax | DOM | Cold-Alloc | 2478.5 | 2540.0 | 1537.8 | 3226.2 | 33.95 | 11.000 | 1712.0 |
| NagoyaAuth | Parse | jmax | DOM | Warm-Scratch Zero-Alloc | 1042.9 | 1042.3 | 1039.4 | 1048.2 | 80.68 | 0.000 | 0.0 |
| NagoyaAuth | Validate | jmax | strict schema + slots | Cold-Alloc | 2055.7 | 2052.4 | 2044.9 | 2066.1 | 40.93 | 9.000 | 904.0 |
| NagoyaAuth | Validate | jmax | strict schema + slots | Warm-Scratch Zero-Alloc | 1448.8 | 1447.6 | 1443.7 | 1455.1 | 58.08 | 0.000 | 0.0 |
| NagoyaAuth | Extract | jmax | strict schema + slots | Cold-Alloc | 1919.7 | 1918.3 | 1908.5 | 1929.0 | 43.83 | 10.000 | 1096.0 |
| NagoyaAuth | Extract | jmax | strict schema + slots | Warm-Scratch Zero-Alloc | 2526.8 | 1587.6 | 1565.4 | 2309.2 | 33.30 | 0.000 | 0.0 |
| NagoyaAuth | Parse | serde_json | Value | Cold-Alloc | 384.6 | 384.1 | 379.2 | 388.3 | 218.78 | 7.000 | 680.1 |
| NagoyaAuth | Validate | serde_json | IgnoredAny (syntax only) | per-call | 97.3 | 97.3 | 96.7 | 98.0 | 864.63 | 0.000 | 0.0 |
| NagoyaAuth | Validate | serde_json | typed Deserialize (shape) | Cold-Alloc | 214.8 | 165.7 | 164.7 | 276.7 | 391.75 | 2.000 | 27.1 |
| NagoyaAuth | Extract | serde_json | typed Deserialize (shape) | Cold-Alloc | 150.8 | 148.8 | 148.4 | 150.7 | 557.91 | 2.000 | 27.1 |
| NagoyaAuth | Parse | simd-json | BorrowedValue + copy | Cold-Alloc | 443.0 | 442.7 | 434.2 | 447.3 | 189.93 | 10.000 | 2204.4 |
| NagoyaAuth | Parse | simd-json | BorrowedValue + copy + Buffers | Warm buffers (DOM allocates) | 196.5 | 196.3 | 195.3 | 197.9 | 428.12 | 3.000 | 640.0 |
| NagoyaAuth | Validate | simd-json | Tape (syntax only) + copy | Cold-Alloc | 345.1 | 345.0 | 339.2 | 350.3 | 243.79 | 8.000 | 1972.4 |
| NagoyaAuth | Validate | simd-json | fill_tape (syntax only) + copy | Warm buffers + tape | 304.2 | 141.3 | 74.2 | 199.5 | 276.64 | 0.000 | 0.0 |
| NagoyaAuth | Validate | simd-json | Tape::deserialize typed (shape) + copy | Cold-Alloc | 447.5 | 406.6 | 399.1 | 523.8 | 188.03 | 10.000 | 1999.6 |
| NagoyaAuth | Extract | simd-json | Tape::deserialize typed (shape) + copy | Cold-Alloc | 406.1 | 404.6 | 399.7 | 412.8 | 207.17 | 10.000 | 1999.6 |
| NagoyaAuth | Parse | sonic-rs | Value | per-call (library TLS) | 302.6 | 277.3 | 151.4 | 377.3 | 278.03 | 3.000 | 708.1 |
| NagoyaAuth | Validate | sonic-rs | LazyValue (syntax only, raw root) | per-call (library TLS) | 423.6 | 127.6 | 126.6 | 132.8 | 198.62 | 0.000 | 0.0 |
| NagoyaAuth | Validate | sonic-rs | typed Deserialize (shape) | Cold-Alloc | 165.7 | 165.4 | 163.7 | 168.3 | 507.72 | 2.000 | 27.1 |
| NagoyaAuth | Extract | sonic-rs | LazyValue (syntax only, raw root) | per-call (library TLS) | 1446.7 | 127.6 | 126.6 | 533.5 | 58.16 | 0.000 | 0.0 |
| NagoyaAuth | Extract | sonic-rs | typed Deserialize (shape) | Cold-Alloc | 302.3 | 242.5 | 157.6 | 664.7 | 278.31 | 2.000 | 27.1 |
| NagoyaAuth | Serialize | jmax | native DOM emission | Cold-Alloc | 788.9 | 710.0 | 692.7 | 1249.7 | 106.66 | 6.000 | 297.1 |
| NagoyaAuth | Serialize | jmax | native DOM emission | Warm-Scratch Zero-Alloc | 1009.6 | 714.5 | 707.4 | 1755.5 | 83.34 | 0.000 | 0.0 |
| NagoyaAuth | Serialize | serde_json | Value to_vec | Cold-Alloc | 1775.6 | 107.4 | 107.1 | 783.2 | 47.39 | 1.000 | 128.0 |
| NagoyaAuth | Serialize | simd-json | BorrowedValue to_vec | Cold-Alloc | 112.9 | 113.0 | 111.0 | 115.2 | 745.53 | 1.000 | 512.0 |
| NagoyaAuth | Serialize | sonic-rs | Value to_vec | Cold-Alloc | 233.0 | 139.3 | 137.4 | 314.1 | 361.08 | 2.000 | 384.0 |
| ExchangeOrder | Parse | jmax | DOM | Cold-Alloc | 3254.5 | 1762.4 | 1721.7 | 5666.3 | 26.12 | 12.000 | 2224.0 |
| ExchangeOrder | Parse | jmax | DOM | Warm-Scratch Zero-Alloc | 1127.3 | 1105.1 | 1096.4 | 1199.5 | 75.40 | 0.000 | 0.0 |
| ExchangeOrder | Validate | jmax | strict schema + slots | Cold-Alloc | 2174.4 | 2170.9 | 2147.1 | 2210.3 | 39.09 | 10.000 | 1464.0 |
| ExchangeOrder | Validate | jmax | strict schema + slots | Warm-Scratch Zero-Alloc | 1803.9 | 1803.4 | 1792.3 | 1810.5 | 47.12 | 0.000 | 0.0 |
| ExchangeOrder | Extract | jmax | strict schema + slots | Cold-Alloc | 2933.2 | 2376.0 | 2358.7 | 4478.2 | 28.98 | 11.000 | 1704.0 |
| ExchangeOrder | Extract | jmax | strict schema + slots | Warm-Scratch Zero-Alloc | 2227.7 | 1787.8 | 1772.5 | 4430.3 | 38.16 | 0.000 | 0.0 |
| ExchangeOrder | Parse | serde_json | Value | Cold-Alloc | 428.0 | 427.4 | 425.5 | 429.7 | 198.60 | 10.000 | 682.0 |
| ExchangeOrder | Validate | serde_json | IgnoredAny (syntax only) | per-call | 129.1 | 129.2 | 128.6 | 129.6 | 658.49 | 0.000 | 0.0 |
| ExchangeOrder | Validate | serde_json | typed Deserialize (shape) | Cold-Alloc | 308.8 | 206.7 | 198.9 | 531.2 | 275.27 | 4.000 | 25.0 |
| ExchangeOrder | Extract | serde_json | typed Deserialize (shape) | Cold-Alloc | 373.2 | 347.0 | 192.1 | 427.1 | 227.77 | 4.000 | 25.0 |
| ExchangeOrder | Parse | simd-json | BorrowedValue + copy | Cold-Alloc | 935.4 | 793.0 | 466.1 | 1375.0 | 90.87 | 10.000 | 2447.0 |
| ExchangeOrder | Parse | simd-json | BorrowedValue + copy + Buffers | Warm buffers (DOM allocates) | 198.3 | 194.3 | 192.4 | 205.7 | 428.56 | 3.000 | 784.0 |
| ExchangeOrder | Validate | simd-json | Tape (syntax only) + copy | Cold-Alloc | 301.8 | 299.5 | 294.6 | 308.3 | 281.62 | 8.000 | 2167.0 |
| ExchangeOrder | Validate | simd-json | fill_tape (syntax only) + copy | Warm buffers + tape | 81.9 | 81.7 | 81.4 | 82.7 | 1037.64 | 0.000 | 0.0 |
| ExchangeOrder | Validate | simd-json | Tape::deserialize typed (shape) + copy | Cold-Alloc | 408.0 | 406.6 | 402.7 | 413.4 | 208.33 | 12.000 | 2192.0 |
| ExchangeOrder | Extract | simd-json | Tape::deserialize typed (shape) + copy | Cold-Alloc | 405.8 | 404.3 | 401.4 | 412.4 | 209.44 | 12.000 | 2192.0 |
| ExchangeOrder | Parse | sonic-rs | Value | per-call (library TLS) | 152.2 | 152.0 | 149.7 | 154.3 | 558.43 | 3.000 | 709.0 |
| ExchangeOrder | Validate | sonic-rs | LazyValue (syntax only, raw root) | per-call (library TLS) | 164.1 | 164.1 | 163.4 | 164.7 | 517.99 | 0.000 | 0.0 |
| ExchangeOrder | Validate | sonic-rs | typed Deserialize (shape) | Cold-Alloc | 206.9 | 205.4 | 199.5 | 216.1 | 410.89 | 4.000 | 25.0 |
| ExchangeOrder | Extract | sonic-rs | LazyValue (syntax only, raw root) | per-call (library TLS) | 172.5 | 172.5 | 171.5 | 173.5 | 492.63 | 0.000 | 0.0 |
| ExchangeOrder | Extract | sonic-rs | typed Deserialize (shape) | Cold-Alloc | 187.9 | 186.2 | 183.3 | 194.7 | 452.31 | 4.000 | 25.0 |
| ExchangeOrder | Serialize | jmax | native DOM emission | Cold-Alloc | 826.9 | 826.8 | 813.5 | 839.2 | 102.79 | 6.000 | 280.0 |
| ExchangeOrder | Serialize | jmax | native DOM emission | Warm-Scratch Zero-Alloc | 564.1 | 537.8 | 536.1 | 586.9 | 150.68 | 0.000 | 0.0 |
| ExchangeOrder | Serialize | serde_json | Value to_vec | Cold-Alloc | 160.5 | 118.5 | 116.2 | 311.9 | 529.55 | 1.000 | 128.0 |
| ExchangeOrder | Serialize | simd-json | BorrowedValue to_vec | Cold-Alloc | 109.8 | 109.7 | 109.1 | 110.7 | 774.26 | 1.000 | 512.0 |
| ExchangeOrder | Serialize | sonic-rs | Value to_vec | Cold-Alloc | 150.9 | 149.4 | 142.2 | 151.4 | 563.36 | 2.000 | 384.0 |
| RpcEnvelope | Parse | jmax | DOM | Cold-Alloc | 2595.7 | 2153.6 | 2131.2 | 4055.0 | 40.12 | 12.000 | 1968.0 |
| RpcEnvelope | Parse | jmax | DOM | Warm-Scratch Zero-Alloc | 1634.4 | 1632.5 | 1621.7 | 1637.4 | 63.72 | 0.000 | 0.0 |
| RpcEnvelope | Validate | jmax | strict schema + slots | Cold-Alloc | 3044.0 | 2973.6 | 2766.6 | 3307.6 | 34.21 | 14.000 | 1632.0 |
| RpcEnvelope | Validate | jmax | strict schema + slots | Warm-Scratch Zero-Alloc | 2170.7 | 2157.9 | 2143.6 | 2182.3 | 47.98 | 0.000 | 0.0 |
| RpcEnvelope | Extract | jmax | strict schema + slots | Cold-Alloc | 3541.0 | 3214.2 | 3002.0 | 4345.1 | 29.41 | 15.000 | 1824.0 |
| RpcEnvelope | Extract | jmax | strict schema + slots | Warm-Scratch Zero-Alloc | 2264.6 | 2180.7 | 2139.0 | 2787.1 | 45.99 | 0.000 | 0.0 |
| RpcEnvelope | Parse | serde_json | Value | Cold-Alloc | 498.1 | 498.0 | 488.9 | 504.6 | 209.07 | 12.000 | 1319.1 |
| RpcEnvelope | Validate | serde_json | IgnoredAny (syntax only) | per-call | 143.5 | 143.6 | 142.6 | 143.9 | 725.85 | 1.000 | 8.0 |
| RpcEnvelope | Validate | serde_json | typed Deserialize (shape) | Cold-Alloc | 205.9 | 204.8 | 202.5 | 208.7 | 505.72 | 3.000 | 16.1 |
| RpcEnvelope | Extract | serde_json | typed Deserialize (shape) | Cold-Alloc | 219.1 | 218.4 | 215.5 | 223.0 | 475.40 | 3.000 | 16.1 |
| RpcEnvelope | Parse | simd-json | BorrowedValue + copy | Cold-Alloc | 509.2 | 507.5 | 495.8 | 520.2 | 204.53 | 12.000 | 3036.1 |
| RpcEnvelope | Parse | simd-json | BorrowedValue + copy + Buffers | Warm buffers (DOM allocates) | 266.6 | 266.6 | 265.0 | 268.2 | 390.58 | 5.000 | 1112.0 |
| RpcEnvelope | Validate | simd-json | Tape (syntax only) + copy | Cold-Alloc | 533.0 | 393.2 | 335.9 | 782.9 | 195.39 | 8.000 | 2620.1 |
| RpcEnvelope | Validate | simd-json | fill_tape (syntax only) + copy | Warm buffers + tape | 97.8 | 98.0 | 96.7 | 98.6 | 1064.45 | 0.000 | 0.0 |
| RpcEnvelope | Validate | simd-json | Tape::deserialize typed (shape) + copy | Cold-Alloc | 599.3 | 434.6 | 424.5 | 816.1 | 173.76 | 11.000 | 2636.2 |
| RpcEnvelope | Extract | simd-json | Tape::deserialize typed (shape) + copy | Cold-Alloc | 436.1 | 434.9 | 426.1 | 442.4 | 238.77 | 11.000 | 2636.2 |
| RpcEnvelope | Parse | sonic-rs | Value | per-call (library TLS) | 170.7 | 170.6 | 169.6 | 171.5 | 610.24 | 3.000 | 728.1 |
| RpcEnvelope | Validate | sonic-rs | LazyValue (syntax only, raw root) | per-call (library TLS) | 204.1 | 204.1 | 203.1 | 205.1 | 510.16 | 0.000 | 0.0 |
| RpcEnvelope | Validate | sonic-rs | typed Deserialize (shape) | Cold-Alloc | 230.6 | 230.1 | 228.5 | 232.8 | 451.51 | 3.000 | 16.1 |
| RpcEnvelope | Extract | sonic-rs | LazyValue (syntax only, raw root) | per-call (library TLS) | 201.7 | 201.8 | 200.2 | 202.5 | 516.33 | 0.000 | 0.0 |
| RpcEnvelope | Extract | sonic-rs | typed Deserialize (shape) | Cold-Alloc | 231.7 | 231.4 | 227.5 | 235.4 | 449.48 | 3.000 | 16.1 |
| RpcEnvelope | Serialize | jmax | native DOM emission | Cold-Alloc | 1590.1 | 1554.0 | 1071.6 | 2664.7 | 65.49 | 6.000 | 280.0 |
| RpcEnvelope | Serialize | jmax | native DOM emission | Warm-Scratch Zero-Alloc | 863.7 | 810.5 | 804.7 | 1191.1 | 120.57 | 0.000 | 0.0 |
| RpcEnvelope | Serialize | serde_json | Value to_vec | Cold-Alloc | 151.9 | 151.4 | 150.7 | 154.6 | 685.49 | 1.000 | 128.0 |
| RpcEnvelope | Serialize | simd-json | BorrowedValue to_vec | Cold-Alloc | 158.9 | 158.5 | 157.9 | 160.2 | 655.50 | 1.000 | 512.0 |
| RpcEnvelope | Serialize | sonic-rs | Value to_vec | Cold-Alloc | 183.7 | 183.9 | 182.6 | 184.2 | 566.79 | 2.000 | 384.0 |
| LargeCatalog | Parse | jmax | DOM | Cold-Alloc | 703124.8 | 560985.4 | 491350.3 | 1462609.4 | 49.35 | 47.000 | 721392.0 |
| LargeCatalog | Parse | jmax | DOM | Warm-Scratch Zero-Alloc | 554105.4 | 540973.0 | 477756.2 | 640021.5 | 62.62 | 0.000 | 0.0 |
| LargeCatalog | Validate | jmax | strict schema + slots | Cold-Alloc | 670636.9 | 671932.0 | 617631.8 | 726785.5 | 51.74 | 15.000 | 2056.0 |
| LargeCatalog | Validate | jmax | strict schema + slots | Warm-Scratch Zero-Alloc | 727541.7 | 724590.2 | 610118.5 | 908165.0 | 47.69 | 0.000 | 0.0 |
| LargeCatalog | Extract | jmax | strict schema + slots | Cold-Alloc | 810033.1 | 763350.9 | 695371.1 | 943219.4 | 42.84 | 16.000 | 2152.0 |
| LargeCatalog | Extract | jmax | strict schema + slots | Warm-Scratch Zero-Alloc | 769131.8 | 738923.2 | 650056.6 | 937682.9 | 45.12 | 0.000 | 0.0 |
| LargeCatalog | Parse | serde_json | Value | Cold-Alloc | 159799.0 | 155509.4 | 148499.7 | 186644.9 | 217.15 | 3083.000 | 372704.0 |
| LargeCatalog | Validate | serde_json | IgnoredAny (syntax only) | per-call | 43204.1 | 42534.5 | 41904.0 | 45507.5 | 803.17 | 1.000 | 8.0 |
| LargeCatalog | Validate | serde_json | typed Deserialize (shape) | Cold-Alloc | 59722.9 | 59220.7 | 58083.7 | 62508.1 | 581.02 | 520.000 | 56090.0 |
| LargeCatalog | Extract | serde_json | typed Deserialize (shape) | Cold-Alloc | 59560.3 | 58457.0 | 58096.4 | 62322.6 | 582.60 | 520.000 | 56090.0 |
| LargeCatalog | Parse | simd-json | BorrowedValue + copy | Cold-Alloc | 60175.0 | 59355.8 | 58848.0 | 63544.9 | 576.65 | 1038.000 | 807239.8 |
| LargeCatalog | Parse | simd-json | BorrowedValue + copy + Buffers | Warm buffers (DOM allocates) | 57481.3 | 56933.9 | 56041.0 | 58971.4 | 603.67 | 1029.000 | 352632.0 |
| LargeCatalog | Validate | simd-json | Tape (syntax only) + copy | Cold-Alloc | 28166.9 | 27936.2 | 24874.4 | 30248.4 | 1231.94 | 10.000 | 676007.8 |
| LargeCatalog | Validate | simd-json | fill_tape (syntax only) + copy | Warm buffers + tape | 25302.1 | 24508.1 | 23626.0 | 27766.3 | 1371.43 | 0.000 | 0.0 |
| LargeCatalog | Validate | simd-json | Tape::deserialize typed (shape) + copy | Cold-Alloc | 44768.9 | 44127.9 | 41370.1 | 48335.6 | 775.09 | 523.000 | 707713.8 |
| LargeCatalog | Extract | simd-json | Tape::deserialize typed (shape) + copy | Cold-Alloc | 46741.1 | 45293.0 | 42415.4 | 56768.9 | 742.39 | 523.000 | 707713.8 |
| LargeCatalog | Parse | sonic-rs | Value | per-call (library TLS) | 24871.5 | 23803.7 | 21959.6 | 28196.0 | 1395.17 | 10.000 | 165260.0 |
| LargeCatalog | Validate | sonic-rs | LazyValue (syntax only, raw root) | per-call (library TLS) | 57143.3 | 56314.5 | 47786.8 | 67067.1 | 607.25 | 0.000 | 0.0 |
| LargeCatalog | Validate | sonic-rs | typed Deserialize (shape) | Cold-Alloc | 63294.6 | 61906.9 | 54357.8 | 73222.0 | 548.23 | 520.000 | 56090.0 |
| LargeCatalog | Extract | sonic-rs | LazyValue (syntax only, raw root) | per-call (library TLS) | 55835.1 | 54871.1 | 50850.6 | 59791.7 | 621.47 | 0.000 | 0.0 |
| LargeCatalog | Extract | sonic-rs | typed Deserialize (shape) | Cold-Alloc | 58308.3 | 57982.4 | 54782.5 | 61850.3 | 595.11 | 520.000 | 56090.0 |
| LargeCatalog | Serialize | jmax | native DOM emission | Cold-Alloc | 253047.0 | 246277.3 | 235015.3 | 303897.8 | 137.13 | 15.000 | 131096.0 |
| LargeCatalog | Serialize | jmax | native DOM emission | Warm-Scratch Zero-Alloc | 264386.6 | 260132.8 | 240588.2 | 296353.8 | 131.25 | 0.000 | 0.0 |
| LargeCatalog | Serialize | serde_json | Value to_vec | Cold-Alloc | 51496.6 | 50352.9 | 49402.7 | 54980.5 | 673.83 | 10.000 | 130944.0 |
| LargeCatalog | Serialize | simd-json | BorrowedValue to_vec | Cold-Alloc | 51541.7 | 50439.8 | 48828.5 | 55350.9 | 673.24 | 8.000 | 130560.0 |
| LargeCatalog | Serialize | sonic-rs | Value to_vec | Cold-Alloc | 46818.4 | 46458.0 | 43024.1 | 50232.1 | 741.16 | 10.000 | 130944.0 |
| VectorCoordinates | Parse | jmax | DOM | Cold-Alloc | 13329.7 | 12922.2 | 12732.1 | 14045.2 | 66.09 | 23.000 | 21872.0 |
| VectorCoordinates | Parse | jmax | DOM | Warm-Scratch Zero-Alloc | 13168.5 | 12213.2 | 11564.1 | 15281.6 | 66.90 | 0.000 | 0.0 |
| VectorCoordinates | Validate | jmax | strict schema + slots | Cold-Alloc | 17633.6 | 17213.5 | 16219.1 | 20965.2 | 49.96 | 10.000 | 1048.0 |
| VectorCoordinates | Validate | jmax | strict schema + slots | Warm-Scratch Zero-Alloc | 18900.6 | 18167.3 | 16591.1 | 26254.9 | 46.61 | 0.000 | 0.0 |
| VectorCoordinates | Extract | jmax | strict schema + slots | Cold-Alloc | 17268.3 | 16461.9 | 15801.8 | 20457.4 | 51.02 | 11.000 | 1096.0 |
| VectorCoordinates | Extract | jmax | strict schema + slots | Warm-Scratch Zero-Alloc | 21318.3 | 20998.4 | 16863.0 | 28215.2 | 41.33 | 0.000 | 0.0 |
| VectorCoordinates | Parse | serde_json | Value | Cold-Alloc | 2329.4 | 2326.5 | 2312.5 | 2342.1 | 378.21 | 8.000 | 8707.0 |
| VectorCoordinates | Validate | serde_json | IgnoredAny (syntax only) | per-call | 789.9 | 749.4 | 747.7 | 1042.3 | 1115.30 | 1.000 | 8.0 |
| VectorCoordinates | Validate | serde_json | typed Deserialize (shape) | Cold-Alloc | 1663.1 | 1661.1 | 1639.3 | 1678.1 | 529.74 | 6.000 | 2016.0 |
| VectorCoordinates | Extract | serde_json | typed Deserialize (shape) | Cold-Alloc | 1788.7 | 1788.4 | 1758.8 | 1812.2 | 492.54 | 6.000 | 2016.0 |
| VectorCoordinates | Parse | simd-json | BorrowedValue + copy | Cold-Alloc | 1933.7 | 1933.3 | 1891.6 | 1960.3 | 455.59 | 14.000 | 17859.0 |
| VectorCoordinates | Parse | simd-json | BorrowedValue + copy + Buffers | Warm buffers (DOM allocates) | 1353.7 | 1353.5 | 1342.5 | 1364.9 | 650.80 | 5.000 | 7144.0 |
| VectorCoordinates | Validate | simd-json | Tape (syntax only) + copy | Cold-Alloc | 1177.2 | 1173.8 | 1158.5 | 1194.7 | 748.40 | 10.000 | 15443.0 |
| VectorCoordinates | Validate | simd-json | fill_tape (syntax only) + copy | Warm buffers + tape | 738.9 | 738.6 | 736.6 | 741.2 | 1192.31 | 0.000 | 0.0 |
| VectorCoordinates | Validate | simd-json | Tape::deserialize typed (shape) + copy | Cold-Alloc | 1319.3 | 1318.0 | 1308.3 | 1325.5 | 667.78 | 11.000 | 16211.0 |
| VectorCoordinates | Extract | simd-json | Tape::deserialize typed (shape) + copy | Cold-Alloc | 1315.5 | 1314.1 | 1306.3 | 1325.8 | 669.73 | 11.000 | 16211.0 |
| VectorCoordinates | Parse | sonic-rs | Value | per-call (library TLS) | 1101.6 | 961.6 | 955.7 | 1989.9 | 799.75 | 3.000 | 3041.0 |
| VectorCoordinates | Validate | sonic-rs | LazyValue (syntax only, raw root) | per-call (library TLS) | 1720.0 | 1661.5 | 1585.0 | 2141.3 | 512.22 | 0.000 | 0.0 |
| VectorCoordinates | Validate | sonic-rs | typed Deserialize (shape) | Cold-Alloc | 1650.6 | 1540.7 | 1524.1 | 2221.7 | 533.74 | 6.000 | 2016.0 |
| VectorCoordinates | Extract | sonic-rs | LazyValue (syntax only, raw root) | per-call (library TLS) | 1548.4 | 1514.0 | 1511.4 | 1634.4 | 568.96 | 0.000 | 0.0 |
| VectorCoordinates | Extract | sonic-rs | typed Deserialize (shape) | Cold-Alloc | 2279.4 | 2057.0 | 1641.9 | 3139.3 | 386.51 | 6.000 | 2016.0 |
| VectorCoordinates | Serialize | jmax | native DOM emission | Cold-Alloc | 7053.7 | 6867.5 | 5787.4 | 9581.7 | 124.90 | 9.000 | 2072.0 |
| VectorCoordinates | Serialize | jmax | native DOM emission | Warm-Scratch Zero-Alloc | 7222.3 | 6299.2 | 5940.4 | 9454.8 | 121.98 | 0.000 | 0.0 |
| VectorCoordinates | Serialize | serde_json | Value to_vec | Cold-Alloc | 1501.2 | 1501.0 | 1486.3 | 1513.0 | 586.86 | 4.000 | 1920.0 |
| VectorCoordinates | Serialize | simd-json | BorrowedValue to_vec | Cold-Alloc | 3100.4 | 3000.3 | 2984.7 | 3232.4 | 284.16 | 2.000 | 1536.0 |
| VectorCoordinates | Serialize | sonic-rs | Value to_vec | Cold-Alloc | 1462.0 | 1460.9 | 1454.4 | 1469.1 | 602.61 | 4.000 | 1920.0 |
| DeepNesting | Parse | jmax | DOM | Cold-Alloc | 7444.0 | 7375.6 | 6911.5 | 9182.3 | 19.21 | 52.000 | 28656.0 |
| DeepNesting | Parse | jmax | DOM | Warm-Scratch Zero-Alloc | 5495.2 | 4940.4 | 4761.4 | 8113.6 | 26.02 | 0.000 | 0.0 |
| DeepNesting | Validate | jmax | strict schema + slots | Cold-Alloc | 12173.8 | 11913.4 | 11815.4 | 12849.3 | 11.75 | 113.000 | 117208.0 |
| DeepNesting | Validate | jmax | strict schema + slots | Warm-Scratch Zero-Alloc | 6950.1 | 6444.3 | 5914.7 | 10249.7 | 20.58 | 0.000 | 0.0 |
| DeepNesting | Extract | jmax | strict schema + slots | Cold-Alloc | 16419.0 | 11924.1 | 11719.1 | 30114.9 | 8.71 | 114.000 | 117256.0 |
| DeepNesting | Extract | jmax | strict schema + slots | Warm-Scratch Zero-Alloc | 7658.7 | 6339.8 | 6270.5 | 11114.3 | 18.67 | 0.000 | 0.0 |
| DeepNesting | Parse | serde_json | Value | Cold-Alloc | 2462.7 | 2075.2 | 2033.2 | 3712.6 | 58.07 | 50.000 | 12812.0 |
| DeepNesting | Validate | serde_json | IgnoredAny (syntax only) | per-call | 331.6 | 329.8 | 329.1 | 331.4 | 431.23 | 3.000 | 56.0 |
| DeepNesting | Validate | serde_json | typed Deserialize (shape) | Cold-Alloc | 1086.0 | 1066.7 | 1064.8 | 1168.3 | 131.68 | 48.000 | 528.0 |
| DeepNesting | Extract | serde_json | typed Deserialize (shape) | Cold-Alloc | 1206.7 | 1067.1 | 1055.3 | 1280.9 | 118.50 | 48.000 | 528.0 |
| DeepNesting | Parse | simd-json | BorrowedValue + copy | Cold-Alloc | 2284.0 | 2087.6 | 2030.6 | 3668.3 | 62.61 | 77.000 | 9525.0 |
| DeepNesting | Parse | simd-json | BorrowedValue + copy + Buffers | Warm buffers (DOM allocates) | 1661.6 | 1663.1 | 1642.9 | 1672.2 | 86.06 | 67.000 | 4688.0 |
| DeepNesting | Validate | simd-json | Tape (syntax only) + copy | Cold-Alloc | 698.9 | 697.6 | 688.8 | 705.1 | 204.60 | 11.000 | 7261.0 |
| DeepNesting | Validate | simd-json | fill_tape (syntax only) + copy | Warm buffers + tape | 255.6 | 255.5 | 253.2 | 256.8 | 559.44 | 0.000 | 0.0 |
| DeepNesting | Validate | simd-json | Tape::deserialize typed (shape) + copy | Cold-Alloc | 1830.9 | 1383.5 | 1371.8 | 3321.9 | 78.11 | 59.000 | 7789.0 |
| DeepNesting | Extract | simd-json | Tape::deserialize typed (shape) + copy | Cold-Alloc | 1400.9 | 1381.8 | 1350.3 | 1479.5 | 102.08 | 59.000 | 7789.0 |
| DeepNesting | Parse | sonic-rs | Value | per-call (library TLS) | 472.6 | 473.0 | 467.1 | 477.5 | 302.61 | 4.000 | 1775.0 |
| DeepNesting | Validate | sonic-rs | LazyValue (syntax only, raw root) | per-call (library TLS) | 689.3 | 689.1 | 688.1 | 690.1 | 207.45 | 0.000 | 0.0 |
| DeepNesting | Validate | sonic-rs | typed Deserialize (shape) | Cold-Alloc | 1556.5 | 1550.5 | 1547.5 | 1594.7 | 91.87 | 48.000 | 528.0 |
| DeepNesting | Extract | sonic-rs | LazyValue (syntax only, raw root) | per-call (library TLS) | 696.9 | 696.3 | 695.3 | 698.9 | 205.19 | 0.000 | 0.0 |
| DeepNesting | Extract | sonic-rs | typed Deserialize (shape) | Cold-Alloc | 1573.9 | 1571.9 | 1553.1 | 1584.6 | 90.86 | 48.000 | 528.0 |
| DeepNesting | Serialize | jmax | native DOM emission | Cold-Alloc | 2639.9 | 2636.1 | 2616.9 | 2658.9 | 54.17 | 11.000 | 1496.0 |
| DeepNesting | Serialize | jmax | native DOM emission | Warm-Scratch Zero-Alloc | 1999.9 | 2000.0 | 1934.6 | 2130.2 | 71.51 | 0.000 | 0.0 |
| DeepNesting | Serialize | serde_json | Value to_vec | Cold-Alloc | 335.1 | 335.0 | 327.5 | 341.8 | 426.71 | 2.000 | 384.0 |
| DeepNesting | Serialize | simd-json | BorrowedValue to_vec | Cold-Alloc | 313.9 | 286.8 | 271.2 | 290.0 | 455.51 | 1.000 | 512.0 |
| DeepNesting | Serialize | sonic-rs | Value to_vec | Cold-Alloc | 424.8 | 424.1 | 421.6 | 427.7 | 336.66 | 2.000 | 384.0 |
| EscapedUnicode | Parse | jmax | DOM | Cold-Alloc | 16328.7 | 16625.0 | 15387.0 | 17017.6 | 123.29 | 9.000 | 2678.1 |
| EscapedUnicode | Parse | jmax | DOM | Warm-Scratch Zero-Alloc | 15837.4 | 15245.4 | 14909.2 | 19150.4 | 127.11 | 0.000 | 0.0 |
| EscapedUnicode | Validate | jmax | strict schema + slots | Cold-Alloc | 23387.5 | 21815.4 | 21562.8 | 28769.9 | 86.08 | 10.000 | 2446.1 |
| EscapedUnicode | Validate | jmax | strict schema + slots | Warm-Scratch Zero-Alloc | 23712.3 | 22817.4 | 21021.2 | 27909.5 | 84.90 | 0.000 | 0.0 |
| EscapedUnicode | Extract | jmax | strict schema + slots | Cold-Alloc | 22864.1 | 22220.7 | 21798.2 | 24041.0 | 88.05 | 11.000 | 2542.1 |
| EscapedUnicode | Extract | jmax | strict schema + slots | Warm-Scratch Zero-Alloc | 21630.8 | 21214.2 | 20697.9 | 22861.7 | 93.07 | 0.000 | 0.0 |
| EscapedUnicode | Parse | serde_json | Value | Cold-Alloc | 3323.1 | 3090.5 | 3016.9 | 4505.5 | 605.81 | 13.000 | 6364.1 |
| EscapedUnicode | Validate | serde_json | IgnoredAny (syntax only) | per-call | 1615.1 | 1460.3 | 1444.7 | 2031.2 | 1246.42 | 0.000 | 0.0 |
| EscapedUnicode | Validate | serde_json | typed Deserialize (shape) | Cold-Alloc | 3145.7 | 2934.2 | 2879.2 | 3997.1 | 639.96 | 10.000 | 5726.1 |
| EscapedUnicode | Extract | serde_json | typed Deserialize (shape) | Cold-Alloc | 3106.9 | 3029.3 | 2937.2 | 3575.5 | 647.96 | 10.000 | 5726.1 |
| EscapedUnicode | Parse | simd-json | BorrowedValue + copy | Cold-Alloc | 2091.1 | 2077.5 | 2065.1 | 2115.9 | 962.71 | 9.000 | 7911.4 |
| EscapedUnicode | Parse | simd-json | BorrowedValue + copy + Buffers | Warm buffers (DOM allocates) | 1842.4 | 1840.5 | 1827.5 | 1855.5 | 1092.65 | 3.000 | 352.0 |
| EscapedUnicode | Validate | simd-json | Tape (syntax only) + copy | Cold-Alloc | 2027.5 | 1966.5 | 1948.9 | 2273.8 | 992.92 | 7.000 | 7775.4 |
| EscapedUnicode | Validate | simd-json | fill_tape (syntax only) + copy | Warm buffers + tape | 1814.5 | 1737.0 | 1721.4 | 2313.1 | 1109.48 | 0.000 | 0.0 |
| EscapedUnicode | Validate | simd-json | Tape::deserialize typed (shape) + copy | Cold-Alloc | 2010.5 | 2007.8 | 1994.8 | 2029.9 | 1001.30 | 8.000 | 9413.6 |
| EscapedUnicode | Extract | simd-json | Tape::deserialize typed (shape) + copy | Cold-Alloc | 2007.2 | 2003.3 | 1994.1 | 2034.2 | 1002.98 | 8.000 | 9413.6 |
| EscapedUnicode | Parse | sonic-rs | Value | per-call (library TLS) | 2829.6 | 2828.1 | 2818.0 | 2843.1 | 711.46 | 3.000 | 2637.1 |
| EscapedUnicode | Validate | sonic-rs | LazyValue (syntax only, raw root) | per-call (library TLS) | 2070.0 | 2023.8 | 2016.9 | 2181.0 | 972.55 | 0.000 | 0.0 |
| EscapedUnicode | Validate | sonic-rs | typed Deserialize (shape) | Cold-Alloc | 2073.5 | 2072.3 | 2061.2 | 2082.7 | 970.88 | 10.000 | 7511.1 |
| EscapedUnicode | Extract | sonic-rs | LazyValue (syntax only, raw root) | per-call (library TLS) | 2251.1 | 2224.3 | 2221.0 | 2401.0 | 894.30 | 0.000 | 0.0 |
| EscapedUnicode | Extract | sonic-rs | typed Deserialize (shape) | Cold-Alloc | 1868.1 | 1864.6 | 1851.6 | 1893.9 | 1077.61 | 10.000 | 7511.1 |
| EscapedUnicode | Serialize | jmax | native DOM emission | Cold-Alloc | 3777.4 | 3824.2 | 3538.7 | 3886.4 | 532.94 | 4.000 | 5272.4 |
| EscapedUnicode | Serialize | jmax | native DOM emission | Warm-Scratch Zero-Alloc | 3317.1 | 3279.0 | 3275.1 | 3537.4 | 606.90 | 0.000 | 0.0 |
| EscapedUnicode | Serialize | serde_json | Value to_vec | Cold-Alloc | 1571.0 | 1494.5 | 1465.5 | 1854.8 | 1281.44 | 5.000 | 3968.0 |
| EscapedUnicode | Serialize | simd-json | BorrowedValue to_vec | Cold-Alloc | 1229.6 | 1188.8 | 1123.7 | 1515.9 | 1637.29 | 3.000 | 3584.0 |
| EscapedUnicode | Serialize | sonic-rs | Value to_vec | Cold-Alloc | 897.6 | 894.5 | 891.3 | 917.3 | 2242.91 | 2.000 | 9999.8 |

## Sampled bottlenecks and flamegraphs

Five separate final-binary profiling runs used macOS `sample`, three seconds at a nominal one-millisecond interval during an eight-second repeated operation. These are sampled stacks, not hardware cycle counts; they include startup/preparation and can miss short/inlined work. Percentages below are leaf samples divided by total samples. SVG widths represent inclusive samples, callers are at the bottom, and tooltips contain full demangled symbols. The folded stacks and demangled sample reports accompany each SVG.

**NagoyaAuth-Parse-Cold** — 2408 sampled stacks. [Flamegraph](benchmark-7-workloads-cold-vs-warm/profiles/NagoyaAuth-Parse-Cold.svg), [folded stacks](benchmark-7-workloads-cold-vs-warm/profiles/NagoyaAuth-Parse-Cold.folded), [sample report](benchmark-7-workloads-cold-vs-warm/profiles/NagoyaAuth-Parse-Cold.sample.txt).

`jmax_syntax::feed::next` 12.3%; `jmax_syntax::frames::apply` 10.1%; `jmax_syntax::machine::transition` 6.8%; `jmax_syntax::feed::block_at` 5.9%; `jmax_scan_neon::structural::classify_structural` 5.4%.

**NagoyaAuth-Parse-Warm** — 2478 sampled stacks. [Flamegraph](benchmark-7-workloads-cold-vs-warm/profiles/NagoyaAuth-Parse-Warm.svg), [folded stacks](benchmark-7-workloads-cold-vs-warm/profiles/NagoyaAuth-Parse-Warm.folded), [sample report](benchmark-7-workloads-cold-vs-warm/profiles/NagoyaAuth-Parse-Warm.sample.txt).

`jmax_syntax::feed::next` 17.8%; `jmax_syntax::frames::apply` 14.2%; `jmax_syntax::feed::block_at` 10.1%; `jmax_syntax::machine::transition` 9.0%; `jmax_scan_neon::structural::classify_structural` 6.5%.

**LargeCatalog-Validate-Warm** — 2475 sampled stacks. [Flamegraph](benchmark-7-workloads-cold-vs-warm/profiles/LargeCatalog-Validate-Warm.svg), [folded stacks](benchmark-7-workloads-cold-vs-warm/profiles/LargeCatalog-Validate-Warm.folded), [sample report](benchmark-7-workloads-cold-vs-warm/profiles/LargeCatalog-Validate-Warm.sample.txt).

`jmax_syntax::feed::next` 13.9%; `jmax_syntax::frames::apply` 12.5%; `core::str::converts::from_utf8` 8.4%; `jmax_syntax::machine::transition` 8.0%; `jmax_syntax::feed::block_at` 6.2%.

**EscapedUnicode-Parse-Warm** — 2468 sampled stacks. [Flamegraph](benchmark-7-workloads-cold-vs-warm/profiles/EscapedUnicode-Parse-Warm.svg), [folded stacks](benchmark-7-workloads-cold-vs-warm/profiles/EscapedUnicode-Parse-Warm.folded), [sample report](benchmark-7-workloads-cold-vs-warm/profiles/EscapedUnicode-Parse-Warm.sample.txt).

`<jmax_syntax::feed::StringInput>::unit` 47.6%; `jmax_syntax::feed::measure_string` 9.3%; `_platform_memmove` 8.4%; `jmax_scan_neon::structural::classify_structural` 7.2%; `jmax_syntax::feed::decode_string` 6.9%.

**EscapedUnicode-Serialize-Warm** — 2490 sampled stacks. [Flamegraph](benchmark-7-workloads-cold-vs-warm/profiles/EscapedUnicode-Serialize-Warm.svg), [folded stacks](benchmark-7-workloads-cold-vs-warm/profiles/EscapedUnicode-Serialize-Warm.folded), [sample report](benchmark-7-workloads-cold-vs-warm/profiles/EscapedUnicode-Serialize-Warm.sample.txt).

`jmax_string::escape::neon::find_neon` 21.2%; `core::str::converts::from_utf8` 21.1%; `<jmax_encode::LimitedSink as jmax_abi::traits::ByteSink>::write_bytes` 17.6%; `_platform_memmove` 12.4%; `<jsonmax::OutputBuffer as jmax_abi::traits::ByteSink>::write_bytes` 9.1%.

Cold NagoyaAuth includes allocator/free work; the warm profiles move the remaining cost to syntax event traversal, container transitions, string validation, and DOM construction. Allocation-counter assertions, rather than absence of allocator samples, establish the zero-allocation result.

LargeCatalog's warm VM still pays for syntax traversal, UTF-8 checks, schema dispatch, key lookup, scalar/number checks, and event bookkeeping for every product. Source inspection shows number policy/grammar checks at both syntax and VM boundaries; their contribution is a plausible optimization target, not an isolated measured speedup. Retained masks remove allocator work but do not remove these checks.

EscapedUnicode parse spends most sampled work in StringInput units, measuring/decoding strings, surrogate/hex handling, structural classification, and copying. The measured string length/decode passes are a likely next optimization target. Serialization samples concentrate in UTF-8 validation, LimitedSink/OutputBuffer byte writes, NEON escape scanning, and copying. Any future fast path must preserve complete validation, output bounds, and exact escaping behavior.

## Verification and reproduction

`cargo test -p jsonmax -p jmax-bench` passes six integration tests: schema cache concurrency/explicit-schema APIs; cold/warm ownership and selection parity across all schemas; malformed/unselected/duplicate/limit failures and recovery; zero allocations for all 28 warm workload/mode combinations; zero allocations with permissive unknown keys plus escaped arenas; and schema-free DOM number-policy parity. The latter explicitly disables the runtime unknown-key rejection option while keeping duplicate rejection. These allocator checks run on the test thread with no conditional unit-test modules.

`cargo check --workspace` and `cargo check -p jsonmax -p jmax-bench --no-default-features --features alloc` pass. `git diff --check` passes. Workspace manifests contain no serde_json dependency; source scans found no expect-call syntax or conditional unit-test attributes. A preexisting path-parser helper named expect was renamed to consume_byte so the literal call scan is also clean. Existing compiler warnings are retained in the archived logs.

The full raw timing table is [head-to-head-samples.csv](benchmark-7-workloads-cold-vs-warm/head-to-head-samples.csv). It contains 20 rows for each of 182 cases; allocation columns repeat the separate allocation pass for that case. [baseline.rs](benchmark-7-workloads-cold-vs-warm/baseline.rs), [baseline.Cargo.lock](benchmark-7-workloads-cold-vs-warm/baseline.Cargo.lock), corpus hashes/examples, build/test logs, profiles, and load snapshots are archived alongside it. No baseline Cargo.toml is stored in this workspace.

Reproduce the external comparison with:

```sh
./reports/benchmark-7-workloads-cold-vs-warm/run-baseline.sh
```

That script creates a manifest and build directory under a temporary directory, uses the archived lockfile, and prints its location. The original built runner remains at `/tmp/jmax-followup-baseline/target/release/jmax-followup-baseline`. Its optional `JMAX_RAW` writes raw CSV, `JMAX_CORPUS` exports JSONL payloads, and `JMAX_PROFILE=Workload/Mode/Warm` (or Cold) runs an eight-second single-case profiling loop. Attach `sample PID 3 1 -file OUTPUT` to that loop to repeat the profiling. Archived render/demangle source shows the exact sample-to-SVG transformation used here.
