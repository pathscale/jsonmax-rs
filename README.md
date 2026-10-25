# jsonmax

Zero-buffer SIMD JSON parser, packed `u64` tape DOM, bytecode schema VM, native AOT endpoint decoder generator, and streaming serializer for Rust.

## Overview

- **Zero-Buffer 64-Byte SIMD Scanning (`jmax-scan-*`, `jmax-dispatch`)**: Classifies input directly in 64-byte blocks without copying or padding input buffers. Supports AArch64 NEON + `PMULL` (`vmull_p64`), x86_64 AVX2/AVX-512 + `PCLMULQDQ`, and SWAR scalar fallback.
- **Strict RFC 8259 Syntax & Shape Memoization (`jmax-syntax`)**: Validates grammar, UTF-8, numbers, depth caps, and duplicate object keys in a single pass, with a 128-entry direct-mapped object shape memo in `strict_fast` for repeated object schemas.
- **Packed `u64` Tape DOM (`jmax-dom`)**: Represents documents in a single contiguous `Vec<u64>` tape allocation plus an optional decoded-escape arena. Unescaped strings borrow directly from the source slice (`BorrowedDocument<'a>`) or retained `Payload` (`TapeDocument`).
- **Schema Bytecode VM & Native AOT Codegen (`jmax-schema-vm`, `jmax-native`, `jmax-codegen-rust`)**: Compiles schemas into deterministic `JMXSCM02` bytecode programs for zero-DOM validation and slot extraction, or emits specialized Rust endpoint decoders (`jsonmax-gen --native`) that decode into preallocated structs with zero per-call heap allocations and exact `Decimal128` arithmetic.

## Quickstart

```rust
#[derive(jmax_derive::JsonDecode, jmax_derive::JsonSchema, jmax_derive::JsonEncode)]
struct MyMessage {
    id: u32,
}
```

### Strict Validation

```rust
let input = br#"{"id":7}"#;
let mut scratch = jsonmax::Scratch::default();
jsonmax::parse::validate_borrowed(input, jsonmax::ParseOptions::default(), &mut scratch)?;
```

### Borrowed DOM

```rust
let input = br#"{"id":7}"#;
let doc = jsonmax::parse::parse_borrowed(input, jsonmax::ParseOptions::default())?;
let root = doc.root_node();
```

### Schema VM Extraction

```rust
use jsonmax::{SchemaProvider, Selection};

let schema_ir = <MyMessage as SchemaProvider>::schema_ir();
let schema = jsonmax::endpoint::compile_schema(
    &schema_ir,
    jsonmax::CompileLimits::default_limits(),
)?;
let selection = Selection { field_ids: vec![], strict_validate_unselected: true };
let slots = jsonmax::endpoint::extract_borrowed(
    &schema, &selection, input, jsonmax::ParseOptions::default(),
)?;
```

### Checked Slot Decoding

```rust
let message: MyMessage = jsonmax::parse::from_borrowed(
    input, jsonmax::ParseOptions::default(),
)?;
```

### Native AOT Decoding

Generate a direct Rust decoder from an endpoint model and native spec:

```sh
jsonmax-gen --input fixtures/native/binance.ron \
  --native fixtures/native/binance.native.ron --format rust --output generated
```

Decode into a reusable target struct with zero per-call heap allocations:

```rust
use jmax_native::{decode_into, NativeContext, NumericPolicy, VmLimits};
use jmax_native_fixtures::binance::models::AggTrade;

let mut context = NativeContext::new(
    VmLimits::default(), NumericPolicy::strict_default(),
);
let mut trade = AggTrade::default();
decode_into(
    &mut context, jmax_native_fixtures::harness::AGG_TRADE.as_bytes(), &mut trade,
)?;
```

### Serialization

```rust
let bytes = jsonmax::encode::to_vec(&message, jsonmax::EncodeOptions::default())?;
let compact = jsonmax::encode::encode_borrowed_document(&doc, jsonmax::EncodeOptions::default())?;
```

## Workspace Crates

| Area | Crates | Description |
|---|---|---|
| Facade & Compatibility | `jsonmax`, `jmax-compat` | Top-level parsing, querying, schema, and encoding APIs; `serde` compatibility adapters |
| ABI & Buffers | `jmax-abi`, `jmax-buffer`, `jmax-error` | Shared types, `Payload` ownership, `Span32`/`StrRef`, resource limits, diagnostics |
| SIMD & Syntax | `jmax-dispatch`, `jmax-scan-scalar`, `jmax-scan-neon`, `jmax-scan-avx2`, `jmax-scan-avx512`, `jmax-utf8`, `jmax-string`, `jmax-number`, `jmax-decimal`, `jmax-syntax` | 64-byte SIMD block scanners, UTF-8 validation, string unescaping, number/decimal parsing, strict grammar |
| Navigation & DOM | `jmax-cursor`, `jmax-path`, `jmax-dom` | Streaming cursor traversal, JSONPath compilation, packed `Vec<u64>` tape DOM |
| Schema & Codegen | `jmax-schema-ir`, `jmax-schema-compile`, `jmax-schema-vm`, `jmax-schema-aot`, `jmax-derive`, `jmax-native`, `jmax-native-fixtures` | Schema IR, bytecode compiler/VM, `JMXSCM02` capsules, proc-macro derives, native AOT runtime |
| Encoding & Formats | `jmax-format`, `jmax-encode` | UUID/IP/Bytea/NanoId format validators and bounded streaming JSON writers |
| Transports & Tooling | `jmax-endpoint-model`, `jmax-endpoint-ron`, `jmax-codegen-spec`, `jmax-codegen-rust`, `jmax-wire`, `jmax-exchange`, `jmax-nagoya`, `jmax-http`, `jmax-wss`, `jmax-mcp`, `jmax-gen-cli`, `jmax-validator`, `jmax-runner`, `jmax-bench`, `jmax-fuzz` | Endpoint RON loader, Rust/spec codegen, HTTP/WSS/MCP integration, CLI generator, benchmarks |

## Sibling Repository Prerequisites

This workspace references local path dependencies checked out alongside `jsonmax-rs`: `nagoya`, `nago-http`, `nago-wss`, `ps-simdutf8`, `ps-rusty_alloc`, and `ps-pikkr`. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and [docs/SCHEMA-AND-CODEGEN.md](docs/SCHEMA-AND-CODEGEN.md).

## Benchmarks (Apple M4 Max)

Summary of three-round medians on Apple M4 Max (`aarch64`). Each cell reports **`MB/s (p50; allocs/call)`**. Full 8-metric tables (including p95 latency, retired instructions, CPU cycles, requested bytes, peak live heap, and retained scratch) are in [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

### Native Typed Decoding (Binance Payloads)

| Route | `aggTrade` (137 B) | `depth_20` (1.2 KB) | `depth_1000` (63.6 KB) | `trades_100` (14.6 KB) |
|---|---:|---:|---:|---:|
| **`typed / jmax native`** | **1608 MB/s (83 ns; 0 allocs)** | **1350 MB/s (916 ns; 0 allocs)** | **1485 MB/s (42.4 us; 0 allocs)** | **1449 MB/s (9.9 us; 0 allocs)** |
| **`typed / jmax native budget (diagnostic)`** | **1558 MB/s (84 ns; 0 allocs)** | **1356 MB/s (917 ns; 0 allocs)** | **1466 MB/s (41.5 us; 0 allocs)** | **1475 MB/s (9.7 us; 0 allocs)** |
| `typed / jmax vm+native (diagnostic)` | 150 MB/s (917 ns; 0 allocs) | 522 MB/s (2.4 us; 0 allocs) | 814 MB/s (75.7 us; 0 allocs) | 871 MB/s (16.6 us; 0 allocs) |
| `typed / sonic-rs fixed` | 865 MB/s (167 ns; 2 allocs) | 924 MB/s (1.3 us; 10 allocs) | 1068 MB/s (56.3 us; 18 allocs) | 837 MB/s (17.1 us; 206 allocs) |
| `typed / serde_json fixed` | 610 MB/s (209 ns; 2 allocs) | 623 MB/s (1.9 us; 10 allocs) | 722 MB/s (87.9 us; 18 allocs) | 587 MB/s (24.7 us; 206 allocs) |
| `typed / simd-json fixed` | 577 MB/s (250 ns; 11 allocs) | 729 MB/s (1.7 us; 13 allocs) | 833 MB/s (73.0 us; 10 allocs) | 763 MB/s (18.8 us; 210 allocs) |
| `query-only / ps-pikkr trained` | 1758 MB/s (83 ns; 1 alloc) | 3923 MB/s (292 ns; 1 alloc) | 4511 MB/s (14.1 us; 1 alloc) | 853 MB/s (17.0 us; 7 allocs) |

### General Corpora

| Route | `twitter` (631 KB) | `citm_catalog` (1.73 MB) | `canada` (2.25 MB) | `github_events` (65 KB) | `apache_builds` (127 KB) | `endpoint_batch` (102 KB) |
|---|---:|---:|---:|---:|---:|---:|
| **`validate / jmax`** | **2497 MB/s (248 us; 0)** | 2843 MB/s (608 us; 0) | **2489 MB/s (899 us; 0)** | 2623 MB/s (24.7 us; 0) | **2039 MB/s (61.5 us; 0)** | **1326 MB/s (75.8 us; 0)** |
| `validate / serde_json` | 2195 MB/s (286 us; 2) | 2059 MB/s (792 us; 1) | 1918 MB/s (1.17 ms; 1) | 2449 MB/s (26.3 us; 1) | 1829 MB/s (68.7 us; 1) | 1085 MB/s (91.4 us; 1) |
| `validate / sonic-rs ignored` | 2189 MB/s (285 us; 0) | 3012 MB/s (563 us; 0) | 2009 MB/s (1.09 ms; 0) | 2367 MB/s (26.7 us; 0) | 1754 MB/s (70.7 us; 0) | 1165 MB/s (87.9 us; 0) |
| `validate / jiter` | 2777 MB/s (224 us; 0) | 2440 MB/s (678 us; 0) | 1683 MB/s (1.33 ms; 0) | 3220 MB/s (19.5 us; 0) | 2250 MB/s (55.8 us; 0) | 1670 MB/s (59.1 us; 0) |
| **`typed / jmax native`** | **2508 MB/s (251 us; 1)** | 2879 MB/s (589 us; 1) | **2518 MB/s (907 us; 1)** | 2136 MB/s (30.0 us; 61) | **1994 MB/s (63.0 us; 1)** | **1334 MB/s (75.9 us; 2)** |
| **`typed / jmax vm`** | **2472 MB/s (253 us; 1)** | 2862 MB/s (597 us; 1) | **2536 MB/s (917 us; 1)** | 1375 MB/s (45.4 us; 61) | 1961 MB/s (64.6 us; 1) | **1308 MB/s (76.0 us; 2)** |
| `typed / serde_json` | 2172 MB/s (290 us; 2) | 2070 MB/s (800 us; 2) | 1873 MB/s (1.18 ms; 2) | 2317 MB/s (27.6 us; 65) | 1836 MB/s (69.3 us; 2) | 1080 MB/s (94.2 us; 3) |
| `typed / sonic-rs` | 2167 MB/s (284 us; 1) | 2970 MB/s (575 us; 1) | 1983 MB/s (1.09 ms; 1) | 2375 MB/s (27.3 us; 64) | 1745 MB/s (75.0 us; 1) | 1159 MB/s (86.5 us; 2) |
| `typed / simd-json` | 2383 MB/s (267 us; 7) | 2703 MB/s (631 us; 7) | 1193 MB/s (1.87 ms; 8) | 2487 MB/s (25.5 us; 68) | 2387 MB/s (51.5 us; 8) | 1162 MB/s (86.4 us; 10) |
| **`dom-borrowed / jmax`** | 848 MB/s (736 us; 3) | 1235 MB/s (1.39 ms; 3) | **2201 MB/s (1.03 ms; 1)** | 916 MB/s (70.4 us; 2) | 761 MB/s (164 us; 2) | 401 MB/s (252 us; 2) |
| **`dom-owned / jmax`** | 842 MB/s (748 us; 4) | 1218 MB/s (1.41 ms; 4) | **2134 MB/s (1.05 ms; 2)** | 886 MB/s (70.6 us; 3) | 740 MB/s (169 us; 3) | 378 MB/s (258 us; 3) |
| **`serialize / jmax`** | **3371 MB/s (186 us; 2)** | **5000 MB/s (334 us; 2)** | **4091 MB/s (547 us; 2)** | **3770 MB/s (16.8 us; 2)** | **2749 MB/s (45.0 us; 2)** | **1328 MB/s (77.2 us; 2)** |
| `serialize / sonic-rs` | 3404 MB/s (182 us; 12) | 4630 MB/s (373 us; 13) | 1315 MB/s (1.72 ms; 15) | 3768 MB/s (16.9 us; 10) | 2957 MB/s (41.7 us; 9) | 1334 MB/s (76.8 us; 11) |
| `serialize / serde_json` | 2361 MB/s (264 us; 13) | 5035 MB/s (327 us; 13) | 1343 MB/s (1.67 ms; 15) | 2183 MB/s (28.9 us; 10) | 2021 MB/s (62.9 us; 11) | 1213 MB/s (83.0 us; 11) |

## License

Licensed under the [MIT License](LICENSE).
