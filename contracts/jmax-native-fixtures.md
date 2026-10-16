# jmax-native-fixtures: frozen Wave 0 contract

Owner: native AOT endpoint decoders. Source of truth: `crates/jmax-native-fixtures/src/lib.rs`.
Public modules: regen, harness, binance, binance_open, shapes, shapes_open. The crate is
`publish = false`, uses std, and exists to hold generated native decoders in the workspace so
that they compile, are exercised, and cannot drift from their generator.

## Generated modules

`binance` and `binance_open` are generated from fixtures/native/binance.ron with
fixtures/native/binance.native.ron; `shapes` and `shapes_open` from shapes.ron and
shapes.native.ron. The `_open` modules use the same specification with reject_unknown set to
false. Every module is the complete `generate_with_native` output (mod.rs, models.rs,
endpoints.rs, registries.rs, schema_tables.rs, compat.rs, native.rs) with emit_schema_tables
off, written under src/<module>/ and never edited by hand. PREDICT_LAYOUT states whether the
checked-in files were generated with layout prediction.

`binance_plain` is the strict binance decoder generated without layout prediction, whatever PREDICT_LAYOUT says, so the differential suite covers both strategies (the other modules follow PREDICT_LAYOUT, which is true). The seeds include documents with keys out of declaration order and a space before a colon, so a predicted order misses.

`regen` holds the recipe: the Fixture constants, VARIANTS (module, fixture, reject_unknown, follows PREDICT_LAYOUT), and generate_all(predict_layout).
`cargo run -p jmax-native-fixtures --example regenerate` rewrites src/<module>/*.rs. The drift
test regenerates in memory and compares byte for byte with the checked-in files.

## Harness

`harness` is the differential check: reference_program lowers the program independently of the
embedded bytes; Subject pairs a root's program with a tree oracle built from jmax-syntax events;
check_one asserts, for one input and one VmLimits, that the core's accept set equals the VM's
(outside the VM's resource errors), that equality holds exactly on budget_safe inputs, that the
public result is the VM's exact error code and offset or a value equal to the oracle's, and that a
reused output equals the oracle's after any earlier decode. differential runs every seed and its
mutants under the limit variants. Panics name the input. Generated decoders carry no post_check,
so the public result of a schema-valid input is always its value.

Tests: tests/drift.rs, tests/generated.rs (the differential suite), tests/steady_state.rs
(zero allocations on the native route after warmup, with a counting allocator).
