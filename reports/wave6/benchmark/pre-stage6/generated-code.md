# Native code inspection (after all timing)

Command: `/usr/bin/otool -tvV reports/wave6/benchmark/bin/jmax_bench-rusty-alloc`.
Exact binary SHA-256 is in `binary-hashes.json`; full output is `native-disassembly.txt`.

The `jmax_scan_neon::structural::classify_neon` symbol at `0x100073524` initializes x8=0 and executes `ldr q29, [x1, x8]` at `0x100073588`. Its loop increments x8 by 16 and branches until x8=64 (`0x100073740`–`0x10007374c`): **four input vector loads per full 64-byte raw classification**. All twelve masks derive from the loaded register; there is no memcpy or input staging call in this kernel. The mask result is stored into caller storage. Callee-save d8–d11 stores/loads remain, so this is not a claim that the whole scanner/parser remains in registers.

`jmax_scan_neon::quotes::prefix_xor_pmull` at `0x100073800` contains `pmull.1q v0, v1, v0` at `0x10007380c`. The native correctness report separately records executed NEON/PMULL differential tests. The disassembly proves instruction emission, not an independent instruction-execution trace.

The bounded `tails::load_block` helper still contains memcpy for short-input staging, as designed; its existence is not evidence that the full-block classifier uses it. Source routing and guard-page tests are recorded in Stage 4. This inspection covers the AArch64 release binary only; no x86 code was executed or disassembled.
