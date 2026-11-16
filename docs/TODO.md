# TODO

## Repository hygiene

- [ ] Revalidate whether `all_crates_marker` is unused, and review Cargo.lock and
  `.cargo` handling against current policy. Clarify facade/reference-path
  ownership versus runtime-neutral core and Nagoya integration boundaries.
  These are review questions, not confirmed defects.
- [ ] Decide allocator adoption and the owner/scope of durable ps-pikkr
  integration before treating either as an accepted dependency.

## Release follow-ups

- [ ] Resolve workspace-wide `cargo fmt --check` drift with a scoped formatting
  review rather than a bulk reformat of the whole tree.
- [ ] Complete a current side-by-side report of the jmax native and VM routes
  against all available rivals.
- [ ] Before publishing, recheck that the bumped crate versions are still
  available and resolve the comparison benches' sibling `ps-pikkr` dependency.

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
