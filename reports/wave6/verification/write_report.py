from pathlib import Path
import json
import re

out = Path(__file__).resolve().parent
entries = []
for path in sorted(out.glob('*.log'), key=lambda p: p.stat().st_mtime_ns):
    text = path.read_text()
    cwd = re.search(r'^cwd=(.*)$', text, re.M)
    command = re.search(r'^command=(.*)$', text, re.M)
    result = re.findall(r'^exit=(\d+)$', text, re.M)
    assert cwd and command and result, path
    command = command[1].replace('env RUSTFLAGS=-C target-cpu=native ', 'RUSTFLAGS="-C target-cpu=native" ')
    command = command.replace('env JMAX_CHECK_ONLY=1 RUSTFLAGS=-C target-cpu=native ', 'JMAX_CHECK_ONLY=1 RUSTFLAGS="-C target-cpu=native" ')
    counts = re.findall(r'test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored', text)
    entries.append(dict(log=path.name, cwd=cwd[1], command=command, exit=int(result[-1]),
                        passed=sum(int(row[0]) for row in counts),
                        failed=sum(int(row[1]) for row in counts),
                        ignored=sum(int(row[2]) for row in counts)))
(out / 'commands.json').write_text(json.dumps(entries, indent=2) + '\n')

latest = ['workspace-check-final-3', 'workspace-test-final-4', 'feature-test-final-3',
          'no-default-final', 'rusty-feature-final', 'workspace-bins-final', 'system-bins-final',
          'pikkr-native-final', 'pikkr-portable-final', 'harness-check-final',
          'harness-system-check', 'harness-preflight-3', 'transport-no-default',
          'transport-all-features-2', 'allocator-consumer', 'format-integration-check',
          'diff-check', 'source-audit-complete', 'architecture-audit', 'environment']
for stem in latest:
    assert next(row for row in entries if row['log'] == stem + '.log')['exit'] == 0, stem

report = '''# Wave 6 Stage 4 Integration & Verification

Date: 2026-10-16. Engineer: V1. Workspace: `/Users/revenge/code/jsonmax-rs`.

**Result: all mandatory correctness, workspace, feature, no-default, binary and competitor compile gates pass. The native Apple Silicon workspace is ready for sequential benchmarking.** No benchmark samples, allocation-free claim, speedup claim, or x86 runtime claim is made by this stage.

## Environment and scope

Apple M4 Max, 16 logical CPUs, 64 GiB RAM, macOS 26.5 (25F71), Darwin 25.5.0, `aarch64-apple-darwin`. Cargo/rustc/rustdoc are **1.98.1**; rustc commit `48a229ceaefd4985c50990b14116b6d856af0985`, LLVM 22.1.8. Native cfg includes `neon` and `aes` (PMULL). See [environment.log](environment.log).

Read the full plan, execution/measurement gates, frozen contracts/transfer log, and all ten S1..S8/L1..L2 handoffs before integration repairs. All workers were idle per the user's transfer. No subagents, commits, dependency fetches, benchmark timing jobs, or protected-sibling source edits were performed.

Every Cargo invocation used `cargo +1.98.1`, including the standalone consumer and ps-pikkr. [run-gate.sh](run-gate.sh) logs cwd, command, output and exit status, with this common environment:

```sh
export PATH="/Users/revenge/.cargo/bin:$PATH"
# The requested directory is absent on this host; S8's existing rustup proxy is used.
export PATH="/Users/revenge/code/ps-pikkr/.tooling/bin:$PATH"
export RUSTC=/Users/revenge/.rustup/toolchains/1.98.1-aarch64-apple-darwin/bin/rustc
export RUSTDOC=/Users/revenge/.rustup/toolchains/1.98.1-aarch64-apple-darwin/bin/rustdoc
```

The explicit compiler/doc pins avoid the previously documented Homebrew wrapper/mixed-toolchain failure. Cargo commands stayed offline. Formatting uses the same toolchain's rustfmt binary.

## Final gate results

Counts below describe each execution; overlapping runs must not be summed as distinct tests.

| Gate | Final result | Evidence |
| --- | --- | --- |
| `cargo +1.98.1 check --workspace --all-targets --offline` | PASS, exit 0 | [workspace-check-final-3.log](workspace-check-final-3.log) |
| `RUSTFLAGS="-C target-cpu=native" cargo +1.98.1 test --workspace --offline` | **121 passed, 0 failed, 0 ignored**: 118 unit/integration + 3 compile-fail doctests; 103 test targets including zero-test targets | [workspace-test-final-4.log](workspace-test-final-4.log) |
| Native `jmax-dispatch`, `jmax-nagoya`, `jsonmax` tests with `jmax-nagoya/nagoya-rt,jsonmax/rusty-alloc` | **32 passed, 0 failed, 1 ignored**: 30 unit/integration + 2 compile-fail doctests; allocator documentation example is ignored | [feature-test-final-3.log](feature-test-final-3.log) |
| `cargo +1.98.1 check -p jsonmax --no-default-features --offline` | PASS, exit 0, alloc-only facade configuration | [no-default-final.log](no-default-final.log) |
| `cargo +1.98.1 check -p jsonmax --features rusty-alloc --offline` | PASS, exit 0 | [rusty-feature-final.log](rusty-feature-final.log) |
| `cargo +1.98.1 check --bins --workspace --offline` | PASS, exit 0 | [workspace-bins-final.log](workspace-bins-final.log) |
| `cargo +1.98.1 check -p jmax-bench --features system-alloc --bins --offline` | PASS, exit 0 | [system-bins-final.log](system-bins-final.log) |
| ps-pikkr: native `RUSTFLAGS="-C target-cpu=native" cargo +1.98.1 test --offline` | **29 passed, 0 failed**: 16 unit + 5 historical issue + 8 regression tests | [pikkr-native-final.log](pikkr-native-final.log) |
| ps-pikkr: `cargo +1.98.1 test --offline --features portable` | **27 passed, 0 failed**: 14 unit + 5 historical issue + 8 regression tests | [pikkr-portable-final.log](pikkr-portable-final.log) |
| `/tmp/jmax-vs-parsers`: native `cargo +1.98.1 check --offline` | PASS, exit 0 | [harness-check-final.log](harness-check-final.log) |
| External harness native check with `--features system-alloc` | PASS, exit 0 | [harness-system-check.log](harness-system-check.log) |
| Transport crates `--no-default-features` and `--all-features` | PASS, exit 0, both configurations | [transport-no-default.log](transport-no-default.log), [transport-all-features-2.log](transport-all-features-2.log) |
| Consumer binary enabling `jsonmax/rusty-alloc` and installing one global allocator | Links and runs successfully; owned parse, native encode and borrowed parse assertions pass | [allocator-consumer.log](allocator-consumer.log), [fixture](allocator-consumer/src/main.rs) |
| External harness `JMAX_CHECK_ONLY=1` | PASS across six corpora: **247 operation preflights**, DOM/typed semantic checks, and **18 conformance cases**; no warmup/sample timing loops | [harness-preflight-3.log](harness-preflight-3.log), [capability rows](harness-preflight.jsonl), [validation matrix](harness-validation-matrix.json) |
| Integration-file rustfmt and `git diff --check` | PASS | [format-integration-check.log](format-integration-check.log), [diff-check.log](diff-check.log), [syntax test format](format-skip-test-check.log) |
| Source, allocator, snapshot and protected-path audit | PASS | [source-audit-complete.log](source-audit-complete.log), [audit.py](audit.py) |

Ordinary unused/dead-code warnings remain in existing derive, exchange, CLI and fuzz code. The final external harness has no compilation errors; dependency warnings remain. The broad `cargo fmt --all -- --check` reports existing formatting differences across the workspace and path dependencies, including protected nago-wss. No bulk formatting was applied to those files; the actual integration files pass the scoped check. This broad formatting failure is retained in the ledger and is not a correctness failure.

## Integration repairs

- Fixed scanner test function paths (`classify::classify` for AVX2/AVX-512), the missing pattern compiler `ValidatedProgram` import, and a benchmark encode borrow conflict by computing limits before borrowing the sink mutably. Workspace check includes compat, bench, encode and facade callers; no additional legacy DOM adapter was inserted.
- Added private-field `jmax_nagoya::ValidationOutput` and the backward-compatible default `ParseValidator::validate_output`. The canonical `SchemaValidator` attaches `ValidationCapability` immediately after direct strict VM extraction. Publication checks exact live backing/range, decoded length and schema evidence before bypassing byte equality. External validators receive the defensive byte comparison. Existing implementers of `validate` continue to work. Owner/result fields cannot be replaced while retaining the private capability.
- Forwarded `jmax-wss/std` weakly to `nago-wss?/std`, resolving the optional all-feature dependency's missing `ToString` prelude without changing the read-only sibling. This does not enable the optional transport for default/no-default consumers.
- Regenerated the sole workspace Cargo.lock with the allocator/dependency changes, and refreshed the external lockfile and complete [harness snapshot](../benchmark/harness-snapshot/).
- External harness fixes: unit-struct allocator imports instead of type aliases used as values; closure ownership for scratch/query/parser state; observe borrowed scratch results inside the closure; retain pikkr query bytes for the jobs' lifetime; use `usize::MAX` for untrained pikkr; select the direct native streaming encoder for its sink job; correct the borrowed syntax operation label. The GitHub row schema is incompatible with the corpus's root array, so three whole-array schema extraction jobs are explicitly N/A; typed row processing and array-index path query remain exercised.
- The conformance probe originally aborted on sonic-rs 0.5.10 LazyValue's invalid-UTF-8 debug panic. It now catches and records that competitor panic separately (`panicked: ["sonic-lazy-document"]` on the invalid-utf8 row). It does not silently count the panic as an ordinary parser rejection. jsonmax passes all 18 cases. The panic hook remains visible in the passing preflight log.
- Removed ps-pikkr's inherited production `.expect(...)` from `QueryNode::id` using an explicit match, preserving its existing documented panic on a root node. A supplemental exclusive transfer is recorded in CONTRACTS. Both required ps-pikkr suites passed again after this edit; S8's original provenance inventories remain historical evidence.

Early failed compilation/test attempts are preserved. Two follow-up compile attempts also found integer literal inference in the newly added x86 gating test; explicit closure/literal types fixed it. An initial static allocator audit counted a documentation example as a library attribute; the corrected audit matches actual source attributes. The syntax test file also needed formatting under the workspace’s 2024 edition rather than its earlier worker check’s edition. There are no unresolved mandatory failures.

## Correctness coverage

| Required behavior | Executed coverage |
| --- | --- |
| Scalar/native SIMD masks, 0..=64, carries, lane edges and short tails | All four scanner regression suites exercise independent byte/quote oracles, byte positions/values, quote/backslash patterns, all tails and multi-block all-backslash carries. NEON + PMULL executes on this host. Added dispatch guard-page differential testing across 0..=3 full blocks, every 0..=64 tail, all four incoming carry states and all available backends; empty slices point exactly at the inaccessible page. |
| Strict grammar, UTF-8, surrogates, duplicates, trailing junk, punctuation, nesting and limits | Syntax, string, UTF-8, DOM, VM and facade suites cover malformed/truncated/overlong UTF-8, surrogate pairs and isolated surrogates, escaped key equivalence, strict EOF/punctuation, nesting, numeric policy, payload/string/digit/decoded/field/item/depth limits and rollback. Added inclusive payload-limit tests for owned, borrowed and proof-only paths, and an explicit strict-discard rejection matrix for crossed braces, raw controls, invalid numbers and escaped duplicates. |
| Query-only proof separation and strict discarded subtrees | Syntax/cursor/VM/facade tests cover honest query flags, query-only skipped malformed interiors, strict rejection of crossed `{]`, controls, invalid numbers, duplicates, and constraints/required fields on unselected values. Query result types have no proof/capability fields or AOT SlotSource implementation. Strict consumers and live-owner capability skips remain separate. |
| Persistent cursor and streaming compatibility | Every incremental split-point comparison, sticky failures, query-finish revalidation, and skip suspend/resume regressions pass. Complete native parse/direct VM retains its cached BlockCursor dispatch and no whole-input token/index buffer. |
| Pointer/range/owner preservation | Facade/DOM/HTTP/WSS/nagoya tests cover Vec adoption, Bytes, Arc<[u8]>, generic Arc<T>, nested checked slices/try_slice, exact pointer/range, owner lifetime after caller drops, invalid ranges, retained backing accounting and cleanup. Borrowed input and scratch-lifetime compile-fail doctests pass. |
| Native u64 tape and encoding | DOM tests cover compact/extended key/string records, compact length boundary, full-u32 extended number operands, parent/subtree/member navigation and explicit legacy conversion/layout rejection. Encoder/facade tests cover owned/borrowed/native/legacy parity, parse-roundtrip, escapes, raw numbers, sink/size/depth failures, streaming/staged publication and scratch recovery. No implicit ValidatedDocument construction was introduced. |
| Hosted rings and capability fast path | Four reactive-ring tests execute controlled futures and wakeups for FIFO sequence/no gaps, rejection, queued cancellation, completion backpressure and shutdown. Four ownership/admission tests verify lease release and byte-comparison fallback. Added a private fast-path predicate test for exact owner/range/proof/decoded mismatches, plus an actual canonical SchemaValidator integration test including policy mismatch and cancellation. `spawn_on` is compiled; network sockets and an external live service are outside these tests. |
| Allocator and durable competitor | All four binary roots have exactly one global allocator and RustyAlloc by default; System remains the benchmark ablation. jsonmax installs no library global. The standalone consumer links and runs with one exported allocator installation. ps-pikkr native/portable issue, speculation/reorder, Unicode, raw-result pointer and guard-page regressions pass. |

The new x86 dispatch gate unit test exercises all 64 combinations of CPU/OS/scanner-build flags on ARM without executing x86 instructions.

## x86 execution and compile/cfg status

**x86 runtime execution: UNEXECUTED.** This is an Apple M4 Max host; no real x86 runner was available. Passing AVX2/AVX-512 crate tests here exercises their scalar fallback/oracles, not x86 intrinsics.

All scanner crates and dispatch compile in the native workspace/cfg matrix. [architecture-audit.log](architecture-audit.log) records direct full-block routing, four-load NEON/two-load AVX2 source kernels, separately gated `neon,aes` PMULL and `pclmulqdq` PCLMUL helpers, persistent cursor dispatch, duplicate owner and SkipMode routes. AArch64 native/portable behavior is executed; no per-block feature-detection or full-block staging fallback was introduced by integration.

The **1.98.1 sysroot has only `aarch64-apple-darwin` installed**, so V1 did not cross-compile the x86 intrinsic bodies. S8's earlier x86 Windows cross-checks under 1.98.0 are recorded in S8.md and its durable logs; those are not counted as this stage's 1.98.1 verification or runtime execution. No real absent-PMULL ARM host was tested. Scalar prefix fallback/oracles and ps-pikkr portable execute locally. Generated assembly/load-count inspection and performance measurements remain for the measurement stage; source inspection alone is not a generated-code guarantee.

## Preservation, limits and benchmark handoff

The four protected repositories were accessed read-only. A start/end SHA-256 inventory verifies **126 allocator, 606 nagoya, 28 nago-http and 32 nago-wss files unchanged**, excluding .git/target. The untracked investigation directory is unchanged; target/ was retained and used for builds, with no cargo clean/removal. See [preserved-source-start.json](preserved-source-start.json) and the final source audit. Final hashes for 293 source/manifest files are in [source-final.sha256](source-final.sha256); six external corpus files plus their two provenance JSON files are hashed in [corpus.sha256](corpus.sha256). Pre-Wave-6 HEAD/tree/manifest hashes remain in CONTRACTS.md.

Ready to hand off: all requested final commands pass, mandatory targeted regressions execute natively, both allocator configurations compile, the durable competitor passes, the six-corpus external harness preflights pass, and the snapshot matches the current external sources/lockfile byte-for-byte. Heavy commands in this stage ran sequentially; no benchmark process was left running.

Preserve semantic labels in the next stage. ps-pikkr remains a query-only selector; trained/untrained selection, strict syntax, strict schema validation/extraction, owned/borrowed/warm DOM, and streaming/staged encoding are distinct operations. Canonical seven-workload definitions were not changed by these integration repairs; the external preflight covers its own six historical corpora, not all seven canonical workloads or every stress workload.

Existing handoff limitations remain explicit: the generic typed `from_borrowed<T: JsonDecode>` compatibility adapter copies input once (borrowed DOM/strict extraction do not); arbitrary caller-constructed native NodeId membership is checked by a record walk while genuine handle navigation is O(1); long synchronous VM turns have before/after cancellation rather than instruction-level interruption; Bytes hidden backing capacity is only a lower-bound account. Benchmark warm allocation freedom, throughput, allocator effects, retained capacities, generated SIMD code and before/after comparisons have not been measured by V1. These limitations do not turn query results into validation evidence or introduce implicit legacy DOM construction.

## Complete verification command ledger

Every build/test/feature/format/environment/audit invocation, including unsuccessful and superseded attempts, is listed below; raw logs retain command cwd and full output. Machine-readable equivalents: [commands.json](commands.json). `W` = workspace, `P` = `/Users/revenge/code/ps-pikkr`, `H` = `/tmp/jmax-vs-parsers`. Commands inherit the pinned environment above. Counts for early attempts are historical, not added to the final totals.

Read-only preparation also used `pwd`, `rg --files`/`rg -n`, `cat`, `sed`, `ls`, `git status --short`, `git diff --stat/--numstat/--check`, and Python reads/hashes to inspect the requested plan/contracts/handoffs, manifests, tests and relevant sibling APIs. Edits used apply_patch/Python, mkdir/cat/chmod for the log runner/fixtures, and shutil.copyfile for snapshots; no destructive cleanup was run. These inspection/edit operations are not test executions.

| Log | Cwd | Command | Exit | Test passed/failed/ignored |
| --- | --- | --- | --- | --- |
'''
for entry in entries:
    code = {'/Users/revenge/code/jsonmax-rs':'W', '/Users/revenge/code/ps-pikkr':'P', '/tmp/jmax-vs-parsers':'H'}.get(entry['cwd'],entry['cwd'])
    counts = f"{entry['passed']}/{entry['failed']}/{entry['ignored']}" if entry['passed'] or entry['failed'] or entry['ignored'] else '—'
    report += f"| [{entry['log']}]({entry['log']}) | {code} | `{entry['command']}` | {entry['exit']} | {counts} |\n"
report += f"\nLedger totals: **{len(entries)} invocations, {sum(e['exit']==0 for e in entries)} exit-zero, {sum(e['exit']!=0 for e in entries)} failed/superseded attempts**. Every final gate listed above exits zero. The broad pre-existing formatting failure is retained explicitly.\n"
(out / 'VERIFICATION.md').write_text(report)
print(f'Wrote VERIFICATION.md and commands.json: {len(entries)} logged commands')
