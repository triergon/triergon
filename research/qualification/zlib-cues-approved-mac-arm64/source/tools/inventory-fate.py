#!/usr/bin/env python3
"""Index FATE identities without executing upstream make/shell code or downloading samples."""
import argparse,hashlib,json,pathlib,re
p=argparse.ArgumentParser();p.add_argument('--source',type=pathlib.Path,required=True);p.add_argument('--output',type=pathlib.Path,required=True);a=p.parse_args()
cases={}
for path in sorted((a.source/'tests/fate').glob('*.mak')):
 text=path.read_text();logical=text.replace('\\\n',' ')
 for n,line in enumerate(text.splitlines(),1):
  if line.lstrip().startswith('#'):continue
  for name in re.findall(r'\bfate-[a-zA-Z0-9][a-zA-Z0-9_.-]*',line):
   row=cases.setdefault(name,dict(id=name,definition_files=[],command_sha256=[],sample_references=[],status='unmapped',rights='unreviewed'))
   location=f'{path.relative_to(a.source)}:{n}'
   if location not in row['definition_files']:row['definition_files'].append(location)
 for line in logical.splitlines():
  m=re.match(r'^(fate-[\w.-]+)\s*:\s*CMD\s*=\s*(.*)',line)
  if not m or m[1] not in cases:continue
  row=cases[m[1]];row['command_sha256'].append(hashlib.sha256(m[2].encode()).hexdigest());row['sample_references'].extend(re.findall(r'\$\(TARGET_SAMPLES\)/([^\s"\']+)',m[2]))
a.output.parent.mkdir(parents=True,exist_ok=True);a.output.write_text(json.dumps({'schema_version':1,'status':'identity inventory only; not the configure-expanded runnable FATE suite','source_root_role':'externally cached FFmpeg snapshot','cases':list(sorted(cases.values(),key=lambda c:c['id']))},indent=2)+'\n')
print(f'Indexed {len(cases)} identities; none executed or accepted.')
