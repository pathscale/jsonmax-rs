# Architecture

## Crate Layering

1. **ABI and Buffer Ownership (`jmax-abi`, `jmax-buffer`, `jmax-error`)**:
   - `jmax-abi` defines stable value representations, spans (`Span32`, `StrRef`), resource limits (`Limits`, `VmLimits`), error codes, schema records, and `ValidationProof`.
   - `Payload` wraps immutable input bytes (`Vec<u8>`, `bytes::Bytes`, or `Arc`-backed slices). Slicing produces zero-copy subviews checked against configured length caps.
   - `StrRef` references either the raw input buffer (for unescaped strings) or `DecodedArena` (for strings containing escapes), paired with a validated Unicode scalar count.
2. **SIMD Classification and Strict Syntax (`jmax-dispatch`, `jmax-scan-*`, `jmax-utf8`, `jmax-string`, `jmax-number`, `jmax-decimal`, `jmax-syntax`)**:
   - `jmax-scan-scalar`, `jmax-scan-neon`, `jmax-scan-avx2`, and `jmax-scan-avx512` classify 64-byte input blocks directly in place without padding or copying input buffers.
   - On AArch64, `jmax-scan-neon` issues four unaligned `vld1q_u8` loads per 64-byte block and uses `vmull_p64` (`PMULL`) for carryless quote prefix XOR when available. On x86_64, `jmax-scan-avx2` and `jmax-scan-avx512` pair vector byte classification with `_mm_clmulepi64_si128` (`PCLMULQDQ`).
   - `jmax-syntax` combines 64-byte structural/quote bitmasks with RFC 8259 grammar validation, UTF-8 verification, numeric lexing, nesting depth limits, and duplicate-key rejection.
   - `strict_fast` provides a single-pass validator and container skipper with a 128-entry direct-mapped object shape memo (up to 256 unescaped keys per shape) and a 64-bit depth mask for repeated object schemas.
3. **Packed Tape DOM (`jmax-dom`, `jmax-cursor`, `jmax-path`)**:
   - `jmax-dom` stores preorder document nodes in a single contiguous `Vec<TapeWord>` (`u64` words) alongside the raw payload and `DecodedArena`:
     - Bits `0..8`: node tag; bits `8..32`: flags and compact byte/element counts; bits `32..64`: raw byte offset.
     - Compact ASCII object keys use 1 word; scalars and compact ASCII strings use 2 words (pairing parent index and subtree-end index in word 2); containers and extended numbers use 3 words; extended strings use 4 words.
   - `BorrowedDocument<'a>` borrows the caller's byte slice; `TapeDocument` retains a `Payload`. Both share the `TapeView` navigation API and can recycle their backing buffers into `DomScratch`.
4. **Schema VM and Native AOT Decoders (`jmax-schema-ir`, `jmax-schema-compile`, `jmax-schema-vm`, `jmax-schema-aot`, `jmax-native`, `jmax-codegen-rust`)**:
   - `jmax-schema-ir` normalizes schemas into a resolved graph of scalar, object, array, enum, and reference nodes.
   - `jmax-schema-compile` lowers the graph in canonical breadth-first order into a `ValidatedProgram` of states, instructions, key dispatch tables, and regex pattern DFAs.
   - `jmax-schema-vm` executes `ValidatedProgram` directly over streaming JSON tokens to validate documents and extract selected slots without constructing a DOM.
   - `jmax-codegen-rust` and `jmax-native` generate specialized zero-allocation Rust decoders per endpoint schema, using `VmLimits` budget accounting and falling back to `jmax-schema-vm` on malformed input for exact byte-offset diagnostics.
5. **Serialization and Service Transports (`jmax-encode`, `jmax-wire`, `jmax-http`, `jmax-wss`, `jmax-nagoya`, `jmax-mcp`)**:
   - `jmax-encode` serializes typed values and DOM tapes through bounded sinks (`VecSink`, `OutputBuffer`).
   - `jmax-http`, `jmax-wss`, `jmax-nagoya`, and `jmax-mcp` integrate payload adoption, WebSocket frame assembly, admission control, and worker pool scheduling.
