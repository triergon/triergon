"""Audit and export the exact shared-core K0–K5 snapshot; never grants module closure."""
from pathlib import Path
import hashlib
import json
import math
import re
import shutil
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from harness.comparison import rule_for, validate_case_evidence
from harness.provenance import source_identity_error
from harness.runner import acceptance, digest, load, measurement_cache

identity = load(ROOT / 'target/harness/identity.json')
ledger = load(ROOT / '.research/core-migration-qualification-progress.json')
assert ledger['identity'] == identity
assert len(ledger['jobs']) == 7 and {job['name'] for job in ledger['jobs']} == {'k0','header','zlib','seek','colour','external','catalogue'}
assert all(job['done'] and job.get('slice_accepted') for job in ledger['jobs']), 'Complete accepted collections are required before export'
paths = load(ROOT / '.research/core-migration-delivery-reports.json')
assert paths == {job['name']: job['path'] for job in ledger['jobs']}
assert identity['schema'] == 2 and source_identity_error(ROOT, identity) is None
assert {package['name'] for package in identity['selected_graph']['packages']} == {'media-worker','safe-media','safe-media-core'}
assert identity['binaries'] == {'candidate':digest(ROOT / 'target/release/media-worker'), 'reference':digest(ROOT / 'target/harness/ffmpeg-worker')}
for record in identity['comparison_references'].values():
    assert digest(ROOT / record['path']) == record['sha256']
for label, name in [('observer','allocations.dylib'), ('probe','memory-check')]:
    assert digest(ROOT / 'target/harness' / name) == identity['instrumentation'][label + '_sha256']

host = ROOT / '.research/workspace-check-12'
check = load(host / 'result.json')
before = load(host / 'before.json')
assert check['returncode'] == 0 and check['unchanged'] and not check['changed_paths']
assert before == load(host / 'after.json') and len(before) == check['source_files'] == 588
for relative, sha in before.items():
    assert digest(host / 'source' / relative) == sha, relative
selected_files = dict(identity['selected_graph']['shared_files'])
for package in identity['selected_graph']['packages']:
    selected_files.update(package['files'])
for relative, sha in selected_files.items():
    if relative in before:
        assert before[relative] == sha, ('host gate differs from selected graph', relative)
checks = (host / 'run.log').read_text()
assert digest(host / 'run.log') == check['log_sha256']
assert sum(map(int,re.findall(r'test result: ok\. (\d+) passed; 0 failed;',checks))) == 315
assert 'Ran 51 tests' in checks and '\nOK\n' in checks and 'exact arithmetic pass' in checks
assert 'Strict-safe workspace: 7 owned packages; no third-party packages' in checks

out = ROOT / 'research/qualification/shared-core-mac-arm64'
assert not out.exists(), 'Never overwrite a qualification archive'
jobs = {'k0':('corpus','slice','fixtures',12), 'header':('header-corpus','header-slice','header-fixtures',4),
        'zlib':('zlib-corpus','zlib-slice','zlib-fixtures',10), 'seek':('seek-corpus','seek-slice','seek-fixtures',8),
        'colour':('colour-corpus','colour-slice','colour-fixtures',33), 'catalogue':('catalogue-corpus','catalogue-slice','catalogue-fixtures',48)}
old = load(ROOT / 'research/qualification/segment-metadata-mac-arm64/identity.json')
old_archive = ROOT / 'research/qualification/segment-metadata-mac-arm64'
old_seal = load(old_archive / 'archive-sha256.json')
assert {str(p.relative_to(old_archive)) for p in old_archive.rglob('*') if p.is_file() and p.name != 'archive-sha256.json'} == set(old_seal)
for relative, sha in old_seal.items():assert digest(old_archive / relative) == sha, ('historical archive changed',relative)
for key in ('source_revision','configuration','reference_kind','reference_archives'):
    assert identity[key] == old[key], ('native FFmpeg baseline changed',key)
for manifest, contract, _, _ in jobs.values():
    for name in (manifest,contract):
        relative = 'harness/' + name + '.json'
        assert digest(ROOT / relative) == old['build_input_hashes'][relative], ('frozen contract/input manifest changed',relative)

external = load(Path(paths['external']))
contract = load(ROOT / 'harness/catalogue-slice.json')
spec, = contract['required_additional_vectors']
assert external['identity'] == identity and external['target'] == 'aarch64-apple-darwin'
assert external['contract_sha256'] == digest(ROOT / 'harness/catalogue-slice.json')
assert external['case']['id'] == external['case']['profile'] == spec['file']
assert external['case']['input_sha256'] == digest(ROOT / '.research/fate-samples' / spec['file']) == spec['sha256']
assert external['redistribution_qualified'] is False
assert acceptance({**contract,'required_profiles':[spec['file']],'required_additional_vectors':[]}, [external['case']], identity, external['target'], []) == external['acceptance']


def audit_case(row, contract_id, source=None):
    selected = validate_case_evidence(row, identity, contract_id)
    parity = row['parity']
    assert parity['status'] == parity['source_timing_status'] == 'pass'
    assert parity['sequential_required'] is True
    assert parity['short_reads'] == [{'chunk':n,'status':'pass'} for n in (1,7,4096)]
    assert parity['differences'] == parity['source_timing_differences'] == []
    timing, memory = row['timing'], row['memory']
    cert = memory['certificate']
    assert timing['worker_hashes'] == cert['worker_hashes'] == selected
    assert timing['input_sha256'] == memory['input_sha256'] == row['input_sha256']
    assert timing['pairs'] == len(timing['samples']) == memory['pairs'] == len(memory['samples']) == 30
    assert timing['iterations'] == 50 and 0 < timing['ratio']['upper_95'] <= .8
    assert [sample['pair'] for sample in timing['samples']] == list(range(30))
    assert all(sample[worker]['data']['iterations'] == 50 for sample in timing['samples'] for worker in ('candidate','reference'))
    rule = rule_for(row,identity,contract_id)
    seek = source.get('seek_target_ns') if source else None
    view = {'K4-colour-hdr-v1':'colour-source-and-ffmpeg9-v1',
            'K5-segment-metadata-v1':'catalogue-source-and-native-v1'}.get(contract_id,'source-frames-ffmpeg9-clock-v1')
    if rule:view = rule['view']
    assert row['operation'] == {'name':'container-demux-v1' if seek is None else 'container-seek-preroll-v1',
                                'seek_target_ns':seek,'view':view}
    assert all(math.isfinite(memory[key]) and 0 < memory[key] < 1 for key in ('heap_ratio_upper_95','rss_ratio_upper_95'))
    assert cert['probe_sha256'] == identity['instrumentation']['probe_sha256']
    assert cert['observer_sha256'] == identity['instrumentation']['observer_sha256']
    assert memory['cpu_before'] is not None and memory['cpu_after'] is not None


reports, assets, summary = {}, {}, {}
for name, (manifest, contract_name, folder, count) in jobs.items():
    report = load(Path(paths[name])); contract_path = ROOT / 'harness' / (contract_name + '.json')
    feature = load(contract_path); corpus = load(ROOT / 'harness' / (manifest + '.json'))
    assert report['identity'] == identity and report['collection_status'] == 'complete'
    assert report['target'] == 'aarch64-apple-darwin'
    assert report['contract_sha256'] == digest(contract_path)
    assert report['corpus_manifest_sha256'] == digest(ROOT / 'harness' / (manifest + '.json'))
    assert len(report['cases']) == len(corpus['cases']) == len(feature['required_profiles']) == count
    assert {r['profile'] for r in report['cases']} == set(feature['required_profiles'])
    assert len({r['id'] for r in report['cases']}) == count
    assert {r['id'] for r in report['cases']} == {r['id'] for r in corpus['cases']}
    assert (feature['runtime_ratio_max'],feature['memory_ratio_max_exclusive'],feature['minimum_pairs']) == (.8,1,30)
    extra = report.get('additional_cases', [])
    if name == 'catalogue': assert extra == [external['case']]
    verdict = acceptance(feature, report['cases'], identity, report['target'], report['fate_coverage'], extra)
    assert verdict == report['acceptance'] and verdict['slice_accepted'] and not verdict['accepted'], (name,verdict)
    authored = {row['id']:row for row in corpus['cases']}
    for row in report['cases']:
        source = authored[row['id']]; path = ROOT / '.research' / folder / source['file']
        assert source['rights'] == 'authored-synthetic' and digest(path) == source['sha256'] == row['input_sha256']
        audit_case(row, feature['id'], source)
        parity = row['parity']
        if name == 'zlib': assert parity['source_content_status'] == 'pass'
        if name == 'seek': assert len(parity['playback_seeks']) == 12 and all(v['status'] == 'pass' for v in parity['playback_seeks'])
        if name == 'colour': assert parity['source_colour_status'] == parity['colour_reference_status'] == 'pass' and parity['colour_short_reads'] == [{'chunk':n,'status':'pass'} for n in (1,7,4096)]
        if name == 'catalogue': assert parity['source_catalogue_status'] == parity['source_colour_status'] == 'pass'
        assets[name + '/' + source['file']] = {'source_path':str(path.relative_to(ROOT)), 'sha256':source['sha256'], 'rights':source['rights']}
    reports[name] = report
reports['external'] = external
audit_case(external['case'],contract['id'])
assert external['acceptance']['slice_accepted'] and not external['acceptance']['accepted']
assert external['case']['parity']['source_catalogue_status'] == external['case']['parity']['source_colour_status'] == 'pass'
assets['external/' + spec['file']] = {'source_path':'.research/fate-samples/' + spec['file'],'sha256':spec['sha256'],'rights':spec['rights']}
assert sum(len(r['cases']) if name != 'external' else 1 for name,r in reports.items()) == 116

# A new window may replace contamination, never a completed clean observation.
# Check the entire retained chain, including cases not revisited before timeout.
collection = Path(ledger['base'])
first_clean, first_sources, receipt_hashes = {}, {}, {}
window_receipts = []
for job in ledger['jobs']:
    windows = sorted(collection.glob(job['name']+'-window-*'),key=lambda p:int(p.name.rsplit('-',1)[1]))
    for window in windows:
        for name in ('partial.json','report.json'):
            path = window / name
            if path.exists():receipt_hashes[digest(path)] = str(path.relative_to(ROOT))
        path = window / 'report.json'
        if not path.exists():path = window / 'partial.json'
        if not path.exists():continue
        receipt = load(path)
        assert receipt['identity'] == identity, ('stale collection window',path)
        rows = [receipt['case']] if job['name']=='external' else list(measurement_cache(receipt).values())
        for row in rows:
            for field, clean_status in (('timing','measured'),('memory','qualified')):
                value = row.get(field,{})
                key = (job['name'],row['id'],field)
                if not value or value.get('status') == 'contaminated':continue
                assert value.get('status') == clean_status, ('terminal measurement failure cannot be replaced',key,path,value.get('status'))
                if key in first_clean:
                    assert value == first_clean[key], ('clean measurement replaced',key,path,first_sources[key])
                else:
                    first_clean[key] = value
                    first_sources[key] = str(path.relative_to(ROOT))
        window_receipts.append({'job':job['name'],'path':str(path.relative_to(ROOT)),'sha256':digest(path)})
for name, report in reports.items():
    rows = [report['case']] if name=='external' else report['cases']
    for row in rows:
        if 'reused_evidence_sha256' in row:
            assert row['reused_evidence_sha256'] in receipt_hashes, ('missing reused receipt',name,row['id'])
        for field in ('timing','memory'):
            assert row[field] == first_clean[(name,row['id'],field)]
collection_hashes = {str(p.relative_to(collection)):digest(p) for p in sorted(collection.rglob('*'))
                     if p.is_file() and p.suffix in ('.json','.log')}
camera_windows = []
for path in sorted(collection.glob('camera-window-*.json')):
    receipt = load(path)
    assert not receipt['paused'] or receipt['resumed'], ('camera not restored',path)
    camera_windows.append({'path':str(path.relative_to(ROOT)),'sha256':digest(path),'receipt':receipt})
assert camera_windows
collection_evidence = {'directory':str(collection.relative_to(ROOT)),'sha256':collection_hashes,
    'window_receipts':window_receipts,'first_clean_measurements':[{'job':key[0],'case':key[1],'measurement':key[2],
        'receipt':first_sources[key]} for key in sorted(first_sources)],'camera_windows':camera_windows,
    'clean_observations_replaced':False,'contaminated_and_incomplete_attempts_preserved':True}

# Verify primary inputs and upstream observations before writing any public receipt.
for source in load(ROOT / 'specs/sources.json')['sources']:
    assert digest(ROOT / source['cached_path']) == source['sha256']
fate_receipts = {}
reference_build = load(ROOT / '.research/reference/build.json')
reference_source = ROOT / '.research/reference' / ('FFmpeg-' + reference_build['revision'])
old_fate = {r['case']:r for r in load(ROOT / 'research/qualification/segment-metadata-mac-arm64/fate-evidence.json')}
fate_case_names = {'xiph':'fate-matroska-xiph-lacing','zlib':'fate-matroska-zlib-decompression',
                   'zero-length':'fate-matroska-zero-length-block'}
assert set(old_fate) == set(fate_case_names.values())
for label in ('xiph','zlib','zero-length'):
    receipt = load(ROOT / '.research/reference' / ('fate-' + label + '.json'))
    expected = old_fate[fate_case_names[label]]
    for key in ('case','sample','sample_sha256','origin','rights','revision','reference_build','upstream_ffmpeg_sha256'):
        assert receipt[key] == expected[key], ('FATE pin changed',label,key)
    assert receipt['revision'] == identity['source_revision'] == reference_build['revision']
    assert receipt['reference_build'] == reference_build
    assert receipt['sample_sha256'] == digest(ROOT / '.research/fate-samples' / receipt['sample'])
    assert receipt['upstream_ffmpeg_sha256'] == digest(reference_source / 'ffmpeg')
    assert receipt['exit_code'] == 0
    assert receipt['adapter_identity'] == identity and receipt['candidate_demux_comparison']['status'] == 'pass'
    assert receipt['upstream_case_executed'] == (label != 'zero-length')
    assert receipt['reference_status'] == ('not-run-demux-only' if label == 'zero-length' else 'pass')
    fate_receipts[label] = receipt

bundle = hashlib.sha256(json.dumps(assets,sort_keys=True).encode()).hexdigest()
for name, record in assets.items():
    dst = ROOT / '.research/frozen-corpora' / bundle / name
    assert dst.resolve().is_relative_to((ROOT / '.research/frozen-corpora' / bundle).resolve())
    dst.parent.mkdir(parents=True,exist_ok=True)
    if not dst.exists(): shutil.copy2(ROOT / record['source_path'],dst)
    assert digest(dst) == record['sha256']
    record['frozen_path'] = str(dst.relative_to(ROOT))
# build-harness records PATH's diagnostic ffprobe, not the pinned build's CLI.
# Preserve both with explicit roles; neither substitutes for native worker work.
ambient_ffprobe = Path(shutil.which('ffprobe'))
assert digest(ambient_ffprobe) == identity['ffprobe_sha256']
binaries = [('candidate',ROOT / 'target/release/media-worker'), *[(name,ROOT / 'target/harness' / name) for name in
    ('ffmpeg-worker','ffmpeg-combined-worker','allocations.dylib','memory-check')],
    ('upstream-fate-ffmpeg',reference_source / 'ffmpeg'),
    ('pinned-build-auxiliary-ffprobe',reference_source / 'ffprobe'),
    ('identity-bound-ambient-diagnostic-ffprobe',ambient_ffprobe)]
frozen_binaries = []
for role, path in binaries:
    sha = digest(path); dst = ROOT / '.research/frozen-workers' / sha / path.name
    dst.parent.mkdir(parents=True,exist_ok=True)
    if not dst.exists(): shutil.copy2(path,dst)
    assert digest(dst) == sha
    source_path = str(path.relative_to(ROOT)) if path.is_relative_to(ROOT) else str(path)
    frozen_binaries.append({'role':role,'source_path':source_path,'frozen_path':str(dst.relative_to(ROOT)),'sha256':sha})
for relative, sha in identity['reference_archives'].items():
    assert digest(ROOT / '.research/reference/install/lib' / relative) == sha

# No archive is created until every raw gate has been recomputed successfully.
out.mkdir(parents=True)
for name, report in reports.items():
    directory = out / name; directory.mkdir()
    shutil.copy2(paths[name], directory / 'report.json')
    raw = Path(paths[name]).parent
    hashes = {str(p.relative_to(raw)):digest(p) for p in sorted(raw.rglob('*.json')) if p.name not in ('partial.json','report.json')}
    (directory / 'raw-trace-evidence.json').write_text(json.dumps({'original_directory':str(raw.relative_to(ROOT)),'sha256':hashes,'external_payload_not_vendored':name == 'external'},indent=2)+'\n')
    rows = [report['case']] if name == 'external' else report['cases']
    summary[name] = {'cases':len(rows),'slice_accepted':True,'complete_module_accepted':False,
        'time_ratio_upper_95_max':max(r['timing']['ratio']['upper_95'] for r in rows),
        'live_heap_ratio_upper_95_max':max(r['memory']['heap_ratio_upper_95'] for r in rows),
        'attributable_rss_ratio_upper_95_max':max(r['memory']['rss_ratio_upper_95'] for r in rows)}
for label, receipt in fate_receipts.items():
    (out / ('fate-' + label + '.json')).write_text(json.dumps(receipt,indent=2)+'\n')
for name, value in [('identity.json',identity),('summary.json',summary),('input-evidence.json',assets),('host-gate.json',check),('binary-evidence.json',frozen_binaries)]:
    (out / name).write_text(json.dumps(value,indent=2)+'\n')
(out / 'collection-evidence.json').write_text(json.dumps(collection_evidence,indent=2)+'\n')
shutil.copy2(host / 'run.log',out / 'checks.log')
# The full host snapshot keeps the virtual workspace buildable. Only the three
# selected packages are numerically qualified; other format source stays bounded
# by its separate prototype evidence.
exported = dict(before)
source_manifest = {}
def export_source(source, relative, sha):
    # Historical Markdown is source data, not another live documentation tree.
    mapped = relative + '.txt' if relative.endswith('.md') else relative
    dst = out / 'source' / mapped
    dst.parent.mkdir(parents=True,exist_ok=True)
    shutil.copy2(source,dst)
    assert digest(dst) == sha
    source_manifest[relative] = {'path':str(dst.relative_to(out)),'sha256':sha}
for relative, sha in before.items():
    export_source(host / 'source' / relative,relative,sha)
for relative, sha in selected_files.items():
    if relative not in exported:
        assert digest(ROOT / relative) == sha
        export_source(ROOT / relative,relative,sha); exported[relative] = sha
(out / 'source-sha256.json').write_text(json.dumps(exported,indent=2)+'\n')
(out / 'source-manifest.json').write_text(json.dumps(source_manifest,indent=2)+'\n')
shutil.copy2(__file__,out / 'audit.py')
(out / 'qualification-state.json').write_text(json.dumps({'source_graph_sha256':identity['selected_graph']['sha256'],
    'frozen_selected_source':'.research/core-migration-parity-1793438243152823000/frozen',
    'frozen_whole_host_source':'.research/workspace-check-12/source','numeric_run':str(Path(ledger['base']).relative_to(ROOT)),
    'all_116_frozen_development_workloads_pass':True,'minimum_pairs':30,'runtime_upper_max':.8,'lower_heap_and_rss_required':True,
    'complete_modules_qualified':False,'streem_or_tv_modified':False,'committed_or_published':False},indent=2)+'\n')
print(json.dumps(summary,indent=2))
print('All116rawselectedgatesverified; archive remains unsealed until claim review/source export.')
