# TODO

## Reconciled master validation

- [x] Retain PR #1 head `21197af0ab0bf2bb6e753793363f63f1830f5c4c` as
  ancestry and replay the local publication split, two harness fixes and plan
  documentation as individual commits. Keep only two publishable crates.
- [x] Carry remote row compiler/VM/scratch/facade paths, fixed scans, CPU and
  selection caches, strict skip probes/window/backoff, native skip memo, DOM
  scratch/duplicate tiers and tracing into the reorganized modules.
- [ ] Run owner-controlled correctness, feature, formatting and lint gates in
  [the reconciliation validation section](plans/native-parser-and-release-readiness.md#pending-owner-validation).
  Integration performed source review only; no runtime success is claimed.
- [ ] Measure both native and general routes against matched baselines and all
  available rivals, including the 1% instruction regression gate. Review native
  instruction results separately from the general report format.
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
