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
