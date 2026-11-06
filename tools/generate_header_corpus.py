#!/usr/bin/env python3
"""Original container-level header-removal fixtures; no downloaded media."""
from pathlib import Path
import struct
import sys

ROOT=Path(__file__).resolve().parents[1];sys.path.insert(0,str(ROOT))
from harness.common import load, write_json
from tools import generate_corpus as base
e,u=base.element,base.uint


def make(scope,lace=False,aac=False):
    header=b"\x11\x88" if aac else b"\x55\xaa"
    raw_private=b"\x56\xe5\x00" if aac else b"cd" if scope&2 else b""
    encoding=e(0x6d80,e(0x6240,u(0x5032,scope)+e(0x5034,u(0x4254,3)+e(0x4255,header))))
    track=u(0xd7,1)+u(0x73c5,1)+u(0x83,2)+e(0x86,b"A_AAC" if aac else b"A_PCM/INT/LIT")
    track+=e(0x63a2,raw_private)+u(0x23e383,1_000_000)+e(0xe1,u(0x9f,1)+u(0x6264,16)+e(0xb5,struct.pack('>d',48000 if aac else 8000)))+encoding
    if aac:
        # Reuse one packet from our own synthetic tone, preserving its bytes.
        source=load(ROOT/'research/implementation/cases/avc-aac-sub/reference.json')['data']
        data=bytes.fromhex(next(p['payload'] for p in source['packets'] if p['track']==1))
        block=b'\x81\x00\x00\x80'+data
    elif lace:
        block=b'\x81\x00\x00\x84\x01\x00\x01\x02\x03'
    else:
        block=b'\x81\x00\x00\x80'
    return base.EBML_HEADER+e(0x18538067,e(0x1549a966,u(0x2ad7b1,1_000_000))+e(0x1654ae6b,e(0xae,track))+e(0x1f43b675,u(0xe7,0)+e(0xa3,block)))


def main():
    output=ROOT/'.research/header-fixtures';output.mkdir(parents=True,exist_ok=True)
    cases=[]
    for name,scope,lace,aac in [('header-empty',1,False,False),('header-lace',1,True,False),('header-private',2,False,True),('header-both',3,False,False)]:
        data=make(scope,lace,aac);file=name+'.mkv';(output/file).write_bytes(data)
        cases.append(base.authored(name,file,data))
    write_json(ROOT/'harness/header-corpus.json', {'schema':1,'cases':cases})
    print('Generated',len(cases),'header-removal cases')


if __name__=='__main__':main()
