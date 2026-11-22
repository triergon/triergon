/* Framing via the pinned FFmpeg public bitstream filters and ADTS muxer. */
#include <libavcodec/bsf.h>
#include <libavutil/intreadwrite.h>
#include <libavutil/mathematics.h>
#include <libavutil/mem.h>

typedef struct {
    const uint8_t *data;
    int64_t length, at;
    AVFormatContext *format;
    AVIOContext *io;
} BridgeInput;

static int bridge_read(void *opaque, uint8_t *buffer, int requested)
{
    BridgeInput *source = opaque;
    int64_t available = source->length - source->at;
    if (!available)
        return AVERROR_EOF;
    int count = available < requested ? (int)available : requested;
    memcpy(buffer, source->data + source->at, (size_t)count);
    source->at += count;
    return count;
}

static int64_t bridge_seek(void *opaque, int64_t offset, int whence)
{
    BridgeInput *source = opaque;
    whence &= ~AVSEEK_FORCE;
    if (whence == AVSEEK_SIZE)
        return source->length;
    int64_t base;
    switch (whence) {
    case SEEK_SET: base = 0; break;
    case SEEK_CUR: base = source->at; break;
    case SEEK_END: base = source->length; break;
    default: return AVERROR(EINVAL);
    }
    if (offset < -base || offset > source->length - base)
        return AVERROR(EINVAL);
    return source->at = base + offset;
}

static int bridge_open(BridgeInput *source, const ViewCase *input, int mp4, int raw)
{
    const int window = 32 * 1024;
    *source = (BridgeInput){.data = input->data, .length = (int64_t)input->size};
    uint8_t *buffer = av_malloc(window);
    if (!buffer)
        return AVERROR(ENOMEM);
    source->io = avio_alloc_context(buffer, window, 0, source, bridge_read, NULL, bridge_seek);
    if (!source->io) {
        av_free(buffer);
        return AVERROR(ENOMEM);
    }
    source->format = avformat_alloc_context();
    if (!source->format)
        return AVERROR(ENOMEM);
    source->format->pb = source->io;
    source->format->flags |= AVFMT_FLAG_CUSTOM_IO;
    if (raw)
        source->format->flags |= AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN;
    AVDictionary *options = NULL;
    if (mp4)
        av_dict_set(&options, "ignore_editlist", "1", 0);
    const AVInputFormat *demuxer = av_find_input_format(mp4 ? "mov" : "matroska");
    int status = demuxer
        ? avformat_open_input(&source->format, NULL, demuxer, &options)
        : AVERROR_DEMUXER_NOT_FOUND;
    av_dict_free(&options);
    return status;
}

static void bridge_close(BridgeInput *source)
{
    avformat_close_input(&source->format);
    if (source->io) {
        av_freep(&source->io->buffer);
        avio_context_free(&source->io);
    }
}

static void bridge_row(ViewWork *work, uint64_t kind, const uint64_t *words,
                       size_t count, const uint8_t *bytes, size_t size, int payload)
{
    uint64_t framed[12];
    if (count)
        memcpy(framed, words, count * sizeof(*words));
    if (payload)
        framed[count] = size;
    view_record(work, kind, framed, count + (size_t)payload, bytes, size);
    if (work->trace) {
        printf("%" PRIu64, kind);
        for (size_t i = 0; i < count; ++i)
            printf(" %" PRIu64, words[i]);
        if (payload) {
            printf(" %zu", size);
            if (size)
                putchar(' ');
            for (size_t i = 0; i < size; ++i)
                printf("%02x", bytes[i]);
        }
        putchar('\n');
    }
}

static int bridge_end(ViewWork *work, int status)
{
    if (status == AVERROR(ENOMEM) || status == AVERROR_DEMUXER_NOT_FOUND ||
        status == AVERROR_BSF_NOT_FOUND || status == AVERROR_MUXER_NOT_FOUND)
        return 2;
    if (status < 0 && work->trace) {
        char message[AV_ERROR_MAX_STRING_SIZE];
        fprintf(stderr, "%s\n", av_make_error_string(message, sizeof(message), status));
    }
    bridge_row(work, status < 0 ? 8 : 7, NULL, 0, NULL, 0, 0);
    return 0;
}

static size_t bridge_time(uint64_t *words, int64_t value, AVRational scale)
{
    words[0] = value != AV_NOPTS_VALUE;
    if (value == AV_NOPTS_VALUE)
        return 1;
    words[1] = (uint64_t)av_rescale_q(value, scale, (AVRational){1, 1000000000});
    return 2;
}

static void bridge_packet(ViewWork *work, const AVPacket *packet, AVRational base,
                          const uint8_t *data, size_t size, int mp4, int changed)
{
    uint64_t words[7] = {(uint64_t)packet->stream_index};
    size_t count = 1;
    count += bridge_time(words + count, packet->pts, base);
    count += bridge_time(words + count, mp4 ? packet->dts : AV_NOPTS_VALUE, base);
    words[count++] = !!(packet->flags & AV_PKT_FLAG_KEY);
    words[count++] = changed;
    bridge_row(work, 1, words, count, data, size, 1);
}

typedef struct {
    uint8_t frame[8191];
    size_t length;
    int64_t frames;
    AVFormatContext *format;
    AVIOContext *io;
} BridgeAdts;

static int bridge_adts_write(void *opaque, const uint8_t *data, int size)
{
    BridgeAdts *adts = opaque;
    if (size < 0 || (size_t)size > sizeof(adts->frame) - adts->length)
        return AVERROR_INVALIDDATA;
    memcpy(adts->frame + adts->length, data, (size_t)size);
    adts->length += (size_t)size;
    return size;
}

static int bridge_adts_open(BridgeAdts *adts, const AVStream *stream)
{
    const int window = 4096;
    int status = avformat_alloc_output_context2(&adts->format, NULL, "adts", NULL);
    if (status < 0)
        return av_guess_format("adts", NULL, NULL) ? status : AVERROR_MUXER_NOT_FOUND;
    AVStream *output = avformat_new_stream(adts->format, NULL);
    if (!output)
        return AVERROR(ENOMEM);
    status = avcodec_parameters_copy(output->codecpar, stream->codecpar);
    if (status < 0)
        return status;
    output->time_base = stream->time_base;
    uint8_t *buffer = av_malloc(window);
    if (!buffer)
        return AVERROR(ENOMEM);
    adts->io = avio_alloc_context(buffer, window, 1, adts, NULL, bridge_adts_write, NULL);
    if (!adts->io) {
        av_free(buffer);
        return AVERROR(ENOMEM);
    }
    adts->format->pb = adts->io;
    status = avformat_write_header(adts->format, NULL);
    return status < 0 ? status : adts->io->error;
}

static int bridge_adts_packet(BridgeAdts *adts, AVPacket *packet)
{
    adts->length = 0;
    int index = packet->stream_index;
    const int64_t pts = packet->pts, dts = packet->dts, duration = packet->duration;
    packet->stream_index = 0;
    /* The ADTS output carries no timestamps. Give its muxer an independent
     * monotonic clock and restore the source packet for the comparison. */
    packet->pts = packet->dts = adts->frames++;
    packet->duration = 0;
    int status = av_write_frame(adts->format, packet);
    avio_flush(adts->io);
    packet->stream_index = index;
    packet->pts = pts;
    packet->dts = dts;
    packet->duration = duration;
    return status < 0 ? status : adts->io->error;
}

static void bridge_adts_close(BridgeAdts *adts)
{
    avformat_free_context(adts->format);
    if (adts->io) {
        av_freep(&adts->io->buffer);
        avio_context_free(&adts->io);
    }
}

static int bridge_bsf_open(AVBSFContext **bsf, const AVStream *stream, const char *name)
{
    const AVBitStreamFilter *filter = av_bsf_get_by_name(name);
    if (!filter)
        return AVERROR_BSF_NOT_FOUND;
    int status = av_bsf_alloc(filter, bsf);
    if (status >= 0)
        status = avcodec_parameters_copy((*bsf)->par_in, stream->codecpar);
    if (status >= 0) {
        (*bsf)->time_base_in = stream->time_base;
        status = av_bsf_init(*bsf);
    }
    return status;
}

static int bridge_stream(const char *text)
{
    if (!*text || strspn(text, "0123456789") != strlen(text))
        return -1;
    unsigned long index = strtoul(text, NULL, 10);
    return index <= INT_MAX ? (int)index : -1;
}

static enum AVCodecID bridge_codec(const char *name)
{
    if (!strcmp(name, "h264")) return AV_CODEC_ID_H264;
    if (!strcmp(name, "hevc")) return AV_CODEC_ID_HEVC;
    if (!strcmp(name, "aac")) return AV_CODEC_ID_AAC;
    if (!strcmp(name, "av1")) return AV_CODEC_ID_AV1;
    return AV_CODEC_ID_NONE;
}

static int bridge_framing(const ViewCase *input, ViewWork *work)
{
    const int mp4 = !strcmp(input->argv[0], "mp4");
    const int index = bridge_stream(input->argv[1]);
    const enum AVCodecID codec = bridge_codec(input->argv[2]);
    const int access = !strcmp(input->prefix, "access-units-");
    BridgeInput source;
    BridgeAdts adts = {0};
    AVBSFContext *bsf = NULL;
    AVPacket *packet = av_packet_alloc(), *filtered = av_packet_alloc();
    int status = bridge_open(&source, input, mp4, 1);
    if (status < 0)
        goto end;
    if (!packet || !filtered) {
        status = AVERROR(ENOMEM);
        goto end;
    }
    if ((unsigned)index >= source.format->nb_streams) {
        status = AVERROR_INVALIDDATA;
        goto end;
    }
    AVStream *stream = source.format->streams[index];
    const int dav1 = codec == AV_CODEC_ID_AV1 && mp4 &&
        stream->codecpar->codec_tag == MKTAG('d', 'a', 'v', '1');
    if (stream->codecpar->codec_id != codec && !dav1) {
        status = AVERROR_INVALIDDATA;
        goto end;
    }
    if (codec == AV_CODEC_ID_AAC)
        status = bridge_adts_open(&adts, stream);
    else if (codec == AV_CODEC_ID_H264 || codec == AV_CODEC_ID_HEVC)
        status = bridge_bsf_open(&bsf, stream, codec == AV_CODEC_ID_H264
                                 ? "h264_mp4toannexb" : "hevc_mp4toannexb");
    int first = 1;
    while (status >= 0 && (status = av_read_frame(source.format, packet)) >= 0) {
        if (packet->stream_index == index) {
            const int changed = first || av_packet_get_side_data(packet, AV_PKT_DATA_NEW_EXTRADATA, NULL);
            first = 0;
            if (adts.format) {
                status = bridge_adts_packet(&adts, packet);
                if (status >= 0) {
                    if (access)
                        bridge_packet(work, packet, stream->time_base, adts.frame, adts.length, mp4, changed);
                    else {
                        uint64_t ordinal = (uint64_t)index;
                        bridge_row(work, 1, &ordinal, 1, adts.frame, adts.length, 1);
                    }
                }
            } else if (bsf) {
                status = av_bsf_send_packet(bsf, packet);
                if (status >= 0) {
                    while ((status = av_bsf_receive_packet(bsf, filtered)) >= 0) {
                        bridge_packet(work, filtered, bsf->time_base_out, filtered->data,
                                      (size_t)filtered->size, mp4, changed);
                        av_packet_unref(filtered);
                    }
                    if (status == AVERROR(EAGAIN))
                        status = 0;
                }
            } else {
                bridge_packet(work, packet, stream->time_base, packet->data, (size_t)packet->size, mp4, changed);
            }
        }
        av_packet_unref(packet);
    }
    if (status == AVERROR_EOF)
        status = first ? AVERROR_INVALIDDATA : 0;
    if (status >= 0 && adts.format)
        status = av_write_trailer(adts.format);
end:
    bridge_adts_close(&adts);
    av_bsf_free(&bsf);
    av_packet_free(&packet);
    av_packet_free(&filtered);
    bridge_close(&source);
    return bridge_end(work, status);
}

static uint64_t bridge_track_code(enum AVCodecID codec)
{
    switch (codec) {
    case AV_CODEC_ID_H264: return 1;
    case AV_CODEC_ID_HEVC: return 2;
    case AV_CODEC_ID_AAC: return 3;
    case AV_CODEC_ID_AC3: return 4;
    case AV_CODEC_ID_EAC3: return 5;
    case AV_CODEC_ID_HDMV_PGS_SUBTITLE: return 6;
    case AV_CODEC_ID_DVB_SUBTITLE: return 7;
    case AV_CODEC_ID_AV1: return 8;
    case AV_CODEC_ID_DVD_SUBTITLE: return 9;
    case AV_CODEC_ID_VP9: return 10;
    default: return 0;
    }
}

static void bridge_replay(ViewWork *work, const AVPacket *packet, AVRational base)
{
    uint64_t words[5] = {(uint64_t)packet->stream_index};
    size_t count = 1;
    count += bridge_time(words + count, packet->pts, base);
    /* A Matroska Block has a presentation timestamp. DTS synthesized by
     * demux or decoding, including DTS=PTS for DVB, is not a source value. */
    words[count++] = 0;
    words[count++] = !!(packet->flags & AV_PKT_FLAG_KEY);
    bridge_row(work, 1, words, count, packet->data, (size_t)packet->size, 1);
}

static void bridge_dolby_duration(ViewWork *work, const AVPacket *packet,
                                  const AVStream *stream)
{
    AVCodecParameters *codec = stream->codecpar;
    int64_t duration = packet->duration;
    if (!duration && codec->sample_rate > 0) {
        int samples = av_get_audio_frame_duration2(codec, packet->size);
        if (samples > 0)
            duration = av_rescale_q(samples, (AVRational){1, codec->sample_rate}, stream->time_base);
    }
    uint64_t words[3] = {(uint64_t)packet->stream_index};
    size_t count = 1 + bridge_time(words + 1, duration > 0 ? duration : AV_NOPTS_VALUE,
                                   stream->time_base);
    bridge_row(work, 3, words, count, NULL, 0, 0);
}

static int bridge_discovery(const ViewCase *input, ViewWork *work)
{
    const int compact = !strcmp(input->argv[0], "compact");
    const int seek = !strcmp(input->argv[0], "seek");
    BridgeInput probe, source = {0};
    AVPacket *packet = av_packet_alloc();
    int status = bridge_open(&probe, input, 0, 0);
    if (status >= 0 && !packet)
        status = AVERROR(ENOMEM);
    if (status < 0)
        goto end;
    status = avformat_find_stream_info(probe.format, NULL);
    if (status < 0)
        goto end;
    int video = -1;
    for (unsigned i = 0; i < probe.format->nb_streams; ++i) {
        const AVCodecParameters *codec = probe.format->streams[i]->codecpar;
        const uint64_t words[] = {i, bridge_track_code(codec->codec_id),
                                  (uint64_t)codec->width, (uint64_t)codec->height};
        bridge_row(work, 5, words, 4, codec->extradata, (size_t)codec->extradata_size, 1);
        if (codec->codec_id == AV_CODEC_ID_HEVC && video < 0)
            video = (int)i;
    }
    status = bridge_open(&source, input, 0, 1);
    if (status < 0)
        goto end;
    if (seek) {
        if (video < 0) {
            status = AVERROR_INVALIDDATA;
            goto end;
        }
        const AVRational base = source.format->streams[video]->time_base;
        int64_t target = av_rescale_q(700, (AVRational){1, 1000}, base);
        status = av_seek_frame(source.format, video, target, AVSEEK_FLAG_BACKWARD);
    }
    uint64_t landed = 0;
    while (status >= 0 && (status = av_read_frame(source.format, packet)) >= 0) {
        AVStream *stream = source.format->streams[packet->stream_index];
        if (!seek || packet->stream_index == video) {
            if (seek && (packet->flags & AV_PKT_FLAG_KEY))
                landed = 1;
            if (!seek || landed) {
                enum AVCodecID codec = stream->codecpar->codec_id;
                if (!compact && (codec == AV_CODEC_ID_AC3 || codec == AV_CODEC_ID_EAC3))
                    bridge_dolby_duration(work, packet, stream);
                bridge_replay(work, packet, stream->time_base);
            }
        }
        av_packet_unref(packet);
    }
    if (status == AVERROR_EOF) {
        if (seek)
            bridge_row(work, 3, &landed, 1, NULL, 0, 0);
        status = 0;
    }
end:
    av_packet_free(&packet);
    bridge_close(&source);
    bridge_close(&probe);
    return bridge_end(work, status);
}

static int playback_bridge_prepare(ViewCase *input)
{
    if (!strcmp(input->prefix, "mkv-discovery-")) {
        if (input->argc != 1 ||
            (strcmp(input->argv[0], "full") && strcmp(input->argv[0], "compact") &&
             strcmp(input->argv[0], "seek")))
            return 2;
        input->name = "bridge-matroska-discovery";
        input->run = bridge_discovery;
        return 0;
    }
    if (input->argc < 3 || input->argc > 4 ||
        (strcmp(input->argv[0], "mp4") && strcmp(input->argv[0], "mkv")) ||
        bridge_stream(input->argv[1]) < 0 || bridge_codec(input->argv[2]) == AV_CODEC_ID_NONE)
        return 2;
    const int access = !strcmp(input->prefix, "access-units-");
    if (input->argc == 4 && (!access || strcmp(input->argv[0], "mkv") ||
                            strcmp(input->argv[2], "aac") || strcmp(input->argv[3], "declared")))
        return 2;
    if (!access && (strcmp(input->argv[0], "mp4") || strcmp(input->argv[2], "aac")))
        return 2;
    input->name = access ? "bridge-access-units" : "bridge-mp4-packets";
    input->run = bridge_framing;
    return 0;
}
