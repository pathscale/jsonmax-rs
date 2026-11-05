# TODO

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
