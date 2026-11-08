/* Subtitle decode reference: the subtitles build of harness/reference.c (tooling only).
 * Uses public FFmpeg APIs; no code copied from FFmpeg implementations.
 *
 * usage: reference sub-trace FILE KIND [ARG...] | sub-fold FILE KIND [ARG...]
 *        | sub-bench FILE ITERATIONS KIND [ARG...] | sub-memory FILE KIND [ARG...]
 *        | sub-input FILE KIND [ARG...] (the prepared packets, for diagnosis)
 * KIND: pgs dvb dvd teletext cc. ARG: stream=N (the Nth stream of KIND's codec,
 * from 0), opt:NAME=VALUE (a decoder option), and for cc: source=video (A/53
 * from the decoded video stream, the default) or source=packets (a stream of
 * eia_608 packets, such as an SCC file).
 *
 * Preparation, outside every timed interval and memory window: open FILE by
 * name, select the stream and copy its packets in demux order. For cc from video,
 * decode the whole video stream and turn each output frame's A/53 side data
 * into one packet carrying the frame's timestamp, in output order.
 *
 * The operation, inside the timed interval and memory window: allocate and open
 * the subtitle decoder with the stream's extradata, packet time base and options;
 * decode every prepared packet in order with one call each; then drain with empty
 * packets until no subtitle comes out (at most 64); fold every output subtitle
 * (all rectangles, palettes, pixels, text and ASS) and free it; free the decoder.
 * The same protocol as crates/subtitle-worker. */
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/dict.h>
#include <libavutil/frame.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define SUB_OPERATION "subtitle-decode"
#define SUB_MAX_DRAIN 64

/* The statically linked libzvbi calls strdup. glibc's strdup allocates
 * outside the interposed entry points the memory observer covers, so the
 * worker defines it here with the same work (one malloc of strlen + 1 and a
 * copy) and the binary imports no allocator the observer cannot see. */
char *strdup(const char *text) {
    size_t size = strlen(text) + 1;
    char *copy = malloc(size);
    return copy ? memcpy(copy, text, size) : NULL;
}

typedef struct {
    enum AVCodecID codec;
    AVCodecParameters *par;
    AVRational time_base;
    AVPacket **packets;
    size_t count, capacity;
    AVDictionary *options;
} SubInput;

typedef struct {
    uint64_t events, bytes, digest;
} SubWork;

static void sub_mix(uint64_t *digest, uint64_t word) { *digest = *digest * 31 + word; }

static void sub_fold_text(SubWork *work, const char *text) {
    size_t n = text ? strlen(text) : 0;
    sub_mix(&work->digest, (uint64_t)n);
    fold_bytes(&work->digest, (const uint8_t *)text, n);
    work->bytes += n;
}

static void sub_trace_text(const char *key, const char *text) {
    printf(",\"%s\":", key);
    if (text) quote(text);
    else printf("null");
}

static enum AVCodecID sub_codec(const char *kind) {
    if (!strcmp(kind, "pgs")) return AV_CODEC_ID_HDMV_PGS_SUBTITLE;
    if (!strcmp(kind, "dvb")) return AV_CODEC_ID_DVB_SUBTITLE;
    if (!strcmp(kind, "dvd")) return AV_CODEC_ID_DVD_SUBTITLE;
    if (!strcmp(kind, "teletext")) return AV_CODEC_ID_DVB_TELETEXT;
    if (!strcmp(kind, "cc")) return AV_CODEC_ID_EIA_608;
    return AV_CODEC_ID_NONE;
}

static int sub_append(SubInput *in, AVPacket *packet) {
    if (in->count == in->capacity) {
        size_t capacity = in->capacity ? in->capacity * 2 : 64;
        AVPacket **grown = av_realloc_array(in->packets, capacity, sizeof(*grown));
        if (!grown) return AVERROR(ENOMEM);
        in->packets = grown;
        in->capacity = capacity;
    }
    in->packets[in->count++] = packet;
    return 0;
}

static void sub_input_free(SubInput *in) {
    for (size_t i = 0; i < in->count; i++) av_packet_free(&in->packets[i]);
    av_free(in->packets);
    avcodec_parameters_free(&in->par);
    av_dict_free(&in->options);
}

/* A/53 side data of every decoded frame, in output order, as eia_608 packets. */
static int sub_collect_video(SubInput *in, AVCodecContext *video, AVFrame *frame) {
    int rc;
    while ((rc = avcodec_receive_frame(video, frame)) >= 0) {
        AVFrameSideData *sd = av_frame_get_side_data(frame, AV_FRAME_DATA_A53_CC);
        if (sd && sd->size) {
            AVPacket *packet = av_packet_alloc();
            if (!packet || av_new_packet(packet, (int)sd->size) < 0) {
                av_packet_free(&packet);
                av_frame_unref(frame);
                return AVERROR(ENOMEM);
            }
            memcpy(packet->data, sd->data, sd->size);
            packet->pts = packet->dts = frame->pts != AV_NOPTS_VALUE ? frame->pts : frame->best_effort_timestamp;
            packet->duration = frame->duration;
            if ((rc = sub_append(in, packet)) < 0) {
                av_packet_free(&packet);
                av_frame_unref(frame);
                return rc;
            }
        }
        av_frame_unref(frame);
    }
    return rc == AVERROR(EAGAIN) || rc == AVERROR_EOF ? 0 : rc;
}

static int sub_prepare(const char *file, int argc, char **argv, SubInput *in) {
    const char *kind = argv[0];
    int ordinal = 0, from_video = 0;
    in->codec = sub_codec(kind);
    if (in->codec == AV_CODEC_ID_NONE) return AVERROR(EINVAL);
    from_video = in->codec == AV_CODEC_ID_EIA_608;
    for (int i = 1; i < argc; i++) {
        if (!strncmp(argv[i], "stream=", 7)) ordinal = atoi(argv[i] + 7);
        else if (!strcmp(argv[i], "source=video")) from_video = 1;
        else if (!strcmp(argv[i], "source=packets")) from_video = 0;
        else if (!strncmp(argv[i], "opt:", 4)) {
            char *pair = av_strdup(argv[i] + 4), *eq = pair ? strchr(pair, '=') : NULL;
            if (!eq) { av_free(pair); return AVERROR(EINVAL); }
            *eq = 0;
            int rc = av_dict_set(&in->options, pair, eq + 1, 0);
            av_free(pair);
            if (rc < 0) return rc;
        } else return AVERROR(EINVAL);
    }
    AVFormatContext *format = NULL;
    AVCodecContext *video = NULL;
    AVPacket *packet = NULL;
    AVFrame *frame = NULL;
    int rc = avformat_open_input(&format, file, NULL, NULL);
    if (rc < 0) return rc;
    if ((rc = avformat_find_stream_info(format, NULL)) < 0) goto done;
    int selected = -1, seen = 0;
    for (unsigned i = 0; i < format->nb_streams && selected < 0; i++) {
        enum AVCodecID id = format->streams[i]->codecpar->codec_id;
        int match = from_video ? format->streams[i]->codecpar->codec_type == AVMEDIA_TYPE_VIDEO : id == in->codec;
        if (match && seen++ == ordinal) selected = (int)i;
    }
    if (selected < 0) { rc = AVERROR_STREAM_NOT_FOUND; goto done; }
    AVStream *stream = format->streams[selected];
    in->time_base = stream->time_base;
    if (!(in->par = avcodec_parameters_alloc())) { rc = AVERROR(ENOMEM); goto done; }
    if (!from_video && (rc = avcodec_parameters_copy(in->par, stream->codecpar)) < 0) goto done;
    in->par->codec_type = AVMEDIA_TYPE_SUBTITLE;
    in->par->codec_id = in->codec;
    if (from_video) {
        const AVCodec *decoder = avcodec_find_decoder(stream->codecpar->codec_id);
        if (!decoder || !(video = avcodec_alloc_context3(decoder)) || !(frame = av_frame_alloc())) {
            rc = AVERROR(ENOMEM);
            goto done;
        }
        if ((rc = avcodec_parameters_to_context(video, stream->codecpar)) < 0) goto done;
        video->pkt_timebase = stream->time_base;
        video->thread_count = 1;
        if ((rc = avcodec_open2(video, decoder, NULL)) < 0) goto done;
    }
    if (!(packet = av_packet_alloc())) { rc = AVERROR(ENOMEM); goto done; }
    while ((rc = av_read_frame(format, packet)) >= 0) {
        if (packet->stream_index != selected) { av_packet_unref(packet); continue; }
        if (from_video) {
            rc = avcodec_send_packet(video, packet);
            av_packet_unref(packet);
            if (rc < 0 && rc != AVERROR_INVALIDDATA) goto done;
            if ((rc = sub_collect_video(in, video, frame)) < 0) goto done;
            continue;
        }
        AVPacket *copy = av_packet_clone(packet);
        av_packet_unref(packet);
        if (!copy) { rc = AVERROR(ENOMEM); goto done; }
        if ((rc = sub_append(in, copy)) < 0) { av_packet_free(&copy); goto done; }
    }
    if (rc != AVERROR_EOF) goto done;
    rc = 0;
    if (from_video) {
        if ((rc = avcodec_send_packet(video, NULL)) < 0) goto done;
        rc = sub_collect_video(in, video, frame);
    }
done:
    av_frame_free(&frame);
    avcodec_free_context(&video);
    av_packet_free(&packet);
    avformat_close_input(&format);
    return rc;
}

static void sub_fold_event(SubWork *work, const AVSubtitle *s, uint64_t at, int trace) {
    work->events++;
    uint64_t fields[] = {at, (uint64_t)s->pts, s->start_display_time, s->end_display_time, s->format, s->num_rects};
    for (size_t i = 0; i < sizeof(fields) / sizeof(fields[0]); i++) sub_mix(&work->digest, fields[i]);
    if (trace) {
        printf("{\"packet\":%" PRIu64 ",\"pts\":", at);
        if (s->pts == AV_NOPTS_VALUE) printf("null");
        else printf("%" PRId64, s->pts);
        printf(",\"start\":%" PRIu32 ",\"end\":%" PRIu32 ",\"format\":%u,\"rects\":[", s->start_display_time,
               s->end_display_time, (unsigned)s->format);
    }
    for (unsigned r = 0; r < s->num_rects; r++) {
        const AVSubtitleRect *rect = s->rects[r];
        int bitmap = rect->type == SUBTITLE_BITMAP && rect->w > 0 && rect->h > 0 && rect->data[0];
        uint64_t head[] = {(uint64_t)rect->type, (uint64_t)rect->x, (uint64_t)rect->y, (uint64_t)rect->w,
                           (uint64_t)rect->h, (uint64_t)rect->nb_colors, (uint64_t)rect->flags};
        for (size_t i = 0; i < sizeof(head) / sizeof(head[0]); i++) sub_mix(&work->digest, head[i]);
        const uint32_t *palette = rect->type == SUBTITLE_BITMAP ? (const uint32_t *)rect->data[1] : NULL;
        int colors = palette ? rect->nb_colors : 0;
        for (int c = 0; c < colors; c++) sub_mix(&work->digest, palette[c]);
        work->bytes += (uint64_t)colors * 4;
        if (bitmap)
            for (int y = 0; y < rect->h; y++)
                fold_bytes(&work->digest, rect->data[0] + (size_t)y * (size_t)rect->linesize[0], (size_t)rect->w);
        if (bitmap) work->bytes += (uint64_t)rect->w * (uint64_t)rect->h;
        sub_fold_text(work, rect->text);
        sub_fold_text(work, rect->ass);
        if (trace) {
            printf("%s{\"type\":%d,\"x\":%d,\"y\":%d,\"w\":%d,\"h\":%d,\"colors\":%d,\"flags\":%d,\"palette\":\"",
                   r ? "," : "", (int)rect->type, rect->x, rect->y, rect->w, rect->h, rect->nb_colors, rect->flags);
            for (int c = 0; c < colors; c++) printf("%08" PRIx32, palette[c]);
            printf("\",\"pixels\":\"");
            if (bitmap)
                for (int y = 0; y < rect->h; y++)
                    for (int x = 0; x < rect->w; x++) printf("%02x", rect->data[0][(size_t)y * (size_t)rect->linesize[0] + (size_t)x]);
            putchar('"');
            sub_trace_text("text", rect->text);
            sub_trace_text("ass", rect->ass);
            putchar('}');
        }
    }
    if (trace) printf("]}\n");
}

static int sub_operation(const SubInput *in, SubWork *work, int trace) {
    const AVCodec *decoder = avcodec_find_decoder(in->codec);
    if (!decoder) return AVERROR_DECODER_NOT_FOUND;
    AVCodecContext *context = avcodec_alloc_context3(decoder);
    AVDictionary *options = NULL;
    AVPacket *empty = av_packet_alloc();
    int rc = context && empty ? 0 : AVERROR(ENOMEM);
    if (rc < 0) goto done;
    if ((rc = avcodec_parameters_to_context(context, in->par)) < 0) goto done;
    context->pkt_timebase = in->time_base;
    if ((rc = av_dict_copy(&options, in->options, 0)) < 0) goto done;
    if ((rc = avcodec_open2(context, decoder, &options)) < 0) goto done;
    if (av_dict_count(options)) { rc = AVERROR_OPTION_NOT_FOUND; goto done; }
    const char *header = context->subtitle_header_size ? (const char *)context->subtitle_header : NULL;
    sub_fold_text(work, header);
    if (trace) {
        printf("{\"schema\":1,\"operation\":\"" SUB_OPERATION "\",\"packets\":%zu", in->count);
        sub_trace_text("header", header);
        printf("}\n");
    }
    for (size_t i = 0; i <= in->count + SUB_MAX_DRAIN; i++) {
        const AVPacket *packet = i < in->count ? in->packets[i] : empty;
        AVSubtitle subtitle;
        int got = 0;
        int used = avcodec_decode_subtitle2(context, &subtitle, &got, (AVPacket *)packet);
        /* A refused packet is reported in the trace only: the decoder's return
         * code is FFmpeg's, and only decoded output is compared. */
        if (trace && used < 0) printf("{\"packet\":%zu,\"error\":%d}\n", i, used);
        if (got) {
            sub_fold_event(work, &subtitle, (uint64_t)(i < in->count ? i : in->count), trace);
            avsubtitle_free(&subtitle);
        } else if (i >= in->count) break;
    }
done:
    av_dict_free(&options);
    av_packet_free(&empty);
    avcodec_free_context(&context);
    return rc;
}

static void sub_trace_input(const SubInput *in) {
    printf("{\"schema\":1,\"operation\":\"subtitle-input\",\"time_base\":[%d,%d],\"configuration\":",
           in->time_base.num, in->time_base.den);
    hex(in->par->extradata, (size_t)in->par->extradata_size);
    printf(",\"packets\":%zu}\n", in->count);
    for (size_t i = 0; i < in->count; i++) {
        const AVPacket *p = in->packets[i];
        printf("{\"input\":%zu,\"pts\":", i);
        if (p->pts == AV_NOPTS_VALUE) printf("null"); else printf("%" PRId64, p->pts);
        printf(",\"dts\":");
        if (p->dts == AV_NOPTS_VALUE) printf("null"); else printf("%" PRId64, p->dts);
        printf(",\"duration\":%" PRId64 ",\"data\":", p->duration);
        hex(p->data, (size_t)p->size);
        printf("}\n");
    }
}

static int sub_main(int argc, char **argv) {
    const int bench = !strcmp(argv[1], "sub-bench"), memory = !strcmp(argv[1], "sub-memory");
    const int trace = !strcmp(argv[1], "sub-trace"), fold_only = !strcmp(argv[1], "sub-fold");
    const int input = !strcmp(argv[1], "sub-input");
    const int first = bench ? 4 : 3;
    if ((!bench && !memory && !trace && !fold_only && !input) || argc <= first) return 2;
    unsigned long iterations = 1;
    if (bench) {
        char *end = NULL;
        iterations = strtoul(argv[3], &end, 10);
        if (*end || !iterations || iterations > 1000000) return 2;
    }
    SubInput in = {0};
    int rc = sub_prepare(argv[2], argc - first, argv + first, &in);
    if (rc < 0) {
        char error[AV_ERROR_MAX_STRING_SIZE];
        av_strerror(rc, error, sizeof(error));
        fprintf(stderr, "reference prepare: %s\n", error);
        sub_input_free(&in);
        return 1;
    }
    if (input) {
        sub_trace_input(&in);
        sub_input_free(&in);
        return 0;
    }
    if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18) return 2;
    SubWork total = {0};
    const uint64_t start = bench ? now_ns() : 0;
    for (unsigned long i = 0; i < iterations && rc >= 0; i++) {
        SubWork once = {0};
        rc = sub_operation(&in, &once, trace);
        total.events += once.events;
        total.bytes += once.bytes;
        sub_mix(&total.digest, once.digest);
    }
    const uint64_t elapsed = bench ? now_ns() - start : 0;
    if (memory && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16) return 2;
    sub_input_free(&in);
    if (rc < 0) {
        char error[AV_ERROR_MAX_STRING_SIZE];
        av_strerror(rc, error, sizeof(error));
        fprintf(stderr, "reference: %s (%d)\n", error, rc);
        return 1;
    }
    if (!trace)
        printf("{\"schema\":1,\"operation\":\"" SUB_OPERATION "\",\"iterations\":%lu,\"packets\":%" PRIu64
               ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true}\n",
               iterations, total.events, total.bytes, total.digest, elapsed);
    return 0;
}
