"""Bounded authorized capture isolation with exact-worker checkpoint reuse."""
import json,os,pathlib,signal,subprocess,sys,time
root=pathlib.Path(__file__).resolve().parents[1];sys.path.insert(0,str(root))
from harness.runner import cpu_snapshot,active_cpu
pid=98465;expected_start='Wed Oct 28 12:21:25 2026'
def camera_matches():
 try:
  return subprocess.check_output(['ps','-p',str(pid),'-o','lstart='],text=True).strip()==expected_start and subprocess.check_output(['ps','-p',str(pid),'-o','comm='],text=True).strip()=='/opt/homebrew/bin/ffmpeg'
 except subprocess.CalledProcessError:return False
if not camera_matches():raise SystemExit('Authorized camera identity changed; no signal sent.')
identity=json.loads((root/'target/harness/identity.json').read_text());run_id='catalogue-'+identity['binaries']['candidate'][:12]+'-'+str(time.time_ns())
ledger=root/'.research/catalogue-qualification-progress.json';previous=json.loads(ledger.read_text()) if ledger.exists() else {}
extras=[('external',[]),('catalogue',['--manifest','harness/catalogue-corpus.json','--contract','harness/catalogue-slice.json','--corpus','.research/catalogue-fixtures']),('colour',['--manifest','harness/colour-corpus.json','--contract','harness/colour-slice.json','--corpus','.research/colour-fixtures']),('zlib',['--manifest','harness/zlib-corpus.json','--contract','harness/zlib-slice.json','--corpus','.research/zlib-fixtures']),('seek',['--manifest','harness/seek-corpus.json','--contract','harness/seek-slice.json','--corpus','.research/seek-fixtures']),('k0',[]),('header',['--manifest','harness/header-corpus.json','--contract','harness/header-slice.json','--corpus','.research/header-fixtures'])]
jobs=[]
for name,extra in extras:
 cached=None
 value=previous.get('jobs',{}).get(name,{}).get('path')
 if value and pathlib.Path(value).is_file():
  report=json.loads(pathlib.Path(value).read_text())
  prior=report.get('identity',{})
  if prior==identity or (prior.get('binaries')==identity['binaries'] and prior.get('comparison_references')==identity.get('comparison_references')):cached=pathlib.Path(value)
 if name=='catalogue' and cached is None:
  initial=root/'.research/catalogue-memory-borrowed/report.json'
  if initial.exists():
   report=json.loads(initial.read_text())
   if report['identity']['binaries']==identity['binaries'] and report['identity'].get('comparison_references')==identity.get('comparison_references'):cached=initial
 jobs.append({'name':name,'extra':extra,'path':cached,'done':False})
watchdog='''import os,signal,subprocess,sys,time
p=int(sys.argv[1]);time.sleep(180)
try:
 if subprocess.check_output(['ps','-p',str(p),'-o','lstart='],text=True).strip()==sys.argv[2] and subprocess.check_output(['ps','-p',str(p),'-o','comm='],text=True).strip()=='/opt/homebrew/bin/ffmpeg':os.kill(p,signal.SIGCONT)
except (ProcessLookupError,subprocess.CalledProcessError):pass
'''
for window in range(24):
 deadline=time.monotonic()+600;quiet=None;notice=0
 while True:
  before={p:v for p,v in cpu_snapshot().items() if p!=pid};time.sleep(.25);after={p:v for p,v in cpu_snapshot().items() if p!=pid};now=time.monotonic()
  if active_cpu(before,after):quiet=None
  elif quiet is None:quiet=now
  if quiet is not None and now-quiet>=3:break
  if now>=deadline:raise SystemExit('No quiet compiler window; camera running; checkpoints retained.')
  if now-notice>=45:print('Waiting for quiet compiler window; camera running.',flush=True);notice=now
 monitor=subprocess.Popen([sys.executable,'-c',watchdog,str(pid),expected_start],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
 started=time.time();paused=resumed=False
 try:
  if not camera_matches():raise RuntimeError('Capture identity changed')
  os.kill(pid,signal.SIGSTOP);paused=True;print('Camera paused with watchdog, window',window,flush=True)
  for job in jobs:
   if job['done']:continue
   if time.time()-started>60:break
   if job['name']=='catalogue' and not jobs[0]['done']:break
   out=root/f".research/{run_id}-{job['name']}-window-{window}";out.mkdir(parents=True,exist_ok=False)
   if job['name']=='external':cmd=[sys.executable,'tools/test-catalogue-vector.py','--output',str(out),'--benchmark','--memory','--pairs','30','--iterations','50']
   else:
    cmd=[sys.executable,'tools/test-media.py',*job['extra'],'--benchmark','--memory','--pairs','30','--iterations','50','--output',str(out),'--fate-cache','.research/fate-samples']
    if job['name']=='catalogue':cmd+=['--additional-report',str(jobs[0]['path'])]
   if job['path']:cmd+=['--reuse-report',str(job['path'])]
   timeout=False
   try:
    with (out/'run.log').open('w') as log:result=subprocess.run(cmd,cwd=root,stdout=log,stderr=subprocess.STDOUT,timeout=max(1,155-(time.time()-started)))
   except subprocess.TimeoutExpired:timeout=True
   receipt=out/'report.json'
   if not receipt.exists():receipt=out/'partial.json'
   if receipt.exists():
    report=json.loads(receipt.read_text());assert report['identity']==identity
    job['path']=receipt
    rows=[report['case']] if job['name']=='external' else report['cases']
    complete=job['name']=='external' or report.get('collection_status')=='complete'
    job['done']=complete and all(r['parity']['status']!='pass' or(r.get('timing',{}).get('status')=='measured' and r.get('memory',{}).get('status')=='qualified') for r in rows)
    print(job['name'],'measurement-complete='+str(job['done']),'slice-accepted='+str(report['acceptance']['slice_accepted']),flush=True)
   progress={'run_id':run_id,'identity':identity,'jobs':{j['name']:{'path':str(j['path']) if j['path'] else None,'done':j['done']} for j in jobs}}
   tmp=ledger.with_suffix('.tmp');tmp.write_text(json.dumps(progress,indent=2)+'\n');tmp.replace(ledger)
   if timeout:break
 finally:
  if paused and camera_matches():os.kill(pid,signal.SIGCONT);resumed=True
  if resumed:monitor.terminate();monitor.wait()
  (root/f'.research/{run_id}-camera-window-{window}.json').write_text(json.dumps({'paused':paused,'resumed':resumed,'window':window,'elapsed_seconds':time.time()-started,'run_id':run_id},indent=2)+'\n')
  print('Camera resumed='+str(resumed),flush=True)
 if all(j['done'] for j in jobs):
  (root/'.research/catalogue-delivery-reports.json').write_text(json.dumps({j['name']:str(j['path']) for j in jobs},indent=2)+'\n')
  print('All measurements complete. Numerical failures remain preserved.',flush=True);break
else:raise SystemExit('Unmeasured cases remain; every checkpoint retained.')
