"""Build/evaluate an isolated proposal; never changes official acceptance."""
import hashlib,importlib.util,json,pathlib,subprocess,tempfile,sys
root=pathlib.Path(__file__).resolve().parents[2];sys.path.insert(0,str(root))
from harness.spec_oracle import elements
where=pathlib.Path(__file__).resolve().parent
source=(root/'harness/reference.c').read_text().replace('#include <libavformat/avformat.h>','#include <libavformat/avformat.h>\n#include "spec-encoding.h"')
source=source.replace('    int result=0;Source source={data,len,0};','    int result=0;Source source={data,len,0};\n    SpecPlan inline_plan;SpecPlan *plans=NULL;')
source=source.replace('    if(seek_ns!=INT64_MIN){','    if(f->nb_streams==1)plans=&inline_plan;\n    else if(f->nb_streams && f->nb_streams<=64)plans=calloc(f->nb_streams,sizeof(*plans));\n    if(!plans||spec_plans(data,len,plans,f->nb_streams)){result=AVERROR(EINVAL);goto done;}\n    if(seek_ns!=INT64_MIN){',1)
source=source.replace('    while((result=av_read_frame(f,p))>=0){','    while((result=av_read_frame(f,p))>=0){\n        if(p->stream_index<0||(unsigned)p->stream_index>=f->nb_streams||spec_restore(&plans[p->stream_index],p)){result=AVERROR(EINVAL);goto done;}')
source=source.replace('done:\n    av_packet_free(&p);','done:\n    if(plans && plans!=&inline_plan)free(plans);\n    av_packet_free(&p);')
p=where/'spec-reference.c';p.write_text(source);identity=json.loads((root/'target/harness/identity.json').read_text());cmd=[str(p) if arg==str(root/'harness/reference.c') else arg for arg in identity['compiler_command']];cmd[-1]=str(where/'spec-reference');subprocess.run(cmd,check=True)
(where/'spec-reference-build.json').write_text(json.dumps({'command':cmd,'status':0},indent=2)+'\n')
spec=importlib.util.spec_from_file_location('fixture',root/'tools/generate-zlib-corpus.py');fixture=importlib.util.module_from_spec(spec);spec.loader.exec_module(fixture)
rows=[]
for kind in ['stored','fixed','dynamic']:
 for lace in range(4):
  for empty in [False,True]:
   original=fixture.make(kind=kind,lace=lace,empty=empty,mixed=True)
   _,a,b,_=next(e for e in elements(original,0,len(original)) if e[0]==0x18538067)
   _,a,b,_=next(e for e in elements(original,a,b) if e[0]==0x1654ae6b)
   _,a,b,_=next(e for e in elements(original,a,b) if e[0]==0xae)
   _,a,b,_=next(e for e in elements(original,a,b) if e[0]==0x6d80)
   entries=[fixture.e(tag,original[x:y]) for tag,x,y,_ in elements(original,a,b)]
   for reverse in [False,True]:
    data=original.replace(fixture.e(0x6d80,b''.join(entries)),fixture.e(0x6d80,b''.join(reversed(entries))) if reverse else fixture.e(0x6d80,b''.join(entries)))
    assert len(data)==len(original)
    with tempfile.TemporaryDirectory() as tmp:
     path=pathlib.Path(tmp)/'mixed.mkv';path.write_bytes(data)
     native=subprocess.run([where/'spec-reference','trace',path],capture_output=True)
     rust=subprocess.run([root/'target/release/media-worker','trace',path],capture_output=True)
     same=native.returncode==rust.returncode==0 and json.loads(native.stdout)==json.loads(rust.stdout)
     rows.append({'kind':kind,'lace':lace,'empty':empty,'reverse_storage':reverse,'input_sha256':hashlib.sha256(data).hexdigest(),'pass':same,'reference_status':native.returncode,'candidate_status':rust.returncode})
receipt={'status':'prototype-only-not-accepted-baseline','canonical_identity':identity,'prototype_sources':{p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in [where/'spec-encoding.h',where/'spec-reference.c',pathlib.Path(__file__)]},'prototype_binary_sha256':hashlib.sha256((where/'spec-reference').read_bytes()).hexdigest(),'cases':rows}
(where/'spec-reference-evaluation.json').write_text(json.dumps(receipt,indent=2)+'\n')
print('Full source-view matches',sum(x['pass'] for x in rows),'/',len(rows))
if not all(x['pass'] for x in rows):raise SystemExit(1)
