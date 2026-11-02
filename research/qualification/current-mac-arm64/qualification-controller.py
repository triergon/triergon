import json,os,pathlib,signal,subprocess,sys,time
root=pathlib.Path('/Users/jonasgrosch/projects/safe-media');sys.path.insert(0,str(root))
from harness.runner import cpu_snapshot,active_cpu
pid=98465
identity=subprocess.check_output(['ps','-p',str(pid),'-o','lstart='],text=True).strip()
jobs=[{'name':'k0','extra':[],'previous':root/'.research/k0-release-memory/report.json'},
      {'name':'header','extra':['--manifest','harness/header-corpus.json','--contract','harness/header-slice.json','--corpus','.research/header-fixtures'],'previous':root/'.research/header-release-memory/report.json'}]
for attempt in range(5):
 quiet=None;last_notice=0;deadline=time.monotonic()+600
 while True:
  before={p:v for p,v in cpu_snapshot().items() if p!=pid};time.sleep(.25)
  after={p:v for p,v in cpu_snapshot().items() if p!=pid};now=time.monotonic()
  if active_cpu(before,after):quiet=None
  elif quiet is None:quiet=now
  if quiet is not None and now-quiet>=3:break
  if now>=deadline:raise SystemExit('Still waiting for an idle compiler window; camera running.')
  if now-last_notice>=45:print('Waiting for a quiet compiler window; camera running.',flush=True);last_notice=now
 watchdog='''import os,signal,subprocess,sys,time
time.sleep(180)
try:
 p=int(sys.argv[1]);s=subprocess.check_output(['ps','-p',str(p),'-o','lstart='],text=True).strip()
 if s==sys.argv[2]:os.kill(p,signal.SIGCONT)
except (ProcessLookupError,subprocess.CalledProcessError):pass
'''
 monitor=subprocess.Popen([sys.executable,'-c',watchdog,str(pid),identity],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
 started=time.time();paused=False;resumed=False
 try:
  if subprocess.check_output(['ps','-p',str(pid),'-o','lstart='],text=True).strip()!=identity:raise RuntimeError('Camera process identity changed')
  os.kill(pid,signal.SIGSTOP);paused=True;print('Qualification window: camera paused with watchdog.',flush=True)
  for job in jobs:
   if job.get('done'):continue
   out=root/f".research/mac-final-{job['name']}-{attempt}";out.mkdir(parents=True,exist_ok=True)
   command=[sys.executable,'tools/test-media.py',*job['extra'],'--benchmark','--memory','--pairs','30','--iterations','50','--fate-cache','.research/fate-samples','--output',str(out),'--require-slice-acceptance','--reuse-report',str(job['previous'])]
   with (out/'run.log').open('w') as log:
    subprocess.run(command,cwd=root,stdout=log,stderr=subprocess.STDOUT,timeout=max(1,160-(time.time()-started)))
   report=json.loads((out/'report.json').read_text())
   for row in report['cases']:
    timing=row.get('timing',{})
    if timing.get('status')=='measured' and timing['ratio']['upper_95']>.8:
     raise RuntimeError('Clean runtime gate failed: '+row['id']+' '+str(timing['ratio']))
   job['previous']=out/'report.json';job['done']=report['acceptance']['slice_accepted']
   print(job['name']+' accepted='+str(job['done']),flush=True)
 finally:
  if paused and subprocess.check_output(['ps','-p',str(pid),'-o','lstart='],text=True).strip()==identity:
   os.kill(pid,signal.SIGCONT);resumed=True
  if resumed:monitor.terminate()
  print('Camera resumed='+str(resumed),flush=True)
  receipt={'authorized':True,'paused':paused,'resumed':resumed,'elapsed_seconds':time.time()-started,'attempt':attempt}
  (root/f'.research/mac-final-window-{attempt}.json').write_text(json.dumps(receipt,indent=2)+'\n')
 if all(job.get('done') for job in jobs):
  (root/'.research/latest-qualified-slices.json').write_text(json.dumps({job['name']:str(job['previous']) for job in jobs},indent=2)+'\n')
  print('Both frozen slices accepted.',flush=True);break
else:raise SystemExit('Some slices remain unqualified; all rejected windows retained.')
