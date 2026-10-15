from pathlib import Path
import subprocess,re,html,hashlib,json
out=Path('/Users/revenge/code/jsonmax-rs/reports/benchmark-7-workloads-cold-vs-warm/profiles');out.mkdir(parents=True,exist_ok=True)
summary={}
for path in Path('/tmp/jmax-followup-profiles').glob('*.sample.txt'):
 text=subprocess.run(['/tmp/jmax-profile-tools/target/release/jmax-profile-tools'],input=path.read_text(),capture_output=True,text=True,check=True).stdout
 (out/path.name).write_text(text)
 body=text.split('Call graph:\n',1)[1].split('Total number in stack',1)[0]
 roots=[];stack=[]
 for line in body.splitlines():
  m=re.match(r'^([ +!:|]*)(\d+) (.+)$',line)
  if not m: continue
  pos=len(m[1]);name=m[3].split('  (in ',1)[0]
  if name.startswith('Thread_') or name.startswith('DispatchQueue'): name='Main thread' if not roots else name
  node={'name':name,'n':int(m[2]),'children':[],'pos':pos}
  while stack and stack[-1]['pos']>=pos: stack.pop()
  (stack[-1]['children'] if stack else roots).append(node)
  stack.append(node)
 folded=[];leaves={};inclusive={}
 def walk(node,parents):
  own=node['n']-sum(c['n'] for c in node['children'])
  if own<0: raise ValueError((path.name,node['name'],own))
  names=parents+[node['name']]
  if own:
   folded.append(';'.join(names)+' '+str(own))
   leaves[node['name']]=leaves.get(node['name'],0)+own
  inclusive[node['name']]=inclusive.get(node['name'],0)+node['n']
  for child in node['children']:walk(child,names)
 for root in roots:walk(root,[])
 key=path.name.replace('.sample.txt','')
 (out/(key+'.folded')).write_text('\n'.join(folded)+'\n')
 total=sum(n['n'] for n in roots)
 maxdepth=max(line.count(';') for line in folded)
 height=80+(maxdepth+1)*19
 svg=[f'<svg xmlns="http://www.w3.org/2000/svg" width="1400" height="{height}" viewBox="0 0 1400 {height}"><style>text{{font-family:monospace;font-size:11px}} rect:hover{{stroke:black;stroke-width:1}}</style><rect width="1400" height="{height}" fill="#fff"/><text x="10" y="22" font-size="16">{html.escape(key)}: {total} sampled stacks (macOS sample, 1 ms, 3 s)</text><text x="10" y="42">Width = sample count; hover for full symbol. Includes profiler startup/preparation. Bottom = callers.</text>']
 def draw(node,x,depth):
  width=node['n']/total*1380;y=height-25-depth*19
  color=int(hashlib.sha256(node['name'].encode()).hexdigest()[:6],16)
  fill=f'rgb({210+color%45},{100+(color//45)%100},{45+(color//4500)%70})'
  title=html.escape(f"{node['name']} — {node['n']} samples ({100*node['n']/total:.2f}%)")
  svg.append(f'<g><title>{title}</title><rect x="{x:.3f}" y="{y}" width="{width:.3f}" height="18" fill="{fill}" stroke="white" stroke-width="0.4"/>')
  label=node['name'];avail=max(0,int(width/6.6)-1)
  if avail>3:svg.append(f'<text x="{x+3:.3f}" y="{y+13}">{html.escape(label[:avail])}</text>')
  svg.append('</g>');cx=x
  for child in node['children']:
   draw(child,cx,depth+1);cx+=child['n']/total*1380
 x=10
 for root in roots:draw(root,x,0);x+=root['n']/total*1380
 svg.append('</svg>');(out/(key+'.svg')).write_text('\n'.join(svg))
 summary[key]={'samples':total,'top_leaf':sorted(leaves.items(),key=lambda x:-x[1])[:15],
  'malloc_leaf_samples':sum(v for k,v in leaves.items() if any(x in k.lower() for x in ['malloc','realloc','free','rust_alloc','rust_dealloc']))}
Path(out.parent/'profile-summary.json').write_text(json.dumps(summary,indent=2))
print(json.dumps(summary,indent=2))
