# TODO

## Reconciled master validation

- [x] Retain PR #1 head `21197af0ab0bf2bb6e753793363f63f1830f5c4c` as
  ancestry and replay the local publication split, two harness fixes and plan
  documentation as individual commits. Keep only two publishable crates. Local
  `master` is 26 commits ahead of `origin/master`; no commits were squashed.
- [x] Rebase on current `origin/master` (`58eff09`) and verify the final source
  tree matches the pre-rebase validated source tree (`c5a251d`). Remove stale
  conflict-marker text from the row-groups integration test; its focused target
  passes all 12 tests. Production source still matches the source used for the
  recorded full correctness and matched regression gates.
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
- [x] Verify both publishable packages with
  `cargo package -p jsonmax -p jsonmax-derive --offline` after the version
  changes: `jsonmax` 0.1.1 and `jsonmax-derive` 0.1.0. Both unpacked package
  sources compiled successfully.
- [x] Run outgoing-history credential/private-path scans over commit messages,
  changed source/docs, and benchmark harness code. No lines were flagged.
  Benchmark case, corpus, matrix, result, and native-fixture files were excluded
  and not read.
- [x] Check remote GitHub Actions workflow paths on `master` and the existing
  PR branch. Neither contains workflow files; no remote CI run is claimed.
- [x] Check registry versions and public API compatibility. Both crates have
  `0.1.0` published. The `jsonmax` API changes are additive and compatible, so
  this PR sets it to `0.1.1`; the derive source/API is unchanged and remains
  `0.1.0`. No crate is published from this PR.
- [ ] Complete a current side-by-side report against all available rivals.
  The matched PR-head check establishes no more than a 1% instruction-count
  regression; it does not establish that JsonMax beats any rival.
- [ ] Before publishing, recheck that `jsonmax` 0.1.1 is still available and
  resolve the comparison benches' sibling dependency.

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
- [x] Review outgoing source/docs, commit messages, and benchmark harness code
  for private disclosures; resolve remote ancestry. No GitHub Actions workflows
  are present on `master` or the existing PR branch. Registry versions and the
  compatible `jsonmax` bump are recorded above; no crate is published here.
- [x] Keep GitHub workflows manual by default. The current public repository has
  no workflow files and no remote CI run is claimed. Do not add benchmark
  content or comparative claims without current, equivalent evidence.
