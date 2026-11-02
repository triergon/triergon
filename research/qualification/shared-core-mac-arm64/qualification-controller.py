"""Fresh, bounded camera-isolated qualification; preserve every clean outcome."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from harness.runner import active_cpu, cpu_snapshot
from harness.provenance import source_identity_error

CAMERA = 98465
CAMERA_START = 'Wed Oct 28 12:21:25 2026'
CAMERA_COMMAND = '/opt/homebrew/bin/ffmpeg'
IDENTITY = json.loads((ROOT / 'target/harness/identity.json').read_text())
if IDENTITY.get('schema') != 2 or source_identity_error(ROOT, IDENTITY):
    raise SystemExit('Build and freeze the fresh selected graph before qualification')
RUN = 'core-migration-' + IDENTITY['binaries']['candidate'][:12] + '-' + str(time.time_ns())
BASE = ROOT / '.research' / RUN
BASE.mkdir()
LEDGER = ROOT / '.research/core-migration-qualification-progress.json'


def camera_matches():
    try:
        return (subprocess.check_output(['ps', '-p', str(CAMERA), '-o', 'lstart='], text=True).strip() == CAMERA_START
                and subprocess.check_output(['ps', '-p', str(CAMERA), '-o', 'comm='], text=True).strip() == CAMERA_COMMAND)
    except subprocess.CalledProcessError:
        return False


def save(path, value):
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(value, indent=2) + '\n')
    temporary.replace(path)


jobs = []
for name, prefix, directory in [('zlib','zlib','.research/zlib-fixtures'), ('k0','','.research/fixtures'),
        ('header','header','.research/header-fixtures'), ('seek','seek','.research/seek-fixtures'),
        ('colour','colour','.research/colour-fixtures'), ('external',None,None),
        ('catalogue','catalogue','.research/catalogue-fixtures')]:
    extra = [] if not prefix else ['--manifest',f'harness/{prefix}-corpus.json','--contract',f'harness/{prefix}-slice.json','--corpus',directory]
    jobs.append({'name':name,'extra':extra,'path':None,'done':False})

WATCHDOG = '''import os,signal,subprocess,sys,time
p=int(sys.argv[1]);time.sleep(180)
try:
 if subprocess.check_output(['ps','-p',str(p),'-o','lstart='],text=True).strip()==sys.argv[2] and subprocess.check_output(['ps','-p',str(p),'-o','comm='],text=True).strip()==sys.argv[3]:os.kill(p,signal.SIGCONT)
except (ProcessLookupError,subprocess.CalledProcessError):pass
'''


def collect(command, output, seconds):
    with (output/'run.log').open('w') as log:
        process = subprocess.Popen(command, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            return {'returncode':process.wait(timeout=seconds),'timed_out':False}
        except (subprocess.TimeoutExpired, KeyboardInterrupt) as error:
            # Kill the whole collection group, including an in-flight worker.
            try:
                os.killpg(process.pid,signal.SIGTERM)
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid,signal.SIGKILL)
                process.wait()
            except ProcessLookupError:
                process.wait()
            if isinstance(error, KeyboardInterrupt):
                raise
            return {'returncode':process.returncode,'timed_out':True}


for window in range(32):
    if not camera_matches():
        raise SystemExit('Authorized camera identity changed; no signal sent')
    deadline, quiet, notice = time.monotonic()+600, None, 0
    while True:
        before={p:v for p,v in cpu_snapshot().items() if p!=CAMERA}
        time.sleep(.25)
        after={p:v for p,v in cpu_snapshot().items() if p!=CAMERA}
        now=time.monotonic()
        if active_cpu(before,after):quiet=None
        elif quiet is None:quiet=now
        if quiet is not None and now-quiet>=3:break
        if now>=deadline:raise SystemExit('No quiet compiler window; camera running; receipts preserved')
        if now-notice>=45:print('Waiting for quiet compiler window; camera running.',flush=True);notice=now
    monitor=subprocess.Popen([sys.executable,'-c',WATCHDOG,str(CAMERA),CAMERA_START,CAMERA_COMMAND],
        stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
    start=time.monotonic();paused=resumed=False
    try:
        if not camera_matches():raise RuntimeError('Camera identity changed before pause')
        os.kill(CAMERA,signal.SIGSTOP);paused=True
        print('Camera paused with watchdog; window',window,flush=True)
        for job in jobs:
            if job['done']:continue
            if time.monotonic()-start>60:break
            if job['name']=='catalogue' and not jobs[-2]['done']:break
            output=BASE/(job['name']+'-window-'+str(window));output.mkdir()
            if job['name']=='external':command=[sys.executable,'tools/test-catalogue-vector.py']
            else:command=[sys.executable,'tools/test-media.py',*job['extra'],'--fate-cache','.research/fate-samples']
            command+=['--output',str(output),'--benchmark','--memory','--pairs','30','--iterations','50']
            if job['name']=='catalogue':command+=['--additional-report',str(jobs[-2]['path'])]
            if job['path']:command+=['--reuse-report',str(job['path'])]
            outcome=collect(command,output,max(1,155-(time.monotonic()-start)))
            receipt=output/'report.json'
            if not receipt.is_file():receipt=output/'partial.json'
            if receipt.is_file():
                report=json.loads(receipt.read_text())
                if report['identity']!=IDENTITY:raise RuntimeError('Worker/source identity changed')
                job['path']=str(receipt)
                rows=[report['case']] if job['name']=='external' else report['cases']
                complete=job['name']=='external' or report.get('collection_status')=='complete'
                # All clean failures are terminal evidence. Only contamination or
                # interruption remains retryable; a bad ratio is never resampled.
                job['done']=complete and all(r['parity']['status']!='pass' or (
                    r.get('timing',{}).get('status') not in (None,'contaminated')
                    and r.get('memory',{}).get('status') not in (None,'contaminated')) for r in rows)
                job['slice_accepted']=report['acceptance']['slice_accepted']
                print(job['name'],'complete='+str(job['done']),'slice_accepted='+str(job['slice_accepted']),flush=True)
            job['last_outcome']=outcome
            save(LEDGER,{'run':RUN,'base':str(BASE),'identity':IDENTITY,'jobs':jobs})
            if outcome['timed_out']:break
    finally:
        if paused and camera_matches():os.kill(CAMERA,signal.SIGCONT);resumed=True
        if resumed:monitor.terminate();monitor.wait()
        save(BASE/('camera-window-'+str(window)+'.json'),{'paused':paused,'resumed':resumed,
            'elapsed_seconds':time.monotonic()-start,'authorized_process':{'pid':CAMERA,'start':CAMERA_START,'comm':CAMERA_COMMAND}})
        print('Camera resumed='+str(resumed),flush=True)
    if all(job['done'] for job in jobs):
        save(ROOT/'.research/core-migration-delivery-reports.json',{job['name']:job['path'] for job in jobs})
        print('Collection complete; all numerical/source failures preserved.',flush=True)
        break
else:
    raise SystemExit('Remaining contaminated/incomplete cases; all receipts preserved')
