#!/usr/bin/env python3
"""Qualify the intact mandatory external HDR vector locally; never vendor its bytes."""
import argparse,platform,sys
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1];sys.path.insert(0,str(ROOT))
from harness.common import digest,load,save_json
from harness.runner import catalogue_parity,benchmark,memory,acceptance
from harness.reuse import retained_memory,retained_timing
from harness.comparison import reference_record,reference_summary
from harness.provenance import source_identity_error

def main():
    p=argparse.ArgumentParser(__doc__);p.add_argument('--output',type=Path,required=True);p.add_argument('--reuse-report',type=Path);p.add_argument('--pairs',type=int,default=30);p.add_argument('--iterations',type=int,default=50);p.add_argument('--benchmark',action='store_true');p.add_argument('--memory',action='store_true');a=p.parse_args()
    contract=ROOT/'harness/catalogue-slice.json';c=load(contract);spec=c['required_additional_vectors'][0];identity=load(ROOT/'target/harness/identity.json');candidate=ROOT/'target/release/media-worker';reference=ROOT/'target/harness/ffmpeg-worker'
    if identity['binaries']!={'candidate':digest(candidate),'reference':digest(reference)}:p.error('worker changed since build identity')
    if error:=source_identity_error(ROOT,identity):p.error(error)
    path=(ROOT/'.research/fate-samples'/spec['file']).resolve();assert path.is_relative_to((ROOT/'.research/fate-samples').resolve())
    if not path.is_file() or digest(path)!=spec['sha256']:p.error('required original external vector missing or changed')
    old=load(a.reuse_report) if a.reuse_report else {};row=old.get('case',{})
    if old and a.output.resolve()==a.reuse_report.parent.resolve():p.error('measurement reuse needs a different output directory to preserve the previous report and raw observations')
    if old and (old.get('identity',{}).get('binaries')!=identity['binaries'] or old.get('contract_sha256')!=digest(contract)):p.error('external reuse requires identical workers, contract and input')
    if row.get('input_sha256')!=spec['sha256']:row={}
    a.output.mkdir(parents=True,exist_ok=True)
    case={'id':spec['file'],'profile':spec['file'],'input_sha256':spec['sha256'],'rights':spec['rights'],'reference':reference_summary(reference_record(identity)),
          'operation':{'name':'container-demux-v1','seek_target_ns':None,'view':'catalogue-source-and-native-v1'},'parity':catalogue_parity(candidate,reference,path,a.output/'raw')}
    if row.get('operation')!=case['operation'] or row.get('reference')!=case['reference']:row={}
    if case['parity']['status']=='pass':
        if a.benchmark:
            try:t=retained_timing(row.get('timing',{}),a.pairs,a.iterations,c['minimum_pairs'])
            except ValueError as error:p.error(str(error))
            case['timing']=t if t is not None else benchmark(candidate,reference,path,a.pairs,a.iterations,280,('catalogue-',[]))
        if a.memory:
            try:m=retained_memory(row.get('memory',{}),a.pairs,c['minimum_pairs'],old.get('identity',{}).get('instrumentation'),identity.get('instrumentation'))
            except ValueError as error:p.error(str(error))
            case['memory']=m if m is not None else memory(candidate,reference,path,a.pairs,280,('catalogue-',[]))
        if row:case['previous_measurement_report']={'path':str(a.reuse_report.resolve()),'sha256':digest(a.reuse_report)}
    derived={**c,'required_profiles':[spec['file']],'required_additional_vectors':[]}
    target={('Darwin','arm64'):'aarch64-apple-darwin',('Linux','aarch64'):'aarch64-unknown-linux-gnu',('Linux','x86_64'):'x86_64-unknown-linux-gnu'}.get((platform.system(),platform.machine()),'unqualified')
    verdict=acceptance(derived,[case],identity,target,[])
    if error:=source_identity_error(ROOT,identity):
        verdict['accepted']=verdict['slice_accepted']=False
        verdict['reasons'].append(error);verdict['slice_reasons'].append(error)
    receipt={'schema':1,'identity':identity,'contract_sha256':digest(contract),'case':case,'acceptance':verdict,'target':target,'input_path':str(path),'origin':'https://fate-suite.ffmpeg.org/'+spec['file'],'redistribution_qualified':False}
    save_json(a.output/'report.json',receipt)
    print(case['id'],case['parity']['status'],'numeric gates',verdict['slice_accepted'])
    return 0 if case['parity']['status']=='pass' else 1
if __name__=='__main__':raise SystemExit(main())
