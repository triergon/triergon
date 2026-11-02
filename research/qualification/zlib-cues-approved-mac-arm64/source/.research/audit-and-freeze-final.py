"""Audit every frozen slice requirement and retain its exact owned evidence."""
import hashlib,json,pathlib,shutil,re,sys
root=pathlib.Path(__file__).resolve().parents[1];sys.path.insert(0,str(root))
from harness.runner import acceptance,load,digest
from harness.comparison import rule_for,reference_record,validate_case_evidence
identity=load(root/'target/harness/identity.json')
out=root/'research/qualification/zlib-cues-approved-mac-arm64';out.mkdir(exist_ok=True)
jobs={'k0':('harness/corpus.json','harness/slice.json','.research/fixtures',12),'header':('harness/header-corpus.json','harness/header-slice.json','.research/header-fixtures',4),'zlib':('harness/zlib-corpus.json','harness/zlib-slice.json','.research/zlib-fixtures',10),'seek':('harness/seek-corpus.json','harness/seek-slice.json','.research/seek-fixtures',8)}
paths=load(root/'.research/k2k3-delivery-reports.json')
for group in ['source_hashes','build_input_hashes']:
 for name,sha in identity[group].items():assert digest(root/name)==sha,(group,name)
for record in identity['comparison_references'].values():assert digest(root/record['path'])==record['sha256']
assert digest(root/'target/release/media-worker')==identity['binaries']['candidate']
specs=load(root/'specs/sources.json')['sources'];assert len(specs)==7
for spec in specs:assert digest(root/spec['cached_path'])==spec['sha256']
summary={};asset_map={};all_reports={}
observer_sha=digest(root/'target/harness/allocations.dylib');probe_sha=digest(root/'target/harness/memory-check')
for name,(manifest_file,contract_file,corpus_folder,count) in jobs.items():
 report=load(pathlib.Path(paths[name]));contract=load(root/contract_file);manifest=load(root/manifest_file)
 assert report['collection_status']=='complete' and report['identity']==identity,name
 assert report['contract_sha256']==digest(root/contract_file)
 assert report['corpus_manifest_sha256']==digest(root/manifest_file)
 assert len(report['cases'])==len(manifest['cases'])==len(contract['required_profiles'])==count
 assert {c['profile'] for c in report['cases']}==set(contract['required_profiles'])
 assert contract['runtime_ratio_max']==.8 and contract['memory_ratio_max_exclusive']==1 and contract['minimum_pairs']==30
 assert report['target']=='aarch64-apple-darwin'
 recomputed=acceptance(contract,report['cases'],identity,report['target'],report['fate_coverage'])
 assert recomputed==report['acceptance'] and recomputed['slice_accepted'],name
 assert not recomputed['accepted'] and contract['pending_module_requirements']
 source_cases={c['id']:c for c in manifest['cases']}
 for case in report['cases']:
  source=source_cases[case['id']];path=root/corpus_folder/source['file']
  assert source['rights']=='authored-synthetic' and digest(path)==source['sha256']==case['input_sha256']
  assert case['parity']['status']==case['parity']['source_timing_status']=='pass'
  assert all(s['status']=='pass' for s in case['parity']['short_reads'])
  if name=='zlib':assert case['parity']['source_content_status']=='pass'
  if name=='seek':
   assert len(case['parity']['playback_seeks'])==12
   assert all(s['status']=='pass' for s in case['parity']['playback_seeks'])
  selected=validate_case_evidence(case,identity,contract['id'])
  timing=case['timing'];memory=case['memory']
  assert timing['worker_hashes']==memory['certificate']['worker_hashes']==selected
  assert timing['input_sha256']==memory['input_sha256']==case['input_sha256']
  assert timing['pairs']==len(timing['samples'])==memory['pairs']==len(memory['samples'])==30
  assert timing['iterations']==50 and timing['ratio']['upper_95']<=.8
  assert memory['rss_ratio_upper_95']<1 and memory['heap_ratio_upper_95']<1
  assert memory['certificate']['observer_sha256']==observer_sha
  assert memory['certificate']['probe_sha256']==probe_sha
  asset_map[name+'/'+source['file']]={'sha256':source['sha256'],'rights':source['rights'],'path':str(path.relative_to(root))}
 directory=out/name;directory.mkdir(exist_ok=True);shutil.copyfile(paths[name],directory/'report.json')
 hashes={}
 for case in report['cases']:
  raw=pathlib.Path(paths[name]).parent/case['id']
  for p in raw.rglob('*.json'):
   json.loads(p.read_text())
   hashes[str(p.relative_to(pathlib.Path(paths[name]).parent))]=digest(p)
 (directory/'trace-sha256.json').write_text(json.dumps(hashes,indent=2)+'\n')
 summary[name]={'contract_id':contract['id'],'cases':count,'parity_pass':count,'slice_accepted':True,'complete_module_accepted':False,'time_ratio_upper_95_max':max(c['timing']['ratio']['upper_95'] for c in report['cases']),'attributable_rss_ratio_upper_95_max':max(c['memory']['rss_ratio_upper_95'] for c in report['cases']),'live_heap_ratio_upper_95_max':max(c['memory']['heap_ratio_upper_95'] for c in report['cases']),'references':sorted({c['reference']['kind'] for c in report['cases']})}
 all_reports[name]=report
# Freeze exact owned inputs and executables outside version control.
bundle_id=hashlib.sha256(json.dumps(asset_map,sort_keys=True).encode()).hexdigest()
for relative,record in asset_map.items():
 destination=root/'.research/frozen-corpora'/bundle_id/relative;destination.parent.mkdir(parents=True,exist_ok=True)
 if not destination.exists():shutil.copyfile(root/record['path'],destination)
 assert digest(destination)==record['sha256'];record['frozen_path']=str(destination.relative_to(root))
binaries=[root/'target/release/media-worker',root/'target/harness/ffmpeg-worker',root/'target/harness/ffmpeg-combined-worker',root/'target/harness/allocations.dylib',root/'target/harness/memory-check']
reference_build=load(root/'.research/reference/build.json')
binaries.append(root/'.research/reference'/('FFmpeg-'+reference_build['revision'])/'ffmpeg')
frozen_binaries=[]
for binary in binaries:
 sha=digest(binary);destination=root/'.research/frozen-workers'/sha/binary.name;destination.parent.mkdir(parents=True,exist_ok=True)
 if not destination.exists():shutil.copy2(binary,destination)
 assert digest(destination)==sha
 frozen_binaries.append({'source_path':str(binary.relative_to(root)),'frozen_path':str(destination.relative_to(root)),'sha256':sha})
for filename,value in [('summary.json',summary),('input-evidence.json',asset_map),('binary-evidence.json',frozen_binaries),('identity.json',identity)]:
 (out/filename).write_text(json.dumps(value,indent=2)+'\n')
checks=(root/'.research/check-final-calibration.log').read_text()
results=re.findall(r'test result: ok\. (\d+) passed; 0 failed;',checks)
assert list(map(int,results))==[1,8,25,0]
assert 'Ran 29 tests' in checks and '\nOK\n' in checks
assert 'Strict-safe workspace: 2 owned packages; no third-party packages' in checks
assert 'Finished' in (root/'.research/final-lib-no-default-features.log').read_text()
for src,name in [('.research/check-final-calibration.log','checks.log'),('.research/final-lib-no-default-features.log','lib-no-default-features.log'),('.research/specification-verification-final.log','specification-verification.log'),('.research/spec-fetch-regression-before.log','spec-fetch-regression-before.log'),('.research/spec-fetch-regression-after.log','spec-fetch-regression-after.log'),('.research/reference-investigation/audio-regression-before.log','seek-regression-before.log')]:shutil.copyfile(root/src,out/name)
fate=[]
for label in ['xiph','zlib','zero-length']:
 receipt=load(root/'.research/reference'/('fate-'+label+'.json'))
 assert receipt['candidate_demux_comparison']['status']=='pass'
 assert receipt['adapter_identity']==identity
 assert receipt['upstream_case_executed']==(label!='zero-length')
 assert receipt['reference_status']==('not-run-demux-only' if label=='zero-length' else 'pass')
 fate.append(receipt)
(out/'fate-evidence.json').write_text(json.dumps(fate,indent=2)+'\n')
audit={'requirements':{'all_four_frozen_profile_sets_preserved':True,'k2_source_bytes_private_and_exact_times_verified':True,'k3_all_96_fresh_eof_packet_sequences_match':True,'all_34_case_parity_gates_pass':True,'all_34_case_runtime_upper_bounds_at_most_080':True,'all_34_case_live_heap_and_attributable_rss_upper_bounds_below_1':True,'all_case_samples_current_selected_workers_inputs_and_instrumentation':True,'all_numerical_verdicts_recomputed':True,'owner_approved_rules_bound_to_exact_input_and_raw_reference_output':True,'all_seven_primary_specification_hashes_verified':True,'host_tests_34_rust_29_python_and_shipping_feature_check_pass':True,'actual_two_upstream_fate_cases_and_separate_zero_length_demux_pass':True,'exact_owned_inputs_workers_sources_and_raw_pairs_retained':True},'remaining_broader_module_work':True,'deferred_targets':['x86_64-unknown-linux-gnu','armv7-linux-webos-32'],'streem_integration_claimed':False,'published_or_committed':False}
(out/'completion-audit.json').write_text(json.dumps(audit,indent=2)+'\n')
print(json.dumps(summary,indent=2))
print('All scoped completion requirements audited; source/doc snapshot and archive manifest will be sealed after prose review.')
