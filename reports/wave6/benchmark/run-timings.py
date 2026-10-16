#!/usr/bin/env python3
from run_stage5 import run,OUT,ROOT
import shutil,json
for alloc in ['rusty-alloc','system-alloc']:
    native=OUT/'bin'/('jmax_bench-'+alloc)
    for number in range(1,4):
        label=f'jmax-bench-{alloc}-r{number}'
        run(label,[str(native)],{'JMAX_SAMPLES_PATH':str(OUT/(label+'-samples.jsonl'))})
    run('jmax-supplement-'+alloc,[str(native),'--wave6',str(OUT/('jmax-supplement-'+alloc+'.jsonl'))])
    with (OUT/('jmax-bench-'+alloc+'.txt')).open('w') as result:
        for number in range(1,4):
            result.write(f'PROCESS {number}\n');result.write((OUT/f'jmax-bench-{alloc}-r{number}.log').read_text());result.write('\n')
        result.write('SUPPLEMENT (operation-only timing):\n');result.write((OUT/('jmax-supplement-'+alloc+'.log')).read_text())
for alloc in ['rusty-alloc','system-alloc']:
    for number in range(1,4):
        label=f'jmax-vs-parsers-{alloc}-r{number}'
        run(label,[str(OUT/'bin'/('jmax-vs-parsers-'+alloc)),str(OUT/(label+'.jsonl'))])
        for path in __import__('pathlib').Path('/tmp/jmax-vs-parsers').glob('*validation*json'):
            shutil.copy2(path,OUT/(label+'-'+path.name))
    with (OUT/('jmax-vs-parsers-'+alloc+'.txt')).open('w') as result:
        for number in range(1,4):
            result.write(f'PROCESS {number}\n');result.write((OUT/f'jmax-vs-parsers-{alloc}-r{number}.log').read_text());result.write('\n')
print('ALL SEQUENTIAL TIMINGS COMPLETE',flush=True)
