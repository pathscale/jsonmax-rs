# Wave 6 post-Stage-6 benchmark artifacts

`../FINAL.md` and `../verification/VERIFICATION.md` record current results. Top-level raw samples, allocator-specific binaries, harness/source snapshots and `tables/` describe the post-P1/P2/P3 remeasurement. `pre-stage6/` preserves the Stage 5 report and evidence before replacement. Comparison tables retain both allocator configurations and report observed combined-change ratios.

Reproduce sequentially on this host, after restoring `/tmp/jmax-vs-parsers` from `harness-snapshot/` and corpora to `/tmp/jmax-bench-data` if absent:

```sh
python3 reports/wave6/benchmark/environment.py
python3 reports/wave6/benchmark/run-stage5.py
python3 reports/wave6/benchmark/freeze.py
python3 reports/wave6/benchmark/run-invariants.py
python3 reports/wave6/benchmark/run-timings.py
python3 reports/wave6/benchmark/verify-artifacts.py
python3 reports/wave6/benchmark/write-report.py
python3 reports/wave6/benchmark/post-stage6-report.py
```

Run one command at a time. Preserve the current command/load logs before starting another run. The runner pins offline cargo +1.98.1, native CPU flags, RUSTC/RUSTDOC and the requested PATH; it uses one Cargo job and one Rust test thread. All four release variants rebuild/copy before timing. The timing runner invokes saved binaries without compiling. The untimed invariant probe builds a source copy from the current frozen archive; its atomic counters are absent from timing binaries. The initial superseded query-allocation assertion is recorded in `invariants-initial.log` and `commands.jsonl`; the final probe passes and records the remaining query-only allocation.

`finish-post-stage6.py` runs the three final reporting/validation scripts sequentially, then refreshes verification evidence, status and deliverable hashes. It is the finalization entry point after timing completes.

`verify-artifacts.py` checks sample counts, binary hashes, successful final attempts, nonoverlapping command intervals and the build-before-timing boundary. `post-stage6-report.py` compares preserved Stage 5 tables, checks generated corpus hashes and verifies all four protected sibling inventories read-only. A fresh reproduction needs a new protected inventory if run on another host. Local path dependencies and compiler pins are host-specific; use `source-snapshot.tar.gz` to restore the sibling layout or adjust paths.

The four requested `*.txt` logs combine each allocator's complete process outputs; original per-run logs and JSONL samples remain separately available. Source, binary and corpus hashes, final audit and deliverable SHA256SUMS are adjacent. `generated-code.md` / `native-disassembly.txt` are explicitly historical Stage 5 inspection evidence, preserved under `pre-stage6/`; they are not a new disassembly of the rebuilt binaries.

Host load monitoring uses read-only ps/pmset snapshots. Unrelated processes were left running. Ratios across stages are not controlled attribution; p95 describes pass/batch averages.
