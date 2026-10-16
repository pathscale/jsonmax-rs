# Wave 6 Frozen Contracts, Ownership Matrix & Baseline Hashes

Date: 2026-10-16  
Workspace: `/Users/revenge/code/jsonmax-rs`

## 1. Pre-Edit Source & Manifest Hashes

- **Git HEAD Commit**: `c3b97ce31eb93725c8494f258be7c6803752e558`
- **Git HEAD Tree**: `85356ed91bcfe7e016f8dfa35718bf81598f127c`
- **Untracked User Paths Preserved**:
  - `reports/investigation-2026-10-16/`
  - `target/`
- **Pre-Edit Manifest SHA-256 Digests**:
  - `Cargo.toml`: `51bb6ec8fdf7923c584ed8dbe969673e1207e8733526774c3fc2ecb2bd52577f`
  - `Cargo.lock`: `bc76c214614a1e67ac29f35a99e53e9d7ac36aefbf43d10933f3d1cb3fc9c487`
  - `/Users/revenge/code/ps-rusty_alloc/Cargo.toml`: `c2bdfa54a2c8e1af09c2dad39b68d04f587acba2cd632acc9349a86e6c8f8932`

## 2. Exclusive Worker Ownership Matrix & Models

- **Concurrency Cap**: At most 3 active workers + C0 coordinator (4 total slots).
- **Models**: `gpt-6.1-sol` (`high`) for S1..S8; `gpt-6-luna` (`high`) for L1..L2; **zero `gpt-6-astra`**.

| ID | Model / Reasoning | Exclusive File Ownership | Deliverable |
|---|---|---|---|
| **C0** | Coordinator | `Cargo.toml`, `Cargo.lock`, `crates/jmax-abi/**`, `crates/jmax-buffer/**`, `crates/jmax-fuzz/**`, unassigned crates, `reports/wave6/{CONTRACTS.md,STATUS.md,FINAL.md,verification/**}` | Frozen ABI contracts, lockfile ownership, cross-crate integration, verification & final report |
| **S1** | `gpt-6.1-sol` / `high` | `crates/jmax-scan-{neon,avx2,avx512,scalar}/**`, `crates/jmax-dispatch/**`, `crates/jmax-utf8/**`, `crates/jmax-string/**`, `reports/wave6/S1.md` | Fused 4-load NEON / 2-load AVX2 64-byte classifier, gated `vmull_p64` / `_mm_clmulepi64_si128` prefix-XOR, zero full-block staging copy, SIMD UTF-8 scalar count & escape masks |
| **S2** | `gpt-6.1-sol` / `high` | `crates/jmax-syntax/**`, `crates/jmax-cursor/**`, `reports/wave6/S2.md` | Persistent `BlockCursor`, borrowed core, one-pass transactional escaped string decoder, bulk unescaped Unicode validation, single-check grammar transition, `skip_query` / `skip_strict` / `skip_validated` |
| **S3** | `gpt-6.1-sol` / `high` | `crates/jmax-dom/**`, `reports/wave6/S3.md` | Packed `Vec<u64>` native `TapeDocument` & `BorrowedDocument<'a>`, private immutable fields, $O(1)$ navigation, explicit `to_legacy` / `from_legacy` conversion |
| **S4** | `gpt-6.1-sol` / `high` | `crates/jmax-schema-vm/**`, `crates/jmax-schema-compile/**`, `crates/jmax-schema-aot/**`, `reports/wave6/S4.md` | Direct `BlockCursor` schema VM execution, precompiled state/constraint dispatch table, single duplicate-key owner (`DuplicatePolicyOwner`), strict validate-without-capture, proof-safe fast skip |
| **S5** | `gpt-6.1-sol` / `high` | `crates/jmax-encode/**`, `reports/wave6/S5.md` | Native tape/borrowed encoding, batched SIMD escape masks & slice writes, trusted internal string access on immutable native documents, streaming vs staged sinks |
| **S6** | `gpt-6.1-sol` / `high` | `crates/jsonmax/**`, `reports/wave6/S6.md` | Owned/borrowed public facade (`parse_borrowed`, `validate_borrowed`, `extract_borrowed`, `query_extract_borrowed`), native `Document` (`TapeDocument`), explicit legacy conversion, optional `rusty-alloc` feature |
| **S7** | `gpt-6.1-sol` / `high` | `crates/jmax-nagoya/**`, `crates/jmax-http/**`, `crates/jmax-wss/**`, `reports/wave6/S7.md` | Bounded reactive job/completion rings, `Payload` owner preservation, `ValidationCapability` fast path in `submit::check_output`, cancellation/admission/order & transport adoption |
| **S8** | `gpt-6.1-sol` / `high` | `/Users/revenge/code/ps-pikkr/**`, `reports/wave6/S8.md` | Promote `/tmp/jmax-vs-parsers/ps-pikkr` to `/Users/revenge/code/ps-pikkr`, reconcile `[lib] name = "pikkr"`, fix cfg collisions, safe tails, cross-block quote parity, pass native Apple Silicon & portable tests |
| **L1** | `gpt-6-luna` / `high` | `crates/jmax-bench/Cargo.toml`, `crates/jmax-bench/src/bin/jmax_bench.rs`, `crates/jmax-{validator,runner,gen-cli}/Cargo.toml`, `crates/jmax-{validator,runner,gen-cli}/src/main.rs`, `reports/wave6/L1.md` | Wire `rusty_alloc::RustyAlloc` as `#[global_allocator]` in all 4 executable roots |
| **L2** | `gpt-6-luna` / `high` | `crates/jmax-bench/**` (except `Cargo.toml` and `src/bin/jmax_bench.rs`), `/tmp/jmax-vs-parsers/{Cargo.toml,Cargo.lock,src/**}`, `reports/wave6/benchmark/**`, `reports/wave6/L2.md` | Native/borrowed/warm/query benchmark modes, `RustyAlloc`-wrapped counting allocator in external harness, `ps-pikkr` integration, reproducible snapshots |

### File Transfer Log
- **Stage 5 user-authorized exclusive benchmark barrier (2026-10-16)**: all implementation/verification workers stopped. Stage 5 owns sequential builds/timings, benchmark CLI and external-harness completion, benchmark artifacts and FINAL/STATUS reporting. Harness-only additions expose missing modes, raw samples and per-operation allocation/byte accounting; parser algorithms remain unchanged. Additional scan counters live only in an archived source copy after all timings.
- None required at Stage 0. Any subsequent shared file handoff must be recorded here before edits.
- **Stage 4 Integration Barrier (all S1..S8, L1..L2 workers idle)**: Exclusive workspace-wide integration & verification authority transferred to Coordinator / `V1` (`gpt-6.1-sol` `high`) to resolve any cross-crate integration issues across `crates/**` and `/tmp/jmax-vs-parsers/**`, run workspace/feature/`no_std`/allocator/`ps-pikkr` correctness gates, and write `reports/wave6/verification/VERIFICATION.md`.
- **Stage 4 constraint repair (S8 idle)**: V1 also takes exclusive ownership of `/Users/revenge/code/ps-pikkr/src/query.rs` for removal of its inherited production `.expect(...)`, preserving the documented root-node panic behavior; rerun both required ps-pikkr suites after the edit. Original S8 provenance inventories remain historical evidence.


---

## 3. Frozen ABI Contracts (`crates/jmax-abi`)

### 3.1 Scanner BlockMasks & Carryless Prefix-XOR (`jmax_abi::scan`)
- `BlockMasks` includes `pub delimiter_quote_mask: u64` alongside `quote_mask`, `backslash_mask`, `in_string_mask`, `colon_mask`, `comma_mask`, `lbrace_mask`, `rbrace_mask`, `lbracket_mask`, `rbracket_mask`, `structural_mask`, `whitespace_mask`, `control_mask`, `digit_mask`, `non_ascii_mask`, `valid_mask`, `valid_len`, and `backend`.
- Helper methods on `BlockMasks`:
  - `delimiter_quotes(&self) -> u64`: returns unescaped quote delimiter positions (`Q = raw_quote_mask & !escaped_follower_mask & valid_mask`).
  - `raw_punctuation_mask(&self) -> u64`
  - `open_container_mask(&self) -> u64` (`(lbrace_mask | lbracket_mask) & !in_string_mask & valid_mask`)
  - `close_container_mask(&self) -> u64` (`(rbrace_mask | rbracket_mask) & !in_string_mask & valid_mask`)
- **Fused Load Contract**:
  - Full 64-byte blocks (`input.len() == 64` or `&[u8; 64]`) MUST be classified directly from input without copying into a 64-byte stack staging buffer.
  - NEON (`jmax-scan-neon`) loads the 64 bytes once (four `vld1q_u8` 16-byte loads) and derives all raw masks (`quote_mask`, `backslash_mask`, `colon_mask`, `comma_mask`, `lbrace_mask`, `rbrace_mask`, `lbracket_mask`, `rbracket_mask`, `whitespace_mask`, `control_mask`, `digit_mask`, `non_ascii_mask`) from those four registers.
  - Carryless prefix-XOR (`Q -> prefix`):
    - On `aarch64` with `target_feature = "aes"` (PMULL), use `vmull_p64(Q, !0u64)` low 64 bits. Retain portable 6-shift prefix-XOR fallback when `aes` is not enabled.
    - On `x86_64` with `target_feature = "pclmulqdq"`, use `_mm_clmulepi64_si128`. Retain portable 6-shift fallback when `pclmulqdq` is unavailable.
    - Word-level odd backslash run resolution (`quotes::quote_masks`) computes `escaped_positions` and `trailing_parity` without per-byte scalar loops.

### 3.2 Zero-Copy Ownership, Subviews & ValidationCapability (`jmax_abi::buffer`)
- `Payload` variants:
  - `Payload::Shared(Arc<[u8]>)`
  - `Payload::Bytes(bytes::Bytes)`
  - `Payload::SharedView(SharedByteView)` — retains `Arc<[u8]>` or generic `Arc<dyn ByteOwner>` (coerced with zero allocation from `Arc<T: AsRef<[u8]> + Send + Sync + 'static>`) plus checked `offset: u32, len: u32`.
- Key methods on `Payload`:
  - `Payload::from_arc_owner<T: AsRef<[u8]> + Send + Sync + 'static>(owner: Arc<T>) -> Result<Payload>`
  - `Payload::try_slice(&self, span: Span32) -> Result<Payload>` and `Payload::slice(&self, range: Range<usize>) -> Result<Payload>`
  - `Payload::backing_identity(&self) -> BackingIdentity`
  - `Payload::shares_backing_with(&self, other: &Payload) -> bool`
- `ValidationCapability`:
  - Private-field token binding `(base_ptr, visible_ptr, visible_len, decoded_len, schema_fingerprint, policy_fingerprint)`.
  - Verifiable via `matches_payload` and `matches_slice`.

### 3.3 Token Metadata, Duplicate Ownership, Skip Modes & `BlockCursor<'a>` (`jmax_abi::syntax` & `jmax_syntax::block_cursor`)
- `StringMeta`: `{ raw_content: Span32, raw_token: Span32, cooked: StrRef, is_unescaped: bool, is_ascii: bool }`
- `KeyMeta`: `{ string: StringMeta, swar_head: u64, cooked_hash: u64 }` with `KeyMeta::compute_swar_head` and `KeyMeta::compute_fnv1a64`.
- `DuplicatePolicyOwner`: `Syntax`, `SchemaVm`, `Disabled`.
- `SkipMode`:
  - `SkipMode::QueryOnly`: structural bitmask boundary skip without interior lexical validation; NEVER issues `ValidationProof`.
  - `SkipMode::StrictValidate`: fused validate-and-discard consumer enforcing syntax, UTF-8, escapes, numbers/numeric policy, duplicate policy, and resource limits without building DOM nodes or slot captures.
  - `SkipMode::ValidatedOwner(ValidationCapability)`: structural bitmask skip permitted only when `capability` matches the exact immutable owner/slice, schema fingerprint, and `limits.policy_fingerprint()`.
- Frozen `jmax_syntax::block_cursor::BlockCursor<'a>` (re-exported as `jmax_syntax::BlockCursor<'a>`) methods:
  - `BlockCursor::new(input: &'a [u8], limits: Limits, numeric: Option<NumericPolicy>, dup_owner: DuplicatePolicyOwner) -> Result<Self>`
  - `input(&self) -> &'a [u8]`, `offset(&self) -> u32`, `limits(&self) -> Limits`, `numeric_policy(&self) -> Option<NumericPolicy>`, `duplicate_owner(&self) -> DuplicatePolicyOwner`
  - `skip_whitespace(&mut self) -> Result<Option<u8>>`
  - `peek_byte(&self) -> Option<u8>`
  - `advance(&mut self, n: u32)`
  - `expect_byte(&mut self, expected: u8, detail: &'static str) -> Result<u32>`
  - `scan_string_meta(&mut self, decoded: &mut Vec<u8>) -> Result<StringMeta>`
  - `scan_key_meta(&mut self, decoded: &mut Vec<u8>, compute_hash: bool) -> Result<KeyMeta>`
  - `scan_number_span(&mut self) -> Result<Span32>`
  - `scan_literal(&mut self) -> Result<(TokenKind, Span32)>`
  - `skip_value(&mut self, decoded: &mut Vec<u8>, current_depth: u16, mode: jmax_abi::syntax::SkipMode) -> Result<Span32>`
  - `finish_eof(&mut self) -> Result<()>`

### 3.4 Native Packed `u64` Tape DOM (`jmax_abi::node`)
- `TapeWord(pub u64)` header & extension word layout:
  - Tags (`bits 0..8`): `TAPE_TAG_NULL (0)`, `TAPE_TAG_FALSE (1)`, `TAPE_TAG_TRUE (2)`, `TAPE_TAG_NUMBER (3)`, `TAPE_TAG_STRING (4)`, `TAPE_TAG_ARRAY (5)`, `TAPE_TAG_OBJECT (6)`, `TAPE_TAG_KEY (7)`.
  - Flags in `aux24` (`bits 8..32`):
    - `TAPE_AUX_LEN_MASK = 0x0007_ffff` (19-bit compact length/count `0..=524_287`)
    - `TAPE_FLAG_KEY_EXTENDED = 0x0008_0000` (on value node: preceding `Key` record is 3 words instead of 1 word)
    - `TAPE_FLAG_HAS_KEY = 0x0010_0000` (on value node: preceded by an object `Key` record)
    - `TAPE_FLAG_EXTENDED = 0x0020_0000` (record uses extended length/scalar words)
    - `TAPE_FLAG_ASCII = 0x0040_0000` (`scalar_len == cooked_byte_len`)
    - `TAPE_FLAG_ESCAPED = 0x0080_0000` (cooked bytes in `DecodedArena` instead of `RawPayload`)
  - Operand (`bits 32..64`): `raw_offset: u32`.
  - Deterministic record sizes (`TapeWord::record_words`):
    - Compact `Key`: 1 word `[header]`
    - Extended `Key`: 3 words `[header, cooked_offset | (cooked_len << 32), scalar_len | (raw_len << 32)]`
    - Compact `Null`/`False`/`True`/`Number`: 2 words `[header, parent_idx | (subtree_end_idx << 32)]`
    - Extended `Number`: 3 words `[header, parent_idx | (subtree_end_idx << 32), raw_len as u64]`
    - Compact `String` (unescaped ASCII, `len <= TAPE_AUX_LEN_MASK`): 2 words `[header, parent_idx | (subtree_end_idx << 32)]`
    - Extended `String`: 4 words `[header, parent_idx | (subtree_end_idx << 32), cooked_offset | (cooked_len << 32), scalar_len | (raw_len << 32)]`
    - `Array` / `Object`: 3 words `[header, parent_idx | (subtree_end_idx << 32), child_count | (raw_end_offset << 32)]`
- Legacy `ValidatedDocument` / `DocumentParts` remains in `jmax_abi::node` for explicit compatibility (`to_legacy` / `from_legacy`) and is never built implicitly during native `TapeDocument` or `BorrowedDocument<'a>` parsing.

### 3.5 Global Allocator (`ps-rusty_alloc`)
- Workspace dependency: `rusty_alloc = { package = "ps-rusty_alloc", path = "../ps-rusty_alloc" }`.
- Installed once as `#[global_allocator] static ALLOCATOR: rusty_alloc::RustyAlloc = rusty_alloc::RustyAlloc;` in `jmax-bench`, `jmax-validator`, `jmax-runner`, and `jmax-gen-cli`.
- `jsonmax` exposes optional feature `rusty-alloc = ["std", "dep:rusty_alloc"]` re-exporting `rusty_alloc::RustyAlloc` without installing a library-level `#[global_allocator]`.

### Stage 6 Polish & Remaining-Bottleneck Elimination (Disjoint Ownership)
- **P1 (`gpt-6.1-sol` `high`)**: `crates/jmax-string/**`, `crates/jmax-syntax/**`, `crates/jmax-cursor/**`, `reports/wave6/P1.md`
- **P2 (`gpt-6.1-sol` `high`)**: `crates/jmax-dom/**`, `crates/jmax-encode/**`, `reports/wave6/P2.md`
- **P3 (`gpt-6.1-sol` `high`)**: `crates/jmax-schema-compile/**`, `crates/jmax-schema-vm/**`, `crates/jmax-schema-aot/**`, `crates/jmax-derive/**`, `crates/jsonmax/**`, `reports/wave6/P3.md`
- **Stage 7 Post-Polish Verification & Sequential Benchmark Barrier (P1, P2, P3 idle)**: Exclusive workspace and benchmark harness authority transferred to `P4` (`gpt-6.1-sol` `high`) to run workspace/feature/`no_std` verification gates, re-run affected sequential benchmarks and scan/allocation probes, and update `reports/wave6/verification/VERIFICATION.md` and `reports/wave6/FINAL.md`.
