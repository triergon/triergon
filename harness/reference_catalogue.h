/* Own public-API catalogue adapter. No FFmpeg implementation code. */
static int is_attachment(const AVStream *s) {
    return s->codecpar->codec_type==AVMEDIA_TYPE_ATTACHMENT || (s->disposition&AV_DISPOSITION_ATTACHED_PIC);
}
static int dict_entry_order(const void *left,const void *right) {
    const AVDictionaryEntry *const *a=left,*const *b=right;return strcmp((*a)->key,(*b)->key);
}
static const AVDictionaryEntry **ordered_dict(const AVDictionary *dict,size_t *count) {
    *count=(size_t)av_dict_count(dict);
    if(!*count)return NULL;
    const AVDictionaryEntry **entries=malloc(*count*sizeof(*entries));if(!entries)return NULL;
    const AVDictionaryEntry *last=NULL;size_t n=0;
    while((last=av_dict_get(dict,"",last,AV_DICT_IGNORE_SUFFIX)))entries[n++]=last;
    qsort(entries,n,sizeof(*entries),dict_entry_order);return entries;
}
static int catalogue_dict(const AVDictionary *dict,int trace,uint64_t *digest) {
    size_t count=0;const AVDictionaryEntry **entries=ordered_dict(dict,&count);
    if(count&&!entries)return AVERROR(ENOMEM);
    fold(digest,count);if(trace)putchar('[');
    for(size_t i=0;i<count;i++) {
        const char *key=entries[i]->key,*value=entries[i]->value;
        fold_bytes(digest,(const uint8_t *)key,strlen(key));fold_bytes(digest,(const uint8_t *)value,strlen(value));
        if(trace){if(i)putchar(',');putchar('[');quote(key);putchar(',');quote(value);putchar(']');}
    }
    if(trace)putchar(']');
    free(entries);return 0;
}
static int catalogue_data(const AVStream *s,const uint8_t **bytes,size_t *length) {
    if(s->disposition&AV_DISPOSITION_ATTACHED_PIC){*bytes=s->attached_pic.data;*length=(size_t)s->attached_pic.size;}
    else {*bytes=s->codecpar->extradata;*length=(size_t)s->codecpar->extradata_size;}
    return 0;
}
static int catalogue_output(AVFormatContext *f,int trace,uint64_t *digest) {
    if(trace)printf(",\"catalogue\":{\"global\":");
    int r=catalogue_dict(f->metadata,trace,digest);if(r<0)return r;
    unsigned tracks=0,attachments=0;for(unsigned i=0;i<f->nb_streams;i++)if(is_attachment(f->streams[i]))attachments++;else tracks++;
    fold(digest,tracks);if(trace)printf(",\"tracks\":[");unsigned at=0;
    for(unsigned i=0;i<f->nb_streams;i++) {AVStream *s=f->streams[i];if(is_attachment(s))continue;
        fold(digest,(uint64_t)s->disposition);if(trace){if(at++)putchar(',');printf("{\"disposition\":%d,\"tags\":",s->disposition);}
        r=catalogue_dict(s->metadata,trace,digest);if(r<0)return r;if(trace)putchar('}');
    }
    if(trace)printf("],\"chapters\":[");
    fold(digest,f->nb_chapters);
    for(unsigned i=0;i<f->nb_chapters;i++) {AVChapter *c=f->chapters[i];
        int64_t start=av_rescale_q(c->start,c->time_base,(AVRational){1,1000000000}),end=av_rescale_q(c->end,c->time_base,(AVRational){1,1000000000});
        fold(digest,(uint64_t)c->id);fold(digest,(uint64_t)start);fold(digest,(uint64_t)end);
        if(trace){if(i)putchar(',');printf("{\"id\":%" PRId64 ",\"start_ns\":%" PRId64 ",\"end_ns\":%" PRId64 ",\"tags\":",c->id,start,end);}
        r=catalogue_dict(c->metadata,trace,digest);if(r<0)return r;if(trace)putchar('}');
    }
    if(trace)printf("],\"attachments\":[");
    fold(digest,attachments);at=0;
    for(unsigned i=0;i<f->nb_streams;i++) {AVStream *s=f->streams[i];if(!is_attachment(s))continue;
        const uint8_t *bytes;size_t length;catalogue_data(s,&bytes,&length);
        if(trace){if(at++)putchar(',');printf("{\"tags\":");}
        r=catalogue_dict(s->metadata,trace,digest);if(r<0)return r;fold(digest,length);fold_bytes(digest,bytes,length);
        if(trace){printf(",\"data\":");hex(bytes,length);putchar('}');}
    }
    if(trace)printf("]}");
    return 0;
}
