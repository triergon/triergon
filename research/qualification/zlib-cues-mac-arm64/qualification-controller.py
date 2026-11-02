"""Bounded owner-authorized camera isolation; atomic per-case checkpoint reuse."""
import json,os,pathlib,signal,subprocess,sys,time
root=pathlib.Path('/Users/jonasgrosch/projects/safe-media');sys.path.insert(0,str(root))
from harness.runner import cpu_snapshot,active_cpu
pid=98465;expected_start='Wed Oct 28 12:21:25 2026'
def camera_matches():
 try:
  start=subprocess.check_output(['ps','-p',str(pid),'-o','lstart='],text=True).strip()
  command=subprocess.check_output(['ps','-p',str(pid),'-o','comm='],text=True).strip()
  return start==expected_start and pathlib.Path(command).name=='ffmpeg'
 except subprocess.CalledProcessError:return False
if not camera_matches():raise SystemExit('Authorized camera process no longer matches; no signal sent.')
identity=json.loads((root/'target/harness/identity.json').read_text())
run_id='alloc-'+identity['binaries']['candidate'][:12]+'-'+str(time.time_ns())
jobs=[]
for name,extra in [('zlib',['--manifest','harness/zlib-corpus.json','--contract','harness/zlib-slice.json','--corpus','.research/zlib-fixtures']),('seek',['--manifest','harness/seek-corpus.json','--contract','harness/seek-slice.json','--corpus','.research/seek-fixtures']),('k0',[]),('header',['--manifest','harness/header-corpus.json','--contract','harness/header-slice.json','--corpus','.research/header-fixtures'])]:
 previous=root/f'.research/k2k3-memory-{name}/report.json'
 report=json.loads(previous.read_text())
 if report['identity']['binaries']!=identity['binaries']:raise SystemExit('Stale memory receipt for '+name)
 jobs.append({'name':name,'extra':extra,'previous':previous})
ledger=root/'.research/k2k3-final-progress.json'
for window in range(12):
 quiet=None;notice=0;deadline=time.monotonic()+600
 while True:
  before={p:v for p,v in cpu_snapshot().items() if p!=pid};time.sleep(.25)
  after={p:v for p,v in cpu_snapshot().items() if p!=pid};now=time.monotonic()
  if active_cpu(before,after):quiet=None
  elif quiet is None:quiet=now
  if quiet is not None and now-quiet>=3:break
  if now>=deadline:raise SystemExit('No quiet compiler window; camera running; checkpoints retained.')
  if now-notice>=45:print('Waiting for quiet compiler window; camera running.',flush=True);notice=now
 watchdog='''import os,signal,subprocess,sys,time,pathlib
time.sleep(180)
try:
 p=int(sys.argv[1]);start=subprocess.check_output(['ps','-p',str(p),'-o','lstart='],text=True).strip();command=subprocess.check_output(['ps','-p',str(p),'-o','comm='],text=True).strip()
 if start==sys.argv[2] and pathlib.Path(command).name=='ffmpeg':os.kill(p,signal.SIGCONT)
except (ProcessLookupError,subprocess.CalledProcessError):pass
'''
 monitor=subprocess.Popen([sys.executable,'-c',watchdog,str(pid),expected_start],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
 started=time.time();paused=resumed=False
 try:
  if not camera_matches():raise RuntimeError('Camera identity changed')
  os.kill(pid,signal.SIGSTOP);paused=True;print('Camera paused with watchdog, window',window,flush=True)
  for job in jobs:
   if job.get('done'):continue
   if time.time()-started>60:break
   out=root/f".research/{run_id}-{job['name']}-window-{window}";out.mkdir(parents=True,exist_ok=False)
   cmd=[sys.executable,'tools/test-media.py',*job['extra'],'--benchmark','--memory','--pairs','30','--iterations','50','--output',str(out),'--fate-cache','.research/fate-samples','--reuse-report',str(job['previous'])]
   timed_out=False
   try:
    with (out/'run.log').open('w') as log:subprocess.run(cmd,cwd=root,stdout=log,stderr=subprocess.STDOUT,timeout=max(1,155-(time.time()-started)))
   except subprocess.TimeoutExpired:timed_out=True
   receipt=out/'report.json'
   if not receipt.exists():receipt=out/'partial.json'
   if receipt.exists():
    report=json.loads(receipt.read_text())
    if report['identity']['binaries']!=identity['binaries']:raise RuntimeError('Receipt worker identity changed')
    job['previous']=receipt
    job['done']=report.get('collection_status')=='complete' and all(row['parity']['status']!='pass' or row.get('timing',{}).get('status')=='measured' for row in report['cases'])
    print(job['name'],'measurement-complete='+str(job['done']),'slice-accepted='+str(report['acceptance']['slice_accepted']),flush=True)
   progress={'run_id':run_id,'binaries':identity['binaries'],'jobs':{job['name']:{'path':str(job['previous']),'done':job.get('done',False)} for job in jobs}}
   temporary=ledger.with_suffix('.tmp');temporary.write_text(json.dumps(progress,indent=2)+'\n');temporary.replace(ledger)
   if timed_out:break
 finally:
  if paused and camera_matches():os.kill(pid,signal.SIGCONT);resumed=True
  if resumed:monitor.terminate();monitor.wait()
  receipt={'authorized':True,'paused':paused,'resumed':resumed,'elapsed_seconds':time.time()-started,'window':window,'run_id':run_id}
  (root/f'.research/{run_id}-camera-window-{window}.json').write_text(json.dumps(receipt,indent=2)+'\n')
  print('Camera resumed='+str(resumed),flush=True)
 if all(job.get('done') for job in jobs):
  (root/'.research/k2k3-delivery-reports.json').write_text(json.dumps({job['name']:str(job['previous']) for job in jobs},indent=2)+'\n')
  print('All measurements complete; failing hard gates remain recorded.',flush=True);break
else:raise SystemExit('Some measurements remain contaminated; all checkpoints retained.')
