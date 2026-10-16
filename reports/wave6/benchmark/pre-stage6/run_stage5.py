#!/usr/bin/env python3
"""Only one child build/benchmark at a time. Read-only load monitoring uses ps/pmset."""
import os,subprocess,time,json,datetime,hashlib,shutil
from pathlib import Path
ROOT=Path('/Users/revenge/code/jsonmax-rs'); OUT=ROOT/'reports/wave6/benchmark'
ENV=os.environ.copy(); ENV['PATH']='/Users/revenge/.cargo/bin:'+ENV['PATH']; ENV['PATH']='/Users/revenge/code/ps-pikkr/.tooling/bin:'+ENV['PATH']
ENV.update(RUSTC='/Users/revenge/.rustup/toolchains/1.98.1-aarch64-apple-darwin/bin/rustc',RUSTDOC='/Users/revenge/.rustup/toolchains/1.98.1-aarch64-apple-darwin/bin/rustdoc',RUSTFLAGS='-C target-cpu=native')
(OUT/'bin').mkdir(exist_ok=True)
def now():return datetime.datetime.now().astimezone().isoformat()
def snapshot(stage):
    row={'at':now(),'stage':stage,'loadavg':os.getloadavg()}
    for key,cmd in [('thermal',['pmset','-g','therm']),('processes',['ps','-axo','pid,ppid,pcpu,comm'])]:
        row[key]=subprocess.run(cmd,capture_output=True,text=True).stdout
    with (OUT/'system-load.jsonl').open('a') as f:f.write(json.dumps(row)+'\n')
def run(label,cmd,extra=None):
    print(now(),label,flush=True);snapshot(label+':start');start=time.monotonic()
    env=ENV.copy();env.update(extra or {})
    with (OUT/(label+'.log')).open('w') as log:
        log.write('cwd: '+str(ROOT)+'\ncommand: '+repr(cmd)+'\n');log.flush()
        p=subprocess.Popen(cmd,cwd=ROOT,env=env,stdout=log,stderr=subprocess.STDOUT)
        while p.poll() is None:
            time.sleep(1)
            if time.monotonic()-start>15:
                snapshot(label+':running');start=time.monotonic()
        rc=p.returncode;log.write('\nexit: '+str(rc)+'\n')
    with (OUT/'commands.jsonl').open('a') as f:f.write(json.dumps({'at':now(),'label':label,'cmd':cmd,'exit':rc})+'\n')
    snapshot(label+':end')
    if rc: print((OUT/(label+'.log')).read_text()[-6000:]);raise SystemExit(rc)
def build_and_preflight():
    # Build both defaults before any timing. Copy exact artifacts before feature rebuilds.
    for alloc in ['rusty-alloc','system-alloc']:
        feature=[] if alloc=='rusty-alloc' else ['--features','system-alloc']
        run('build-jmax-'+alloc,['cargo','+1.98.1','build','--release','-p','jmax-bench','--bin','jmax_bench','--offline']+feature)
        shutil.copy2(ROOT/'target/release/jmax_bench',OUT/'bin'/('jmax_bench-'+alloc))
        run('build-external-'+alloc,['cargo','+1.98.1','build','--release','--manifest-path','/tmp/jmax-vs-parsers/Cargo.toml','--offline']+feature)
        shutil.copy2('/tmp/jmax-vs-parsers/target/release/jmax-vs-parsers',OUT/'bin'/('jmax-vs-parsers-'+alloc))
    for alloc in ['rusty-alloc','system-alloc']:
        run('preflight-jmax-'+alloc,[str(OUT/'bin'/('jmax_bench-'+alloc)),'--wave6',str(OUT/('preflight-jmax-'+alloc+'.jsonl'))],{'WAVE6_CHECK_ONLY':'1'})
        run('preflight-external-'+alloc,[str(OUT/'bin'/('jmax-vs-parsers-'+alloc)),str(OUT/('preflight-external-'+alloc+'.jsonl'))],{'JMAX_CHECK_ONLY':'1'})
    print('BUILDS AND PREFLIGHTS COMPLETE. Timing is a separate invocation.',flush=True)

if __name__=="__main__": build_and_preflight()
