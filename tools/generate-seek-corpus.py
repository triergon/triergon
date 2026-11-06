#!/usr/bin/env python3
"""Authored Cues/SeekHead/preroll workload, codec bytes from our synthetic signals."""
import importlib.util,struct,json,hashlib
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('base',ROOT/'tools/generate-corpus.py');base=importlib.util.module_from_spec(spec);spec.loader.exec_module(base)
e,u=base.element,base.uint

def make(preroll=0,delay=0,relative=False,missing=False,sparse=False,unknown=False,front=False):
    own=json.loads((ROOT/'research/implementation/cases/avc-aac-sub/reference.json').read_text())['data']
    packet=bytes.fromhex(next(p['payload'] for p in own['packets'] if p['track']==0 and p['key']))
    private=bytes.fromhex(own['tracks'][0]['private'])
    video=u(0xd7,1)+u(0x73c5,1)+u(0x83,1)+e(0x86,b'V_MPEG4/ISO/AVC')+e(0x63a2,private)+u(0x23e383,500_000_000)+e(0xe0,u(0xb0,160)+u(0xba,90))
    audio=u(0xd7,2)+u(0x73c5,2)+u(0x83,2)+e(0x86,b'A_PCM/INT/LIT')+u(0x23e383,250_000_000)+u(0x56bb,preroll)+u(0x56aa,delay)+e(0xe1,u(0x9f,1)+u(0x6264,16)+e(0xb5,struct.pack('>d',8000)))
    tracks=e(0x1654ae6b,e(0xae,video)+e(0xae,audio));info=e(0x1549a966,u(0x2ad7b1,1_000_000))
    def block(track,offset,payload,key=True):return e(0xa3,bytes([0x80+track])+struct.pack('>h',offset)+bytes([0x80 if key else 0])+payload)
    clusters=[];relative_positions=[]
    for time in range(0,16000,1000):
        timestamp=u(0xe7,time);audio_first=block(2,0,bytes(range(32)))
        body=timestamp+audio_first+block(1,0,packet)+block(2,250,bytes(range(32)))+block(1,500,packet,False)+block(2,500,bytes(range(32)))+block(2,750,bytes(range(32)))
        relative_positions.append(len(timestamp)+len(audio_first))
        clusters.append((b'\x1f\x43\xb6\x75\xff'+body) if unknown else e(0x1f43b675,body))
    head=lambda position:e(0x114d9b74,e(0x4dbb,e(0x53ab,b'\x1c\x53\xbb\x6b')+e(0x53ac,position.to_bytes(8,'big'))))
    metadata=info+tracks+(b'' if missing or front else head(0))
    def cues(offset):
        entries=b'';position=offset
        for i,cluster in enumerate(clusters):
            if not sparse or i%4==0:
                fields=u(0xf7,1)+e(0xf1,position.to_bytes(8,'big'))
                if relative:fields+=u(0xf0,relative_positions[i])+u(0x5378,2)
                entries+=e(0xbb,u(0xb3,i*1000)+e(0xb7,fields))
            position+=len(cluster)
        return e(0x1c53bb6b,entries)
    if missing:body=metadata+b''.join(clusters)
    elif front:
        first=cues(len(metadata));index=cues(len(metadata)+len(first));assert len(first)==len(index)
        body=metadata+index+b''.join(clusters)
    else:body=info+tracks+head(len(metadata)+sum(map(len,clusters)))+b''.join(clusters)+cues(len(metadata))
    header=e(0x1a45dfa3,u(0x4286,1)+u(0x42f7,1)+u(0x42f2,4)+u(0x42f3,8)+e(0x4282,b'matroska')+u(0x4287,4)+u(0x4285,2))
    return header+e(0x18538067,body)

def main():
    out=ROOT/'.research/seek-fixtures';out.mkdir(parents=True,exist_ok=True);rows=[]
    cases=[('cues-end',{}),('cues-relative',dict(relative=True)),('cues-front',dict(front=True)),('cues-preroll',dict(preroll=3_000_000_000)),('cues-delay-preroll',dict(preroll=80_000_000,delay=50_000_000)),('cues-sparse',dict(sparse=True,preroll=3_000_000_000)),('cues-unknown-cluster',dict(unknown=True,preroll=3_000_000_000)),('cues-missing',dict(missing=True,preroll=3_000_000_000))]
    for name,options in cases:
        data=make(**options);file=name+'.mkv';(out/file).write_bytes(data)
        rows.append({'id':name,'profile':name,'file':file,'sha256':hashlib.sha256(data).hexdigest(),'rights':'authored-synthetic','seek_target_ns':7_500_000_000})
    (ROOT/'harness/seek-corpus.json').write_text(json.dumps({'schema':1,'cases':rows},indent=2)+'\n')
    contract={'schema':1,'id':'K3-cues-preroll-v1','operation':'container-seek-preroll-v1','contract':['bounded Cues/SeekHead lookup','validated CueRelativePosition and CueBlockNumber','sparse and absent index fallback','all-track SeekPreRoll and codec delay','all packets after seek including decode-only preroll','fresh, repeated and after-EOF seeks','unknown Cluster lengths','seek cancellation, I/O failures and malformed indexes'],'required_profiles':[r['profile'] for r in rows],'required_targets':['aarch64-unknown-linux-gnu'],'runtime_ratio_max':1.2,'memory_ratio_max_exclusive':1.0,'minimum_pairs':30,'confidence':0.95,'pending_module_requirements':['remaining M01/M02 features','complete applicable FATE coverage','deferred targets'],'runtime_optimization_ratio_target':0.8,'runtime_policy':'initial-delivery-runtime-v2'}
    (ROOT/'harness/seek-slice.json').write_text(json.dumps(contract,indent=2)+'\n');print('Generated',len(rows),'seek cases')
if __name__=='__main__':main()
