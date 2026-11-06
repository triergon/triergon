#!/usr/bin/env python3
"""Authored Matroska Colour/MasteringMetadata vectors, without downloaded media."""
from pathlib import Path
import struct
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from harness.common import load, write_json
from tools import generate_corpus as base
e, u = base.element, base.uint


def mastering(width=8, partial=False, values=None):
    values = values or [.68, .32, .265, .69, .15, .06, .3127, .329, 1000., .005]
    return e(0x55D0, b''.join(e(0x55D1+i, struct.pack('>f' if width == 4 else '>d', value))
                             for i, value in enumerate(values) if not partial or i in (0, 8)))


def profiles():
    pq = {0x55B1: 9, 0x55B2: 10, 0x55B9: 1, 0x55BA: 16, 0x55BB: 9}
    rows = [('colour-absent', None, b'', 1), ('colour-empty', {}, b'', 1),
            ('colour-sdr', {0x55B1:1,0x55B9:1,0x55BA:1,0x55BB:1}, b'',1),
            ('colour-rgb-full', {0x55B1:0,0x55B2:8,0x55B9:2,0x55BA:13,0x55BB:1}, b'',1),
            ('colour-hdr10', {**pq,0x55BC:1000,0x55BD:400},mastering(),1),
            ('colour-hdr32', pq, mastering(4),1),
            ('colour-hlg', {**pq,0x55BA:18},b'',1),
            ('colour-p3', {**pq,0x55BB:12},mastering(),1),
            ('colour-cll-only', {0x55BC:1000},b'',1),
            ('colour-fall-only', {0x55BD:400},b'',1),
            ('colour-light-zero', {0x55BC:0,0x55BD:0},b'',1),
            ('colour-master-empty', {},e(0x55D0,b''),1),
            ('colour-master-partial', pq,mastering(partial=True),1),
            ('colour-master-zero', {},mastering(values=[0.]*10),1),
            ('colour-master-boundary', pq,mastering(values=[1.,0.,.5,.25,.125,.0625,.5,.5,10000.,0.]),1),
            ('colour-subsampling', {0x55B2:12,0x55B3:1,0x55B4:1,0x55B5:0,0x55B6:0},b'',1),
            ('colour-many-tracks', {**pq,0x55BC:4000,0x55BD:600},mastering(),64)]
    rows += [(f'colour-siting-{h}-{v}', {0x55B7:h,0x55B8:v}, b'',1) for h in range(3) for v in range(3)]
    rows += [
        ('colour-empty-elements', {}, e(0x55B1,b'')+e(0x55BA,b'')+e(0x55BB,b'')+e(0x55D0,e(0x55D9,b'')),1),
        ('colour-registry-codes', {0x55B1:255,0x55BA:255,0x55BB:255,0x55B9:3},b'',1),
        ('colour-luminance-only', {}, e(0x55D0,e(0x55D9,struct.pack('>d',1000.))+e(0x55DA,b'')),1),
        ('colour-primaries-only', {}, e(0x55D0,b''.join(e(0x55D1+i,struct.pack('>d',.5)) for i in range(8))),1),
        ('colour-master-ones', {}, mastering(values=[1.]*8+[1000.,0.]),1),
        ('colour-master-tiny', {}, mastering(values=[1e-20]*8+[1000.,1e-20]),1),
        ('colour-master-half-minimum', {}, mastering(values=[.5/(2**31-1)]*8+[1000.,.5/(2**31-1)]),1),
    ]
    return rows


def make(fields, extra=b'', tracks=1):
    source = load(ROOT/'research/implementation/cases/avc-aac-sub/reference.json')['data']
    video = next(t for t in source['tracks'] if t['kind'] == 1)
    packet = bytes.fromhex(next(p['payload'] for p in source['packets'] if p['track'] == 0))
    colour = b'' if fields is None else e(0x55B0, b''.join(u(k,v) for k,v in fields.items()) + extra)
    entries = []; blocks = []
    for n in range(1, tracks+1):
        track = u(0xD7,n)+u(0x73C5,n)+u(0x83,1)+e(0x86,b'V_MPEG4/ISO/AVC')
        track += e(0x63A2,bytes.fromhex(video['private']))+e(0x22B59C,b'eng')+u(0x23E383,40_000_000)
        track += e(0xE0,u(0xB0,128)+u(0xBA,72)+colour)
        entries.append(e(0xAE,track))
        for at in range(4):
            block=base.vint(n)+int(at*40).to_bytes(2,'big',signed=True)+b'\x80'+packet
            blocks.append(e(0xA3,block))
    return base.EBML_HEADER+e(0x18538067,e(0x1549A966,u(0x2AD7B1,1_000_000))+e(0x1654AE6B,b''.join(entries))+e(0x1F43B675,u(0xE7,0)+b''.join(blocks)))


def main():
    out=ROOT/'.research/colour-fixtures';out.mkdir(parents=True,exist_ok=True);cases=[]
    for name,fields,extra,tracks in profiles():
        data=make(fields,extra,tracks);filename=name+'.mkv';(out/filename).write_bytes(data)
        cases.append(base.authored(name,filename,data))
    write_json(ROOT/'harness/colour-corpus.json', {'schema':1,'cases':cases})
    print('Generated',len(cases),'colour/HDR profiles')


if __name__=='__main__':main()
