# Wave 6 execution status

Updated 2026-10-16T02:36:40.363945+07:00.

- Stage 0 — C0 contracts/ABI/integration: COMPLETE; [CONTRACTS.md](CONTRACTS.md).
- Stages 1–3 — S1..S8 and L1..L2 implementation/handoffs: COMPLETE with documented remaining limits; individual handoff reports retained.
- Stage 4 — Integration and mandatory native correctness/feature/allocator gates: PASS; [VERIFICATION.md](verification/VERIFICATION.md). Workspace 121 passed; targeted feature run 32 passed/1 ignored; ps-pikkr native 29 and portable 27 passed. Counts overlap. Real x86 execution UNEXECUTED. Broad pre-existing workspace formatting differences remain recorded.
- Stage 5 — Sequential release builds, both allocator suites, both workload generators, stress/regime/allocation/ownership/lease measurements, three-process external competitor runs, snapshots and final report: COMPLETE; [FINAL.md](FINAL.md), [benchmark artifacts](benchmark/). All timing processes exit 0; no task-owned Cargo/rustc build overlaps timing.

Measured: all 28 canonical warm owned operations allocate zero after warming. Sparse strict schema stress still allocates 4 times / 720 bytes per document. Generic borrowed JsonDecode remains a one-copy compatibility adapter; synchronous skip cancellation, Bytes backing lower bounds, arbitrary NodeId membership cost, x86 runtime, and host contention are explicit limits. Stage 5 completion does not claim these remaining architectural/measurement limits are resolved. No Wave 5/RustyAlloc baseline was measured.

Historical reports/investigation/wave5 files are preserved; the external final snapshot and lockfile match the measured harness; timed source hashes remain unchanged. No commit, deployment or unrelated process termination was performed.
