/* MOV packet and stream comparisons through the public demuxer interface. */
#define now_ns view_now
#include "reference_packet_fold.h"
#undef now_ns
#include <limits.h>

static void mov_view_row(ViewWork *work, uint64_t kind, const uint64_t *words,
                         size_t count, const uint8_t *data, size_t size)
{
    view_record(work, kind, words, count, data, size);
    if (!work->trace)
        return;
    printf("%" PRIu64, kind);
    for (size_t i = 0; i < count; ++i)
        printf(" %" PRIu64, words[i]);
    if (size) {
        uint64_t digest = 0;
        fold_bytes(&digest, data, size);
        printf(" bytes=%zu digest=%" PRIu64, size, digest);
    }
    putchar('\n');
}

static void mov_view_terminal(ViewWork *work, int code, int compare_code)
{
    const uint64_t word = (uint64_t)(int64_t)code;
    mov_view_row(work, code == AVERROR_EOF ? 7 : 8, &word, compare_code, NULL, 0);
}

static void mov_view_packet(ViewWork *work, const AVPacket *packet)
{
    const uint64_t fields[] = {
        packet->stream_index,
        packet->pts != AV_NOPTS_VALUE, (uint64_t)packet->pts,
        packet->dts != AV_NOPTS_VALUE, (uint64_t)packet->dts,
        (uint64_t)packet->duration, (uint64_t)packet->pos, packet->flags, packet->size,
    };
    mov_view_row(work, 1, fields, sizeof(fields) / sizeof(*fields),
                 packet->data, (size_t)packet->size);
}

static void mov_view_string(ViewWork *work, uint64_t field, const uint8_t *value, size_t size)
{
    const uint64_t fields[] = {field, size};
    mov_view_row(work, 3, fields, 2, value, size);
}

static void mov_view_tag(ViewWork *work, const AVStream *stream, uint64_t field, const char *key)
{
    const AVDictionaryEntry *entry = av_dict_get(stream->metadata, key, NULL, 0);
    if (entry)
        mov_view_string(work, field, (const uint8_t *)entry->value, strlen(entry->value));
    else
        mov_view_string(work, field, NULL, 0);
}

static void mov_view_encoded_stream(ViewWork *work, const AVStream *stream)
{
    const AVCodecParameters *par = stream->codecpar;
    const uint64_t fields[] = {
        stream->index, (uint64_t)(int64_t)stream->id, par->codec_type,
        stream->time_base.num, stream->time_base.den, par->seek_preroll,
    };
    mov_view_row(work, 2, fields, sizeof(fields) / sizeof(*fields), NULL, 0);
    const char *codec = avcodec_get_name(par->codec_id);
    mov_view_string(work, 1, (const uint8_t *)codec, strlen(codec));
    mov_view_string(work, 2, par->extradata, (size_t)par->extradata_size);
    mov_view_tag(work, stream, 3, "language");
    mov_view_tag(work, stream, 4, "handler_name");
    mov_view_tag(work, stream, 5, "vendor_id");
    mov_view_tag(work, stream, 6, "creation_time");
    const AVPacketSideData *skip = av_packet_side_data_get(
        par->coded_side_data, par->nb_coded_side_data, AV_PKT_DATA_SKIP_SAMPLES);
    mov_view_string(work, 7, skip ? skip->data : NULL, skip ? skip->size : 0);
}

static int mov_view_read(AVFormatContext *format, ViewWork *work, int encoded)
{
    AVPacket *packet = av_packet_alloc();
    if (!packet)
        return AVERROR(ENOMEM);
    if (encoded) {
        const uint64_t count = format->nb_streams;
        mov_view_row(work, 2, &count, 1, NULL, 0);
        for (unsigned index = 0; index < format->nb_streams; ++index)
            mov_view_encoded_stream(work, format->streams[index]);
    }
    int code;
    while ((code = av_read_frame(format, packet)) >= 0) {
        mov_view_packet(work, packet);
        if (encoded) {
            size_t size = 0;
            const uint8_t *skip = av_packet_get_side_data(packet, AV_PKT_DATA_SKIP_SAMPLES, &size);
            mov_view_string(work, 8, skip, size);
        }
        av_packet_unref(packet);
    }
    av_packet_free(&packet);
    return code;
}

typedef struct {
    uint64_t bytes, digest;
} MovPcmBytes;

static uint64_t mov_pcm_encoding(enum AVCodecID codec)
{
    switch (codec) {
    case AV_CODEC_ID_PCM_U8: return 1;
    case AV_CODEC_ID_PCM_S16BE: return 2;
    case AV_CODEC_ID_PCM_S16LE: return 3;
    case AV_CODEC_ID_PCM_S24BE: return 4;
    case AV_CODEC_ID_PCM_S24LE: return 5;
    case AV_CODEC_ID_PCM_S32BE: return 6;
    case AV_CODEC_ID_PCM_S32LE: return 7;
    case AV_CODEC_ID_PCM_S8: return 8;
    default: return 0;
    }
}

static int qt_pcm_read(AVFormatContext *format, ViewWork *work)
{
    MovPcmBytes *streams = av_calloc(format->nb_streams, sizeof(*streams));
    AVPacket *packet = av_packet_alloc();
    int code = AVERROR(ENOMEM);
    if (!streams || !packet)
        goto done;
    while ((code = av_read_frame(format, packet)) >= 0) {
        MovPcmBytes *pcm = &streams[packet->stream_index];
        fold_bytes(&pcm->digest, packet->data, (size_t)packet->size);
        pcm->bytes += (size_t)packet->size;
        av_packet_unref(packet);
    }
    if (code != AVERROR_EOF)
        goto done;
    for (unsigned index = 0; index < format->nb_streams; ++index) {
        const AVCodecParameters *p = format->streams[index]->codecpar;
        if (p->codec_type != AVMEDIA_TYPE_AUDIO)
            continue;
        const MovPcmBytes *pcm = &streams[index];
        /* MOV leaves block_align unset for the ordinary PCM entries. */
        const int align = p->block_align ? p->block_align
            : (p->ch_layout.nb_channels * p->bits_per_coded_sample) / 8;
        const uint64_t config[] = {
            index, mov_pcm_encoding(p->codec_id), p->ch_layout.nb_channels,
            p->sample_rate, p->bits_per_coded_sample, align,
        };
        mov_view_row(work, 2, config, sizeof(config) / sizeof(*config), NULL, 0);
        const uint64_t frames[] = {
            align > 0 ? pcm->bytes / (uint64_t)align : 0, pcm->bytes, pcm->digest,
        };
        mov_view_row(work, 7, frames, sizeof(frames) / sizeof(*frames), NULL, 0);
        work->bytes += pcm->bytes;
    }
done:
    av_packet_free(&packet);
    av_free(streams);
    return code;
}

static void mov_dts_entries(AVFormatContext *format, ViewWork *work, int check_typed_format)
{
    const uint64_t count = format->nb_streams;
    mov_view_row(work, 2, &count, 1, NULL, 0);
    for (unsigned index = 0; index < format->nb_streams; ++index) {
        const AVCodecParameters *p = format->streams[index]->codecpar;
        if (p->codec_type != AVMEDIA_TYPE_AUDIO)
            continue;
        if (check_typed_format && p->codec_tag != MKTAG('d', 't', 's', 'h')) {
            const uint64_t fields[] = {index, 2, 0, 0};
            mov_view_row(work, 2, fields, sizeof(fields) / sizeof(*fields), NULL, 0);
            continue;
        }
        const int dts = p->codec_id == AV_CODEC_ID_DTS;
        const uint64_t fields[] = {
            index, dts, dts ? p->sample_rate : 0,
            dts && p->frame_size > 0 ? p->frame_size : 0,
        };
        mov_view_row(work, 2, fields, sizeof(fields) / sizeof(*fields), NULL, 0);
    }
}

typedef enum {
    MOV_PCM_SAMPLES, MOV_PCM_PACKETS, MOV_RECOVERY, MOV_DTS, MOV_ENCODED,
} MovViewOperation;

typedef struct {
    MovViewOperation operation;
    int window, check_typed_format;
} MovViewSchedule;

static int isobmff_packets_run(const ViewCase *input, ViewWork *work)
{
    const MovViewSchedule *schedule = input->prepared;
    const int recovery = schedule->operation == MOV_RECOVERY;
    const int encoded = schedule->operation == MOV_ENCODED;
    const int pcm_samples = schedule->operation == MOV_PCM_SAMPLES;
    const int dts = schedule->operation == MOV_DTS;
    const AVInputFormat *mov = av_find_input_format("mov");
    if (!mov)
        return 2;
    PacketInput source = {
        .data = input->data, .size = (int64_t)input->size, .quantum = schedule->window,
    };
    AVFormatContext *format = NULL;
    int code = packet_open(&format, &source, 1, mov, AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN);
    if (code == AVERROR(ENOMEM))
        return 2;
    if (code < 0) {
        mov_view_terminal(work, code, 0);
        return 0;
    }
    if (dts) {
        mov_dts_entries(format, work, schedule->check_typed_format);
        code = AVERROR_EOF;
    } else if (pcm_samples) {
        code = qt_pcm_read(format, work);
    } else {
        code = mov_view_read(format, work, encoded);
    }
    packet_close(&format);
    if (code == AVERROR(ENOMEM))
        return 2;
    if (!dts && (!pcm_samples || code != AVERROR_EOF))
        mov_view_terminal(work, code, recovery);
    return 0;
}

static int isobmff_packets_prepare(ViewCase *input)
{
    /* Only the shared AVIO part of this header is used by the views protocol. */
    (void)packet_fold;
    (void)packet_main;
    const int recovery = !strcmp(input->prefix, "mov-recover-");
    const int encoded = !strcmp(input->prefix, "mov-encoded-");
    const int dts = !strcmp(input->prefix, "dts-entry-");
    const int mismatched = dts && input->argc == 3 &&
        !strcmp(input->argv[2], "mismatched-format");
    if (input->argc != (recovery || encoded || mismatched ? 3 : 2))
        return 2;
    unsigned long arguments[2];
    for (int i = 0; i < 2; ++i) {
        const char *number = input->argv[i];
        if (!*number || strspn(number, "0123456789") != strlen(number))
            return 2;
        arguments[i] = strtoul(number, NULL, 10);
        if (!arguments[i] || arguments[i] > INT_MAX)
            return 2;
    }
    if (recovery && strcmp(input->argv[2], "run") && strcmp(input->argv[2], "direct") &&
        strcmp(input->argv[2], "physical"))
        return 2;
    if (encoded && strcmp(input->argv[2], "metadata") &&
        strcmp(input->argv[2], "recovery") && strcmp(input->argv[2], "dependency"))
        return 2;
    MovViewOperation operation;
    if (recovery) {
        input->name = "mov-recover";
        operation = MOV_RECOVERY;
    } else if (encoded) {
        input->name = "mov-encoded";
        operation = MOV_ENCODED;
    } else if (!strcmp(input->prefix, "qt-pcm-")) {
        input->name = "qt-pcm";
        operation = MOV_PCM_SAMPLES;
    } else if (!strcmp(input->prefix, "mov-pcm-")) {
        input->name = "mov-pcm";
        operation = MOV_PCM_PACKETS;
    } else if (dts) {
        input->name = "dts-entry";
        operation = MOV_DTS;
    } else {
        return 2;
    }
    MovViewSchedule *schedule = malloc(sizeof(*schedule));
    if (!schedule)
        return 2;
    *schedule = (MovViewSchedule){
        .operation = operation, .window = (int)arguments[0], .check_typed_format = mismatched,
    };
    input->prepared = schedule;
    input->release = free;
    input->run = isobmff_packets_run;
    return 0;
}
