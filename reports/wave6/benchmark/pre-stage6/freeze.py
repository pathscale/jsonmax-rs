import json,hashlib,shutil,tarfile,os
from pathlib import Path
R=Path('/Users/revenge/code/jsonmax-rs'); O=R/'reports/wave6/benchmark'
(O/'jmax-bench-snapshot').mkdir(exist_ok=True)
shutil.copytree(R/'crates/jmax-bench',O/'jmax-bench-snapshot',dirs_exist_ok=True)
shutil.copy2(R/'Cargo.lock',O/'workspace.Cargo.lock');shutil.copy2(R/'Cargo.toml',O/'workspace.Cargo.toml')
shutil.copytree('/tmp/jmax-bench-data',O/'corpora',dirs_exist_ok=True)
files=[R/'Cargo.toml',R/'Cargo.lock']+sorted((R/'crates').rglob('*.rs'))+sorted((R/'crates').rglob('Cargo.toml'))
manifest={str(p.relative_to(R)):hashlib.sha256(p.read_bytes()).hexdigest() for p in files}
(O/'source-final.sha256.json').write_text(json.dumps(manifest,indent=2))
(O/'source-manifest.sha256').write_text(hashlib.sha256((O/'source-final.sha256.json').read_bytes()).hexdigest()+'\n')
(O/'binary-hashes.json').write_text(json.dumps({p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in (O/'bin').iterdir()},indent=2))
(O/'corpus-hashes.json').write_text(json.dumps({p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in (O/'corpora').glob('*.json')},indent=2))
# Include all local crate sources/manifests/tests without targets or repository history.
with tarfile.open(O/'source-snapshot.tar.gz','w:gz') as tar:
 for repo in ['jsonmax-rs','ps-pikkr','ps-rusty_alloc','ps-simdutf8','nagoya','nago-http','nago-wss']:
  base=R.parent/repo
  local_files=[]
  if repo=='jsonmax-rs': local_files=files
  else:
   for directory,dirs,names in os.walk(base):
    dirs[:]=[d for d in dirs if d not in ['target','.git','node_modules','verification','provenance','.tooling']]
    local_files.extend(Path(directory)/name for name in names)
  for p in sorted(local_files):
   if not p.is_file():continue
   rel=p.relative_to(base)
   if any(c in ['target','.git','node_modules','verification','provenance','.tooling'] for c in rel.parts):continue
   if repo!='jsonmax-rs' and not (p.suffix in ['.rs','.toml','.lock'] or p.name.startswith('LICENSE')):continue
   tar.add(p,arcname=repo+'/'+str(rel),recursive=False)
