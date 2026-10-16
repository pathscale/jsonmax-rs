"""Compare remeasured post-Stage-6 outputs with the preserved Stage 5 run."""
from pathlib import Path
import json,csv,math,statistics,hashlib,os
R=Path('/Users/revenge/code/jsonmax-rs'); O=R/'reports/wave6/benchmark'; T=O/'tables'; B=O/'pre-stage6/tables'
def load(p):return json.loads(p.read_text())
def md(h,rows):return '\n'.join(['| '+' | '.join(h)+' |','| '+' | '.join(['---']*len(h))+' |']+['| '+' | '.join(map(str,r))+' |' for r in rows])+'\n'
def save(n,rows):
 (T/(n+'.json')).write_text(json.dumps(rows,indent=2)+'\n')
 if rows:
  with (T/(n+'.csv')).open('w') as f:
   w=csv.DictWriter(f,fieldnames=list(rows[0]));w.writeheader();w.writerows(rows)
def gm(x):return math.exp(statistics.mean(math.log(v) for v in x))
comparisons=[]
for suite,keys in [('native-regimes',['allocator','workload','mode','regime']),('competitors',['allocator','corpus','category','parser'])]:
 old={tuple(r[k] for k in keys):r for r in load(B/(suite+'.json'))}
 for row in load(T/(suite+'.json')):
  before=old.get(tuple(row[k] for k in keys))
  if before:
   comparisons.append(dict(suite=suite,allocator=row['allocator'],workload=row.get('workload',row.get('corpus')),operation=row.get('mode',row.get('parser')),regime=row.get('regime',row.get('category')),before_mb_s=before['mb_s'],after_mb_s=row['mb_s'],observed_throughput_ratio=row['mb_s']/before['mb_s'],before_allocs_op=before['allocs_op'],after_allocs_op=row['allocs_op'],before_bytes_op=before['bytes_op'],after_bytes_op=row['bytes_op']))
save('post-stage6-comparison',comparisons)
selected=[r for r in comparisons if r['allocator']=='rusty-alloc' and ((r['suite']=='competitors' and r['operation'] in ['jmax-dom-borrowed','jmax-native-tape-encode-sink','jmax-query-only-extract-borrowed','jmax-vm-owned-extract']) or (r['suite']=='native-regimes' and r['workload'] in ['EscapedUnicode','sparse-large-subtree','long-unescaped-ascii','long-unescaped-unicode','wide-object'] and r['operation'] in ['Parse','Validate-Schema','Extract-Strict','Extract-Query-Path','Serialize-Staged-Sink','Serialize-Native-Direct']))]
(T/'post-stage6-comparison.md').write_text('Stage 5 and post-Stage-6 were measured at different times and host loads. Ratios are observations of the combined changes, not isolated attribution to one optimization. RustyAlloc selected rows below; JSON/CSV include both allocators and every matched row.\n\n'+md(['Workload','Operation','Regime','Before MB/s','After MB/s','Ratio','Before allocs / bytes','After allocs / bytes'],[[r['workload'],r['operation'],r['regime'],f"{r['before_mb_s']:.2f}",f"{r['after_mb_s']:.2f}",f"{r['observed_throughput_ratio']:.2f}×",f"{r['before_allocs_op']:g} / {r['before_bytes_op']:g}",f"{r['after_allocs_op']:g} / {r['after_bytes_op']:g}"] for r in selected]))
new_scan=load(T/'scan-invariants.json'); old_scan=load(B/'scan-invariants.json')
a=next(r for r in old_scan if r.get('workload')=='EscapedUnicode' and r.get('mode')=='Parse'); b=next(r for r in new_scan if r.get('workload')=='EscapedUnicode' and r.get('mode')=='Parse')
assert b['input_bytes']==a['input_bytes']==2012 and b['cursor_classified_bytes']==2012
assert b['decode_helper_blocks']<a['decode_helper_blocks'] and b['decode_helper_classified_bytes']<a['decode_helper_classified_bytes']
query=[r for r in new_scan if r['category']=='query-allocation'];assert len(query)==4 and all(r['trie_builds']==0 for r in query)
assert all(r['allocs']==0 for r in query if r['warm'] and r['mode']=='Strict')
sparse=[r for r in load(T/'native-regimes.json') if r['workload']=='sparse-large-subtree' and r['mode'] in ['Validate-Schema','Extract-Strict'] and r['regime'].startswith('Warm')]
assert len(sparse)==8 and all(r['allocs_op']==0 and r['bytes_op']==0 for r in sparse),sparse
storage=[]
old_storage={r['workload']:r for r in load(B/'storage-ownership.json') if r.get('category')=='storage' and r['allocator']=='rusty-alloc'}
for row in load(T/'storage-ownership.json'):
 if row.get('category')=='storage' and row['allocator']=='rusty-alloc':
  before=old_storage[row['workload']]
  storage.append(dict(workload=row['workload'],before_duplicate_slots=before['duplicate_slots'],after_duplicate_slots=row['duplicate_slots'],before_tape_capacity_bytes=before['warm_tape_capacity_bytes'],after_tape_capacity_bytes=row['warm_tape_capacity_bytes']))
save('post-stage6-storage',storage)
(T/'post-stage6-storage.md').write_text('Slot counts are capacities, not bytes or RSS. Per-depth entries differ in layout from the previous global table. Tape reservation trades fewer cold growth calls for possible retained slack.\n\n'+md(['Workload','Before duplicate slots','After duplicate slots','Before tape capacity bytes','After tape capacity bytes'],[[r[k] for k in r] for r in storage]))
corpus=[]
for p in sorted((O/'generated-corpora').glob('*.jsonl')):
 entry=load(O/'generated-corpus-manifest.json')[p.stem]
 assert hashlib.sha256(p.read_bytes()).hexdigest()==entry['sha256_jsonl'],p
 corpus.append(p.name)
# Check all protected files, including any target artifacts, without writing there.
protected=load(R/'reports/wave6/verification/post-stage6-protected-start.json'); protected_results={}
for directory,expected in protected.items():
 actual={}
 for parent,dirs,names in os.walk(directory):
  dirs[:]=sorted(d for d in dirs if d!='.git')
  for name in sorted(names):
   p=Path(parent)/name
   if p.is_file():actual[str(p.relative_to(directory))]=hashlib.sha256(p.read_bytes()).hexdigest()
 assert actual==expected,directory
 protected_results[directory]=len(actual)
(R/'reports/wave6/verification/post-stage6-protected-final.json').write_text(json.dumps({'unchanged':True,'files_per_repository':protected_results},indent=2)+'\n')
cli_before={(r['allocator'],r['workload'],r['mode'],r['regime']):r for r in load(B/'native-cli.json')}
cli_ratios={alloc:gm([r['mb_s']/cli_before[(r['allocator'],r['workload'],r['mode'],r['regime'])]['mb_s'] for r in load(T/'native-cli.json') if r['allocator']==alloc]) for alloc in ['rusty-alloc','system-alloc']}
summary=dict(escaped_unicode_before=a,escaped_unicode_after=b,query_allocation=query,sparse_warm_rows=sparse,matched_comparison_rows=len(comparisons),generated_corpora_unchanged=len(corpus),protected_repositories=protected_results,canonical_cli_observed_stage6_over_stage5=cli_ratios)
(O/'post-stage6-summary.json').write_text(json.dumps(summary,indent=2)+'\n')
section='''## Post-Stage-6 bottleneck verification

P1/P2/P3 changes are integrated and all requested correctness gates pass. The pre-pass Stage 5 report, raw samples, binaries, tables and source snapshot are preserved in [pre-stage6/](benchmark/pre-stage6/). Current top-level benchmark files and tables describe the rebuilt post-Stage-6 run. [Every matched comparison](benchmark/tables/post-stage6-comparison.md) includes allocation/byte counts and observed throughput ratios; JSON/CSV include both allocators.

'''
section+=md(['EscapedUnicode first document','Stage 5','Post-Stage-6'],[['Input bytes',a['input_bytes'],b['input_bytes']],['Cursor classified bytes',a['cursor_classified_bytes'],b['cursor_classified_bytes']],['Escape helper blocks',a['decode_helper_blocks'],b['decode_helper_blocks']],['Escape helper classified bytes',a['decode_helper_classified_bytes'],b['decode_helper_classified_bytes']],['Helper bytes / input',f"{a['decode_helper_classified_bytes']/a['input_bytes']:.3f}×",f"{b['decode_helper_classified_bytes']/b['input_bytes']:.3f}×"]])
section+=f"\nHelper block classifications fell **{100*(1-b['decode_helper_blocks']/a['decode_helper_blocks']):.1f}%** and helper-classified bytes fell **{100*(1-b['decode_helper_classified_bytes']/a['decode_helper_classified_bytes']):.1f}%**. One additional bounded escape-helper traversal remains; the original cursor pass also remains. This removes the measured 4.05× redundant helper work rather than eliminating every additional pass. Counters are inserted only in the source copy at the current fixed-block discovery site, with scalar tails counted too.\n\n"
section+='Sparse-large-subtree strict schema **validation and extraction each fell from 4 allocations / 720 requested bytes to 0 / 0 per call**, for warm owned and borrowed execution under both RustyAlloc and System. Cold/new shapes can still grow retained skip storage.\n\n'
section+=md(['Query mode','Scratch/result reuse','Calls','Trie builds','Total allocs','Total requested bytes'],[[r['mode'],r['warm'],r['calls'],r['trie_builds'],r['allocs'],r['requested_bytes']] for r in query])
section+='\nThe plan counter records one build during compilation and zero during either execution API. Cold query execution still allocates traversal/results. On this nested fixture, warmed query-only skipping retains one 8-byte delimiter-stack allocation per call; strict execution with recycled scratch/results allocates zero. Overlapping parent/descendant selectors still use the event traversal and are outside this zero-allocation probe. An initial probe asserted all warmed query modes were allocation-free; it was corrected to record the observed query-only count. The failed assertion and superseding successful run are retained in the command ledger and [initial probe log](benchmark/invariants-initial.log).\n\n'
section+='DOM storage now reuses per-depth duplicate tables; [measured slot/tape capacities](benchmark/tables/post-stage6-storage.md) show both reductions and cases with increased depth/tape slack. Native serialization now emits trusted unescaped quoted tokens directly. Numeric lexemes already used the trusted direct path before P2 and retain it; this pass does not establish a new number-only optimization. The following cold borrowed DOM and direct native serialization results measure the combined P1/P2/P3 changes, with setup outside timing. They do not isolate individual fast paths.\n\n'
section+=md(['Corpus','Operation','Stage 5 MB/s','Post-Stage-6 MB/s','Observed ratio','Allocs before → after','Bytes before → after'],[[r['workload'],r['operation'],f"{r['before_mb_s']:.2f}",f"{r['after_mb_s']:.2f}",f"{r['observed_throughput_ratio']:.2f}×",f"{r['before_allocs_op']:g} → {r['after_allocs_op']:g}",f"{r['before_bytes_op']:g} → {r['after_bytes_op']:g}"] for r in comparisons if r['suite']=='competitors' and r['allocator']=='rusty-alloc' and r['operation'] in ['jmax-dom-borrowed','jmax-native-tape-encode-sink']])
section+='\nThe targeted unescaped-string rows below preserve the corresponding escaped workload as well, so allocation reductions are not treated as a universal timing gain. Native number emission predates this pass.\n\n'
section+=md(['Workload','Serializer regime','Before MB/s','After MB/s','Observed ratio','Allocs before → after','Bytes before → after'],[[r['workload'],r['operation']+'/'+r['regime'],f"{r['before_mb_s']:.2f}",f"{r['after_mb_s']:.2f}",f"{r['observed_throughput_ratio']:.2f}×",f"{r['before_allocs_op']:g} → {r['after_allocs_op']:g}",f"{r['before_bytes_op']:g} → {r['after_bytes_op']:g}"] for r in comparisons if r['suite']=='native-regimes' and r['allocator']=='rusty-alloc' and r['workload'] in ['EscapedUnicode','long-unescaped-ascii','long-unescaped-unicode'] and r['operation'] in ['Serialize-Staged-Sink','Serialize-Native-Direct']])
section+=f"\nAcross 56 matched canonical CLI rows, observed post-Stage-6/Stage-5 geometric-mean throughput is **{cli_ratios['rusty-alloc']:.3f}× for RustyAlloc** and **{cli_ratios['system-alloc']:.3f}× for System**. Runs occur at different times and host loads; these are combined-change observations, not controlled attribution. All {len(corpus)} generated corpus hashes match the preserved run. Full hashes confirm all four protected sibling repositories unchanged.\n\n"
p=R/'reports/wave6/FINAL.md';s=p.read_text();s=s.replace('## Implemented architecture and integration',section+'## Implemented architecture and integration');p.write_text(s)
p=R/'reports/wave6/STATUS.md';s=p.read_text()+'''\n- Stage 6 — P1/P2/P3 bottleneck pass: COMPLETE; post-pass verification and sequential remeasurement COMPLETE. Workspace 148 passed; native feature tests 35 passed/1 ignored. EscapedUnicode helper blocks 129→32; helper bytes 8,154→1,989. Warm sparse schema validation/extraction 4 allocations/720 bytes→0/0 in both allocators and both ownership modes. Trie builds per execution: zero. Warm query-only nested discard still allocates one 8-byte delimiter stack; warm strict query probe allocates zero. See FINAL.md and post-stage6-summary.json.\n''';p.write_text(s)
print(json.dumps({'comparisons':len(comparisons),'scan_helper_blocks':[a['decode_helper_blocks'],b['decode_helper_blocks']],'scan_helper_bytes':[a['decode_helper_classified_bytes'],b['decode_helper_classified_bytes']],'sparse_warm_rows_zero':len(sparse),'cli_ratios':cli_ratios,'protected_unchanged':True},indent=2))
