# Wave 6 execution status

Updated 2026-10-16T03:33:07.227235+07:00.

- Stages 0–3: implementation/contracts complete; S1–S8 and L1–L2 handoffs retained.
- Stage 4: earlier integration ledger retained in VERIFICATION.md; native ps-pikkr 29 and portable 27 passing tests are historical, not rerun totals.
- Stage 5: original report, samples, binaries, snapshots and tables preserved under benchmark/pre-stage6/.
- Stage 6: P1/P2/P3 bottleneck changes integrated. All requested checks pass: workspace 148 passed/0 failed; native feature tests 35 passed/0 failed/1 ignored. Counts overlap.
- Post-Stage-6 measurement: all four release variants rebuilt/preflighted before any timing; all 14 timing processes exited zero and ran sequentially. Complete current results: FINAL.md, benchmark/tables/ and benchmark/post-stage6-summary.json.

EscapedUnicode helper work fell from 129 blocks/8,154 bytes to 32/1,989. Sparse-subtree warm strict schema validation/extraction fell from 4 allocations/720 bytes to 0/0 in both allocators and ownership modes. All 28 canonical warm owned operations per allocator remain zero-allocation. Trie builds during execution are zero; the warmed nested query-only fixture still allocates one 8-byte delimiter stack per call, while warmed strict query execution allocates zero. Derived borrowed typed decoding reads the original input without the former facade payload copy; owned output fields/custom decoders may still copy.

Source and sibling inventories, snapshots, sample counts, hashes and command interval audits pass. All four protected sibling repositories remain unchanged and read-only. No commit or unrelated process termination occurred. Real x86 execution, isolated attribution, individual-request latency tails and a Wave 5/RustyAlloc baseline remain unmeasured. Host load, cold/new-shape allocations, overlapping query traversal and capacity tradeoffs are documented in FINAL.md. The initial superseded probe assertion is preserved in the ledger.
