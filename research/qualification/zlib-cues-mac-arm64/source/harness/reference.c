/* Independent reference adapter. Native tooling only; never linked into Rust.
 * Uses public FFmpeg headers. No code copied from FFmpeg implementations. */
#include <libavformat/avformat.h>
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
static void fold_bytes(uint64_t *digest,const uint8_t *bytes,size_t n){for(size_t i=0;i<n;i++)fold(digest,bytes[i]);}
static int64_t ns(int64_t value,AVRational tb){return value==AV_NOPTS_VALUE?INT64_MIN:av_rescale_q(value,tb,(AVRational){1,1000000000});}
static int demux(const uint8_t *data,size_t len,int trace,int64_t seek_ns,int after_eof,uint64_t *packets,uint64_t *bytes,uint64_t *digest) {
    int result=0;Source source={data,len,0};
    AVFormatContext *f=avformat_alloc_context();
    uint8_t *buf=av_malloc(32768);AVIOContext *io=NULL;AVPacket *p=NULL;
    if(!f||!buf){av_free(buf);result=AVERROR(ENOMEM);goto done;}
    io=avio_alloc_context(buf,32768,0,&source,read_bytes,NULL,seek_bytes);
    if(!io){av_free(buf);result=AVERROR(ENOMEM);goto done;}
    f->pb=io;f->flags|=AVFMT_FLAG_CUSTOM_IO|AVFMT_FLAG_NOPARSE|AVFMT_FLAG_NOFILLIN;
    result=avformat_open_input(&f,NULL,av_find_input_format("matroska,webm"),NULL);
    if(result<0)goto done;
    if(seek_ns!=INT64_MIN){
        AVPacket *discard=av_packet_alloc();if(!discard){result=AVERROR(ENOMEM);goto done;}
        if(after_eof){while((result=av_read_frame(f,discard))>=0)av_packet_unref(discard);if(result!=AVERROR_EOF){av_packet_free(&discard);goto done;}}
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
    }
    for(unsigned i=0;i<f->nb_streams;i++){AVStream *s=f->streams[i];AVCodecParameters *c=s->codecpar;
        const char *name=avcodec_get_name(c->codec_id);fold_bytes(digest,(const uint8_t *)name,strlen(name));
        fold_bytes(digest,c->extradata,(size_t)c->extradata_size);
        name=tag(s,"language","und");fold_bytes(digest,(const uint8_t *)name,strlen(name));
        name=tag(s,"title","");fold_bytes(digest,(const uint8_t *)name,strlen(name));
        uint64_t fields[]={c->codec_type==AVMEDIA_TYPE_VIDEO?1:c->codec_type==AVMEDIA_TYPE_AUDIO?2:17,(uint64_t)c->width,(uint64_t)c->height,(uint64_t)c->ch_layout.nb_channels,(uint64_t)c->sample_rate,!!(s->disposition&AV_DISPOSITION_DEFAULT),!!(s->disposition&AV_DISPOSITION_FORCED)};
        for(unsigned j=0;j<sizeof(fields)/sizeof(fields[0]);j++)fold(digest,fields[j]);
    }
    if(trace){printf("{\"schema\":1,\"operation\":\"container-demux-v1\",\"tracks\":[");
        for(unsigned i=0;i<f->nb_streams;i++){AVStream *s=f->streams[i];AVCodecParameters *c=s->codecpar;
            if(i)putchar(',');printf("{\"codec\":");quote(avcodec_get_name(c->codec_id));
            printf(",\"kind\":%d,\"private\":",c->codec_type==AVMEDIA_TYPE_VIDEO?1:c->codec_type==AVMEDIA_TYPE_AUDIO?2:17);hex(c->extradata,(size_t)c->extradata_size);
            printf(",\"width\":%d,\"height\":%d,\"channels\":%d,\"sample_rate\":%d,\"language\":",c->width,c->height,c->ch_layout.nb_channels,c->sample_rate);quote(tag(s,"language","und"));
            printf(",\"name\":");quote(tag(s,"title",""));printf(",\"default\":%s,\"forced\":%s}",s->disposition&AV_DISPOSITION_DEFAULT?"true":"false",s->disposition&AV_DISPOSITION_FORCED?"true":"false");
        }printf("],\"packets\":[");
    }
    p=av_packet_alloc();if(!p){result=AVERROR(ENOMEM);goto done;}int first=1;
    while((result=av_read_frame(f,p))>=0){
        (*packets)++;*bytes+=(uint64_t)p->size;
        fold_bytes(digest,p->data,(size_t)p->size);
        size_t trim_size=0;const uint8_t *trim=av_packet_get_side_data(p,AV_PKT_DATA_SKIP_SAMPLES,&trim_size);
        uint64_t fields[]={(uint64_t)p->stream_index,(uint64_t)ns(p->pts,f->streams[p->stream_index]->time_base),(uint64_t)ns(p->dts,f->streams[p->stream_index]->time_base),(uint64_t)ns(p->duration,f->streams[p->stream_index]->time_base),!!(p->flags&AV_PKT_FLAG_KEY),trim&&trim_size>=10?AV_RL32(trim):0,trim&&trim_size>=10?AV_RL32(trim+4):0};
        for(unsigned j=0;j<sizeof(fields)/sizeof(fields[0]);j++)fold(digest,fields[j]);
        if(trace){if(!first)putchar(',');first=0;
            printf("{\"track\":%d,\"pts_ns\":",p->stream_index);AVRational tb=f->streams[p->stream_index]->time_base;time_value(p->pts,tb);
            printf(",\"dts_ns\":");time_value(p->dts,tb);
            printf(",\"duration_ns\":");if(p->duration)time_value(p->duration,tb);else printf("null");
            size_t sd_size=0;const uint8_t *sd=av_packet_get_side_data(p,AV_PKT_DATA_SKIP_SAMPLES,&sd_size);
            uint32_t begin=0,end=0;if(sd&&sd_size>=10){begin=AV_RL32(sd);end=AV_RL32(sd+4);}
            printf(",\"key\":%s,\"trim_start_samples\":%" PRIu32 ",\"trim_end_samples\":%" PRIu32 ",\"payload\":",p->flags&AV_PKT_FLAG_KEY?"true":"false",begin,end);hex(p->data,(size_t)p->size);putchar('}');
        }av_packet_unref(p);
    }
    if(result==AVERROR_EOF){result=0;if(trace)printf("]}\n");}
done:
    av_packet_free(&p);avformat_close_input(&f);
    if(io){av_freep(&io->buffer);avio_context_free(&io);}
    return result;
}
static uint64_t now_ns(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return(uint64_t)t.tv_sec*1000000000+(uint64_t)t.tv_nsec;}
int main(int argc,char **argv){
    av_log_set_level(AV_LOG_ERROR);
    if(argc<3)return 2;
    if(!strcmp(argv[1],"identity")){printf("{\"version\":\"%s\",\"configuration\":",av_version_info());quote(avformat_configuration());printf("}\n");return 0;}
    FILE *file=fopen(argv[2],"rb");if(!file)return 2;
    if(fseek(file,0,SEEK_END)||ftell(file)<0){fclose(file);return 2;}
    long size=ftell(file);rewind(file);if(size>64*1024*1024){fclose(file);return 2;}
    uint8_t *data=malloc((size_t)size+1);if(!data){fclose(file);return 2;}
    if(fread(data,1,(size_t)size,file)!=(size_t)size){free(data);fclose(file);return 2;}fclose(file);
    if(!strcmp(argv[1],"baseline")){printf("{\"input_bytes\":%ld}\n",size);free(data);return 0;}
    int seek_mode=!strncmp(argv[1],"seek-",5);
    int64_t seek_ns=INT64_MIN;
    if(seek_mode){char *end=NULL;int index=!strcmp(argv[1],"seek-bench")?4:3;if(argc<=index){free(data);return 2;}seek_ns=strtoll(argv[index],&end,10);if(*end||seek_ns==INT64_MIN){free(data);return 2;}}
    if(!strcmp(argv[1],"memory")||!strcmp(argv[1],"memory-baseline")||!strcmp(argv[1],"seek-memory")){
        if(write(STDOUT_FILENO,"SAFE_MEDIA_BEGIN_V1\n",20)!=20){free(data);return 2;}
        uint64_t packets=0,bytes=0,digest=0;int result=0;
        if(strcmp(argv[1],"memory-baseline"))result=demux(data,(size_t)size,0,seek_ns,0,&packets,&bytes,&digest);
        if(write(STDOUT_FILENO,"SAFE_MEDIA_END_V1\n",18)!=18){free(data);return 2;}
        printf("{\"packets\":%" PRIu64 ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 "}\n",packets,bytes,digest);
        free(data);return result<0?1:0;
    }
    int trace=!strcmp(argv[1],"trace")||!strcmp(argv[1],"seek-trace");
    unsigned long iterations=1;
    if(!trace){char *end=NULL;if(argc<4){free(data);return 2;}iterations=strtoul(argv[3],&end,10);if(*end||!iterations||iterations>1000000){free(data);return 2;}}
    uint64_t packets=0,bytes=0,digest=0,start=now_ns();int result=0;
    for(unsigned long i=0;i<iterations;i++){result=demux(data,(size_t)size,trace,seek_ns,argc>4&&!strcmp(argv[4],"after-eof"),&packets,&bytes,&digest);if(result<0)break;}
    uint64_t elapsed=now_ns()-start;free(data);
    if(result<0){char err[256];av_strerror(result,err,sizeof(err));fprintf(stderr,"%s\n",err);return 1;}
    if(!trace)printf("{\"schema\":1,\"iterations\":%lu,\"elapsed_ns\":%" PRIu64 ",\"packets\":%" PRIu64 ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 "}\n",iterations,elapsed,packets,bytes,digest);
    return 0;
}
