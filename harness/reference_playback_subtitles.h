/* Live demux and subtitle decode over the preloaded container. */
static void subtitle_quote(const char *text)
{
    putchar('"');
    for (const unsigned char *p = (const unsigned char *)text; *p; ++p) {
        if (*p == '"' || *p == '\\')
            putchar('\\');
        if (*p < 32)
            printf("\\u%04x", *p);
        else
            putchar(*p);
    }
    putchar('"');
}

static void subtitle_hex(const uint8_t *bytes, size_t size)
{
    putchar('"');
    for (size_t i = 0; i < size; ++i)
        printf("%02x", bytes[i]);
    putchar('"');
}

static int sub_main(int argc, char **argv) __attribute__((unused));
#define quote subtitle_quote
#define hex subtitle_hex
#define now_ns view_now
#define TRIERGON_SUBTITLE_PLAIN_REFERENCE
#include "reference_subtitles.h"
#undef TRIERGON_SUBTITLE_PLAIN_REFERENCE
#undef now_ns
#undef hex
#undef quote
#include <libavutil/intreadwrite.h>

#define BRIDGE_ABSENT ((uint64_t)AV_NOPTS_VALUE)

typedef struct {
    const uint8_t *data;
    size_t size, offset;
} BridgeIo;

static int subtitle_bridge_read(void *opaque, uint8_t *dst, int bytes)
{
    BridgeIo *io = opaque;
    size_t available = io->size - io->offset;
    if (!available)
        return AVERROR_EOF;
    size_t count = FFMIN((size_t)bytes, available);
    memcpy(dst, io->data + io->offset, count);
    io->offset += count;
    return (int)count;
}

static int64_t subtitle_bridge_seek(void *opaque, int64_t offset, int whence)
{
    BridgeIo *io = opaque;
    if (whence == AVSEEK_SIZE)
        return (int64_t)io->size;
    int64_t base;
    switch (whence & ~AVSEEK_FORCE) {
    case SEEK_SET: base = 0; break;
    case SEEK_CUR: base = (int64_t)io->offset; break;
    case SEEK_END: base = (int64_t)io->size; break;
    default: return AVERROR(EINVAL);
    }
    if (offset < -base || offset > (int64_t)io->size - base)
        return AVERROR(EINVAL);
    io->offset = (size_t)(base + offset);
    return (int64_t)io->offset;
}

static void subtitle_bridge_record(ViewWork *work, uint64_t kind, const uint64_t *words,
                          size_t count, const uint8_t *data, size_t size)
{
    view_record(work, kind, words, count, data, size);
    if (!work->trace)
        return;
    printf("%" PRIu64, kind);
    for (size_t i = 0; i < count; ++i)
        printf(" %" PRIu64, words[i]);
    if (size)
        putchar(' ');
    for (size_t i = 0; i < size; ++i)
        printf("%02x", data[i]);
    putchar('\n');
}

static uint64_t subtitle_bridge_micros(int64_t value, AVRational base)
{
    return value == AV_NOPTS_VALUE ? BRIDGE_ABSENT : (uint64_t)av_rescale_q(value, base, AV_TIME_BASE_Q);
}

static void subtitle_bridge_cue(ViewWork *work, uint64_t pts, uint64_t duration, const char *line)
{
    size_t length = line ? strlen(line) : 0;
    uint64_t fields[] = {pts, duration, length};
    subtitle_bridge_record(work, 1, fields, 3, (const uint8_t *)line, length);
}

/* The ASS decoder event's Text field, after its eight structural commas.
 * For a plain text cue only, resolve the ASS line break and space escapes
 * and remove the decoder's override blocks from that field. */
static char *subtitle_bridge_visible_text(const char *line)
{
    for (int i = 0; i < 8; ++i) {
        line = strchr(line, ',');
        if (!line)
            return NULL;
        ++line;
    }
    char *text = av_malloc(strlen(line) + 1);
    if (!text)
        return NULL;
    char *out = text;
    while (*line) {
        if (line[0] == '{' && line[1] == '\\') {
            const char *end = strchr(line + 2, '}');
            if (end) {
                line = end + 1;
                continue;
            }
        }
        if (line[0] == '\\' && (line[1] == 'N' || line[1] == 'n' || line[1] == 'h')) {
            *out++ = line[1] == 'h' ? ' ' : '\n';
            line += 2;
            continue;
        }
        *out++ = *line++;
    }
    *out = 0;
    return text;
}

/* Matroska's WebVTT mapping stores Settings, Identifier and Comments
 * in a type-1 BlockAdditional. FFmpeg exposes it as an eight-byte ID
 * followed by the original bytes when the track's codec is unknown. */
static void subtitle_bridge_webvtt_fields(ViewWork *work, const AVPacket *packet)
{
    size_t size = 0;
    const uint8_t *extra = av_packet_get_side_data(packet, AV_PKT_DATA_MATROSKA_BLOCKADDITIONAL, &size);
    const uint8_t *data[2] = {NULL, NULL};
    size_t sizes[2] = {0, 0};
    if (extra && size >= 8 && AV_RB64(extra) == 1) {
        const uint8_t *settings = extra + 8;
        const uint8_t *first = memchr(settings, '\n', size - 8);
        const uint8_t *second = first ? memchr(first + 1, '\n', size - (size_t)(first + 1 - extra)) : NULL;
        if (second) {
            data[0] = first + 1;
            sizes[0] = (size_t)(second - data[0]);
            data[1] = settings;
            sizes[1] = (size_t)(first - settings);
        }
    } else {
        data[0] = av_packet_get_side_data(packet, AV_PKT_DATA_WEBVTT_IDENTIFIER, &sizes[0]);
        data[1] = av_packet_get_side_data(packet, AV_PKT_DATA_WEBVTT_SETTINGS, &sizes[1]);
    }
    for (size_t i = 0; i < 2; ++i) {
        if (data[i] && sizes[i]) {
            uint64_t length = sizes[i];
            subtitle_bridge_record(work, 2 + i, &length, 1, data[i], sizes[i]);
        }
    }
}

static int subtitle_bridge_text_event(ViewWork *work, const AVSubtitle *subtitle, const AVPacket *packet,
                             AVRational base, enum AVCodecID codec)
{
    const uint64_t pts = subtitle_bridge_micros(packet->pts, base);
    const uint64_t duration = packet->duration ? subtitle_bridge_micros(packet->duration, base) : BRIDGE_ABSENT;
    for (unsigned i = 0; i < subtitle->num_rects; ++i) {
        const AVSubtitleRect *rect = subtitle->rects[i];
        char *text = NULL;
        if (rect->ass && codec != AV_CODEC_ID_ASS && codec != AV_CODEC_ID_SSA) {
            text = subtitle_bridge_visible_text(rect->ass);
            if (!text)
                return AVERROR_INVALIDDATA;
        }
        subtitle_bridge_cue(work, pts, duration, text ? text : rect->ass ? rect->ass : rect->text);
        av_free(text);
        if (codec == AV_CODEC_ID_WEBVTT)
            subtitle_bridge_webvtt_fields(work, packet);
    }
    return 0;
}

static void subtitle_bridge_bitmap_event(ViewWork *work, const AVSubtitle *subtitle, enum AVCodecID codec)
{
    int dvd = codec == AV_CODEC_ID_DVD_SUBTITLE;
    uint64_t fields[] = {(uint64_t)subtitle->pts, dvd ? subtitle->start_display_time : 0,
                         dvd ? subtitle->end_display_time : BRIDGE_ABSENT, subtitle->num_rects};
    subtitle_bridge_record(work, 4, fields, 4, NULL, 0);
    for (unsigned i = 0; i < subtitle->num_rects; ++i) {
        const AVSubtitleRect *rect = subtitle->rects[i];
        const uint32_t *palette = rect->type == SUBTITLE_BITMAP ? (const uint32_t *)rect->data[1] : NULL;
        int colors = palette ? rect->nb_colors : 0;
        uint64_t geometry[] = {(uint64_t)rect->x, (uint64_t)rect->y, (uint64_t)rect->w, (uint64_t)rect->h,
                               (rect->flags & AV_SUBTITLE_FLAG_FORCED) != 0, (uint64_t)colors};
        subtitle_bridge_record(work, 5, geometry, 6, NULL, 0);
        for (int color = 0; color < colors; ++color) {
            uint64_t word = palette[color];
            subtitle_bridge_record(work, 6, &word, 1, NULL, 0);
        }
        if (rect->type == SUBTITLE_BITMAP && rect->data[0] && rect->w > 0) {
            uint64_t width = (uint64_t)rect->w;
            for (int y = 0; y < rect->h; ++y)
                subtitle_bridge_record(work, 8, &width, 1, rect->data[0] + (size_t)rect->linesize[0] * (size_t)y, (size_t)width);
        }
    }
}

typedef struct {
    AVCodecContext *decoder;
    ViewWork *work;
    enum AVCodecID codec;
    AVRational base;
    int text, probe, draining;
} BridgeDecode;

static int subtitle_bridge_decode(BridgeDecode *run, AVPacket *packet)
{
    AVSubtitle event = {0};
    int got = 0;
    if (run->probe && !run->draining) {
        uint64_t fields[] = {subtitle_bridge_micros(packet->pts, run->base),
                             packet->duration ? subtitle_bridge_micros(packet->duration, run->base) : BRIDGE_ABSENT};
        subtitle_bridge_record(run->work, 7, fields, 2, packet->data, (size_t)packet->size);
    }
    int consumed = avcodec_decode_subtitle2(run->decoder, &event, &got, packet);
    if (consumed < 0 && run->work->trace)
        printf("# reference decoder refused %d\n", consumed);
    if (got && run->work->trace) {
        SubWork diagnostic = {0};
        sub_fold_event(&diagnostic, &event, 0, 1);
    }
    int result = got;
    if (got && run->text) {
        int rc = subtitle_bridge_text_event(run->work, &event, packet, run->base, run->codec);
        if (rc < 0)
            result = rc;
    } else if (got && (run->codec == AV_CODEC_ID_EIA_608 || run->codec == AV_CODEC_ID_DVB_TELETEXT)) {
        uint64_t duration = event.end_display_time == UINT32_MAX ? BRIDGE_ABSENT :
                            (uint64_t)(event.end_display_time - event.start_display_time) * 1000;
        if (!event.num_rects) {
            uint64_t fields[] = {(uint64_t)event.pts, duration};
            subtitle_bridge_record(run->work, 11, fields, 2, NULL, 0);
        }
        for (unsigned i = 0; i < event.num_rects; ++i) {
            const AVSubtitleRect *rect = event.rects[i];
            if (rect->ass || rect->text)
                subtitle_bridge_cue(run->work, (uint64_t)event.pts, duration, rect->ass ? rect->ass : rect->text);
            else {
                uint64_t fields[] = {(uint64_t)event.pts, duration};
                subtitle_bridge_record(run->work, 11, fields, 2, NULL, 0);
            }
        }
    } else if (got) {
        subtitle_bridge_bitmap_event(run->work, &event, run->codec);
    }
    avsubtitle_free(&event);
    return result;
}

static int subtitle_bridge_open_video(AVCodecContext **context, const AVStream *stream)
{
    const AVCodec *codec = avcodec_find_decoder(stream->codecpar->codec_id);
    if (!codec)
        return AVERROR_DECODER_NOT_FOUND;
    *context = avcodec_alloc_context3(codec);
    if (!*context)
        return AVERROR(ENOMEM);
    int rc = avcodec_parameters_to_context(*context, stream->codecpar);
    if (rc < 0)
        return rc;
    (*context)->pkt_timebase = stream->time_base;
    (*context)->thread_count = 1;
    return avcodec_open2(*context, codec, NULL);
}

static int subtitle_bridge_decode_video(BridgeDecode *run, AVCodecContext *video, AVFrame *frame,
                               AVPacket *packet, AVPacket *caption)
{
    int rc = avcodec_send_packet(video, packet);
    if (rc < 0 && rc != AVERROR_INVALIDDATA)
        return rc;
    while ((rc = avcodec_receive_frame(video, frame)) >= 0) {
        const AVFrameSideData *sd = av_frame_get_side_data(frame, AV_FRAME_DATA_A53_CC);
        if (sd && sd->size) {
            av_packet_unref(caption);
            int status = av_new_packet(caption, (int)sd->size);
            if (status < 0)
                return status;
            memcpy(caption->data, sd->data, sd->size);
            caption->pts = caption->dts = frame->pts != AV_NOPTS_VALUE ? frame->pts : frame->best_effort_timestamp;
            caption->duration = frame->duration;
            status = subtitle_bridge_decode(run, caption);
            if (status < 0)
                return status;
        }
        av_frame_unref(frame);
    }
    return rc == AVERROR(EAGAIN) || rc == AVERROR_EOF ? 0 : rc;
}

static enum AVCodecID subtitle_bridge_codec(const char *name)
{
    if (!strcmp(name, "tx3g")) return AV_CODEC_ID_MOV_TEXT;
    if (!strcmp(name, "utf8")) return AV_CODEC_ID_SUBRIP;
    if (!strcmp(name, "ass")) return AV_CODEC_ID_ASS;
    if (!strcmp(name, "vtt")) return AV_CODEC_ID_WEBVTT;
    return sub_codec(name);
}

static int subtitle_bridge_select(const AVFormatContext *format, enum AVCodecID codec, int video,
                         long ordinal, long program)
{
    for (unsigned i = 0; i < format->nb_streams; ++i) {
        const AVCodecParameters *par = format->streams[i]->codecpar;
        if (program && format->nb_programs) {
            int found = 0;
            for (unsigned j = 0; j < format->nb_programs; ++j) {
                const AVProgram *p = format->programs[j];
                if (p->program_num != program)
                    continue;
                for (unsigned k = 0; k < p->nb_stream_indexes; ++k)
                    found |= p->stream_index[k] == i;
            }
            if (!found)
                continue;
        }
        int match = video ? (par->codec_id == AV_CODEC_ID_H264 || par->codec_id == AV_CODEC_ID_HEVC ||
                             par->codec_id == AV_CODEC_ID_MPEG2VIDEO || par->codec_id == AV_CODEC_ID_MPEG1VIDEO) :
                    par->codec_id == codec || (codec == AV_CODEC_ID_ASS && par->codec_id == AV_CODEC_ID_SSA) ||
                    (codec == AV_CODEC_ID_WEBVTT && par->codec_id == AV_CODEC_ID_NONE && par->codec_type == AVMEDIA_TYPE_SUBTITLE);
        if (match && !ordinal--)
            return (int)i;
    }
    return AVERROR_STREAM_NOT_FOUND;
}

static int subtitle_bridge_options(const ViewCase *input, AVDictionary **options, enum AVCodecID codec)
{
    if (codec == AV_CODEC_ID_DVB_TELETEXT && av_dict_set(options, "txt_format", "ass", 0) < 0)
        return AVERROR(ENOMEM);
    for (int i = 5; i < input->argc; ++i) {
        if (strncmp(input->argv[i], "opt:", 4))
            return AVERROR(EINVAL);
        char *key = av_strdup(input->argv[i] + 4);
        char *value = key ? strchr(key, '=') : NULL;
        if (!value) {
            av_free(key);
            return AVERROR(EINVAL);
        }
        *value++ = 0;
        /* A descriptor ordinal is a candidate-side source selector. The
         * demuxer has already merged or replaced descriptors in its extradata. */
        int bridge_only = !strcmp(key, "descriptor") || !strcmp(key, "progressive") ||
                          !strcmp(key, "probe_packets");
        int rc = bridge_only ? 0 : av_dict_set(options, key, value, 0);
        av_free(key);
        if (rc < 0)
            return rc;
    }
    return 0;
}

static int subtitle_bridge_consume(const ViewCase *input, ViewWork *work)
{
    BridgeIo source = {.data = input->data, .size = input->size};
    AVFormatContext *format = avformat_alloc_context();
    uint8_t *buffer = av_malloc(32 * 1024);
    AVIOContext *io = buffer ? avio_alloc_context(buffer, 32 * 1024, 0, &source, subtitle_bridge_read, NULL, subtitle_bridge_seek) : NULL;
    AVCodecContext *decoder = NULL, *video = NULL;
    AVPacket *packet = av_packet_alloc(), *caption = av_packet_alloc();
    AVFrame *frame = NULL;
    AVDictionary *options = NULL;
    int rc = AVERROR(ENOMEM);
    if (!format || !io || !packet || !caption)
        goto done;
    format->pb = io;
    format->flags |= AVFMT_FLAG_CUSTOM_IO;
    const char *carrier = input->argv[1];
    const char *family = !strcmp(carrier, "mov") ? "mov" :
                         !strcmp(carrier, "mkv") ? "matroska" : "mpegts";
    const AVInputFormat *demuxer = av_find_input_format(family);
    if (!demuxer) {
        rc = AVERROR_DEMUXER_NOT_FOUND;
        goto done;
    }
    if ((rc = avformat_open_input(&format, NULL, demuxer, NULL)) < 0)
        goto done;
    if ((rc = avformat_find_stream_info(format, NULL)) < 0)
        goto done;
    enum AVCodecID codec = subtitle_bridge_codec(input->argv[2]);
    int from_video = codec == AV_CODEC_ID_EIA_608;
    int selected = subtitle_bridge_select(format, codec, from_video, strtol(input->argv[3], NULL, 10), strtol(input->argv[4], NULL, 10));
    if (selected < 0) {
        rc = selected;
        goto done;
    }
    AVStream *stream = format->streams[selected];
    const AVCodec *implementation = avcodec_find_decoder(codec);
    if (!implementation || !(decoder = avcodec_alloc_context3(implementation))) {
        rc = AVERROR_DECODER_NOT_FOUND;
        goto done;
    }
    if (!from_video) {
        if ((rc = avcodec_parameters_to_context(decoder, stream->codecpar)) < 0)
            goto done;
        decoder->codec_id = codec;
    } else {
        if (!(frame = av_frame_alloc())) {
            rc = AVERROR(ENOMEM);
            goto done;
        }
        if ((rc = subtitle_bridge_open_video(&video, stream)) < 0)
            goto done;
    }
    decoder->pkt_timebase = stream->time_base;
    if ((rc = subtitle_bridge_options(input, &options, codec)) < 0 ||
        (rc = avcodec_open2(decoder, implementation, &options)) < 0)
        goto done;
    if (av_dict_count(options)) {
        rc = AVERROR_OPTION_NOT_FOUND;
        goto done;
    }
    if (from_video || codec == AV_CODEC_ID_DVB_TELETEXT) {
        uint64_t length = decoder->subtitle_header ? strlen((const char *)decoder->subtitle_header) : 0;
        subtitle_bridge_record(work, 10, &length, 1, decoder->subtitle_header, (size_t)length);
    }
    BridgeDecode run = {.decoder = decoder, .work = work, .codec = codec, .base = stream->time_base,
                        .text = !strcmp(input->argv[0], "text"),
                        .probe = !strcmp(input->argv[0], "mkv-bitmap-probe")};
    while ((rc = av_read_frame(format, packet)) >= 0) {
        if (packet->stream_index == selected) {
            if (work->trace && !strcmp(carrier, "mkv") && codec == AV_CODEC_ID_DVB_SUBTITLE)
                printf("# Matroska DVB demux DTS %" PRId64 "\n", packet->dts);
            rc = from_video ? subtitle_bridge_decode_video(&run, video, frame, packet, caption) : subtitle_bridge_decode(&run, packet);
        }
        av_packet_unref(packet);
        if (rc < 0)
            goto done;
    }
    if (rc != AVERROR_EOF)
        goto done;
    rc = 0;
    if (from_video && (rc = subtitle_bridge_decode_video(&run, video, frame, NULL, caption)) < 0)
        goto done;
    run.draining = 1;
    for (int i = 0; i < SUB_MAX_DRAIN; ++i) {
        int result = subtitle_bridge_decode(&run, packet);
        if (result <= 0) {
            rc = result;
            break;
        }
    }
done:
    if (work->trace && rc < 0)
        printf("# reference: %d\n", rc);
    uint64_t failure = rc < 0;
    subtitle_bridge_record(work, 9, &failure, 1, NULL, 0);
    avcodec_free_context(&decoder);
    avcodec_free_context(&video);
    av_frame_free(&frame);
    av_packet_free(&packet);
    av_packet_free(&caption);
    av_dict_free(&options);
    avformat_close_input(&format);
    if (io) {
        av_freep(&io->buffer);
        avio_context_free(&io);
    } else {
        av_free(buffer);
    }
    return 0;
}

static int playback_subtitles_prepare(ViewCase *input)
{
    if (input->argc < 5 || input->argc > 12 || subtitle_bridge_codec(input->argv[2]) == AV_CODEC_ID_NONE)
        return 2;
    for (int i = 3; i < 5; ++i)
        if (!*input->argv[i] || strspn(input->argv[i], "0123456789") != strlen(input->argv[i]))
            return 2;
    input->name = !strcmp(input->prefix, "text-bridge-") ? "text-subtitle-bridge" : "subtitle-bridge";
    input->run = subtitle_bridge_consume;
    return 0;
}

/* The wrapped link identifies host-dependent conversions using the same
 * interposed C library calls as the standalone subtitle receiver reference. */
static int playback_subtitle_command(int argc, char **argv)
{
    if (argc < 8)
        return 2;
    const char *mode = argv[1] + strlen("subtitle-bridge-");
    int bench = !strcmp(mode, "bench"), memory = !strcmp(mode, "memory"), trace = !strcmp(mode, "trace");
    if (!bench && !memory && !trace && strcmp(mode, "fold"))
        return 2;
    unsigned long iterations = 1;
    int first = 3;
    if (bench) {
        const char *count = argv[first++];
        if (!*count || strspn(count, "0123456789") != strlen(count))
            return 2;
        iterations = strtoul(count, NULL, 10);
        if (!iterations || iterations > 1000000)
            return 2;
    }
    ViewCase input = {.prefix = "subtitle-bridge-", .argc = argc - first, .argv = argv + first};
    if (playback_subtitles_prepare(&input))
        return 2;
    input.data = view_load(argv[2], &input.size);
    if (!input.data)
        return 2;
    if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18) {
        free((void *)input.data);
        return 2;
    }
#ifdef TRIERGON_PLAYBACK_SUBTITLE_WRAP
    atomic_store(&sub_host_dependent, false);
#endif
    uint64_t start = bench ? view_now() : 0;
    ViewWork total = {0};
    for (unsigned long i = 0; i < iterations; ++i) {
        ViewWork once = {.trace = trace};
        input.run(&input, &once);
        total.packets += once.packets;
        total.bytes += once.bytes;
        fold(&total.digest, once.digest);
    }
    uint64_t elapsed = bench ? view_now() - start : 0;
    free((void *)input.data);
    if (memory && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16)
        return 2;
    const char *host = "";
#ifdef TRIERGON_PLAYBACK_SUBTITLE_WRAP
    if (atomic_load(&sub_host_dependent))
        host = ",\"host_dependent\":true";
#endif
    printf("{\"schema\":1,\"operation\":\"%s\",\"iterations\":%lu,\"packets\":%" PRIu64
           ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true%s}\n",
           input.name, iterations, total.packets, total.bytes, total.digest, elapsed, host);
    return 0;
}
