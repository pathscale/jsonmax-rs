import json,hashlib
from pathlib import Path
O=Path(__file__).resolve().parent
rows=[]
for alloc in ['rusty-alloc','system-alloc']:
 for n in range(1,4):
  native=[json.loads(s) for s in (O/f'jmax-bench-{alloc}-r{n}-samples.jsonl').read_text().splitlines()]
  assert len(native)==56*20
  ext=[json.loads(s) for s in (O/f'jmax-vs-parsers-{alloc}-r{n}.jsonl').read_text().splitlines()]
  timed=[r for r in ext if 'median_ns' in r]
  assert len(timed)==259 and all(len(r['samples_ns'])==61 for r in timed)
  assert all(len(r['chronological_samples_ns'])==61 for r in timed)
  assert any(r.get('allocator')==('RustyAlloc' if alloc=='rusty-alloc' else 'System') for r in ext)
  rows.append({'allocator':alloc,'run':n,'canonical_pass_samples':len(native),'external_rows':len(timed),'external_batch_samples':sum(len(r['samples_ns']) for r in timed)})
 sup=[json.loads(s) for s in (O/f'jmax-supplement-{alloc}.jsonl').read_text().splitlines()]
 timed=[r for r in sup if 'mode' in r]
 assert len(timed)==348 and all(len(r['samples_pass_ns'])==20 for r in timed)
 assert all(r['copied_input_bytes']==0 for r in timed)
 leases=[r for r in sup if r.get('category')=='leases'];assert leases[0]['accepted_and_completed']==3072 and leases[0]['final_jobs']==0
# No task-owned concurrent jobs: runner uses blocking child wait; timestamps ledger separates stages.
commands=[json.loads(s) for s in (O/'commands.jsonl').read_text().splitlines()]
assert all(r['exit']==0 or r.get('superseded',False) for r in commands)
from datetime import datetime
for previous,current in zip(commands,commands[1:]):
 assert datetime.fromisoformat(previous['end'])<=datetime.fromisoformat(current['start']), (previous,current)
# All four release variants precede every timed run; source-copy builds also finish first.
first_timing=next(i for i,r in enumerate(commands) if r['label'].startswith('jmax-bench-'))
assert {r['label'] for r in commands[:first_timing] if r['label'].startswith(('build-jmax-','build-external-'))}=={'build-jmax-rusty-alloc','build-jmax-system-alloc','build-external-rusty-alloc','build-external-system-alloc'}
assert not any(r['cmd'][0]=='cargo' for r in commands[first_timing:])

manifest=json.loads((O/'binary-hashes.json').read_text())
assert all(hashlib.sha256((O/'bin'/p).read_bytes()).hexdigest()==h for p,h in manifest.items())
(O/'artifact-validation.json').write_text(json.dumps({'pass':True,'runs':rows,'supplement_operation_rows':696,'supplement_pass_samples':13920,'canonical_pass_samples':6720,'external_batch_samples':94794,'successful_commands':sum(r['exit']==0 for r in commands),'failed_superseded_commands':sum(r['exit']!=0 for r in commands)},indent=2))
print((O/'artifact-validation.json').read_text())
