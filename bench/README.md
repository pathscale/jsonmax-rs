# bench

Out-of-workspace benchmark and differential tools. Both crates carry their own
`[workspace]` table and are listed in the root `exclude`, so `cargo build/test --workspace`
never builds them. They need the sibling checkout `../ps-rusty_alloc` (same as the root
workspace) and the crates in `../crates`.

| path | what |
|---|---|
| `harness/` | `jmax-harness`: per-corpus, per-op timing (median ns per call, one TSV line each) and a mutation differential (`DIFF=N`) |
| `dcheck/` | prints which scan backend `jmax-dispatch` picks per input size and the detected CPU capabilities |
| `data/` | corpora (see below) |

## Corpora

Vendored (small, deterministic): `data/endpoint_batch.json` (102,494 B, 175 endpoint-style
records), `data/rpc_small.json`, `data/binance_small.json`.

Not vendored, fetched at a pinned commit of
[simdjson/simdjson-data](https://github.com/simdjson/simdjson-data) with sha256 checks:

```sh
bench/data/fetch.sh     # twitter, citm_catalog, canada, github_events, apache_builds
```

The files are git-ignored. `JMAX_DATA=<dir>` overrides the data directory.

## Apple M4 (aarch64, NEON path)

```sh
bench/data/fetch.sh
cd bench/dcheck && cargo run --release        # expect neon: true, backend Neon for large inputs
cd ../harness
RUSTFLAGS="-C target-cpu=native" cargo build --release

# one run, 15 interleaved samples per job, median ns per call (TSV: corpus, op, ns)
./target/release/jmax-harness > run1.tsv

# mutation differential; output must stay byte-identical across a change
DIFF=30000 ./target/release/jmax-harness | md5sum    # x86_64 AVX2/AVX-512 host: 6715c04f984ed8838a4f1678ced61116
```

The `DIFF` md5 above was taken on x86_64. Record your own on the M4 with the base commit
first, then compare after a change on the same machine.

Environment knobs: `SAMPLES` (default 15), `CORPORA=twitter,canada,...` (`small` adds the two
small fixtures), `FILTER=substr,substr` on `corpus<TAB>op`, `TARGET_NS` (per-sample budget,
default 2 ms), `FIXED=1`, `NODUP=1` (disable duplicate-key rejection), `SERDE=1` (also run
the serde_json rows). To A/B two builds, run them alternately several times and take the
median of the per-run medians; on a shared machine repeat any suspicious run.

For stable M4 numbers: plugged in, not in low-power mode, nothing else running, and pin
nothing (macOS has no hard affinity); prefer 5+ rounds.

## Cross-parser comparison

`harness/benches/compare.rs` times jmax against serde_json, simd-json, sonic-rs, jiter and
json-rust (json-rust crate). Competitors are `[dev-dependencies]` of the harness crate only;
the bench target is the only code that links them. Every typed result is asserted equal to
serde_json's before timing, and every serializer's output is parsed back and compared to the
reference value (a mismatch prints a `# serialize check` line on stderr).

```sh
bench/compare.sh 5                     # 5 interleaved rounds, medians, markdown to stdout
SAMPLES=15 bench/compare.sh 5          # more samples per round (default 9)
OUT=run.tsv bench/compare.sh 5 > run.md  # keep raw rows: round, corpus, class, parser, bytes, ns
cargo bench --bench compare            # (from bench/harness) one raw TSV run; FILTER, CORPORA work
```

Each round runs every job (all corpora, parsers and ops) in a fresh random order per sample,
so drift on a shared machine hits all parsers alike; the table value is the median over
rounds of each round's median over samples. Repeat any run that looks off.

Classes and what jmax's 1.0 baseline row is:

| class | jmax row | competitors |
|---|---|---|
| validate | strict syntax scan (`validate_syntax`) | serde_json `IgnoredAny`; sonic-rs lazy `get` of the root and `IgnoredAny`; jiter `next_skip` |
| dom-owned | `parse_slice` | serde_json `Value`; sonic-rs `Value`; simd-json `to_owned_value`; json-rust; jiter `JsonValue` + `into_static` |
| dom-borrowed | `parse_borrowed` | simd-json tape and `to_borrowed_value`; jiter `JsonValue` |
| typed | extraction of 1 to 5 fields into the structs in `src/common.rs` | serde_json, simd-json serde, sonic-rs (all `Deserialize`, unknown fields skipped); jiter and json-rust have no serde path and are absent |
| serialize | `encode_document_streaming` of the parsed document | serde_json `Value`, sonic-rs `Value`, simd-json `OwnedValue`, json-rust `dump` (jiter has no writer) |

`memcpy` (class `ref`) is the cost of copying the input, for context on simd-json.

### Not directly comparable

- **simd-json mutates its input.** Each simd-json job first `copy_from_slice`s the corpus into
  a scratch buffer; that copy is inside the timing (see the memcpy row; it is tens of GB/s,
  so small, but it is not free for the 100 KB inputs). jmax and the others read the input
  in place.
- **sonic-rs lazy skip does not look at untouched values the way a validator does**: `get`
  with an empty path finds the end of the root value with a SIMD skipper and does not
  validate every number or string escape. Its typed path also skips unknown fields lazily.
  Treat `sonic-rs lazy-skip` as an upper bound for "find the end", not a validator.
- **Validation strictness differs.** jmax runs strict RFC 8259 syntax, UTF-8 validation of the
  whole input, number grammar, and rejects duplicate keys (set `NODUP=1` for the single-binary
  harness only). serde_json validates UTF-8 only inside strings it visits; simd-json validates
  UTF-8 over the whole input; json-rust converts via `str::from_utf8` first; jiter validates
  while skipping; none of the others reject duplicate keys.
- **Number handling differs.** serde_json defaults to a fast but not correctly rounded float
  parser (no `float_roundtrip`); simd-json and sonic-rs use their own float paths; jmax parses
  to its own number representation. The serialize check compares numbers with a 1e-12
  relative tolerance for that reason.
- **SIMD differs by host.** simd-json and sonic-rs pick AVX2/AVX-512 or NEON at runtime or
  compile time; jmax dispatches on input size (`dcheck` shows which backend). On x86_64 run
  `RUSTFLAGS="-C target-cpu=native"` for all of them or for none; do not compare a native
  build run against a baseline build.
- **Typed work is not identical.** jmax extracts into slots compiled from a schema (and for
  `github_events` splits the root array first, two passes); serde-based parsers drive the
  struct's `Deserialize`. Field counts are the same.
- **jiter** is a pull parser: its `validate` row is skip-only and its DOM is the generic
  `JsonValue` (borrowed strings; the `into_static` row copies them).
