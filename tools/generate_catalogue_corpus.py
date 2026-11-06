#!/usr/bin/env python3
"""Authored complete-document metadata fixtures, specified independently of Rust."""
import sys,struct,zlib
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1];sys.path.insert(0,str(ROOT))
from harness.common import write_json
from harness.spec_oracle import elements
from tools import generate_colour_corpus as base
from tools.generate_corpus import authored
e,u=base.e,base.u

def display(name,language=b'eng',ietf=None):
    return e(0x80,e(0x85,name)+e(0x437c,language)+(e(0x437d,ietf) if ietf is not None else b''))

def chapter(uid=1,start=0,end=1_000_000_000,extra=b''):
    return e(0xb6,u(0x73c4,uid)+u(0x91,start)+(u(0x92,end) if end is not None else b'')+extra)

def chapters(payload=None,ordered=False):
    return e(0x1043a770,e(0x45b9,u(0x45bc,1)+u(0x45db,1)+u(0x45dd,int(ordered))+(payload if payload is not None else chapter(extra=display(b'Opening')))))

def attachment(uid=1,data=b'font-data',name=b'demo.ttf',mime=b'application/x-truetype-font'):
    return e(0x61a7,u(0x46ae,uid)+e(0x466e,name)+e(0x4660,mime)+e(0x467e,b'Authored fixture')+e(0x465c,data))

def simple(name=b'TITLE',value=b'Test',extra=b'',binary=False):
    return e(0x67c8,e(0x45a3,name)+(e(0x4485 if binary else 0x4487,value) if value is not None else b'')+extra)

def tags(payload=None,targets=b''):
    return e(0x1254c367,e(0x7373,e(0x63c0,targets)+(payload if payload is not None else simple())))

def profiles():
    attach=lambda data=b'font-data':e(0x1941a469,attachment(data=data))
    two=chapter(1,0,1_000_000_000,display(b'Opening'))+chapter(2,1_000_000_000,2_000_000_000,display(b'Finish'))
    nested=chapter(1,0,None,display(b'Part')+chapter(2,0,1_000_000_000,display(b'Scene')))
    chain=chapter(16,0,1_000_000_000,display(b'Leaf'))
    for uid in range(15,0,-1):chain=chapter(uid,0,None,chain)
    rows = [
      ('catalogue-empty',[],b'',False),
      ('chapters-simple',[chapters(two)],b'',False),
      ('chapters-nested',[chapters(nested)],b'',False),
      ('chapters-multilingual',[chapters(chapter(extra=display(b'English')+display('Français'.encode(),b'fre',b'fr-CA')))],b'',False),
      ('chapters-ordered',[chapters(two,True)],b'',False),
      ('chapters-ordered-parent',[chapters(nested,True)],b'',False),
      ('chapters-editions',[e(0x1043a770,e(0x45b9,u(0x45bc,1)+chapter(1,extra=display(b'A')))+e(0x45b9,u(0x45bc,2)+u(0x45db,1)+chapter(2,extra=display(b'B'))))],b'',False),
      ('chapters-missing-end',[chapters(chapter(1,0,None,display(b'First'))+chapter(2,1_000_000_000,None,display(b'Last')))],b'',False),
      ('chapters-empty-display',[chapters(chapter(extra=display(b'')))],b'',False),
      ('chapters-many',[chapters(b''.join(chapter(n+1,n*1000,(n+1)*1000,display(str(n).encode())) for n in range(512)))],b'',False),
      ('chapters-depth',[chapters(chain)],b'',False),
      ('tags-global',[tags(simple(b'TITLE',b'Example')+simple(b'ARTIST',b'Creator'))],b'',False),
      ('tags-track',[tags(targets=u(0x63c5,1))],b'',False),
      ('tags-chapter',[chapters(),tags(targets=u(0x63c4,1))],b'',False),
      ('tags-attachment',[attach(),tags(targets=u(0x63c6,1))],b'',False),
      ('tags-edition',[chapters(),tags(targets=u(0x63c9,1))],b'',False),
      ('tags-nested',[tags(simple(b'GROUP',None,simple(b'NAME',b'Nested')))],b'',False),
      ('tags-multilingual',[tags(simple(b'TITLE',b'English',e(0x447a,b'eng'))+simple(b'TITLE','Français'.encode(),e(0x447a,b'fre')+e(0x447b,b'fr-CA')+u(0x4484,0)))],b'',False),
      ('tags-binary',[tags(simple(b'BINARY',bytes(range(256)),binary=True))],b'',False),
      ('tags-repeated',[tags(simple(b'TITLE',b'First')+simple(b'TITLE',b'Last'))],b'',False),
      ('tags-future',[tags(simple(b'X_NEW_FIELD',b'opaque')+simple(b'EMPTY',b''))],b'',False),
      ('attachments-empty',[attach(b'')],b'',False),
      ('attachments-font',[attach()],b'',False),
      ('attachments-unknown',[e(0x1941a469,attachment(name=b'note.bin',mime=b'application/octet-stream'))],b'',False),
      ('attachments-large',[attach(bytes(range(256))*4096)],b'',False),
      ('attachments-many',[e(0x1941a469,b''.join(attachment(uid=n+1,name=f'font{n}.ttf'.encode()) for n in range(32)))],b'',False),
      ('metadata-combined',[chapters(two),attach(),tags(simple(b'TITLE',b'Movie')),tags(simple(b'TITLE',b'Track'),u(0x63c5,1))],e(0x22b59d,b'en-GB')+u(0x55ab,1),False),
      ('metadata-late',[chapters(two),attach(),tags()],b'',True),
      ('track-flags',[],u(0xb9,1)+u(0x88,0)+u(0x55aa,1)+u(0x55ab,1)+u(0x55ac,1)+u(0x55ad,1)+u(0x55ae,1)+u(0x55af,1),False),
      ('track-disabled',[],u(0xb9,0),False),
      ('track-ietf-before',[],e(0x22b59d,b'fr-CA')+e(0x22b59c,b'fre'),False),
      ('track-ietf-after',[],e(0x22b59c,b'fre')+e(0x22b59d,b'fr-CA'),False),
    ]

    def chunk(kind,data):return struct.pack('>I',len(data))+kind+data+struct.pack('>I',zlib.crc32(kind+data))
    png=b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>IIBBBBB',1,1,8,6,0,0,0))+chunk(b'IDAT',zlib.compress(b'\0\xff\0\0\xff'))+chunk(b'IEND',b'')
    rows += [('attachments-image',[e(0x1941a469,attachment(data=png,name=b'cover.png',mime=b'image/png'))],b'',False),('attachments-large-late',[attach(bytes(range(256))*4096)],b'',True)]
    rows += [('info-title',[],b'',False,e(0x7ba9,b'Authored title')+e(0x4d80,b'Muxer')+e(0x5741,b'Writer'))]
    for name,date in [('epoch',0),('negative',-1001),('positive',1001),('leap',730_944_000_123456789),('earliest',-(1<<63)),('latest',(1<<63)-1)]:
        rows.append(('info-date-'+name,[],b'',False,e(0x4461,date.to_bytes(8,'big',signed=True))))
    rows += [
        ('tags-multi-target',[chapters(two),tags(simple(b'DESCRIPTION',b'Selected chapters'),u(0x63c4,1)+u(0x63c4,2))],b'',False),
        ('tags-wildcard',[tags(targets=u(0x63c5,0))],b'',False),
        ('tags-cross-target',[chapters(two),attach(),tags(simple(b'DESCRIPTION',b'Multi-kind'),u(0x63c5,1)+u(0x63c4,2)+u(0x63c6,1))],b'',False),
        ('track-original-false',[],u(0x55ae,0),False),
    ]
    rows += [
        ('tags-many',[tags(b''.join(simple(f'X_FIELD_{i:04}'.encode(),f'Value {i}'.encode()) for i in range(512)))],b'',False),
        ('chapters-unsorted',[chapters(chapter(1,1_000_000_000,2_000_000_000,display(b'Later'))+chapter(2,0,1_000_000_000,display(b'Earlier')))],b'',False),
    ]
    rows += [('info-duration',[],b'',False,e(0x4489,struct.pack('>d',160.)))]
    return rows

def make(metadata,extra=b'',late=False,info=b''):
    data=base.make({0x55b1:9,0x55ba:16,0x55bb:9},base.mastering())
    header_end=next(b for k,a,b,_ in elements(data,0,len(data)) if k==0x1a45dfa3)
    a,b=next((a,b) for k,a,b,_ in elements(data,0,len(data)) if k==0x18538067)
    pieces=[]
    for k,x,y,_ in elements(data,a,b):
        payload=data[x:y]
        if k==0x1549a966:payload+=info
        if k==0x1654ae6b:
            _,p,q,_=next(elements(data,x,y));track=data[p:q]
            if extra:
                track=b''.join(e(fid,data[s:t]) for fid,s,t,_ in elements(data,p,q) if fid not in (0x22b59c,0x22b59d))+extra
            payload=e(0xae,track)
        pieces.append((k,e(k,payload)))
    index=3 if late else 2
    all_parts=pieces[:index]+[(int.from_bytes(item[:4],'big'),item) for item in metadata]+pieces[index:]
    # Fixed-width relative offsets keep construction deterministic and support late metadata.
    def head(positions):return e(0x114d9b74,b''.join(e(0x4dbb,e(0x53ab,k.to_bytes(4,'big'))+e(0x53ac,pos.to_bytes(8,'big'))) for k,pos in positions))
    initial=head([(k,0) for k,_ in all_parts if k!=0x1f43b675]);positions=[];offset=len(initial)
    for k,raw in all_parts:
        if k!=0x1f43b675:positions.append((k,offset))
        offset+=len(raw)
    seek=head(positions);assert len(seek)==len(initial)
    return data[:header_end]+e(0x18538067,seek+b''.join(raw for _,raw in all_parts))

def main():
    output=ROOT/'.research/catalogue-fixtures';output.mkdir(parents=True,exist_ok=True);rows=[]
    for name,*args in profiles():
        data=make(*args);(output/(name+'.mkv')).write_bytes(data)
        rows.append(authored(name,name+'.mkv',data))
    write_json(ROOT/'harness/catalogue-corpus.json', {'schema':1,'cases':rows})
    print('Generated',len(rows),'catalogue profiles')

if __name__=='__main__':main()
