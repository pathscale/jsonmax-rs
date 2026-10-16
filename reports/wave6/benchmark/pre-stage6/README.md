# Wave 6 benchmark artifacts

`../FINAL.md` explains semantics and limitations. `tables/` contains Markdown/CSV/JSON results. The requested four `*.txt` logs combine each allocator's complete process outputs; original per-run logs and JSONL samples remain separately available.

Reproduce on this host:

```sh
python3 reports/wave6/benchmark/run-stage5.py
python3 reports/wave6/benchmark/run-timings.py
python3 reports/wave6/benchmark/run-invariants.py
python3 reports/wave6/benchmark/verify-artifacts.py
python3 reports/wave6/benchmark/write-report.py
```

Run these sequentially. The scripts use cargo +1.98.1, offline mode, native CPU flags, the requested Cargo PATH and the existing rustup proxy/compiler pins. The first script builds/copies all four allocator-specific binaries before any timing. The timing script invokes saved binaries and never compiles. The invariant script compiles a separately instrumented source copy only after timings; its counts are not performance samples.

The external project was restored to `/tmp/jmax-vs-parsers` from `harness-snapshot`; copy that exact Cargo.toml, Cargo.lock and src there if absent. Corpora live in `corpora/` and can be restored to `/tmp/jmax-bench-data`. Preserve absolute local dependency paths or restore the sibling source layout from `source-snapshot.tar.gz` and adjust paths. The invariant source copy has a separate lockfile and explicitly archived instrumentation patch. Original timed source hashes are in `source-final.sha256.json`; binary/corpus hashes and final audit are adjacent. Generated workloads and their per-document/JSONL hashes are under `generated-corpora/` and `generated-corpus-manifest.json`.

System observations are read-only ps/pmset snapshots. Unrelated user processes were never stopped; these measurements are not host-isolated. Raw p95 values describe batch/pass averages.
