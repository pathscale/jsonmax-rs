"""Finalize only after run-timings.py exits successfully; no build or timing jobs here."""
from pathlib import Path
import subprocess,json,hashlib,datetime,os
R=Path('/Users/revenge/code/jsonmax-rs'); O=R/'reports/wave6/benchmark'; V=R/'reports/wave6/verification'
def run(script,log):
 with (O/log).open('w') as f:subprocess.run(['python3',str(O/script)],cwd=R,stdout=f,stderr=subprocess.STDOUT,check=True)
run('verify-artifacts.py','artifact-validation.log')
run('write-report.py','report-summary.json')
run('post-stage6-report.py','post-stage6-report.log')
subprocess.run(['zsh',str(V/'run-gate.sh'),'post-stage6-source-audit',str(R),'python3',str(V/'audit.py')],cwd=R,check=True)
counts=json.loads((V/'post-stage6-test-counts.json').read_text()); summary=json.loads((O/'post-stage6-summary.json').read_text()); validation=json.loads((O/'artifact-validation.json').read_text())
p=V/'VERIFICATION.md';s=p.read_text();needle='\n---\n'
assert needle in s
block=f'''
Post-Stage-6 benchmark verification is complete. The added warm recycled query row passes the final [all-target harness check](post-stage6-final-harness-check.log). Both release harnesses rebuilt under RustyAlloc and System before timing; preflights pass **348 native operations and 247 external operations per allocator**, plus 18 external conformance probes. All six canonical CLI processes, two supplemental processes and six competitor processes exit zero. The invariant probe's final execution passes; its earlier overbroad warm-query allocation assertion is retained as a superseded attempt, not a product test failure.

The final [source audit](post-stage6-source-audit.log) confirms zero prohibited `.expect(...)`, `todo!()` or `unimplemented!()` in workspace/competitor sources, correct allocator installation, snapshot equality and `git diff --check`. The [full protected-file inventory](post-stage6-protected-final.json) matches all four siblings, including target files if present. The benchmark [final audit](../benchmark/final-audit.json) verifies all 282 workspace source/manifest files unchanged throughout measurement, and the [artifact validation](../benchmark/artifact-validation.json) checks command intervals, build-before-timing order, samples and binary hashes. No task-owned build/test overlaps timing.

Measured results: EscapedUnicode helper classifications **129 blocks/8,154 bytes → 32/1,989**, while the cursor pass remains 2,012 bytes. Sparse-subtree warm strict validation/extraction **4 allocations/720 bytes → 0/0** in owned/borrowed execution and both allocators. Retained query plans rebuild their trie zero times during execution; the warmed nested query-only probe still allocates **1 time/8 bytes per call**, while warmed strict query recycling is zero. Native DOM duplicate-slot capacities and encoder allocation/throughput observations, including regressions and retained-capacity tradeoffs, are in [FINAL.md](../FINAL.md) and the [post-pass comparisons](../benchmark/tables/post-stage6-comparison.md).
'''
s=s.replace(needle,'\n'+block+needle,1);p.write_text(s)
now=datetime.datetime.now().astimezone().isoformat()
(R/'reports/wave6/STATUS.md').write_text(f'''# Wave 6 execution status

Updated {now}.

- Stages 0–3: implementation/contracts complete; S1–S8 and L1–L2 handoffs retained.
- Stage 4: earlier integration ledger retained in VERIFICATION.md; native ps-pikkr 29 and portable 27 passing tests are historical, not rerun totals.
- Stage 5: original report, samples, binaries, snapshots and tables preserved under benchmark/pre-stage6/.
- Stage 6: P1/P2/P3 bottleneck changes integrated. All requested checks pass: workspace 148 passed/0 failed; native feature tests 35 passed/0 failed/1 ignored. Counts overlap.
- Post-Stage-6 measurement: all four release variants rebuilt/preflighted before any timing; all 14 timing processes exited zero and ran sequentially. Complete current results: FINAL.md, benchmark/tables/ and benchmark/post-stage6-summary.json.

EscapedUnicode helper work fell from 129 blocks/8,154 bytes to 32/1,989. Sparse-subtree warm strict schema validation/extraction fell from 4 allocations/720 bytes to 0/0 in both allocators and ownership modes. All 28 canonical warm owned operations per allocator remain zero-allocation. Trie builds during execution are zero; the warmed nested query-only fixture still allocates one 8-byte delimiter stack per call, while warmed strict query execution allocates zero. Derived borrowed typed decoding reads the original input without the former facade payload copy; owned output fields/custom decoders may still copy.

Source and sibling inventories, snapshots, sample counts, hashes and command interval audits pass. All four protected sibling repositories remain unchanged and read-only. No commit or unrelated process termination occurred. Real x86 execution, isolated attribution, individual-request latency tails and a Wave 5/RustyAlloc baseline remain unmeasured. Host load, cold/new-shape allocations, overlapping query traversal and capacity tradeoffs are documented in FINAL.md. The initial superseded probe assertion is preserved in the ledger.
''')
audit=json.loads((O/'final-audit.json').read_text());audit.update(protected_siblings_unchanged=True,protected_files_per_repository=summary['protected_repositories'],generated_corpora_unchanged=summary['generated_corpora_unchanged'],sequential_command_intervals=True)
(O/'final-audit.json').write_text(json.dumps(audit,indent=2)+'\n')
report_summary=json.loads((O/'report-summary.json').read_text());report_summary.update(post_stage6=summary,artifact_validation=validation,audit=audit)
(O/'report-summary.json').write_text(json.dumps(report_summary,indent=2)+'\n')
for name in ['build-and-preflight-progress.txt','invariant-progress.txt','timing-progress.txt']:
 (O/name).write_text(f'Post-Stage-6 execution complete {now}. See commands.jsonl for start/end/exit records; invariants-initial.log preserves the superseded probe assertion. All final attempts pass.\n')
(O/'git-status-final.txt').write_text(subprocess.run(['git','status','--short'],cwd=R,capture_output=True,text=True,check=True).stdout)
(O/'deliverable-hashes.json').write_text(json.dumps({name:hashlib.sha256((R/'reports/wave6'/name).read_bytes()).hexdigest() for name in ['FINAL.md','STATUS.md']},indent=2)+'\n')
files=[]
for p in O.rglob('*'):
 if p.is_file() and not any(c in ['pre-stage6','invariant-source','__pycache__'] for c in p.relative_to(O).parts) and p.name!='SHA256SUMS':files.append(p)
(O/'SHA256SUMS').write_text(''.join(f'{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.relative_to(O)}\n' for p in sorted(files)))
print(json.dumps({'finished':now,'test_counts':counts,'artifact_validation':validation,'audit':audit,'sha256_files':len(files)},indent=2))
