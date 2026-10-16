#!/usr/bin/env python3
import json,csv,math,statistics,hashlib,datetime
from collections import defaultdict
from pathlib import Path
R=Path('/Users/revenge/code/jsonmax-rs');O=R/'reports/wave6/benchmark';T=O/'tables';T.mkdir(exist_ok=True)
def read(p):return [json.loads(s) for s in Path(p).read_text().splitlines() if s.startswith('{')]
def pct(a,p):return sorted(a)[math.ceil(len(a)*p)-1]
def gm(a):return math.exp(statistics.mean(math.log(v) for v in a))
def save(name,rows):
 (T/(name+'.json')).write_text(json.dumps(rows,indent=2)+'\n')
 if rows:
  keys=list(dict.fromkeys(k for r in rows for k in r if not isinstance(r[k],(dict,list))))
  with (T/(name+'.csv')).open('w') as f:
   w=csv.DictWriter(f,fieldnames=keys,extrasaction='ignore');w.writeheader();w.writerows(rows)
def md(headers,rows):return '\n'.join(['| '+' | '.join(headers)+' |','| '+' | '.join(['---']*len(headers))+' |']+['| '+' | '.join(map(str,r))+' |' for r in rows])+'\n'
def cell(r):return 'N/A' if not r else f"{r['mb_s']:.2f}; {r['median_ns']:.1f} / {r['p95_ns']:.1f}"
# Native CLI pools 3 processes x20 pass averages. Throughput uses all elapsed times.
native=[]
for alloc in ['system-alloc','rusty-alloc']:
 groups=defaultdict(list);run_medians=defaultdict(list)
 for i in range(1,4):
  rows=read(O/f'jmax-bench-{alloc}-r{i}-samples.jsonl');rg=defaultdict(list)
  for row in rows:groups[(row['workload'],row['mode'],row['regime'])].append(row);rg[(row['workload'],row['mode'],row['regime'])].append(row['elapsed_ns']/row['documents'])
  for key,vals in rg.items():run_medians[key].append(pct(vals,.5))
 for (workload,mode,regime),vals in groups.items():
  ns=[v['elapsed_ns']/v['documents'] for v in vals];byte=vals[0]['input_bytes'];assert all(v['input_bytes']==byte and v['documents']==128 for v in vals)
  native.append(dict(allocator=alloc,workload=workload,mode=mode,regime=regime,payload_bytes=byte,documents=128,median_ns=pct(ns,.5),p95_ns=pct(ns,.95),mb_s=sum(v['input_bytes'] for v in vals)*1000/sum(v['elapsed_ns'] for v in vals),run_medians_ns=run_medians[(workload,mode,regime)],process_spread_pct=(max(run_medians[(workload,mode,regime)])/min(run_medians[(workload,mode,regime)])-1)*100))
save('native-cli',native)
# A is the preserved native CLI output, rather than a different external harness.
historical=[]
for line in (R/'reports/benchmark-7-workloads-cold-vs-warm/native-cli.txt').read_text().splitlines():
 c=[v.strip() for v in line.split('|')[1:-1]]
 if len(c)==10 and c[1] in ['Parse','Validate','Extract','Serialize']:
  historical.append(dict(workload=c[0],mode=c[1],regime=c[2],payload_bytes=int(c[3]),median_ns=int(c[5])/128,p95_ns=int(c[6])/128,mb_s=float(c[7])))
assert len(historical)==56,len(historical)
ni={(r['allocator'],r['workload'],r['mode'],r['regime']):r for r in native};comparison=[];comparison_md=[]
for a in historical:
 key=(a['workload'],a['mode'],a['regime']);c=ni[('system-alloc',)+key];d=ni[('rusty-alloc',)+key];assert a['payload_bytes']==c['payload_bytes']==d['payload_bytes']
 comparison.append(dict(workload=key[0],mode=key[1],regime=key[2],payload_bytes=a['payload_bytes'],wave5_system_mb_s=a['mb_s'],wave5_system_median_ns=a['median_ns'],wave5_system_p95_ns=a['p95_ns'],wave6_system_mb_s=c['mb_s'],wave6_system_median_ns=c['median_ns'],wave6_system_p95_ns=c['p95_ns'],wave6_rusty_mb_s=d['mb_s'],wave6_rusty_median_ns=d['median_ns'],wave6_rusty_p95_ns=d['p95_ns'],C_over_A_throughput=c['mb_s']/a['mb_s'],D_over_C_throughput=d['mb_s']/c['mb_s']))
 comparison_md.append([key[0],key[1],key[2].split(' ')[0],cell(a),cell(c),cell(d)])
save('before-after',comparison)
(T/'before-after.md').write_text('Each cell: **MB/s; p50 / p95 ns/doc**. A is archived System; C/D are pooled Wave 6. MB/s uses mean pass time.\n\n'+md(['Workload','Mode','Regime','A: Wave 5 System','C: Wave 6 System','D: Wave 6 RustyAlloc'],comparison_md))
supplement=[];storage=[];invariants=[]
for alloc in ['system-alloc','rusty-alloc']:
 for row in read(O/f'jmax-supplement-{alloc}.jsonl'):
  row={**row,"allocator":alloc}
  if 'mode' in row:supplement.append(row)
  else:storage.append(row)
save('native-regimes',supplement);save('storage-ownership',storage)
si={(r['allocator'],r['workload'],r['mode'],r['regime']):r for r in supplement}
regime_md=[]
for d in [r for r in supplement if r['allocator']=='rusty-alloc']:
 c=si[('system-alloc',d['workload'],d['mode'],d['regime'])]
 regime_md.append([d['workload'],d['mode'],d['regime'],cell(c),cell(d),f"{d['allocs_op']:.2f} / {d['bytes_op']:.2f}"])
(T/'native-regimes.md').write_text('Operation-only supplemental loop; each cell MB/s; p50 / p95 ns/doc. Allocations/bytes are a separate untimed pass. A/B unavailable for these exact loops.\n\n'+md(['Workload','Mode','Regime','C: System','D: RustyAlloc','D allocs / bytes per op'],regime_md))
stress_names=['long-unescaped-ascii','long-unescaped-unicode','escape-dense','long-backslash-runs','wide-object','reordered-fields','sparse-large-subtree']
(T/'stress.md').write_text(md(['Workload','Mode','Regime','C: System','D: RustyAlloc','D allocs / bytes per op'],[r for r in regime_md if r[0] in stress_names]))
# Competitors: 183 per-job sample averages; prepared simd rows also 183, 8 refreshed buffers/sample.
external=[];capabilities=[]
for alloc in ['system-alloc','rusty-alloc']:
 g=defaultdict(list)
 for number in range(1,4):
  for row in read(O/f'jmax-vs-parsers-{alloc}-r{number}.jsonl'):
   if 'median_ns' in row:g[(row['corpus'],row['category'],row['parser'])].append(row)
   elif number==1:capabilities.append({**row,"allocator":alloc})
 for (corpus,category,parser),vals in g.items():
  assert len(vals)==3
  ns=[n for r in vals for n in r['samples_ns']];med=pct(ns,.5)
  external.append(dict(allocator=alloc,corpus=corpus,category=category,parser=parser,bytes=vals[0]['bytes'],median_ns=med,p95_ns=pct(ns,.95),mb_s=vals[0]['bytes']*1000/med,allocs_op=statistics.mean(r['allocs_op'] for r in vals),bytes_op=statistics.mean(r['bytes_op'] for r in vals),samples=len(ns),run_medians_ns=[r['median_ns'] for r in vals],run_reps=[r['reps'] for r in vals],process_spread_pct=(max(r['median_ns'] for r in vals)/min(r['median_ns'] for r in vals)-1)*100))
save('competitors',external);save('capabilities',capabilities)
ei={(r['allocator'],r['corpus'],r['parser']):r for r in external}
(T/'competitors.md').write_text('Each throughput is input-normalized, including query-only rows; query MB/s is not full validation bandwidth.\n\n'+md(['Allocator','Corpus','Category','Implementation','MB/s','p50 ns/doc','p95 ns/doc','allocs/op','bytes/op'],[[r['allocator'],r['corpus'],r['category'],r['parser'],f"{r['mb_s']:.2f}",f"{r['median_ns']:.1f}",f"{r['p95_ns']:.1f}",f"{r['allocs_op']:.2f}",f"{r['bytes_op']:.2f}"] for r in external]))
old_external=json.loads((R/'reports/benchmark-jmax-vs-parsers-2026-10-15/aggregate.json').read_text());oei={(r['corpus'],r['parser']):r for r in old_external}
six_comparison=[]
for corpus in ['twitter','citm_catalog','canada','github_events','apache_builds','endpoint_batch']:
 for mode,old,new in [('Syntax scan','jmax-cursor-strict+copy','jmax-syntax-strict-borrowed'),('DOM + input copy','jmax-dom+copy','jmax-dom+copy'),('Owned typed projection','jmax-vm-owned-extract','jmax-vm-owned-extract'),('Native serialize','jmax-dom-writer','jmax-native-tape-encode-sink')]:
  a=oei[(corpus,old)];c=ei[('system-alloc',corpus,new)];d=ei[('rusty-alloc',corpus,new)]
  six_comparison.append(dict(corpus=corpus,mode=mode,A_parser=old,CD_parser=new,A_mb_s=a['mb_s'],A_median_ns=a['median_ns'],A_p95_ns=a['p95_ns'],C_mb_s=c['mb_s'],C_median_ns=c['median_ns'],C_p95_ns=c['p95_ns'],D_mb_s=d['mb_s'],D_median_ns=d['median_ns'],D_p95_ns=d['p95_ns']))
save('historical-six-corpus',six_comparison)
(T/'historical-six-corpus.md').write_text('Scan removes the historical input copy; serialization uses the new direct native streaming path. These API transitions are part of the observed change, not isolated kernel speedups.\n\n'+md(['Corpus','Mode','A MB/s; p50/p95 ns','C MB/s; p50/p95 ns','D MB/s; p50/p95 ns'],[[r['corpus'],r['mode']]+[f"{r[a+'_mb_s']:.2f}; {r[a+'_median_ns']:.1f} / {r[a+'_p95_ns']:.1f}" for a in ['A','C','D']] for r in six_comparison]))
# System observations: exact snapshots and warnings are retained.
loads=[r for r in read(O/'system-load.jsonl') if r['stage'].startswith(('jmax-bench-','jmax-supplement-','jmax-vs-parsers-'))]
loadstats={'snapshots':len(loads),'load1_min':min(r['loadavg'][0] for r in loads),'load1_median':statistics.median(r['loadavg'][0] for r in loads),'load1_max':max(r['loadavg'][0] for r in loads),'first_at':loads[0]['at'],'last_at':loads[-1]['at']}
othercpu=[];compiler_cpu=[];unrelated_bench=[]
for r in loads:
 total=rust=0
 for line in r['processes'].splitlines()[1:]:
  c=line.strip().split(None,3)
  if len(c)!=4:continue
  try:cpu=float(c[2])
  except ValueError:continue
  command=c[3]
  if 'reports/wave6/benchmark/bin/' in command:continue
  if cpu>=5:total+=cpu
  if command.endswith('rustc') or '/rustc ' in command:rust+=cpu
  if ('parallel_timing' in command or 'chuzz-headless' in command) and cpu>0:unrelated_bench.append({'at':r['at'],'process':command,'cpu':cpu})
 othercpu.append(total);compiler_cpu.append(rust)
loadstats.update(other_cpu_median=statistics.median(othercpu),other_cpu_max=max(othercpu),compiler_cpu_max=max(compiler_cpu),unrelated_benchmark_snapshots=len(unrelated_bench))
(O/'load-summary.json').write_text(json.dumps(loadstats,indent=2))
# Scan/growth probe is intentionally untimed and uses a separately instrumented source copy.
if (O/'invariants.log').exists():
 invariant_log=max([p for p in [O/'invariants.log',O/'invariants-owner-clone.log'] if p.exists()],key=lambda p:p.stat().st_mtime)
 invariants=read(invariant_log);save('scan-invariants',invariants)
 (T/'scan-invariants.md').write_text(md(['Workload','Operation','Input bytes','Cursor blocks','Cursor bytes classified','Escape helper blocks','Escape helper bytes classified','Query skip call bytes','Strict skip call bytes'],[[r['workload'],r['mode'],r['input_bytes'],r['cursor_blocks'],r['cursor_classified_bytes'],r['decode_helper_blocks'],r['decode_helper_classified_bytes'],r['query_skip_call_bytes'],r['strict_skip_call_bytes']]for r in invariants if r['category']=='scan-invariants']))
# Freeze/preservation audit, independent of benchmark runtime.
source=json.loads((O/'source-final.sha256.json').read_text());changed=[p for p,h in source.items() if hashlib.sha256((R/p).read_bytes()).hexdigest()!=h]
history=json.loads((O/'historical-preserved-start.json').read_text());history_changed=[p for p,h in history.items() if not (R/p).exists() or hashlib.sha256((R/p).read_bytes()).hexdigest()!=h]
audit={'timed_source_unchanged':not changed,'source_files':len(source),'changed_source':changed,'historical_files':len(history),'historical_unchanged':not history_changed,'historical_changes':history_changed,'snapshot_matches':all((O/'harness-snapshot'/p).read_bytes()==(Path('/tmp/jmax-vs-parsers')/p).read_bytes() for p in ['Cargo.toml','Cargo.lock','src/main.rs'])}
assert all([audit['timed_source_unchanged'],audit['historical_unchanged'],audit['snapshot_matches']]),audit
(O/'final-audit.json').write_text(json.dumps(audit,indent=2))
# Main report is deliberately generated from the same machine-readable tables.
canonical=['NagoyaAuth','ExchangeOrder','RpcEnvelope','LargeCatalog','VectorCoordinates','DeepNesting','EscapedUnicode']
mb_ratios=[r['C_over_A_throughput'] for r in comparison];ablation=[r['D_over_C_throughput'] for r in comparison]
zero=[r for r in supplement if r['allocator']=='rusty-alloc' and r['workload'] in canonical and (r['regime']=='Warm-Owned') and r['mode'] in ['Parse','Validate-Schema','Extract-Strict','Serialize-Native-Direct']]
assert len(zero)==28 and all(r['allocs_op']==0 for r in zero)
head=[]
for corpus in ['twitter','citm_catalog','canada','github_events','apache_builds','endpoint_batch']:
 for op,names in [('DOM Parse',['jmax-dom-borrowed','serde-json-value','simd-json-borrowed+copy','simd-json-borrowed-prepared','sonic-rs-value']),('Typed strict projection',['jmax-vm-owned-extract','serde-json-typed','simd-json-typed+copy','sonic-rs-typed']),('Serialize',['jmax-native-tape-encode-sink','serde-json-to-vec','simd-json-to-vec','sonic-rs-to-vec']),('Query only',['jmax-query-only-extract-borrowed','ps-pikkr-trained-query','ps-pikkr-untrained-query'])]:
  for name in names:
   r=ei.get(('rusty-alloc',corpus,name));head.append([corpus,op,name,cell(r),f"{r['allocs_op']:.1f}" if r else 'N/A'])
(T/'head-to-head-rusty.md').write_text(md(['Corpus','Operation','Implementation','MB/s; p50 / p95 ns/doc','allocs/op'],head))
memory=[r for r in storage if r['allocator']=='rusty-alloc' and r.get('category')=='storage']
(T/'memory.md').write_text(md(['Workload (first doc)','Input','u64 tape live','Decoded live','Warm tape capacity','Warm decoded capacity','Legacy 6 Vec live','Legacy 6 Vec capacity'],[[r['workload'],r['input_bytes'],r['tape_bytes'],r['decoded_bytes'],r['warm_tape_capacity_bytes'],r['warm_decoded_capacity_bytes'],r['legacy_six_vec_live_bytes'],r['legacy_six_vec_capacity_bytes']]for r in memory]))
dom_ratios=[ei[('rusty-alloc',n,'serde-json-value')]['median_ns']/ei[('rusty-alloc',n,'jmax-dom-borrowed')]['median_ns'] for n in ['twitter','citm_catalog','canada','github_events','apache_builds','endpoint_batch']]
typed_ratios=[ei[('rusty-alloc',n,'serde-json-typed')]['median_ns']/ei[('rusty-alloc',n,'jmax-vm-owned-extract')]['median_ns'] for n in ['twitter','citm_catalog','canada','github_events','apache_builds','endpoint_batch']]
serialize_ratios=[ei[('rusty-alloc',n,'serde-json-to-vec')]['median_ns']/ei[('rusty-alloc',n,'jmax-native-tape-encode-sink')]['median_ns'] for n in ['twitter','citm_catalog','canada','github_events','apache_builds','endpoint_batch']]
report=f'''# Wave 6 sequential benchmark and final report

Completed {datetime.datetime.now().astimezone().isoformat()}. Workspace `/Users/revenge/code/jsonmax-rs`.

## Executive result

Wave 6's native tape, borrowed input, direct schema execution, fused classifiers and allocator integration are implemented and pass the recorded correctness gates. The 56 matched canonical CLI rows show **{gm(mb_ratios):.2f}× geometric-mean observed throughput for Wave 6/System versus archived Wave 5/System** (per-row range {min(mb_ratios):.2f}–{max(mb_ratios):.2f}×). This is a historical observation under different toolchains, API internals and host contention; it is not an isolated architecture speedup. The matched Wave 6 allocator ablation gives **{gm(ablation):.3f}× RustyAlloc/System** across these rows, range {min(ablation):.3f}–{max(ablation):.3f}×. Allocator effects cannot be subtracted from the historical comparison.

All **28 canonical warm owned cases** (7 workloads × Parse, strict schema Validate, strict Extract, direct Serialize) allocate **0 times / 0 requested bytes per document** after warming. The sparse-subtree strict schema validation/extraction results are remeasured below; retained scratch now covers discarded-subtree state. There is no universal zero-allocation claim. Geometric-mean speed ratios against serde_json are **{gm(dom_ratios):.3f}× for borrowed DOM**, **{gm(typed_ratios):.3f}× for owned typed projection**, and **{gm(serialize_ratios):.3f}× for serialization** (below 1 means slower). The ps-pikkr rows measure a narrower query-only task. These semantic differences and all values remain explicit below.

## Implemented architecture and integration

| Owner | Delivered behavior |
| --- | --- |
| C0 | Frozen ABI masks, tape and ownership contracts; checked owner subviews/capabilities; integrated callers and verification repairs. |
| S1 | Four-load NEON / two-load AVX2 fused classification, direct full blocks, word backslash parity, independently gated PMULL/PCLMUL, bulk UTF-8 and escape helpers. |
| S2 | Persistent BlockCursor with cached dispatch, one checked grammar transition, borrowed core, bulk strings, transactional escaped output, separate strict/query/live-owner skips. |
| S3 | Private immutable `TapeDocument` / `BorrowedDocument`, variable-width `Vec<u64>` records, parent/subtree navigation, explicit legacy conversion. |
| S4 | Direct schema consumer, prevalidated dispatch, explicit duplicate owner, proof-only validation and strict capture; query result types carry no validation proof. |
| S5 | Native tape/borrowed encoding, cached block escape masks and maximal slice writes; direct streaming separated from staged publication. |
| S6 | Owned/borrowed facade and scratch APIs, native DOM, strict/query extraction, optional allocator export. Derived borrowed typed decoding uses `JsonDecodeBorrowed` over the original slice; caller-compiled scratch variants retain VM storage. |
| S7 | Real nagoya sync primitives for bounded job/completion rings, admission/cancellation/order, private canonical validator capability, owner-preserving transport seams. |
| S8 | Durable ps-pikkr fork with provenance, crate-name/cfg repairs, NEON+PMULL, portable and x86-gated paths, bounded tails and quote carry. |
| L1 | RustyAlloc in all four executable roots; counted System feature for benchmark ablation; no library global allocator. |
| L2 / Stage 5 | Native and external harnesses; two seven-workload generators plus seven stress inputs, borrowed/scratch/query/syntax modes, per-operation allocation accounting, prepared simd kernels, reproducible snapshots. |

Details: [contracts](CONTRACTS.md), [S1](S1.md), [S2](S2.md), [S3](S3.md), [S4](S4.md), [S5](S5.md), [S6](S6.md), [S7](S7.md), [S8](S8.md), [L1](L1.md), [L2](L2.md). P1/P2/P3 handoff tests now execute in the post-Stage-6 correctness ledger; older stage results remain preserved.

## Verification and correctness gates

[VERIFICATION.md](verification/VERIFICATION.md) is the authoritative gate ledger: post-Stage-6 native workspace **148 passed, 0 failed** (145 unit/integration + 3 compile-fail doctests); targeted native feature run **35 passed, 0 failed, 1 ignored**. Earlier Stage 4 ps-pikkr native **29 passed**, portable **27 passed** remain historical verification, not rerun counts. Counts overlap and must not be summed. Workspace/all-target/bin checks, jsonmax default/no-default alloc-only/rusty-alloc configurations, transport no-default/all-feature checks and the standalone single-global-allocator consumer pass. x86 runtime is **UNEXECUTED**; S8's 1.98.0 cross-check is not native x86 execution or a 1.98.1 x86 check. No physical ARM machine without PMULL was available. Broad `cargo fmt --all -- --check` retains pre-existing differences; scoped integration formatting passes.

Post-Stage-6 verification rebuilt both harnesses in both allocator configurations before any timing. Both new supplemental preflights pass all exposed operations; external preflights pass **247 operations per allocator** plus 18 conformance probes and semantic/typed equality. Native harness compilation and `cargo +1.98.1 test -p jmax-bench --offline` pass (three zero-test targets; these are not extra correctness tests). All timing processes exit 0. The new allocation, pointer, nonempty selection, scratch growth and reactive lease assertions pass. Sonic LazyValue's invalid-UTF-8 panic remains separately recorded in conformance output; it is not counted as an ordinary rejection. Root-array GitHub schema projection and pikkr array-index capability rows are explicitly N/A where unsupported.

## Build, environment, sequencing and measurement boundaries

Apple M4 Max, 16 logical CPUs, 64 GiB RAM, macOS 26.5, AArch64. rustc/cargo **1.98.1**, LLVM 22.1.8; native cfg includes `neon` and `aes` (PMULL). Every Cargo invocation is offline; release builds and native tests use `RUSTFLAGS="-C target-cpu=native"`, while the requested plain checks omit it. The exact requested `export PATH="/Users/revenge/code/ps-pikkr/.tooling/bin:/Users/revenge/.cargo/bin:$PATH"` and pinned RUSTC/RUSTDOC are applied. Builds and tests run with one Cargo job and one Rust test thread. No global toolchain settings changed. Exact commands, environment, source/manifest/binary/corpus hashes and process observations are archived in [benchmark/](benchmark/).

The task's jobs run strictly sequentially: one child build/test/benchmark at a time, all timed release builds/preflights and the bench test completed before timing, with no task-owned Cargo/rustc build during timing. All four timed harness variants are invoked from saved allocator-specific binaries; no `cargo run` appears in timing. Unrelated user processes were left running. Across {loadstats['snapshots']} timing observations, one-minute load was **{loadstats['load1_min']:.2f}–{loadstats['load1_max']:.2f}**, median **{loadstats['load1_median']:.2f}**. Other processes individually at ≥5% CPU summed to median **{loadstats['other_cpu_median']:.1f}%**, maximum **{loadstats['other_cpu_max']:.1f}%** (100%=one core). Unrelated rustc reached **{loadstats['compiler_cpu_max']:.1f}%**; unrelated benchmark applications were visible in {loadstats['unrelated_benchmark_snapshots']} observations. Exclusive access applies to this task's jobs, not host isolation. `pmset -g therm` reports no recorded thermal/performance/CPU-power warning; direct temperature/frequency telemetry is unavailable, so thermal headroom is unknown. No CPU affinity or power control was applied.

- Canonical CLI: unchanged follow-up generators, seed 5354007379338153539, 128 docs, items=32, 3 warmups, 20 pass samples in each of 3 fresh processes per allocator. Pooled p50/p95 divide pass times by 128; MB/s uses total bytes/total elapsed time. CLI observation/digest work is timed. Each raw pass is saved, including digests. “Cold” means fresh result/scratch allocation, with prepared immutable input/schema owners, not cold CPU caches.
- Supplemental operation-only loop: the seven follow-up workloads, seven original generators (including Unicode), and seven stress cases at scale=32; 128 docs, 3 warmups, 20 measured passes, one process per allocator. Results are black-boxed and dropped/recycled inside timing. Schema compilation, input generation, serializer input DOMs, preflight comparisons and output I/O are untimed. Its lighter observation boundary differs from CLI; do not splice its values into historical CLI ratios. Stress repetitions share one immutable payload owner.
- External suite: same six byte-identical historical corpora, 3 fresh processes ×61 calibrated samples per allocator. Minimum 50ms/5 operations of warmup per job, sample target 2ms, deterministic shuffled job order within each corpus. Each pooled row has 183 batch averages. Prepared simd rows use 8 fresh mutable buffers/sample refreshed outside each timer, 3 warmup samples; parse/result allocation and destruction remain timed. These are prepared-input kernels, with different cache/working-set conditions, not end-to-end timings.
- Allocations and requested bytes are separate untimed operation passes after capacities warm (128 docs supplemental; 5 repetitions per external job). Realloc counts as one call and charges the full new requested size. Deallocations do not add requested bytes. Counters remain active in timings for both allocator builds; archived harness instrumentation differs. Their atomic overhead can disproportionately affect allocation-heavy cases; no uninstrumented timing rerun was performed. Allocator metadata, TLS pools, malloc usable sizes and peak RSS are not measured.
- **p95 is a percentile of corpus/batch averages, not individual-request tail latency.** All rates are decimal MB/s. Serialization rates use original JSON input bytes, not output bytes; escaped-string-array rates use decoded source string/key bytes. Query rates are input-normalized selection rates, not evidence that every byte was fully validated.

## Historical A/B/C/D comparison

A = archived Wave 5/System; B = Wave 5/RustyAlloc **not measured**; C = Wave 6/System; D = Wave 6/RustyAlloc. A's seven-workload report used rustc 1.99.0 and significant contention; its six-corpus report used 1.98.1 and different profiles/earlier source. No new Wave 5 rebuild was attempted: the preserved `run-baseline.sh` points at moving APIs and a different simd-json version. Native payload pass sizes match A exactly for all 56 rows, and all seven generated follow-up JSONL SHA-256 hashes match the archived Wave 5 corpus manifest byte-for-byte. The [generated corpus manifest](benchmark/generated-corpus-manifest.json) also saves per-document hashes for both seven-workload generators and the seven stress cases. Old schema Validate captured slots; Wave 6 Validate is proof-only. Native DOM/layout/digest and encoder internals also changed. These changes are disclosed rather than treated as allocator-only differences. B remains N/A, not an inferred estimate.

Each cell below is **MB/s; p50 / p95 ns/doc**. Complete machine-readable values and per-process variability: [before-after CSV](benchmark/tables/before-after.csv), [native CLI JSON](benchmark/tables/native-cli.json).

'''+md(['Workload','Mode','Regime','A: Wave 5 System','C: Wave 6 System','D: Wave 6 RustyAlloc'],comparison_md)+'''
Borrowed DOM, warm borrowed DOM, strict syntax-only, schema Validate, strict/query Extract and staged/direct serialization have distinct rows in the [complete native-regime table](benchmark/tables/native-regimes.md) ([CSV](benchmark/tables/native-regimes.csv), [JSON](benchmark/tables/native-regimes.json)). Every measured row has MB/s and p50/p95. Historical borrowed/query rows without an equivalent operation are N/A; no values were invented. The [six-corpus before/after table](benchmark/tables/historical-six-corpus.md) additionally compares the earlier scan/DOM/typed/serialize results, with copy/API transitions labeled.

### Supplemental canonical regimes

'''
small=[]
for name in canonical:
 for mode,regime in [('Parse','Cold-Borrowed'),('Parse','Warm-Borrowed'),('Validate-Syntax','Warm-Borrowed'),('Validate-Schema','Warm-Borrowed'),('Extract-Strict','Warm-Borrowed'),('Extract-Query-Schema','Cold-Borrowed'),('Extract-Query-Path','Cold-Borrowed')]:
  c=si[('system-alloc',name,mode,regime)];d=si[('rusty-alloc',name,mode,regime)]
  small.append([name,mode,regime,cell(c),cell(d),f"{d['allocs_op']:.0f} / {d['bytes_op']:.0f}"])
report+=md(['Workload','Mode','Regime','C: System','D: RustyAlloc','D allocs / bytes per op'],small)
report+='''
### Stress cases

These valid deterministic inputs cover long ASCII/Unicode, dense escapes, long backslashes, 1,024-field objects, reordered fields and a sparse selected field over an 89,045-byte document. Full regimes and allocation rows are in [stress.md](benchmark/tables/stress.md). The table below selects cold borrowed Parse, warm strict schema validation, and query schema extraction; A/B are N/A.

'''
rows=[]
for name in stress_names:
 for mode,regime in [('Parse','Cold-Borrowed'),('Validate-Schema','Warm-Owned'),('Extract-Query-Schema','Cold-Borrowed')]:
  c=si[('system-alloc',name,mode,regime)];d=si[('rusty-alloc',name,mode,regime)];rows.append([name,mode,cell(c),cell(d),f"{d['allocs_op']:.0f} / {d['bytes_op']:.0f}"])
report+=md(['Stress','Operation','C: System','D: RustyAlloc','D allocs / bytes per op'],rows)
report+='''
## Head-to-head competitors

All implementations in each process use the same counting allocator backend. The main table below is **RustyAlloc**; the complete [competitor table](benchmark/tables/competitors.md) ([CSV](benchmark/tables/competitors.csv), [JSON](benchmark/tables/competitors.json)) includes System, all scan/typed/DOM/string jobs and prepared simd kernels. Main dependency versions: serde/serde_derive 1.0.229, serde_json 1.0.154, simd-json 0.18.1, sonic-rs 0.5.10, ps-pikkr 0.16.1; json-rust/miniserde are retained as additional rows. The external profile is opt-level=3, thin LTO, one codegen unit; native CLI uses the ordinary workspace release profile.

Strict jmax schema checks, syntax-only scans, DOM construction, typed projections and query selection are different workloads. serde IgnoredAny, simd tape and sonic LazyValue have different Unicode/surrogate/error policies, as recorded by the conformance matrix. jmax's strict APIs check syntax/UTF-8/escapes/numeric/duplicates/limits; schema APIs additionally enforce the compiled shape. No other parser row is labeled equivalent strict schema validation merely because it accepts these valid fixtures.

`simd-json ... +copy` includes cloning immutable input to mutable storage inside timing. `... prepared` excludes input refresh and input-buffer destruction; result allocation/destruction remains included. jmax borrowed DOM excludes copying input, while `jmax-dom+copy` and typed adapters include their explicit copy. DOMs differ in number representation: jmax retains spelling with strict numeric-policy checks, while materialized competitors generally convert numbers. Typed rows produce the same owned scalar projections and strings; GitHub's jmax adapter builds a native borrowed DOM then extracts each event object, unlike competitors' direct array deserialization.

ps-pikkr is a **query-only raw-value selector**, with no full validation, schema proof, DOM or serialization. Trained setup is outside measurement; untrained uses `usize::MAX`. Captured values are compared against the reference. It retains bitmap/per-depth indexes and learned state. Unsupported DOM Parse / Full Validate / Serialize are **N/A on all six corpora**, and GitHub's array-index query is **N/A**. Success does not establish validity of unselected JSON, UTF-8, escapes, numbers, duplicate policy or trailing bytes. Object-order deoptimization correctness is tested in S8; the six-corpus timing reuses each fixed corpus and does not measure a reordered-stream deoptimization distribution. Sonic's partial lookup sometimes returns a parent object (search_metadata/venueNames) while the jmax/pikkr path returns its leaf; those partial-query jobs are separately labeled in the full table.

'''+(T/'head-to-head-rusty.md').read_text()
report+='''
## Allocation, copying and retained memory

The [complete native-regime allocation table](benchmark/tables/native-regimes.md) and [competitor table](benchmark/tables/competitors.md) report allocs/op and requested bytes/op separately from timing. Cold owned native Parse adopts/clones prepared Payload owners; it does **not** include an input copy. Borrowed Parse/schema/query input paths copy **0 input bytes**. External `jmax-dom+copy`, `jmax-dom+number-conversion+copy`, VM-any+copy and owned typed adapters copy the fixture once per operation. simd +copy rows copy the full input once inside timing; prepared kernels copy it outside timing. These are explicit API-path accounts and pointer/invariant checks, not a hardware memmove profiler. Escaped decoded output and final serialization bytes remain necessary writes.

For the first document in each workload, native live tape is exactly active u64 words ×8, plus decoded bytes. Warm tape/decoded capacities are measured after recycling into DomScratch. Legacy figures below come from explicit conversion of the **same document** into the six Vec representation, including NodeMeta, spans, containers and ConvertedNumber storage. They are representation comparisons, not a historical RSS measurement. Raw input backing is shared and excluded from both representation totals. Scratch stack and duplicate slot capacities are recorded separately in JSON; their private entry byte layouts, VM capacities, allocator pools and externally retained indexes are not included in this table. The `u64` tape is variable-width records, not one word per arbitrary node.

'''+md(['Workload (first doc)','Input bytes','Tape live','Decoded live','Warm tape capacity','Legacy 6 Vec live'],[[r['workload'],r['input_bytes'],r['tape_bytes'],r['decoded_bytes'],r['warm_tape_capacity_bytes'],r['legacy_six_vec_live_bytes']]for r in memory])+'''
Full decoded/tape capacities and legacy capacity totals: [memory.md](benchmark/tables/memory.md). Retained input ownership is in addition to these totals. Bytes slices expose only a lower bound on pinned allocation size; Arc views report the whole backing length.

Pointer/range assertions preserve **Bytes, Arc<[u8]>, and generic Arc<T>** through checked subviews and owned native Parse. Each measured subview operation allocates 0 times / 0 bytes. That Bytes fixture uses static backing. A separate heap-Bytes probe finds the first clone allocates **one 24-byte shared-owner wrapper** while preserving its byte pointer; subsequent clones allocate zero. Native corpus owners are promoted during untimed preflight/warmup, so this once-per-owner cost is excluded from warmed per-document rows and is counted here explicitly. The eight-byte JSON view retains a 12-byte Arc backing (exact); Bytes reports the visible 8-byte lower bound. Stage 4 additionally verifies Vec adoption, nested ranges, lifetime after caller owner drop, actual Arc retain/release, borrowed lifetimes and invalid ranges. Derived borrowed typed decoders now read slots tied to the original slice. A custom decoder can still choose owned copies; owned String fields still allocate their contents.

Warm reuse is capacity-bounded: a larger ASCII string can need no new tape/decoded storage. The updated scan-invariants JSON records exact cold/growth/equal/smaller allocation counts for wide-object and escape-dense inputs; equal/smaller reuse remains checked separately. These are untimed source-copy probes. They exclude corpus generation and retain the expanded scratch capacities. Vec adoption and generic Arc<T> owner adaptation independently measure zero allocations/bytes and preserve the input pointer. Untimed growth probes and scan counts are archived separately from timings. “Zero buffers” excludes intermediate whole-input/token/index buffers on the native path; final tape, selected slots, duplicate/nesting state, escaped decoded output and final encoded bytes remain real storage.

## Reactive handoff

'''
r=si[('rusty-alloc','NagoyaAuth','Reactive-Handoff','Single-Lane-Poll')];c=si[('system-alloc','NagoyaAuth','Reactive-Handoff','Single-Lane-Poll')]
report+=md(['Operation','C: System MB/s; p50/p95 ns/doc','D: RustyAlloc MB/s; p50/p95 ns/doc','D allocs / bytes per op'],[['NagoyaAuth parse+strict capture+admission+rings',cell(c),cell(r),f"{r['allocs_op']:.0f} / {r['bytes_op']:.0f}"]])
report+='''
The measured lane uses real HostedPool/HostedWorker futures and nagoya locks/notifications, cooperatively polled in-process with one request in flight. It includes schema validation, capture, admission, completion ownership, pointer and lease assertions. All **3,072 requests per allocator** (warmup, measurement and allocation pass) submit and complete in order; final jobs, retained-byte and scratch-byte counters are zero after result drop. No payload byte copy occurs. This is synchronous lane handoff throughput with pass-average latency; it is not socket throughput, runtime scheduling under contention, or concurrent service request tails. Stage 4 separately executes rejection, cancellation, completion backpressure, shutdown and exactly-once release tests. No external live service was launched.

## Remaining bottlenecks and limitations

- Historical speedups include API/layout/toolchain/instrumentation changes and different host contention. Only C/D are matched source/build allocator ablations. Fixed allocator order can interact with drift; per-process medians/spread and all chronological samples are retained. Close rankings and p95 differences are not statistically isolated.
- Strict-discard scratch is now retained across calls; the updated sparse-subtree rows record the observed warmed counts. Query paths and cold result storage can allocate; warm canonical zeros do not generalize to new shapes, larger inputs or user decoders. Materialized ps-pikkr bitmap/depth/training retention and process RSS are not exposed by this harness; N/A does not mean zero.
- `CompiledQuery` retains its SelectorTrie. The source-copy counter checks zero trie builds during repeated execution, including `execute_borrowed`; cold traversal/result storage can still allocate. The added Warm-Borrowed path-query row recycles results and retains traversal storage.
- DOM duplicate tables now reuse generation-tagged storage per active depth; raw/decoded key identity and parent duplicate rejection remain tested. Arbitrary caller-created NodeId resolution checks record membership by walking earlier records; genuine-handle iteration/navigation remains O(1) per step. A large tape and duplicate table still cost memory/CPU even with borrowed raw strings.
- Strict processing must inspect unselected syntax, escapes, numeric/duplicate policies and applicable schema constraints. Query-only results intentionally do less and never gain a validation proof. Long skip cancellation is checked before/after synchronous VM turns rather than via an instruction-level external callback.
- Borrowed typed decoding now requires the additive `JsonDecodeBorrowed` trait, generated by derives. Handwritten owned-only implementations must add it to use borrowed typed entry points. Borrowed DOM and borrowed strict slot extraction preserve input. Arbitrarily fragmented incremental input, WebSocket unmasking/decompression, assembly, TLS/kernel copying and partial transport writes are outside the complete-input zero-copy measurements.
- Native Apple Silicon and scalar portable paths execute; real x86 AVX2/PCLMUL/AVX-512 performance and correctness remain unexecuted. The earlier Stage 5 [generated-code inspection](benchmark/pre-stage6/generated-code.md) confirmed four input vector loads in the release NEON raw classifier and an emitted PMULL instruction; callee-save register spills and returned mask stores remain. It does not prove whole-parser register residency. Physical no-PMULL ARM hardware was unavailable.
- These runs do not establish an attribution profile for every remaining CPU hotspot or a universal 5×–15× improvement. Warm CPU work and serialization/escape/duplicate/schema costs remain relevant even when allocator calls are zero. The separately instrumented scan probe records classification work and skip spans without contaminating timing. The updated EscapedUnicode counts and their Stage 5 comparison are reported in the post-Stage-6 table. Instrumentation now counts actual fixed-block EscapeMaskCursor classifications, including bounded scalar tails; it no longer targets the removed stateless helper. This measures classification work, not a percentage of total CPU time.

## Reproduction and preservation

[run-stage5.py](benchmark/run-stage5.py) builds and preflights sequentially; [run-timings.py](benchmark/run-timings.py) runs saved binaries sequentially; [write-report.py](benchmark/write-report.py) derives these tables from raw samples. The original temporary external project was absent at Stage 5 entry and was restored byte-for-byte from the verified snapshot before the documented counter/kernel additions. The [final harness snapshot](benchmark/harness-snapshot/) contains the exact external Cargo.toml, Cargo.lock and src used. [jmax-bench-snapshot](benchmark/jmax-bench-snapshot/), workspace manifest/lockfile copies and [source-snapshot.tar.gz](benchmark/source-snapshot.tar.gz) preserve native and local dependency sources. Local sibling dependency declarations remain path-based; restore the archived sibling layout or adjust those paths to reproduce elsewhere. The saved binaries and hashes are host-specific.

Raw requested logs: [jmax RustyAlloc](benchmark/jmax-bench-rusty-alloc.txt), [jmax System](benchmark/jmax-bench-system-alloc.txt), [competitors RustyAlloc](benchmark/jmax-vs-parsers-rusty-alloc.txt), [competitors System](benchmark/jmax-vs-parsers-system-alloc.txt). Original JSONL pass/batch samples, chronological external samples, capability rows, conformance matrices, environment/load observations and command logs remain under benchmark/. All machine-readable and Markdown tables are under [tables/](benchmark/tables/).

'''
report+=f"Final audit: **{audit['source_files']} timed source/manifest files unchanged during measurement**, **{audit['historical_files']} protected historical files preserved**, exact external snapshot/lockfile match. HEAD remains `c3b97ce31eb93725c8494f258be7c6803752e558`; the measured workspace contains uncommitted Wave 6 changes, so the final source inventory digest, not HEAD alone, identifies the measured source: `{(O/'source-manifest.sha256').read_text().strip()}`. Protected older reports are unchanged; Stage 5 outputs were archived before their top-level replacements. No unrelated process was stopped.\n"
if invariants:
 report+='\nUntimed scan and growth evidence: [scan-invariants.md](benchmark/tables/scan-invariants.md), [JSON](benchmark/tables/scan-invariants.json), [instrumentation patch](benchmark/invariant-instrumentation.patch), [probe source](benchmark/invariant-probe/src/main.rs). Skip-byte counters sum successful skip-call lengths; they are not a deduplicated coverage proof. Only the source copy contains these counters.\n'
(R/'reports/wave6/FINAL.md').write_text(report)
status=f'''# Wave 6 execution status

Updated {datetime.datetime.now().astimezone().isoformat()}.

- Stage 0 — C0 contracts/ABI/integration: COMPLETE; [CONTRACTS.md](CONTRACTS.md).
- Stages 1–3 — S1..S8 and L1..L2 implementation/handoffs: COMPLETE with documented remaining limits; individual handoff reports retained.
- Stage 4 — Integration and mandatory native correctness/feature/allocator gates: PASS; [VERIFICATION.md](verification/VERIFICATION.md). Post-Stage-6 workspace 148 passed; targeted feature run 35 passed/1 ignored; historical ps-pikkr native 29 and portable 27 passed. Counts overlap. Real x86 execution UNEXECUTED. Broad pre-existing workspace formatting differences remain recorded.
- Stage 5 — Sequential release builds, both allocator suites, both workload generators, stress/regime/allocation/ownership/lease measurements, three-process external competitor runs, snapshots and final report: COMPLETE; [FINAL.md](FINAL.md), [benchmark artifacts](benchmark/). All timing processes exit 0; no task-owned Cargo/rustc build overlaps timing.

Measured: all 28 canonical warm owned operations allocate zero after warming. Sparse strict schema and query allocations were remeasured after retained skip/trie changes. Derived borrowed typed decoding now avoids the facade input copy; synchronous skip cancellation, Bytes backing lower bounds, arbitrary NodeId membership cost, x86 runtime, and host contention are explicit limits. Stage 5 completion does not claim these remaining architectural/measurement limits are resolved. No Wave 5/RustyAlloc baseline was measured.

Historical reports/investigation/wave5 files are preserved; the external final snapshot and lockfile match the measured harness; timed source hashes remain unchanged. No commit, deployment or unrelated process termination was performed.
'''
(R/'reports/wave6/STATUS.md').write_text(status)
print(json.dumps({'native_cli_rows':len(native),'matched_before_after':len(comparison),'supplement_rows':len(supplement),'competitor_rows':len(external),'geomean_C_A':gm(mb_ratios),'geomean_D_C':gm(ablation),'load':loadstats,'audit':audit},indent=2))
