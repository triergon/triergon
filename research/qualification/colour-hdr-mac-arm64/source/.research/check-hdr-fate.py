"""Extra demux/source comparison of an upstream HDR vector; no full FATE claim."""
import pathlib,urllib.request,json,sys,hashlib
root=pathlib.Path(__file__).resolve().parents[1];sys.path.insert(0,str(root))
from harness.runner import colour_parity,load,digest
p=root/'.research/fate-samples/mkv/hdr10tags-both.mkv';p.parent.mkdir(parents=True,exist_ok=True)
if not p.exists():
 request=urllib.request.Request('https://fate-suite.ffmpeg.org/mkv/hdr10tags-both.mkv',headers={'User-Agent':'safe-media-local-fate-check'})
 with urllib.request.urlopen(request,timeout=30) as response:data=response.read(8*1024*1024+1)
 if len(data)>8*1024*1024:raise ValueError('FATE input size bound')
 p.write_bytes(data)
identity=load(root/'target/harness/identity.json')
assert digest(root/'target/release/media-worker')==identity['binaries']['candidate']
assert digest(root/'target/harness/ffmpeg-worker')==identity['binaries']['reference']
raw=root/'.research/fate-colour-hdr';result=colour_parity(root/'target/release/media-worker',root/'target/harness/ffmpeg-worker',p,raw)
receipt={'sample':'mkv/hdr10tags-both.mkv','sample_sha256':digest(p),'origin':'https://fate-suite.ffmpeg.org/mkv/hdr10tags-both.mkv','rights':'external public vector; redistribution unreviewed','related_upstream_cases':['fate-matroska-side-data-pref-codec','fate-matroska-side-data-pref-packet'],'upstream_case_executed':False,'operation':'no-parser container packet/static-colour demux and independent source proof; no decoded-frame side-data priority claim','result':result,'adapter_identity':identity,'script_sha256':digest(pathlib.Path(__file__)),'raw_trace_sha256':{x.name:digest(x) for x in sorted(raw.glob('*.json'))}}
(root/'.research/fate-colour-hdr.json').write_text(json.dumps(receipt,indent=2)+'\n')
print(result['status'],result.get('packets'),result.get('colour_reference_differences'),result.get('source_colour_differences'))
raise SystemExit(0 if result['status']=='pass' else 1)
