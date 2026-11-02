#!/usr/bin/env python3
"""Validate design/evidence integrity, not media correctness or benchmark acceptance."""
import json,pathlib,re
root=pathlib.Path(__file__).resolve().parents[1]
errors=[]
for p in root.rglob('*.md'):
 if '.research' in p.parts or 'target' in p.parts:continue
 for target in re.findall(r'\]\(([^)]+)\)',p.read_text()):
  if '://' in target or target.startswith('#'):continue
  f=(p.parent/target.split('#',1)[0]).resolve()
  if not f.exists():errors.append(f'{p.relative_to(root)}: missing link {target}')
for p in (root/'research').rglob('*.json'):
 if 'target' in p.parts:continue
 try:json.loads(p.read_text())
 except (ValueError,UnicodeError) as e:errors.append(f'{p.relative_to(root)}: {e}')
g=json.loads((root/'research/module-gates.json').read_text())
if g['owner_requirements']['runtime_ratio_max']!=1.25:errors.append('delivery runtime gate differs from owner authorization')
if g['owner_requirements'].get('runtime_optimization_ratio_target')!=0.8:errors.append('runtime optimization target differs from owner authorization')
if any(m['accepted'] for m in g['modules']):errors.append('a module was marked accepted without implementation evidence')
fate=json.loads((root/'research/fate-index.json').read_text())
if any(c['status']!='unmapped' or c['rights']!='unreviewed' for c in fate['cases']):errors.append('FATE index misrepresents coverage/rights')
rows=json.loads((root/'research/results/summary.json').read_text())
for row in rows:
 stem=row['fixture']+'.'+row['candidate']+'.json'
 if not (root/'research/results'/stem).exists():errors.append('missing raw result '+stem)
 if not (root/'research/results'/(row['fixture']+'.ffprobe.json')).exists():errors.append('missing reference result')
if errors:
 print('\n'.join(errors));raise SystemExit(1)
print(f'Design integrity OK: local links, JSON, {len(g["modules"])} unaccepted module gates, {len(fate["cases"])} unmapped FATE identities, {len(rows)} candidate observations.')
print('This is not a feature-parity, performance, memory or FATE pass.')
