"""Independent exact-source document/track oracle from RFC 9559 scalar rules."""
import struct
from .spec_oracle import elements,integer,variable
MASTER={0x1549a966,0x1941a469,0x1043a770,0x1254c367,0x61a7,0x45b9,0xb6,0x80,0x8f,0x6944,0x6911,0x7373,0x63c0,0x67c8}
UINT={0x2ad7b1,0x46ae,0x73c4,0x45bc,0x45bd,0x45db,0x45dd,0x91,0x92,0x98,0x4598,0x6ebc,0x63c3,0x89,0x6955,0x6922,0x68ca,0x63c5,0x63c9,0x63c4,0x63c6,0x4484}
TEXT={0x7ba9,0x4d80,0x5741,0x467e,0x466e,0x5654,0x85,0x45a3,0x4487,0x4660,0x437c,0x437d,0x437e,0x63ca,0x447a,0x447b}

def document(data,kind,source_offset):
    nodes=[]
    def walk(kind,start,stop,parent,depth):
        if depth>64 or len(nodes)>=16384:raise ValueError('oracle metadata bounds')
        n=len(nodes);uint=None;text=None
        if kind in UINT:uint=integer(data,start,stop) if start!=stop else {0x2ad7b1:1000000,0x4598:1,0x4484:1,0x68ca:50}.get(kind,0)
        if kind in TEXT:text=data[start:stop].rstrip(b'\0').decode('utf-8') if start!=stop else {0x437c:'eng',0x447a:'und'}.get(kind,'')
        node={'id':kind,'range':[start,stop],'parent':parent,'next':None,'uint':uint,'text':text,'signed':integer(data,start,stop,True) if kind==0x4461 else None,'float_bits':struct.pack('>d',0. if start==stop else struct.unpack('>f' if stop-start==4 else '>d',data[start:stop])[0]).hex() if kind==0x4489 else None};nodes.append(node)
        if kind in MASTER:
            for key,a,b,unknown in elements(data,start,stop):
                if unknown:raise ValueError('unknown metadata child size')
                walk(key,a,b,n,depth+1)
        node['next']=len(nodes)
    walk(kind,0,len(data),None,0)
    return {'kind':kind,'source_offset':source_offset,'raw':data.hex(),'nodes':nodes}

def source_catalogue(path):
    if path.stat().st_size>64*1024*1024:raise ValueError('oracle file bound')
    data=path.read_bytes();documents=[];tracks=[]
    segment=next((a,b) for k,a,b,_ in elements(data,0,len(data)) if k==0x18538067)
    for kind,a,b,_ in elements(data,*segment):
        if kind in (0x1549a966,0x1941a469,0x1043a770,0x1254c367):documents.append(document(data[a:b],kind,a))
        if kind!=0x1654ae6b:continue
        for key,x,y,_ in elements(data,a,b):
            if key!=0xae:continue
            fields={k:(p,q) for k,p,q,_ in elements(data,x,y)}
            legacy='eng' if 0x22b59c not in fields else data[slice(*fields[0x22b59c])].rstrip(b'\0').decode() or 'eng'
            ietf=None if 0x22b59d not in fields else data[slice(*fields[0x22b59d])].rstrip(b'\0').decode()
            flags=[]
            for fid,default in [(0xb9,True),(0x88,True),(0x55aa,False),(0x9c,True),(0x55ab,None),(0x55ac,None),(0x55ad,None),(0x55ae,None),(0x55af,None)]:
                if fid not in fields:flags.append(default)
                else:
                    p,q=fields[fid];flags.append(bool(integer(data,p,q)) if p!=q else bool(default))
            tracks.append({'raw_track':data[x:y].hex(),'number':integer(data,*fields[0xd7]),'uid':integer(data,*fields[0x73c5]),'name':data[slice(*fields[0x536e])].rstrip(b'\0').decode() if 0x536e in fields else '', 'legacy_language':legacy,'bcp47':ietf,'effective_language':ietf if ietf is not None else legacy,'flags':flags})
    return {'schema':1,'documents':documents,'tracks':tracks}
