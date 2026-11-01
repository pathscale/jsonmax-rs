# Architecture

## Module Layering

1. **ABI and Buffer Ownership (`jsonmax::abi`, `jsonmax::buffer`, `jsonmax::error`)**:
   - `jsonmax::abi` defines stable value representations, spans (`Span32`, `StrRef`), resource limits (`Limits`, `VmLimits`), error codes, schema records, and `ValidationProof`.
   - `Payload` wraps immutable input bytes (`Vec<u8>`, `bytes::Bytes`, or `Arc`-backed slices). Slicing produces zero-copy subviews checked against configured length caps.
   - `StrRef` references either the raw input buffer (for unescaped strings) or `DecodedArena` (for strings containing escapes), paired with a validated Unicode scalar count.
2. **SIMD Classification and Strict Syntax (`jsonmax::dispatch`, `jsonmax::scan_*`, `jsonmax::utf8`, `jsonmax::string`, `jsonmax::number`, `jsonmax::decimal`, `jsonmax::syntax`)**:
   - `jsonmax::scan_scalar`, `jsonmax::scan_neon`, `jsonmax::scan_avx2`, and `jsonmax::scan_avx512` classify 64-byte input blocks directly in place without padding or copying input buffers.
   - On AArch64, `jsonmax::scan_neon` issues four unaligned `vld1q_u8` loads per 64-byte block and uses `vmull_p64` (`PMULL`) for carryless quote prefix XOR when available. On x86_64, `jsonmax::scan_avx2` and `jsonmax::scan_avx512` pair vector byte classification with `_mm_clmulepi64_si128` (`PCLMULQDQ`).
   - `jsonmax::syntax` combines 64-byte structural/quote bitmasks with RFC 8259 grammar validation, UTF-8 verification, numeric lexing, nesting depth limits, and duplicate-key rejection.
   - `strict_fast` provides a single-pass validator and container skipper with a 128-entry direct-mapped object shape memo (up to 256 unescaped keys per shape) and a 64-bit depth mask for repeated object schemas.
3. **Packed Tape DOM (`jsonmax::dom`, `jsonmax::cursor`, `jsonmax::path`)**:
   - `jsonmax::dom` stores preorder document nodes in a single contiguous `Vec<TapeWord>` (`u64` words) alongside the raw payload and `DecodedArena`:
     - Bits `0..8`: node tag; bits `8..32`: flags and compact byte/element counts; bits `32..64`: raw byte offset.
     - Compact ASCII object keys use 1 word; scalars and compact ASCII strings use 2 words (pairing parent index and subtree-end index in word 2); containers and extended numbers use 3 words; extended strings use 4 words.
   - `BorrowedDocument<'a>` borrows the caller's byte slice; `TapeDocument` retains a `Payload`. Both share the `TapeView` navigation API and can recycle their backing buffers into `DomScratch`.
4. **Schema VM and Native AOT Decoders (`jsonmax::schema_ir`, `jsonmax::schema_compile`, `jsonmax::schema_vm`, `jsonmax::schema_aot`, `jsonmax::native`, `jsonmax::codegen_rust`)**:
   - `jsonmax::schema_ir` normalizes schemas into a resolved graph of scalar, object, array, enum, and reference nodes.
   - `jsonmax::schema_compile` lowers the graph in canonical breadth-first order into a `ValidatedProgram` of states, instructions, key dispatch tables, and regex pattern DFAs.
   - `jsonmax::schema_vm` executes `ValidatedProgram` directly over streaming JSON tokens to validate documents and extract selected slots without constructing a DOM.
   - `jsonmax::codegen_rust` and `jsonmax::native` generate specialized zero-allocation Rust decoders per endpoint schema, using `VmLimits` budget accounting and falling back to `jsonmax::schema_vm` on malformed input for exact byte-offset diagnostics.
5. **Serialization and Service Transports (`jsonmax::encoder`, `jsonmax::wire`, `jsonmax::http`, `jsonmax::wss`, `jsonmax::nagoya`, `jsonmax::mcp`)**:
   - `jsonmax::encoder` serializes typed values and DOM tapes through bounded sinks (`VecSink`, `OutputBuffer`).
   - `jsonmax::http`, `jsonmax::wss`, `jsonmax::nagoya`, and `jsonmax::mcp` integrate payload adoption, WebSocket frame assembly, admission control, and worker pool scheduling.
