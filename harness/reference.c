/* Independent reference adapter. Native tooling only; never linked into Rust.
 * Uses public FFmpeg headers. No code copied from FFmpeg implementations. */
#include <libavformat/avformat.h>
#ifdef SAFE_MEDIA_COMBINED_REFERENCE
#include "reference_content_encoding.h"
#endif
#include <libavutil/intreadwrite.h>
#include <libavutil/time.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

typedef struct { const uint8_t *data; size_t len, at; } Source;
static int read_bytes(void *opaque, uint8_t *buf, int n) {
    Source *s=opaque; size_t left=s->len-s->at;
    if (!left) return AVERROR_EOF;
    if ((size_t)n>left) n=(int)left;
    memcpy(buf,s->data+s->at,(size_t)n);s->at+=(size_t)n;return n;
}
static int64_t seek_bytes(void *opaque,int64_t offset,int whence) {
    Source *s=opaque; if (whence==AVSEEK_SIZE) return (int64_t)s->len;
    whence &= ~AVSEEK_FORCE;
    int64_t base=whence==SEEK_SET?0:whence==SEEK_CUR?(int64_t)s->at:whence==SEEK_END?(int64_t)s->len:-1;
    if(base<0 || offset < -base || offset>(int64_t)s->len-base) return AVERROR(EINVAL);
    s->at=(size_t)(base+offset);return (int64_t)s->at;
}
static void quote(const char *s) {
    putchar('"');if(s)for(;*s;s++){unsigned char c=(unsigned char)*s;
        if(c=='"'||c=='\\'){putchar('\\');putchar(c);}else if(c<32)printf("\\u%04x",c);else putchar(c);
    }putchar('"');
}
static void hex(const uint8_t *p,size_t n) {putchar('"');for(size_t i=0;i<n;i++)printf("%02x",p[i]);putchar('"');}
static void time_value(int64_t value,AVRational tb) {
    if(value==AV_NOPTS_VALUE)printf("null");else printf("%" PRId64,av_rescale_q(value,tb,(AVRational){1,1000000000}));
}
static const char *tag(AVStream *s,const char *key,const char *fallback) {
    AVDictionaryEntry *e=av_dict_get(s->metadata,key,NULL,0);return e?e->value:fallback;
}
static void fold(uint64_t *digest,uint64_t n){*digest=*digest*31+n;}
/* Same exact eight-byte polynomial, grouped into bounded 32-bit dots.
 * Four-byte maximum is 7,849,920, so these intermediate sums never wrap. */
static uint32_t fold_four(const uint8_t *b){
    return (uint32_t)b[0]*UINT32_C(29791)+(uint32_t)b[1]*UINT32_C(961)
        +(uint32_t)b[2]*UINT32_C(31)+(uint32_t)b[3];
}
static uint64_t fold_eight(const uint8_t *b){
    return (uint64_t)fold_four(b)*UINT64_C(923521)+(uint64_t)fold_four(b+4);
}
static void fold_bytes(uint64_t *digest,const uint8_t *bytes,size_t n){
    /* Same ordered checksum and polynomial as the Rust worker. All payload and
     * metadata bytes remain consumed inside each measured operation. */
    size_t i=0;
    if(n>=256){
        const uint64_t step=UINT64_C(3671467063254694913);
        size_t exponent=n/32;uint64_t power=1,base=step,lanes[4]={0,0,0,0};
        while(exponent){if(exponent&1)power*=base;base*=base;exponent>>=1;}
        for(;n-i>=32;i+=32)for(unsigned j=0;j<4;j++)lanes[j]=lanes[j]*step+fold_eight(bytes+i+j*8);
        *digest=*digest*power+lanes[0]*UINT64_C(13349005091172011265)
            +lanes[1]*UINT64_C(12662688596514954753)+lanes[2]*UINT64_C(852891037441)+lanes[3];
    }
    for(;n-i>=8;i+=8)*digest=*digest*UINT64_C(852891037441)+fold_eight(bytes+i);
    for(;i<n;i++)fold(digest,bytes[i]);
}
#include "reference_colour.h"
#include "reference_catalogue.h"
static int64_t ns(int64_t value,AVRational tb){return value==AV_NOPTS_VALUE?INT64_MIN:av_rescale_q(value,tb,(AVRational){1,1000000000});}
static int demux(const uint8_t *data,size_t len,int trace,int64_t seek_ns,int after_eof,int metadata,uint64_t *packets,uint64_t *bytes,uint64_t *digest) {
    int result=0;Source source={data,len,0};
#ifdef SAFE_MEDIA_COMBINED_REFERENCE
    SpecPlan inline_plan;SpecPlan *plans=NULL;
#endif
    AVFormatContext *f=avformat_alloc_context();
    uint8_t *buf=av_malloc(32768);AVIOContext *io=NULL;AVPacket *p=NULL;
    if(!f||!buf){av_free(buf);result=AVERROR(ENOMEM);goto done;}
    io=avio_alloc_context(buf,32768,0,&source,read_bytes,NULL,seek_bytes);
    if(!io){av_free(buf);result=AVERROR(ENOMEM);goto done;}
    f->pb=io;f->flags|=AVFMT_FLAG_CUSTOM_IO|AVFMT_FLAG_NOPARSE|AVFMT_FLAG_NOFILLIN;
    result=avformat_open_input(&f,NULL,av_find_input_format("matroska,webm"),NULL);
    if(result<0)goto done;
#ifdef SAFE_MEDIA_COMBINED_REFERENCE
    if(f->nb_streams==1)plans=&inline_plan;
    else if(f->nb_streams && f->nb_streams<=64)plans=calloc(f->nb_streams,sizeof(*plans));
    if(!plans||spec_plans(data,len,plans,f->nb_streams)){result=AVERROR(EINVAL);goto done;}
#endif
    if(seek_ns!=INT64_MIN){
        AVPacket *discard=av_packet_alloc();if(!discard){result=AVERROR(ENOMEM);goto done;}
        if(after_eof){while((result=av_read_frame(f,discard))>=0)av_packet_unref(discard);if(result!=AVERROR_EOF){av_packet_free(&discard);goto done;}}
        else {
            /* Establish packet-reader state before a fresh playback seek. A raw
             * open/seek on unknown-sized Clusters differs from initialized seeks
             * in the pinned demuxer. Include this priming work in timing/memory;
             * no full stream-info scan or codec probing is required. */
            result=av_read_frame(f,discard);av_packet_unref(discard);
            if(result<0 && result!=AVERROR_EOF){av_packet_free(&discard);goto done;}
        }
        av_packet_free(&discard);
        int video=av_find_best_stream(f,AVMEDIA_TYPE_VIDEO,-1,-1,NULL,0);if(video<0){result=video;goto done;}
        int64_t preroll=0;
        for(unsigned i=0;i<f->nb_streams;i++){AVCodecParameters *c=f->streams[i]->codecpar;
            if(c->sample_rate>0 && c->seek_preroll>0){int64_t value=av_rescale_q(c->seek_preroll,(AVRational){1,c->sample_rate},(AVRational){1,1000000000});if(value>preroll)preroll=value;}
        }
        if(seek_ns<INT64_MIN+preroll){result=AVERROR(EINVAL);goto done;}
        seek_ns=seek_ns>preroll?seek_ns-preroll:0;
        int64_t target=av_rescale_q(seek_ns,(AVRational){1,1000000000},f->streams[video]->time_base);
        result=av_seek_frame(f,video,target,AVSEEK_FLAG_BACKWARD);if(result<0)goto done;
        /* A keyframe seek can discard audio shifted before the video RAP by
         * CodecDelay. Use the native index and an ANY-frame seek to retain that
         * recovery data. At a negative beginning, reopen rather than accepting
         * the demuxer's zero-time clamp. All work stays inside the operation. */
        int64_t delay=0;
        for(unsigned i=0;i<f->nb_streams;i++){AVCodecParameters *c=f->streams[i]->codecpar;
            if(c->sample_rate>0 && c->initial_padding>0){int64_t v=av_rescale_q(c->initial_padding,(AVRational){1,c->sample_rate},f->streams[video]->time_base);if(v>delay)delay=v;}}
        if(delay>0){const AVIndexEntry *entry=avformat_index_get_entry_from_timestamp(f->streams[video],target,AVSEEK_FLAG_BACKWARD);
            if(!entry){result=AVERROR(EINVAL);goto done;}
            if(entry->timestamp<INT64_MIN+delay){result=AVERROR(EINVAL);goto done;}
            int64_t earliest=entry->timestamp-delay;
            const AVIndexEntry *first=avformat_index_get_entry(f->streams[video],0);
            if(earliest<0 && first && entry->pos==first->pos){
                avformat_close_input(&f);
                if(avio_seek(io,0,SEEK_SET)<0){result=AVERROR(EIO);goto done;}
                f=avformat_alloc_context();if(!f){result=AVERROR(ENOMEM);goto done;}
                f->pb=io;f->flags|=AVFMT_FLAG_CUSTOM_IO|AVFMT_FLAG_NOPARSE|AVFMT_FLAG_NOFILLIN;
                result=avformat_open_input(&f,NULL,av_find_input_format("matroska,webm"),NULL);if(result<0)goto done;
            }else{result=av_seek_frame(f,video,earliest,AVSEEK_FLAG_BACKWARD|AVSEEK_FLAG_ANY);if(result<0)goto done;}
        }
    }
    for(unsigned i=0;i<f->nb_streams;i++){AVStream *s=f->streams[i];if(metadata==2&&is_attachment(s))continue;AVCodecParameters *c=s->codecpar;
        const char *name=avcodec_get_name(c->codec_id);fold_bytes(digest,(const uint8_t *)name,strlen(name));
        fold_bytes(digest,c->extradata,(size_t)c->extradata_size);
        if(metadata!=2){
            name=tag(s,"language","und");fold_bytes(digest,(const uint8_t *)name,strlen(name));
            name=tag(s,"title","");fold_bytes(digest,(const uint8_t *)name,strlen(name));
        }
        uint64_t fields[]={c->codec_type==AVMEDIA_TYPE_VIDEO?1:c->codec_type==AVMEDIA_TYPE_AUDIO?2:17,(uint64_t)c->width,(uint64_t)c->height,(uint64_t)c->ch_layout.nb_channels,(uint64_t)c->sample_rate,!!(s->disposition&AV_DISPOSITION_DEFAULT),!!(s->disposition&AV_DISPOSITION_FORCED)};
        for(unsigned j=0;j<sizeof(fields)/sizeof(fields[0]);j++)fold(digest,fields[j]);
        if(metadata){int64_t values[32];size_t count=colour_values(c,values);for(size_t j=0;j<count;j++)fold(digest,(uint64_t)values[j]);}
    }
    if(trace){printf("{\"schema\":%d,\"operation\":\"%s\",\"tracks\":[",metadata==2?2:1,metadata==2?"container-catalogue-demux-v1":"container-demux-v1");int first_track=1;
        for(unsigned i=0;i<f->nb_streams;i++){AVStream *s=f->streams[i];if(metadata==2&&is_attachment(s))continue;AVCodecParameters *c=s->codecpar;
            if(!first_track)putchar(',');
            first_track=0;printf("{\"codec\":");quote(avcodec_get_name(c->codec_id));
            printf(",\"kind\":%d,\"private\":",c->codec_type==AVMEDIA_TYPE_VIDEO?1:c->codec_type==AVMEDIA_TYPE_AUDIO?2:17);hex(c->extradata,(size_t)c->extradata_size);
            printf(",\"width\":%d,\"height\":%d,\"channels\":%d,\"sample_rate\":%d",c->width,c->height,c->ch_layout.nb_channels,c->sample_rate);
            if(metadata!=2){printf(",\"language\":");quote(tag(s,"language","und"));printf(",\"name\":");quote(tag(s,"title",""));}
            printf(",\"default\":%s,\"forced\":%s",s->disposition&AV_DISPOSITION_DEFAULT?"true":"false",s->disposition&AV_DISPOSITION_FORCED?"true":"false");if(metadata)colour_trace(c);putchar('}');
        }printf("],\"packets\":[");
    }
    p=av_packet_alloc();if(!p){result=AVERROR(ENOMEM);goto done;}int first=1;
    while((result=av_read_frame(f,p))>=0){
#ifdef SAFE_MEDIA_COMBINED_REFERENCE
        if(p->stream_index<0||(unsigned)p->stream_index>=f->nb_streams||spec_restore(&plans[p->stream_index],p)){result=AVERROR(EINVAL);goto done;}
#endif
        if(p->stream_index<0||(unsigned)p->stream_index>=f->nb_streams){result=AVERROR(EINVAL);goto done;}
        if(metadata==2&&is_attachment(f->streams[p->stream_index])){av_packet_unref(p);continue;}
        unsigned stream_index=(unsigned)p->stream_index;
        if(metadata==2){stream_index=0;for(unsigned i=0;i<(unsigned)p->stream_index;i++)if(!is_attachment(f->streams[i]))stream_index++;}
        (*packets)++;*bytes+=(uint64_t)p->size;
        fold_bytes(digest,p->data,(size_t)p->size);
        size_t trim_size=0;const uint8_t *trim=av_packet_get_side_data(p,AV_PKT_DATA_SKIP_SAMPLES,&trim_size);
        uint64_t fields[]={(uint64_t)stream_index,(uint64_t)ns(p->pts,f->streams[p->stream_index]->time_base),(uint64_t)ns(p->dts,f->streams[p->stream_index]->time_base),(uint64_t)ns(p->duration,f->streams[p->stream_index]->time_base),!!(p->flags&AV_PKT_FLAG_KEY),trim&&trim_size>=10?AV_RL32(trim):0,trim&&trim_size>=10?AV_RL32(trim+4):0};
        for(unsigned j=0;j<sizeof(fields)/sizeof(fields[0]);j++)fold(digest,fields[j]);
        if(trace){if(!first)putchar(',');first=0;
            printf("{\"track\":%d,\"pts_ns\":",stream_index);AVRational tb=f->streams[p->stream_index]->time_base;time_value(p->pts,tb);
            printf(",\"dts_ns\":");time_value(p->dts,tb);
            printf(",\"duration_ns\":");if(p->duration)time_value(p->duration,tb);else printf("null");
            size_t sd_size=0;const uint8_t *sd=av_packet_get_side_data(p,AV_PKT_DATA_SKIP_SAMPLES,&sd_size);
            uint32_t begin=0,end=0;if(sd&&sd_size>=10){begin=AV_RL32(sd);end=AV_RL32(sd+4);}
            printf(",\"key\":%s,\"trim_start_samples\":%" PRIu32 ",\"trim_end_samples\":%" PRIu32 ",\"payload\":",p->flags&AV_PKT_FLAG_KEY?"true":"false",begin,end);hex(p->data,(size_t)p->size);putchar('}');
        }av_packet_unref(p);
    }
    if(result==AVERROR_EOF){result=0;if(trace)putchar(']');if(metadata==2)result=catalogue_output(f,trace,digest);if(result>=0&&trace)printf("}\n");}
done:
#ifdef SAFE_MEDIA_COMBINED_REFERENCE
    if(plans && plans!=&inline_plan)free(plans);
#endif
    av_packet_free(&p);avformat_close_input(&f);
    if(io){av_freep(&io->buffer);avio_context_free(&io);}
    return result;
}
static uint64_t now_ns(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return(uint64_t)t.tv_sec*1000000000+(uint64_t)t.tv_nsec;}
#ifdef SAFE_MEDIA_FEED_REFERENCE
#include "reference_feed.h"
#endif
int main(int argc,char **argv){
    av_log_set_level(AV_LOG_ERROR);
    if(argc<3)return 2;
    if(!strcmp(argv[1],"identity")){printf("{\"version\":\"%s\",\"configuration\":",av_version_info());quote(avformat_configuration());
#ifdef SAFE_MEDIA_COMBINED_REFERENCE
        printf(",\"adapter\":\"ffmpeg-plus-native-combined-encoding\"");
#else
        printf(",\"adapter\":\"native-ffmpeg-packets\"");
#endif
        printf("}\n");return 0;}
    FILE *file=fopen(argv[2],"rb");if(!file)return 2;
    if(fseek(file,0,SEEK_END)||ftell(file)<0){fclose(file);return 2;}
    long size=ftell(file);rewind(file);if(size>64*1024*1024){fclose(file);return 2;}
    uint8_t *data=malloc((size_t)size+1);if(!data){fclose(file);return 2;}
    if(fread(data,1,(size_t)size,file)!=(size_t)size){free(data);fclose(file);return 2;}fclose(file);
    if(!strcmp(argv[1],"baseline")){printf("{\"input_bytes\":%ld}\n",size);free(data);return 0;}
#ifdef SAFE_MEDIA_FEED_REFERENCE
    if(!strncmp(argv[1],"feed-",5)){int result=feed_main(argc,argv,data,size);free(data);return result;}
#endif
    int metadata=!strncmp(argv[1],"catalogue-",10)?2:!strncmp(argv[1],"metadata-",9);
    int seek_mode=!strncmp(argv[1],"seek-",5);
    int64_t seek_ns=INT64_MIN;
    if(seek_mode){char *end=NULL;int index=!strcmp(argv[1],"seek-bench")?4:3;if(argc<=index){free(data);return 2;}seek_ns=strtoll(argv[index],&end,10);if(*end||seek_ns==INT64_MIN){free(data);return 2;}}
    if(!strcmp(argv[1],"memory")||!strcmp(argv[1],"memory-baseline")||!strcmp(argv[1],"seek-memory")||!strcmp(argv[1],"codec-memory")||!strcmp(argv[1],"metadata-memory")||!strcmp(argv[1],"catalogue-memory")){
        if(write(STDOUT_FILENO,"SAFE_MEDIA_BEGIN_V1\n",20)!=20){free(data);return 2;}
        uint64_t packets=0,bytes=0,digest=0;int result=0;
        if(strcmp(argv[1],"memory-baseline"))result=demux(data,(size_t)size,0,seek_ns,0,metadata,&packets,&bytes,&digest);
        if(write(STDOUT_FILENO,"SAFE_MEDIA_END_V1\n",18)!=18){free(data);return 2;}
        printf("{\"packets\":%" PRIu64 ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 "}\n",packets,bytes,digest);
        free(data);return result<0?1:0;
    }
    int trace=!strcmp(argv[1],"trace")||!strcmp(argv[1],"seek-trace")||!strcmp(argv[1],"codec-trace")||!strcmp(argv[1],"metadata-trace")||!strcmp(argv[1],"catalogue-trace");
    unsigned long iterations=1;
    if(!trace){char *end=NULL;if(argc<4){free(data);return 2;}iterations=strtoul(argv[3],&end,10);if(*end||!iterations||iterations>1000000){free(data);return 2;}}
    uint64_t packets=0,bytes=0,digest=0,start=now_ns();int result=0;
    for(unsigned long i=0;i<iterations;i++){result=demux(data,(size_t)size,trace,seek_ns,argc>4&&!strcmp(argv[4],"after-eof"),metadata,&packets,&bytes,&digest);if(result<0)break;}
    uint64_t elapsed=now_ns()-start;free(data);
    if(result<0){char err[256];av_strerror(result,err,sizeof(err));fprintf(stderr,"%s\n",err);return 1;}
    if(!trace)printf("{\"schema\":1,\"iterations\":%lu,\"elapsed_ns\":%" PRIu64 ",\"packets\":%" PRIu64 ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 "}\n",iterations,elapsed,packets,bytes,digest);
    return 0;
}
