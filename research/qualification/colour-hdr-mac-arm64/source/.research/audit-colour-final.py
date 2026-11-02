"""Exact-source slice audit. Run after qualification, FATE and prose checks."""
import hashlib,json,pathlib,shutil,sys,re
root=pathlib.Path(__file__).resolve().parents[1];sys.path.insert(0,str(root))
from harness.runner import load,digest,acceptance
from harness.comparison import validate_case_evidence
out=root/'research/qualification/colour-hdr-mac-arm64';out.mkdir(exist_ok=True)
identity=load(root/'target/harness/identity.json');paths=load(root/'.research/colour-delivery-reports.json')
jobs={'k0':('corpus','slice','fixtures',12),'header':('header-corpus','header-slice','header-fixtures',4),'zlib':('zlib-corpus','zlib-slice','zlib-fixtures',10),'seek':('seek-corpus','seek-slice','seek-fixtures',8),'colour':('colour-corpus','colour-slice','colour-fixtures',33)}
for group in ['source_hashes','build_input_hashes']:
 for name,h in identity[group].items():assert digest(root/name)==h,(group,name)
assert {name:digest(root/('target/release/media-worker' if name=='candidate' else 'target/harness/ffmpeg-worker')) for name in identity['binaries']}==identity['binaries']
assert all(digest(root/r['path'])==r['sha256'] for r in identity['comparison_references'].values())
for name in ['observer','probe']:
 p=root/('target/harness/allocations.dylib' if name=='observer' else 'target/harness/memory-check')
 assert digest(p)==identity['instrumentation'][name+'_sha256']
# Original feature contracts/profiles remain byte-for-byte unchanged.
old=load(root/'research/qualification/zlib-cues-approved-mac-arm64/identity.json')
for manifest,contract,_,_ in list(jobs.values())[:4]:
 for name in [manifest,contract]:
  path='harness/'+name+'.json';assert digest(root/path)==old['build_input_hashes'][path]
summary={};assets={};reports={}
for name,(manifest,contract,folder,count) in jobs.items():
 report=load(pathlib.Path(paths[name]));contract_path=root/'harness'/(contract+'.json');c=load(contract_path);m=load(root/'harness'/(manifest+'.json'))
 assert report['identity']==identity and report['collection_status']=='complete'
 assert report['target']=='aarch64-apple-darwin'
 assert report['contract_sha256']==digest(contract_path) and report['corpus_manifest_sha256']==digest(root/'harness'/(manifest+'.json'))
 assert len(report['cases'])==len(m['cases'])==len(c['required_profiles'])==count
 assert c['runtime_ratio_max']==.8 and c['memory_ratio_max_exclusive']==1 and c['minimum_pairs']==30
 verdict=acceptance(c,report['cases'],identity,report['target'],report['fate_coverage'])
 assert verdict==report['acceptance'] and verdict['slice_accepted'] and not verdict['accepted'],name
 sources={v['id']:v for v in m['cases']}
 for case in report['cases']:
  src=sources[case['id']];p=root/'.research'/folder/src['file']
  assert digest(p)==case['input_sha256']==src['sha256'] and src['rights']=='authored-synthetic'
  par=case['parity'];assert par['status']==par['source_timing_status']=='pass'
  assert all(x['status']=='pass' for x in par['short_reads'])
  if name=='zlib':assert par['source_content_status']=='pass'
  if name=='seek':assert len(par['playback_seeks'])==12 and all(x['status']=='pass' for x in par['playback_seeks'])
  if name=='colour':
   assert par['source_colour_status']==par['colour_reference_status']=='pass'
   assert par['colour_short_reads']==[{'chunk':n,'status':'pass'} for n in (1,7,4096)]
  hashes=validate_case_evidence(case,identity,c['id']);t=case['timing'];mem=case['memory'];cert=mem['certificate']
  assert t['worker_hashes']==cert['worker_hashes']==hashes and t['input_sha256']==mem['input_sha256']==src['sha256']
  assert t['pairs']==len(t['samples'])==mem['pairs']==len(mem['samples'])==30 and t['iterations']==50
  assert t['ratio']['upper_95']<=.8 and mem['rss_ratio_upper_95']<1 and mem['heap_ratio_upper_95']<1
  assert cert['probe_sha256']==identity['instrumentation']['probe_sha256'] and cert['observer_sha256']==identity['instrumentation']['observer_sha256']
  assets[name+'/'+src['file']]={'source_path':str(p.relative_to(root)),'sha256':src['sha256'],'rights':src['rights']}
 directory=out/name;directory.mkdir(exist_ok=True);shutil.copyfile(paths[name],directory/'report.json')
 raw=pathlib.Path(paths[name]).parent;hashes={str(p.relative_to(raw)):digest(p) for p in sorted(raw.rglob('*.json')) if p.name not in ['partial.json','report.json']}
 (directory/'raw-trace-evidence.json').write_text(json.dumps({'original_directory':str(raw.relative_to(root)),'sha256':hashes},indent=2)+'\n')
 summary[name]={'contract_id':c['id'],'cases':count,'slice_accepted':True,'complete_module_accepted':False,'time_ratio_upper_95_max':max(x['timing']['ratio']['upper_95'] for x in report['cases']),'attributable_rss_ratio_upper_95_max':max(x['memory']['rss_ratio_upper_95'] for x in report['cases']),'live_heap_ratio_upper_95_max':max(x['memory']['heap_ratio_upper_95'] for x in report['cases']),'references':sorted({x['reference']['kind'] for x in report['cases']})}
 reports[name]=report
# Keep exact authored inputs and all binaries separately from mutable build output.
bundle=hashlib.sha256(json.dumps(assets,sort_keys=True).encode()).hexdigest()
for name,r in assets.items():
 dst=root/'.research/frozen-corpora'/bundle/name;dst.parent.mkdir(parents=True,exist_ok=True)
 if not dst.exists():shutil.copyfile(root/r['source_path'],dst)
 assert digest(dst)==r['sha256'];r['frozen_path']=str(dst.relative_to(root))
refs=load(root/'.research/reference/build.json');binaries=[root/'target/release/media-worker',*[root/'target/harness'/n for n in ['ffmpeg-worker','ffmpeg-combined-worker','allocations.dylib','memory-check']],root/'.research/reference'/('FFmpeg-'+refs['revision'])/'ffmpeg'];frozen=[]
for p in binaries:
 h=digest(p);dst=root/'.research/frozen-workers'/h/p.name;dst.parent.mkdir(parents=True,exist_ok=True)
 if not dst.exists():shutil.copy2(p,dst)
 assert digest(dst)==h;frozen.append({'source_path':str(p.relative_to(root)),'frozen_path':str(dst.relative_to(root)),'sha256':h})
checks=(root/'.research/colour-check-final.log').read_text();assert list(map(int,re.findall(r'test result: ok\. (\d+) passed; 0 failed;',checks)))==[1,12,27,0]
assert 'Ran 33 tests' in checks and '\nOK\n' in checks and 'Strict-safe workspace: 2 owned packages; no third-party packages' in checks
assert 'Finished' in (root/'.research/colour-lib-check.log').read_text()
for r in load(root/'specs/sources.json')['sources']:assert digest(root/r['cached_path'])==r['sha256']
fate=[]
for label in ['xiph','zlib','zero-length']:
 r=load(root/'.research/reference'/('fate-'+label+'.json'));assert r['adapter_identity']==identity and r['candidate_demux_comparison']['status']=='pass'
 assert r['upstream_case_executed']==(label!='zero-length') and r['reference_status']==('not-run-demux-only' if label=='zero-length' else 'pass');fate.append(r)
for name,value in [('summary.json',summary),('identity.json',identity),('input-evidence.json',assets),('binary-evidence.json',frozen),('fate-evidence.json',fate)]: (out/name).write_text(json.dumps(value,indent=2)+'\n')
for src,dst in [('colour-check-final.log','checks.log'),('colour-lib-check.log','lib-no-default-features.log'),('colour-specs.log','specifications.log'),('colour-regression-before.log','regression-before.log'),('colour-rational-boundary-results.json','boundary-before.json'),('colour-qualification.log','qualification-windows.log')]:shutil.copyfile(root/'.research'/src,out/dst)
audit={'all_67_cases_pass_parity_time_and_both_memory_gates':True,'full_colour_source_ieee_bits_and_raw_track_proof':True,'all_33_colour_native_views_and_99_short_read_sequences_pass':True,'original_34_profiles_and_four_contracts_unchanged':True,'all_96_fresh_eof_seek_sequences_pass':True,'exact_inputs_workers_calibration_and_sources_verified':True,'two_upstream_fate_targets_and_three_demux_comparisons_pass':True,'host_tests_40_rust_33_python_pass':True,'complete_module_accepted':False,'streem_or_tv_integration_claimed':False,'committed_or_published':False}
hdr=load(root/'.research/fate-colour-hdr.json')
assert digest(root/'.research/fate-samples'/hdr['sample'])==hdr['sample_sha256']
assert hdr['adapter_identity']==identity and not hdr['upstream_case_executed']
assert 'attachments/chapters' in load(root/'.research/fate-colour-hdr/colour-candidate.json')['stderr']
assert hdr['result']['status']!='pass'
additional={k:hdr[k] for k in ['sample','sample_sha256','origin','rights','related_upstream_cases','upstream_case_executed','operation','script_sha256','raw_trace_sha256']}
additional.update({'status':'unsupported-chapters-attachments','result':hdr['result'],'source_path':'.research/fate-samples/'+hdr['sample'],'adapter_binaries':identity['binaries'],'complete_module_acceptance':False})
(out/'additional-hdr-input.json').write_text(json.dumps(additional,indent=2)+'\n')
audit['additional_real_hdr_file_remains_unaccepted_for_missing_chapters_attachments']=True
audit['upstream_hdr_priority_case_executed']=False
(out/'completion-audit.json').write_text(json.dumps(audit,indent=2)+'\n');print(json.dumps(summary,indent=2));print('Qualification audit passed; seal source/docs after final claim review.')
