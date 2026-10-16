import os,subprocess,json,hashlib,datetime
from pathlib import Path
R=Path('/Users/revenge/code/jsonmax-rs');O=R/'reports/wave6/benchmark'
def cmd(a):
 p=subprocess.run(a,capture_output=True,text=True);return {'command':a,'exit':p.returncode,'stdout':p.stdout,'stderr':p.stderr}
env={'at':datetime.datetime.now().astimezone().isoformat(),'loadavg':os.getloadavg(),'commands':[]}
compiler='/Users/revenge/.rustup/toolchains/1.98.1-aarch64-apple-darwin/bin/'
for a in [[compiler+'rustc','-vV'],[compiler+'cargo','--version'],['uname','-a'],['sw_vers'],['sysctl','-n','machdep.cpu.brand_string','hw.memsize','hw.ncpu'],['pmset','-g','therm'],['git','rev-parse','HEAD','HEAD^{tree}'],[compiler+'rustc','-C','target-cpu=native','--print','cfg']]:env['commands'].append(cmd(a))
(O/'environment.json').write_text(json.dumps(env,indent=2))
paths=list(R.glob('reports/BENCHMARK-*'))+[R/'reports/investigation-2026-10-16',R/'reports/wave5']
files=[]
for p in paths:files.extend([p] if p.is_file() else [q for q in p.rglob('*') if q.is_file()])
(O/'historical-preserved-start.json').write_text(json.dumps({str(p.relative_to(R)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(files)},indent=2))
