# Native parser and release readiness plan

This plan records design requirements and follow-up work. It does not claim that
the proposed architecture is implemented, tested, or faster. Keep each task
within the owning repository and review changes against the current code before
using historical notes as authority.

## Goals and boundaries

- Improve useful JSON operations while preserving equivalent syntax, schema,
  ownership, and output guarantees. Compare like-for-like operations and make no
  general performance claim without current, reproducible evidence.
- Preserve existing endpoint and protocol behavior during any adapter
  migration. Keep parser-core APIs independent from optional transport
  integrations.
- Keep every GitHub Actions workflow manual by default. Add automatic CI only
  when separately requested.
- This plan does not authorize source or manifest changes, benchmark execution,
  benchmark cases/corpora/results additions, publication, or release actions.

## Architecture contracts

### Parsing and validation

1. Keep strict validation distinct from query-only extraction. Strict parsing
   validates the complete JSON syntax and all applicable schema constraints,
   including unselected fields. Query-only results must not be represented as
   fully validated documents or carry a validation proof.
2. SIMD classification is an optimization over a scalar reference with
   identical masks and error behavior. Carry quote and backslash state across
   blocks, mask invalid tail lanes, and never read beyond initialized input.
   Detect the backend once per parse; gate architecture-specific instructions
   on their actual CPU and toolchain features.
3. A block cursor may skip uninteresting positions, but it is not a parser by
   itself. Preserve grammar checks for atoms, separators, nesting, trailing
   content, UTF-8, escapes, limits, and duplicate policy.
4. For unescaped strings, retain validated source spans where possible. Decode
   escaped strings in a bounded pass, preserve scalar-count semantics, and
   roll back partial decoded output on failure. Do not trade validation for
   skipped work.
5. Speculative field-order or offset hints may accelerate lookup only. Verify
   the actual key, type, depth, and bounds; fall back to ordinary dispatch on
   reordering or misses. Never use attacker-controlled global mutable training
   state.

### Ownership and documents

1. Make borrowed results explicitly borrow the input lifetime. Owned results
   retain their actual `Bytes`, `Arc`, or transferred `Vec` allocation where
   possible. Do not manufacture `'static` borrows, self-referential owners, or
   claim zero-copy when conversion copies.
2. Keep payload owners immutable after validation. Validation proofs must be
   private, bound to the exact owner/range and policy, and impossible to forge
   through safe public constructors. Resolve arbitrary external spans with
   bounds checks.
3. Separate mutable construction from frozen shared documents. Account for
   retained backing allocations when a small view pins a large buffer.
4. If a compact native tape is introduced, specify record and extension-word
   layouts, overflow behavior, and navigation before implementation. Keep any
   legacy document representation as an explicit conversion; do not silently
   construct both forms on the normal path.

### Schemas and encoding

1. Ahead-of-time and startup schema compilation should share a versioned
   intermediate representation. Validate references, names, constraints,
   recursion/resource limits, and supported schema features before publishing
   an immutable compiled registry.
2. Known-key dispatch must compare complete decoded keys after hash/table
   lookup. Escaped equivalent key spellings have the same duplicate and
   required-field semantics. Keep one explicit owner for duplicate detection.
3. Unsupported schema features must fail compilation or use a clearly
   identified bounded compatibility path; they must not silently lose a
   constraint or claim full validation.
4. Encode from validated views where possible, batch ordinary string runs, and
   preserve escaping, numeric spelling, output limits, sink failure behavior,
   and any existing atomic-publication contract. Keep direct streaming output
   distinct from staged output when their failure semantics differ.

## Implementation sequence

1. **Inventory and contracts:** inspect the current tree and existing public
   APIs. Record ownership, error, mask, token, proof, schema-image, and document
   layout contracts before changing cross-crate interfaces.
2. **Classifier and strings:** establish scalar reference behavior; implement
   fused architecture-specific masks, safe tails, block-spanning quote state,
   UTF-8 handling, and transactional escape decoding.
3. **Cursor and strict consumer:** add a forward cursor over classified blocks;
   integrate grammar transitions and strict validate-and-discard behavior.
   Keep query-only result types separate from validated results.
4. **Document and schema paths:** implement borrowed/owned facades and any native
   tape behind explicit APIs. Connect compiled schema dispatch directly where
   safe, retaining compatibility paths until semantics match.
5. **Encoding and adapters:** consume validated native/borrowed views; then
   update transport adapters while preserving framing, cancellation,
   backpressure, ordering, and owner lifetimes.
6. **Integration and evidence:** review each changed public contract, feature
   combination, error path, and adapter boundary. Any later performance claim
   requires matched operations, equivalent validation, recorded environment,
   and reproducible current evidence. Do not add benchmark content as part of
   this documentation integration.

## Public PR readiness

Before preparing a public PR, review the exact proposed diff and every outgoing
commit, including messages and generated/package contents, for credentials,
private keys, credential-bearing URLs, local absolute paths, internal hosts,
private repository references, unpublished material, and generated operational
outputs. Review context rather than treating generic temporary-directory
references as leaks. Keep secrets out of reports and remediation notes; if a
credential is exposed, stop publication and arrange rotation.

Also resolve the intended base and current remote ancestry, review the complete
workspace manifest/version/lockfile and dependent API compatibility, and check
that public build dependencies do not expose private paths or make clean
checkouts unbuildable. Do not infer a package version bump from a PR alone;
follow the publication policy for already released packages. State precisely
what was reviewed and which checks were not run. A clean working tree by itself
does not establish release readiness.

## Deliberate exclusions

Do not carry forward old worker assignments, prompts, logs, transcripts,
machine-specific paths, stale performance figures, or copied benchmark
artifacts. Benchmark filenames may be inventoried as metadata, but do not add,
reproduce, summarize, or modify benchmark cases, corpora, or results under this
plan. Avoid lexical-identity learning, statistical proxies, benchmark-specific
shortcuts, and speculative fixes.

## Local reconciliation status (2026-11-05)

Canonical local `master` now descends from the existing PR #1 head,
`21197af0ab0bf2bb6e753793363f63f1830f5c4c`. Its 18 commits after `845b38e`
remain intact. The publication split and both local harness fixes were replayed
as individual commits, followed by the existing planning documentation. No push
or PR change is part of this reconciliation.

The two publishable crates remain `jsonmax` and `jsonmax-derive`; development
crates remain unpublished. Tracing is integrated as `jsonmax::trace`, with one
`trace` feature and benchmark feature forwarding, rather than an additional
publishable crate. The source review retained:

- `RowGroup` / `RowShape` compiler indexes, the repeated-item slot exception,
  per-element staging, scratch storage, borrowed rows API and endpoint facade.
- `FixedObjectPlan` / `FixedScan`, root and item-object integration, leaf pair
  rows, field-order successors, cached capture masks and transactional rollback.
- Program/field selection caching and cached CPU capabilities with supported
  backend resolution.
- Strict repeated-structure probes, per-shape failure backoff, verified spans,
  escaped-string handling and the reusable 64-byte classification window.
- Native first-byte skip routing, vector short-string scanning, per-decode skip
  memo lifecycle and small unknown-key duplicate handling.
- Retained DOM shape scratch, root-shape avoidance, lazy duplicate tiers,
  dedicated top frame and allocation/bookkeeping changes.
- Existing trace sites, disabled guards and enabled counters, plus the local
  paired-measurement observation checks and explicit operation-filter fixes.

This is source/history reconciliation, not evidence of runtime equivalence or
speed. Source comparisons against the remote implementations were reviewed
after adapting former crate paths. No build, test, formatter, benchmark,
runtime or model call was run during integration. No benchmark corpus or
results content was inspected or changed. No temporary patch files were used.

### Owner validation commands

Use the repository root and one target directory for owner-controlled checks.
The correctness and feature checks below were run on 2026-11-05; the matched
performance comparison remains open:

```sh
cargo fmt --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features
cargo check -p jsonmax --no-default-features --features alloc
cargo check -p jsonmax --no-default-features --features alloc,trace
cargo build --manifest-path bench/harness/Cargo.toml --release --features trace -j 4
(cd bench/harness && DIFF=30000 ./target/release/jmax-harness | md5sum)
(cd bench/harness && NODUP=1 DIFF=3000 ./target/release/jmax-harness | md5sum)
cargo bench --manifest-path bench/harness/Cargo.toml --bench compare --features trace --no-run -j 4
cargo build --manifest-path bench/native/Cargo.toml --release --features trace -j 4
```

The existing differential gates expect respectively
`6715c04f984ed8838a4f1678ced61116` and `bb107953a10f4344c43003d1964bf341`.
Check the harness exit status as well as each digest. Workspace tests include
the preserved row-group integration and exact-engine comparison tests. The two
explicit checks cover core compilation without `std`, including tracing.

For performance, use a quiet machine with no competing build. Keep outputs
under ignored target directories and compare with a matched baseline supplied
by the owner; do not substitute historical figures from commit messages:

```sh
bench/icount/icount.sh bench/icount/target/reconciled.tsv
bench/icount/cmp.sh "$JSONMAX_ICOUNT_BASELINE" bench/icount/target/reconciled.tsv
GENERAL_CORPORA='twitter citm_catalog canada github_events apache_builds endpoint_batch' bench/native/icount.sh 200 > bench/native/target/reconciled.tsv
bench/compare.sh 1
cargo build --manifest-path bench/native/Cargo.toml --release --bin time -j 4
(cd bench/native && ./target/release/time > target/reconciled-time.tsv && ./tables.sh target/reconciled-time.tsv)
```

`JSONMAX_ICOUNT_BASELINE` must identify a comparable owner-provided baseline.
Performance commands build without the tracing feature; keep tracing disabled
for baseline comparisons.
The native instruction report also needs a matched native baseline review;
the general `cmp.sh` does not consume the native report format. Report both
native and VM routes, all available rivals, losses and machine details.

#### Results and remaining gates

- `cargo test --workspace --all-features` passes after moving the global trace
  counter assertions into `crates/jsonmax/tests/trace.rs`, its own integration
  test binary. Running those exact hit-count assertions beside parser tests was
  racy because the trace counters are intentionally global and relaxed.
- `cargo clippy --workspace --all-targets --all-features` exits successfully;
  it reports warnings across the workspace.
- `cargo check -p jsonmax --no-default-features --features alloc` and the same
  command with `--features alloc,trace` both pass.
- The harness release build, comparison benchmark `--no-run` build, and native
  benchmark release build pass. Both differential checks match their expected
  digests and exit successfully. These are correctness gates, not speed
  measurements.
- Workspace `cargo fmt --check` still fails with formatting diffs across the
  tree. Direct `rustfmt --check` passes for the changed trace source and the
  new trace integration test; the trace source at PR head `21197af` also failed
  direct formatting, so its cleanup is not newly introduced by this test fix.
- Matched performance comparison is still required. Compare the preserved PR
  head `21197af0ab0bf2bb6e753793363f63f1830f5c4c` with the integrated result on
  the same machine and target. Do not treat the differential digests or build
  success as evidence of no performance regression.

Publication remains gated on clean-checkout package verification, registry
availability/version policy, feature checks and a complete outgoing-history
disclosure review, including the retained remote ancestry. Manifest/source
inspection alone does not establish package or release readiness. Excluded
comparison benches still require the sibling `ps-pikkr` checkout. Future native
tape, ownership/proof and adapter requirements above remain open; reconciliation
does not mark them implemented.
