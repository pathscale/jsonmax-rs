from pathlib import Path
import subprocess,os,time,json,datetime
archive=Path('/Users/revenge/code/jsonmax-rs/reports/benchmark-7-workloads-cold-vs-warm')
env=dict(os.environ,RUSTFLAGS='-C target-cpu=native',JMAX_RAW=str(archive/'head-to-head-samples.csv'),JMAX_CORPUS='/tmp/jmax-followup-corpus')
commands=[(['cargo','run','--release','-p','jmax-bench','--bin','jmax_bench'],'native-cli.txt','native-cli-build.log'),(['/tmp/jmax-followup-baseline/target/release/jmax-followup-baseline'],'head-to-head.md','head-to-head-run.log')]
with open(archive/'system-load.jsonl','w') as load:
 for command,output,errors in commands:
  with open(archive/output,'w') as out,open(archive/errors,'w') as err:
   p=subprocess.Popen(command,env=env,stdout=out,stderr=err)
   while p.poll() is None:
    ps=subprocess.run(['ps','-Ao','pid=,pcpu=,comm='],capture_output=True,text=True).stdout
    entries=[]
    for line in ps.splitlines():
     fields=line.strip().split(None,2)
     if len(fields)==3 and float(fields[1])>=5: entries.append({'pid':int(fields[0]),'cpu':float(fields[1]),'command':fields[2]})
    load.write(json.dumps({'utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'benchmark_pid':p.pid,'command':command,'processes_at_least_5_cpu':entries})+'\n');load.flush()
    time.sleep(2)
   print(output,'exit',p.returncode,flush=True)
   if p.returncode:raise SystemExit(p.returncode)
