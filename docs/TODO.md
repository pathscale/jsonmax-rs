# TODO

## Reconciled master validation

- [x] Retain PR #1 head `21197af0ab0bf2bb6e753793363f63f1830f5c4c` as
  ancestry and replay the local publication split, two harness fixes and plan
  documentation as individual commits. Keep only two publishable crates.
- [x] Carry remote row compiler/VM/scratch/facade paths, fixed scans, CPU and
  selection caches, strict skip probes/window/backoff, native skip memo, DOM
  scratch/duplicate tiers and tracing into the reorganized modules.
- [x] Run `cargo test --workspace --all-features`, `cargo clippy
  --workspace --all-targets --all-features`, and the `alloc` / `alloc,trace`
  no-std feature checks. The first test run exposed trace counters being read
  alongside parser tests that use the same global slots; move the counter test
  to its own integration-test binary so its exact hit assertions are isolated.
  The final workspace test run passes. Clippy exits successfully with existing
  warnings.
- [x] Build the paired harness and native benchmark executables and run both
  exact differential checksum gates. Both match their expected digests; this
  verifies correctness only, not relative speed.
- [ ] Resolve workspace-wide `cargo fmt --check`. It reports formatting drift
  across the large existing source tree; the modified trace source and new
  integration test pass direct `rustfmt --check`. Avoid bulk formatting the
  entire imported tree without a scoped formatting review.
- [x] Compare the integrated source with PR #1's preserved head on the same
  machine and target. The general instruction gate covers 30 matched jmax
  rows; the separate native report covers 10 VM and 10 native rows. All three
  route groups meet the 1% regression gate. Exact conditions and limitations
  are recorded in the [validation plan](plans/native-parser-and-release-readiness.md).
- [ ] Complete a current side-by-side report against all available rivals.
  The matched PR-head check establishes no more than a 1% instruction-count
  regression; it does not establish that JsonMax beats any rival.
- [ ] Verify packages from a clean checkout and review the complete outgoing
  history for disclosures before any separately authorized publication. Resolve
  registry/version policy and the comparison benches' sibling dependency.

## Parser architecture and release readiness

- [ ] Review the current public APIs and freeze parser ownership, validation
  proof, classifier mask, token, document, and schema-image contracts before
  cross-crate changes. See
  [the implementation plan](plans/native-parser-and-release-readiness.md).
- [ ] Define and preserve separate strict-validation and query-only result
  contracts; strict paths must validate unselected syntax and applicable
  constraints.
- [ ] If implementing block SIMD, retain scalar-equivalent behavior, safe tail
  loads, cross-block quote/backslash state, and actual feature gating.
- [ ] Keep borrowed lifetimes explicit and owned payload allocation identity
  intact where supported; bind validation proofs to immutable owners/ranges.
- [ ] Make schema compilation reject unsupported constraints or route them
  through a bounded, explicitly identified compatibility path.
- [ ] Preserve encoding and transport behavior while integrating validated
  views and adapters.
- [ ] Before a public PR, review current changes and outgoing history for
  private disclosures, generated operational output, and build dependencies;
  resolve remote ancestry and package publication policy.
- [ ] Keep GitHub workflows manual by default. Do not add benchmark content or
  make performance claims without current, equivalent, reproducible evidence.
