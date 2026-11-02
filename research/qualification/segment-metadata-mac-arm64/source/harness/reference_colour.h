/* Own reference tooling: public FFmpeg metadata APIs only. */
#include <libavutil/mastering_display_metadata.h>

static size_t colour_values(const AVCodecParameters *c, int64_t v[32]) {
    size_t n=0;
    v[n++]=c->color_space;v[n++]=c->color_range;v[n++]=c->color_trc;
    v[n++]=c->color_primaries;v[n++]=c->bits_per_raw_sample;v[n++]=c->chroma_location;
    const AVPacketSideData *sd=av_packet_side_data_get(c->coded_side_data,c->nb_coded_side_data,AV_PKT_DATA_CONTENT_LIGHT_LEVEL);
    const AVContentLightMetadata *light=sd&&sd->size>=sizeof(*light)?(const AVContentLightMetadata *)sd->data:NULL;
    v[n++]=light!=NULL;v[n++]=light?light->MaxCLL:0;v[n++]=light?light->MaxFALL:0;
    sd=av_packet_side_data_get(c->coded_side_data,c->nb_coded_side_data,AV_PKT_DATA_MASTERING_DISPLAY_METADATA);
    const AVMasteringDisplayMetadata *m=sd&&sd->size>=sizeof(*m)?(const AVMasteringDisplayMetadata *)sd->data:NULL;
    v[n++]=m!=NULL;v[n++]=m&&m->has_primaries;v[n++]=m&&m->has_luminance;
    for(unsigned i=0;i<8;i++){
        AVRational q={0,0};
        if(m&&m->has_primaries)q=i<6?m->display_primaries[i/2][i%2]:m->white_point[i-6];
        v[n++]=q.num;v[n++]=q.den;
    }
    AVRational lo=m&&m->has_luminance?m->min_luminance:(AVRational){0,0};
    AVRational hi=m&&m->has_luminance?m->max_luminance:(AVRational){0,0};
    v[n++]=lo.num;v[n++]=lo.den;v[n++]=hi.num;v[n++]=hi.den;
    return n;
}
static void colour_trace(const AVCodecParameters *c) {
    int64_t v[32];size_t n=colour_values(c,v);printf(",\"colour\":[");
    for(size_t i=0;i<n;i++)printf("%s%" PRId64,i?",":"",v[i]);putchar(']');
}
