/* Independent native reference supplement, owner approved 2026-10-31.
 * Test tooling only; never linked into the safe Rust library. RFC 8794/9559
 * metadata parsing and scope-1 combined zlib/header restoration supplement a
 * missing pinned-FFmpeg feature. Other combined scopes fail explicitly. The
 * system zlib is already used by the pinned FFmpeg build. No FFmpeg
 * implementation code was copied. */
#include <libavcodec/packet.h>
#include <libavcodec/defs.h>
#include <libavutil/buffer.h>
#include <zlib.h>
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <limits.h>

typedef struct { uint64_t order,scope,algorithm,type; const uint8_t *prefix; size_t prefix_n; } SpecEncoding;
typedef struct { SpecEncoding enc[8]; unsigned count; } SpecPlan;
typedef struct { uint64_t id; size_t start,end; int unknown; } SpecElement;

static int spec_var(const uint8_t *data,size_t end,size_t *at,int identifier,uint64_t *value,int *unknown){
    if(*at>=end||!data[*at])return -1;
    unsigned width=1;uint8_t marker=128;
    while(!(data[*at]&marker)){width++;marker>>=1;}
    if(width>(identifier?4u:8u)||end-*at<width)return -1;
    uint64_t n=identifier?data[*at]:(data[*at]&(marker-1));
    for(unsigned i=1;i<width;i++)n=(n<<8)|data[*at+i];
    *at+=width;*value=n;
    *unknown=!identifier&&n==((UINT64_C(1)<<(width*7))-1);
    return 0;
}
static int spec_element(const uint8_t *data,size_t end,size_t *at,SpecElement *e){
    uint64_t size;int unused;
    if(spec_var(data,end,at,1,&e->id,&unused)||spec_var(data,end,at,0,&size,&e->unknown))return -1;
    e->start=*at;
    if(e->unknown)e->end=end;
    else {if(size>end-*at)return -1;e->end=*at+(size_t)size;}
    *at=e->end;return 0;
}
static int spec_uint(const uint8_t *data,SpecElement e,uint64_t *out){
    if(e.unknown||e.end-e.start>8)return -1;
    uint64_t n=0;for(size_t i=e.start;i<e.end;i++)n=(n<<8)|data[i];*out=n;return 0;
}
static int spec_encoding(const uint8_t *data,SpecElement parent,SpecEncoding *e){
    *e=(SpecEncoding){.scope=1};
    size_t at=parent.start;
    while(at<parent.end){SpecElement f;if(spec_element(data,parent.end,&at,&f)||f.unknown)return -1;
        if(f.id==0x5031){if(spec_uint(data,f,&e->order))return -1;}
        else if(f.id==0x5032){if(spec_uint(data,f,&e->scope))return -1;}
        else if(f.id==0x5033){if(spec_uint(data,f,&e->type))return -1;}
        else if(f.id==0x5035)e->type=1;
        else if(f.id==0x5034){size_t p=f.start;while(p<f.end){SpecElement c;if(spec_element(data,f.end,&p,&c)||c.unknown)return -1;
            if(c.id==0x4254){if(spec_uint(data,c,&e->algorithm))return -1;}
            else if(c.id==0x4255){e->prefix=data+c.start;e->prefix_n=c.end-c.start;}
        }}
    }
    return 0;
}
static int spec_track(const uint8_t *data,SpecElement parent,SpecPlan *plan){
    size_t at=parent.start;memset(plan,0,sizeof(*plan));
    while(at<parent.end){SpecElement f;if(spec_element(data,parent.end,&at,&f)||f.unknown)return -1;
        if(f.id!=0x6d80)continue;
        size_t p=f.start;while(p<f.end){SpecElement c;if(spec_element(data,f.end,&p,&c)||c.unknown)return -1;
            if(c.id!=0x6240)continue;
            if(plan->count==8||spec_encoding(data,c,&plan->enc[plan->count]))return -1;
            plan->count++;
        }
    }
    for(unsigned i=0;i<plan->count;i++)for(unsigned j=i+1;j<plan->count;j++){
        if(plan->enc[i].order==plan->enc[j].order)return -1;
        if(plan->enc[i].order<plan->enc[j].order){SpecEncoding swap=plan->enc[i];plan->enc[i]=plan->enc[j];plan->enc[j]=swap;}
    }
    if(plan->count>1)for(unsigned i=0;i<plan->count;i++){
        SpecEncoding e=plan->enc[i];if(e.scope!=1||e.type||!(e.algorithm==0||e.algorithm==3))return -1;
    }
    return 0;
}
static int spec_plans(const uint8_t *data,size_t n,SpecPlan plans[64],unsigned streams){
    if(streams>64)return -1;
    memset(plans,0,streams*sizeof(*plans));size_t at=0;SpecElement segment={0};
    while(at<n){SpecElement e;if(spec_element(data,n,&at,&e))return -1;if(e.id==0x18538067){segment=e;break;}}
    if(!segment.id)return -1;
    unsigned count=0;at=segment.start;
    while(at<segment.end){SpecElement e;if(spec_element(data,segment.end,&at,&e))return -1;
        if(e.id==0x1f43b675)break; /* Selected prototype: Tracks before Clusters. */
        if(e.id!=0x1654ae6b)continue;
        size_t p=e.start;while(p<e.end){SpecElement t;if(spec_element(data,e.end,&p,&t)||t.unknown)return -1;
            if(t.id!=0xae)continue;
            if(count==streams||spec_track(data,t,&plans[count]))return -1;
            count++;
        }
    }
    return count==streams?0:-1;
}
static int spec_inflate(const uint8_t *data,size_t n,uint8_t **output,size_t *length){
    const size_t ceiling=16*1024*1024;
    if(n>UINT_MAX)return -1;
    size_t limit=n>ceiling/1024?ceiling:n*1024;
    size_t capacity=n>1024?8192:n*8;if(capacity<256)capacity=256;if(capacity>limit)capacity=limit;
    if(!capacity)capacity=1;
    uint8_t *out=malloc(capacity);if(!out)return -1;
    z_stream z={0};z.next_in=(Bytef *)data;z.avail_in=(uInt)n;
    if(inflateInit(&z)!=Z_OK){free(out);return -1;}
    int result=-1;
    for(unsigned steps=0;steps<1u<<20;steps++){
        if(z.total_out==capacity){
            if(capacity>=limit)break;
            size_t next=capacity>limit/2?limit:capacity*2;uint8_t *grown=realloc(out,next);if(!grown)break;
            out=grown;capacity=next;
        }
        z.next_out=out+z.total_out;z.avail_out=(uInt)(capacity-z.total_out);
        uLong before_in=z.total_in,before_out=z.total_out;
        int status=inflate(&z,Z_NO_FLUSH);
        if(status==Z_STREAM_END){if(z.total_in==n&&z.total_out<=limit){*output=out;*length=(size_t)z.total_out;result=0;}break;}
        if(status!=Z_OK||(z.total_in==before_in&&z.total_out==before_out))break;
    }
    inflateEnd(&z);if(result)free(out);return result;
}
static void spec_free_buffer(void *opaque,uint8_t *data){(void)opaque;free(data);}
static int spec_restore(const SpecPlan *plan,AVPacket *packet){
    if(plan->count<2)return 0; /* Ordinary features stay entirely native FFmpeg. */
    if(packet->size<0)return -1;
    const uint8_t *current=packet->data;size_t n=(size_t)packet->size;uint8_t *owned=NULL;
    for(unsigned i=0;i<plan->count;i++){
        SpecEncoding e=plan->enc[i];uint8_t *next=NULL;size_t next_n=0;
        if(e.algorithm==0){if(spec_inflate(current,n,&next,&next_n)){free(owned);return -1;}}
        else {
            if(n>16*1024*1024||e.prefix_n>16*1024*1024-n){free(owned);return -1;}
            next_n=n+e.prefix_n;next=malloc(next_n?next_n:1);if(!next){free(owned);return -1;}
            if(e.prefix_n)memcpy(next,e.prefix,e.prefix_n);if(n)memcpy(next+e.prefix_n,current,n);
        }
        free(owned);owned=next;current=next;n=next_n;
    }
    /* Transfer the restored buffer instead of adding a benchmark-only payload
     * copy. The matching allocator/free callback and decoder padding are explicit. */
    uint8_t *padded=realloc(owned,n+AV_INPUT_BUFFER_PADDING_SIZE);
    if(!padded){free(owned);return -1;}owned=padded;
    memset(owned+n,0,AV_INPUT_BUFFER_PADDING_SIZE);
    AVPacket *replacement=av_packet_alloc();int result=-1;
    if(replacement&&n<=INT_MAX){
        replacement->buf=av_buffer_create(owned,n+AV_INPUT_BUFFER_PADDING_SIZE,spec_free_buffer,NULL,0);
        if(replacement->buf){
            replacement->data=owned;replacement->size=(int)n;owned=NULL;
            if(av_packet_copy_props(replacement,packet)>=0){
                av_packet_unref(packet);av_packet_move_ref(packet,replacement);result=0;
            }
        }
    }
    av_packet_free(&replacement);free(owned);return result;
}
