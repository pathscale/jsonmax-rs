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
