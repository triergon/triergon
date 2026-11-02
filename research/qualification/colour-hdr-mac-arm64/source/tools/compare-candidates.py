#!/usr/bin/env python3
"""Bounded evaluation of upstream candidates against ffprobe; no product acceptance implied."""
import argparse,collections,fractions,json,pathlib,subprocess
p=argparse.ArgumentParser();p.add_argument('--fixtures',type=pathlib.Path,required=True);p.add_argument('--binary',type=pathlib.Path,required=True);p.add_argument('--output',type=pathlib.Path,required=True);a=p.parse_args();a.output.mkdir(parents=True,exist_ok=True)
summary=[]
for path in sorted(a.fixtures.iterdir()):
 if path.suffix not in ['.mkv','.mp4','.ts']:continue
 ref=json.loads(subprocess.check_output(['ffprobe','-v','error','-show_streams','-show_packets','-show_data_hash','sha256','-of','json',str(path)],timeout=15))
 (a.output/(path.name+'.ffprobe.json')).write_text(json.dumps(ref,indent=2)+'\n')
 reference=collections.defaultdict(list);bases={s['index']:fractions.Fraction(s['time_base']) for s in ref['streams']}
 for pkt in ref.get('packets',[]):
  ns=fractions.Fraction(pkt['pts'])*bases[pkt['stream_index']]*1000000000 if 'pts' in pkt else None
  reference[pkt['stream_index']].append((pkt['data_hash'].split(':',1)[1].lower(),ns))
 for candidate in ['symphonia','sequential','matroska']:
  if candidate=='matroska' and path.suffix!='.mkv':continue
  run=subprocess.run([str(a.binary.resolve()),candidate,str(path.resolve())],text=True,capture_output=True,timeout=15)
  data=json.loads(run.stdout);(a.output/(path.name+'.'+candidate+'.json')).write_text(json.dumps(data,indent=2)+'\n')
  row={'fixture':path.name,'candidate':candidate,'exit_code':run.returncode}
  if 'error' in data:row['error']=data['error'];summary.append(row);continue
  got=collections.defaultdict(list);ids={t['id']:i for i,t in enumerate(data['tracks'])}
  for pkt in data['packets']:
   ns=fractions.Fraction(pkt['pts_ns']) if pkt.get('pts_ns') is not None else None
   got[ids[pkt['track']]].append((pkt['sha256'],ns))
  rows=[]
  for i in sorted(set(reference)|set(got)):
   expected=reference[i];actual=got[i]
   rows.append({'track_ordinal':i,'ffprobe_packets':len(expected),'candidate_packets':len(actual),'payload_sequence_equal':[x[0] for x in expected]==[x[0] for x in actual],'pts_equal':len(expected)==len(actual) and all((x[1] is None and y[1] is None) or (x[1] is not None and y[1] is not None and abs(x[1]-y[1])<=1) for x,y in zip(expected,actual))})
  row.update(tracks=rows,seeks_after_eof=data.get('seeks_after_eof',data.get('seeks')),fresh_seeks=data.get('fresh_seeks'),termination=data.get('termination')); summary.append(row)
(a.output/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
for x in summary:
 print(x['fixture'],x['candidate'],x.get('error') or [(r['track_ordinal'],r['payload_sequence_equal'],r['pts_equal'],r['candidate_packets'],r['ffprobe_packets']) for r in x['tracks']])
