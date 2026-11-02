"""Independent EBML traversal of exact source Colour values, not FFmpeg output."""
import struct
from .spec_oracle import elements, integer


def source_colour(path):
    data=path.read_bytes();result=[]
    segment=next((a,b) for k,a,b,_ in elements(data,0,len(data)) if k==0x18538067)
    for key,a,b,_ in elements(data,*segment):
        if key!=0x1654AE6B:continue
        for key,x,y,_ in elements(data,a,b):
            if key!=0xAE:continue
            colour=None
            for key,v,w,_ in elements(data,x,y):
                if key!=0xE0:continue
                for key,i,j,_ in elements(data,v,w):
                    if key!=0x55B0:continue
                    fields=[2,0,None,None,None,None,0,0,0,2,2,None,None];master=None
                    for key,c,d,_ in elements(data,i,j):
                        if 0x55B1<=key<=0x55BD:fields[key-0x55B1]=2 if c==d and key in (0x55B1,0x55BA,0x55BB) else integer(data,c,d)
                        if key==0x55D0:
                            master=[None]*10
                            for fid,p,q,_ in elements(data,c,d):
                                if 0x55D1<=fid<=0x55DA:
                                    raw=data[p:q]
                                    value=0. if not raw else struct.unpack('>f' if len(raw)==4 else '>d',raw)[0]
                                    master[fid-0x55D1]=struct.pack('>d',value).hex()
                    colour={'integers':fields,'mastering':master}
            result.append({'raw_track':data[x:y].hex(),'colour':colour})
    return {'schema':1,'tracks':result}
