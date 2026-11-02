"""Measure a review proposal without changing contracts or official acceptance."""
import hashlib,json,os,pathlib,signal,subprocess,sys,time
root=pathlib.Path(__file__).resolve().parents[2];sys.path.insert(0,str(root))
from harness.runner import execute,differences,memory,benchmark,cpu_snapshot,active_cpu
where=pathlib.Path(__file__).resolve().parent
identity=json.loads((root/'target/harness/identity.json').read_text());candidate=root/'target/release/media-worker'
evaluation=json.loads((where/'spec-reference-evaluation.json').read_text())
assert all(c['pass'] for c in evaluation['cases'])
assert evaluation['canonical_identity']['binaries']==identity['binaries']
for name,sha in evaluation['prototype_sources'].items():assert hashlib.sha256((where/name).read_bytes()).hexdigest()==sha
receipt=where/('proposal-'+identity['binaries']['candidate'][:12]+'-'+str(time.time_ns())+'.json')
report={'status':'review-proposal-not-an-official-acceptance','canonical_identity':identity,'prototype_evaluation':evaluation,'cases':[]}
def save():receipt.write_text(json.dumps(report,indent=2)+'\n')
for name,reference in [('zlib-empty',root/'target/harness/ffmpeg-worker'),('zlib-mixed',where/'spec-reference')]:
 path=root/'.research/zlib-fixtures'/(name+'.mkv');left=execute([candidate,'codec-trace',path]);right=execute([reference,'codec-trace',path])
 assert left['status']==right['status']=='ok' and not differences(left['data'],right['data'])
 row={'id':name,'reference_kind':'unmodified pinned FFmpeg packet API' if name=='zlib-empty' else 'pinned FFmpeg plus explicit native combined-encoding restoration','view':'optional-pcm-decoder-packets-v1','input_sha256':hashlib.sha256(path.read_bytes()).hexdigest(),'reference_sha256':hashlib.sha256(reference.read_bytes()).hexdigest(),'parity':'pass','candidate':left,'reference':right,'memory':memory(candidate,reference,path,30,280,('codec-',[]))}
 report['cases'].append(row);save();print(name,'memory',row['memory']['status'],flush=True)
pid=98465;expected='Wed Oct 28 12:21:25 2026'
def camera_matches():
 try:return subprocess.check_output(['ps','-p',str(pid),'-o','lstart='],text=True).strip()==expected and pathlib.Path(subprocess.check_output(['ps','-p',str(pid),'-o','comm='],text=True).strip()).name=='ffmpeg'
 except subprocess.CalledProcessError:return False
if not camera_matches():raise SystemExit('Camera process identity mismatch; no signal sent')
quiet=None;deadline=time.monotonic()+600;notice=0
while True:
 before={p:v for p,v in cpu_snapshot().items() if p!=pid};time.sleep(.25);after={p:v for p,v in cpu_snapshot().items() if p!=pid};now=time.monotonic()
 if active_cpu(before,after):quiet=None
 elif quiet is None:quiet=now
 if quiet is not None and now-quiet>=3:break
 if now>=deadline:raise SystemExit('No quiet compiler window; memory proposal retained; camera running')
 if now-notice>45:print('Waiting for quiet compiler window; camera running.',flush=True);notice=now
watchdog='''import os,signal,subprocess,pathlib,sys,time
time.sleep(180)
try:
 pid=int(sys.argv[1]);start=subprocess.check_output(['ps','-p',str(pid),'-o','lstart='],text=True).strip();command=subprocess.check_output(['ps','-p',str(pid),'-o','comm='],text=True).strip()
 if start==sys.argv[2] and pathlib.Path(command).name=='ffmpeg':os.kill(pid,signal.SIGCONT)
except (ProcessLookupError,subprocess.CalledProcessError):pass
'''
monitor=subprocess.Popen([sys.executable,'-c',watchdog,str(pid),expected],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
paused=resumed=False;started=time.monotonic()
try:
 if not camera_matches():raise RuntimeError('Camera identity changed')
 os.kill(pid,signal.SIGSTOP);paused=True;print('Camera paused with independent resume watchdog.',flush=True)
 for row in report['cases']:
  reference=root/'target/harness/ffmpeg-worker' if row['id']=='zlib-empty' else where/'spec-reference'
  for _ in range(3):
   if time.monotonic()-started>145:raise RuntimeError('Bounded isolation window exhausted')
   row['timing']=benchmark(candidate,reference,root/'.research/zlib-fixtures'/(row['id']+'.mkv'),30,50,280,('codec-',[]));save()
   if row['timing']['status']!='contaminated':break
  print(row['id'],'timing',row['timing']['status'],row['timing'].get('ratio'),flush=True)
finally:
 if paused and camera_matches():os.kill(pid,signal.SIGCONT);resumed=True
 if resumed:monitor.terminate();monitor.wait()
 report['camera_isolation']={'owner_authorized':True,'paused':paused,'resumed':resumed,'elapsed_seconds':time.monotonic()-started};save();print('Camera resumed',resumed,flush=True)
(root/'.research/latest-review-proposal.json').write_text(json.dumps({'path':str(receipt)},indent=2)+'\n')
print(receipt,flush=True)
