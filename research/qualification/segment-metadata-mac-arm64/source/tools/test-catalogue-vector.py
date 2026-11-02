#!/usr/bin/env python3
"""Qualify the intact mandatory external HDR vector locally; never vendor its bytes."""
import argparse,json,platform,sys
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1];sys.path.insert(0,str(ROOT))
from harness.runner import load,digest,catalogue_parity,benchmark,memory,acceptance
from harness.memory import reusable_memory
from harness.comparison import reference_record,reference_summary

def main():
    p=argparse.ArgumentParser(__doc__);p.add_argument('--output',type=Path,required=True);p.add_argument('--reuse-report',type=Path);p.add_argument('--pairs',type=int,default=30);p.add_argument('--iterations',type=int,default=50);p.add_argument('--benchmark',action='store_true');p.add_argument('--memory',action='store_true');a=p.parse_args()
    contract=ROOT/'harness/catalogue-slice.json';c=load(contract);spec=c['required_additional_vectors'][0];identity=load(ROOT/'target/harness/identity.json');candidate=ROOT/'target/release/media-worker';reference=ROOT/'target/harness/ffmpeg-worker'
    assert identity['binaries']=={'candidate':digest(candidate),'reference':digest(reference)}
    assert all(digest(ROOT/n)==h for group in ['source_hashes','build_input_hashes'] for n,h in identity[group].items())
    path=(ROOT/'.research/fate-samples'/spec['file']).resolve();assert path.is_relative_to((ROOT/'.research/fate-samples').resolve())
    if not path.is_file() or digest(path)!=spec['sha256']:p.error('required original external vector missing or changed')
    old=load(a.reuse_report) if a.reuse_report else {};row=old.get('case',{})
    if old and (old.get('identity',{}).get('binaries')!=identity['binaries'] or old.get('contract_sha256')!=digest(contract)):p.error('external reuse requires identical workers, contract and input')
    if row.get('input_sha256')!=spec['sha256']:row={}
    a.output.mkdir(parents=True,exist_ok=True)
    case={'id':spec['file'],'profile':spec['file'],'input_sha256':spec['sha256'],'rights':spec['rights'],'reference':reference_summary(reference_record(identity)),
          'operation':{'name':'container-demux-v1','seek_target_ns':None,'view':'catalogue-source-and-native-v1'},'parity':catalogue_parity(candidate,reference,path,a.output/'raw')}
    if case['parity']['status']=='pass':
        if a.benchmark:
            t=row.get('timing',{});case['timing']=t if t.get('status')=='measured' and t.get('pairs')==a.pairs and t.get('iterations')==a.iterations else benchmark(candidate,reference,path,a.pairs,a.iterations,280,('catalogue-',[]))
        if a.memory:
            m=row.get('memory',{});case['memory']=m if reusable_memory(m,a.pairs,identity.get('instrumentation')) else memory(candidate,reference,path,a.pairs,280,('catalogue-',[]))
    derived={**c,'required_profiles':[spec['file']],'required_additional_vectors':[]}
    target='aarch64-apple-darwin' if(platform.system(),platform.machine())==('Darwin','arm64') else 'unqualified'
    verdict=acceptance(derived,[case],identity,target,[])
    receipt={'schema':1,'identity':identity,'contract_sha256':digest(contract),'case':case,'acceptance':verdict,'target':target,'input_path':str(path),'origin':'https://fate-suite.ffmpeg.org/'+spec['file'],'redistribution_qualified':False}
    tmp=a.output/'report.tmp';tmp.write_text(json.dumps(receipt,indent=2)+'\n');tmp.replace(a.output/'report.json')
    print(case['id'],case['parity']['status'],'numeric gates',verdict['slice_accepted'])
    return 0 if case['parity']['status']=='pass' else 1
if __name__=='__main__':raise SystemExit(main())
