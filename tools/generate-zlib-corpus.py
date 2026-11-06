#!/usr/bin/env python3
"""Authored Matroska content-compression matrix, Python zlib is an external oracle."""
import hashlib, importlib.util, json, random, struct, zlib
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('base',ROOT/'tools/generate-corpus.py')
base=importlib.util.module_from_spec(spec);spec.loader.exec_module(base)
e,u=base.element,base.uint

def compress(data,kind):
    c=zlib.compressobj(0 if kind=='stored' else 6,zlib.DEFLATED,15,8,zlib.Z_FIXED if kind=='fixed' else zlib.Z_DEFAULT_STRATEGY)
    return c.compress(data)+c.flush()

def make(kind='dynamic',scope=1,lace=0,aac=False,mixed=False,empty=False):
    rng=random.Random(1951)
    payload=b'' if empty else bytes(rng.randrange(256) for _ in range(32768)) if kind=='stored' else (b'bounded zlib track, signed timestamps, cues and decoder preroll. '+bytes(range(32)))*256
    if aac:
        source=json.loads((ROOT/'research/implementation/cases/avc-aac-sub/reference.json').read_text())['data']
        payload=bytes.fromhex(next(p['payload'] for p in source['packets'] if p['track']==1))
    private=b'\x11\x88\x56\xe5\x00' if aac else b'abcd'
    header=payload[:4] if mixed else b''
    encoded=compress(payload[len(header):],kind) if scope&1 else payload
    private=compress(private,kind) if scope&2 else private
    entries=e(0x6240,u(0x5031,1 if mixed else 0)+u(0x5032,scope)+e(0x5034,u(0x4254,0)))
    if mixed: entries+=e(0x6240,u(0x5031,0)+u(0x5032,1)+e(0x5034,u(0x4254,3)+e(0x4255,header)))
    track=u(0xd7,1)+u(0x73c5,1)+u(0x83,2)+e(0x86,b'A_AAC' if aac else b'A_PCM/INT/LIT')+e(0x63a2,private)
    track+=u(0x23e383,1_000_000)+e(0xe1,u(0x9f,1)+u(0x6264,16)+e(0xb5,struct.pack('>d',48000 if aac else 8000)))+e(0x6d80,entries)
    # Two equal-size frames, testing each legal lacing scheme independently.
    if lace==1:
        size=len(encoded);sizes=bytes([255])*(size//255)+bytes([size%255]);body=b'\x01'+sizes+encoded*2
    elif lace==2:body=b'\x01'+encoded*2
    elif lace==3:body=b'\x01'+base.vint(len(encoded))+encoded*2
    else:body=encoded
    block=b'\x81\x00\x00'+bytes([0x80|lace*2])+body
    header=e(0x1a45dfa3,u(0x4286,1)+u(0x42f7,1)+u(0x42f2,4)+u(0x42f3,8)+e(0x4282,b'matroska')+u(0x4287,4)+u(0x4285,2))
    return header+e(0x18538067,e(0x1549a966,u(0x2ad7b1,1_000_000))+e(0x1654ae6b,e(0xae,track))+e(0x1f43b675,u(0xe7,0)+e(0xa3,block)))

def main():
    out=ROOT/'.research/zlib-fixtures';out.mkdir(parents=True,exist_ok=True);rows=[]
    matrix=[('zlib-stored',dict(kind='stored')),('zlib-fixed',dict(kind='fixed')),('zlib-dynamic',{}),('zlib-empty',dict(empty=True)),('zlib-private',dict(scope=2,aac=True)),('zlib-both',dict(scope=3)),('zlib-mixed',dict(mixed=True))]
    matrix += [('zlib-'+name,dict(lace=mode)) for name,mode in [('xiph',1),('fixed-lace',2),('ebml',3)]]
    for name,options in matrix:
        data=make(**options);file=name+'.mkv';(out/file).write_bytes(data)
        rows.append({'id':name,'profile':name,'file':file,'sha256':hashlib.sha256(data).hexdigest(),'rights':'authored-synthetic'})
    (ROOT/'harness/zlib-corpus.json').write_text(json.dumps({'schema':1,'cases':rows},indent=2)+'\n')
    contract={'schema':1,'id':'K2-bounded-zlib-v1','operation':'container-demux-v1','contract':['RFC 1950 checksum and wrapper','RFC 1951 stored/fixed/dynamic blocks','ContentEncoding scopes 1/2/3','all lacing schemes','ordered mixed header/zlib encodings','exact source and FFmpeg packet times','bounded decoded output, expansion ratio, work and cancellation'],'required_profiles':[r['profile'] for r in rows],'required_targets':['aarch64-unknown-linux-gnu'],'runtime_ratio_max':1.2,'memory_ratio_max_exclusive':1.0,'minimum_pairs':30,'confidence':0.95,'pending_module_requirements':['remaining M01/M02 features','complete applicable FATE coverage','deferred targets'],'runtime_optimization_ratio_target':0.8,'runtime_policy':'initial-delivery-runtime-v2'}
    (ROOT/'harness/zlib-slice.json').write_text(json.dumps(contract,indent=2)+'\n')
    print('Generated',len(rows),'zlib cases')
if __name__=='__main__':main()
