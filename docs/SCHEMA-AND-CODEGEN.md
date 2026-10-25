# Schema Compilation, VM Execution, and Native Codegen

## Schema IR and Normalization

`jmax-abi::schema::SchemaIr` defines the flat public schema representation emitted by `#[derive(JsonSchema)]` and endpoint specifications. `jmax-schema-ir` normalizes definitions into a resolved graph of scalar, object, array, enum, `Any`, and reference nodes while enforcing node, reference-hop, property, and identifier limits.

## Bytecode Lowering and Static Capsules (`JMXSCM02`)

`jmax-schema-compile` lowers a resolved schema graph into a `ValidatedProgram` with default compile caps of 65,536 states, 1,048,576 instructions, 65,536 keys, and 4,096 regex pattern states.

`jmax-schema-aot` serializes compiled programs into deterministic `JMXSCM02` static capsules (`dump_program` / `load_program`). When loading a static capsule, `jmax-schema-aot` verifies the magic header, reconstructs and validates the schema graph, and checks the canonical FNV-1a schema fingerprint before constructing a `SlotReader`.

## Schema VM Execution (`jmax-schema-vm`)

`jmax-schema-vm` validates inputs against a `ValidatedProgram` in a single streaming pass over JSON tokens:
- Enforces required fields, unknown-key policies, duplicate-key rejection, string formats (`jmax-format`), regex patterns, integer bounds, and exact `Decimal128` scale/precision constraints.
- Supports both full-document validation (`validate_borrowed`) and selective slot capture (`extract_borrowed`, `extract_query_borrowed`) without building a DOM tree.
- Tracks syntax depth, input size, VM step counts, and scratch memory against `VmLimits`.

## Native AOT Code Generation (`jsonmax-gen --native`)

`jmax-gen-cli` (`jsonmax-gen`) compiles `.ron` endpoint and model definitions into static Rust types, schema capsules, and direct `jmax-native` decoders:

```sh
jsonmax-gen \
  --input fixtures/native/binance.ron \
  --native fixtures/native/binance.native.ron \
  --format rust \
  --output generated
```

Generated decoders in `rust/native.rs` parse JSON bytes directly into preallocated Rust structs with zero per-call heap allocations:
1. Predictive field-order dispatch matches expected object keys in a single unaligned load and falls back to hash/byte dispatch when fields arrive out of order.
2. Inlined fast paths parse integers, booleans, short ASCII strings, and fixed-scale `Decimal128` values directly from the input slice.
3. `decode_into` verifies that observed work stays within `VmLimits`. Whenever an input exceeds fast-path budget bounds or fails fast-path parsing, `decode_into` delegates to `jmax-schema-vm` so callers always receive exact schema error codes and byte offsets.
