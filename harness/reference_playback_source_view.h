/* Read the reference's stream catalogue and forward packets through public AVIO. */
#define now_ns view_now
#include "reference_packet_fold.h"
#undef now_ns

typedef struct {
    const char *demuxer;
    int seekable, ignore_edits, transport;
    uint8_t *entities;
    size_t size, pid_count;
    unsigned pids[64];
} PlaybackSourceCase;

static int source_view_codec_is_pcm(enum AVCodecID codec)
{
    const AVCodecDescriptor *descriptor = avcodec_descriptor_get(codec);
    return descriptor && !strncmp(descriptor->name, "pcm_", 4);
}

static const char *source_view_codec(const AVCodecParameters *codec)
{
    switch (codec->codec_id) {
    case AV_CODEC_ID_MPEG1VIDEO: return "mpeg2video";
    case AV_CODEC_ID_MP1:
    case AV_CODEC_ID_MP2:
    case AV_CODEC_ID_MP3: return "mpeg_audio";
    case AV_CODEC_ID_NONE: return "other";
    default: return source_view_codec_is_pcm(codec->codec_id) ? "pcm" : avcodec_get_name(codec->codec_id);
    }
}

static uint64_t source_view_kind(enum AVMediaType kind)
{
    switch (kind) {
    case AVMEDIA_TYPE_VIDEO: return 1;
    case AVMEDIA_TYPE_AUDIO: return 2;
    case AVMEDIA_TYPE_SUBTITLE: return 3;
    case AVMEDIA_TYPE_DATA:
    case AVMEDIA_TYPE_ATTACHMENT: return 4;
    default: return 0;
    }
}

static void source_view_string(ViewWork *work, const uint8_t *data, size_t size)
{
    fold(&work->digest, size);
    if (size)
        fold_bytes(&work->digest, data, size);
    work->bytes += size;
}

static void source_view_hex(const uint8_t *data, size_t size)
{
    if (!size)
        putchar('-');
    for (size_t i = 0; i < size; ++i)
        printf("%02x", data[i]);
}

static uint64_t source_view_track_id(const AVStream *stream)
{
    return stream->id > 0 ? (uint64_t)stream->id : (uint64_t)stream->index + 1;
}

static void source_view_track(ViewWork *work, const AVStream *stream, const PlaybackSourceCase *source)
{
    const AVCodecParameters *parameters = stream->codecpar;
    const char *codec = source_view_codec(parameters);
    const uint64_t id = source_view_track_id(stream);
    const uint64_t kind = source_view_kind(parameters->codec_type);
    const AVDictionaryEntry *language = av_dict_get(stream->metadata, "language", NULL, 0);
    const uint64_t default_track = 1 + !!(stream->disposition & AV_DISPOSITION_DEFAULT);
    const uint64_t forced = 1 + !!(stream->disposition & AV_DISPOSITION_FORCED);
    const uint32_t tag = parameters->codec_tag;
    const uint8_t tag_bytes[] = {tag, tag >> 8, tag >> 16, tag >> 24};
    const size_t tag_size = source->ignore_edits ? 4 : source->transport && tag <= 255 ? 1 : tag ? 4 : 0;
    const size_t config_size = parameters->extradata_size > 0 ? (size_t)parameters->extradata_size : 0;
    fold(&work->digest, 2);
    fold(&work->digest, id);
    fold(&work->digest, kind);
    source_view_string(work, (const uint8_t *)codec, strlen(codec));
    source_view_string(work, tag_bytes, tag_size);
    source_view_string(work, parameters->extradata, config_size);
    fold(&work->digest, language != NULL);
    if (language)
        source_view_string(work, (const uint8_t *)language->value, strlen(language->value));
    fold(&work->digest, default_track);
    fold(&work->digest, forced);
    ++work->packets;
    if (work->trace) {
        printf("2 %" PRIu64 " %" PRIu64 " %s ", id, kind, codec);
        source_view_hex(tag_bytes, tag_size);
        putchar(' ');
        source_view_hex(parameters->extradata, config_size);
        if (language)
            printf(" Some(\"%s\")", language->value);
        else
            printf(" None");
        printf(" %" PRIu64 " %" PRIu64 "\n", default_track, forced);
    }
}

static int source_view_open_mp4(AVFormatContext **format, PacketInput *input, int seekable)
{
    uint8_t *buffer = av_malloc(input->quantum);
    if (!buffer)
        return AVERROR(ENOMEM);
    AVIOContext *io = avio_alloc_context(buffer, input->quantum, 0, input, packet_input_read,
                                         NULL, seekable ? packet_input_seek : NULL);
    if (!io) {
        av_free(buffer);
        return AVERROR(ENOMEM);
    }
    *format = avformat_alloc_context();
    if (!*format) {
        av_freep(&io->buffer);
        avio_context_free(&io);
        return AVERROR(ENOMEM);
    }
    (*format)->pb = io;
    (*format)->flags |= AVFMT_FLAG_CUSTOM_IO;
    AVDictionary *options = NULL;
    int rc = av_dict_set(&options, "ignore_editlist", "1", 0);
    if (rc >= 0)
        rc = avformat_open_input(format, NULL, av_find_input_format("mov"), &options);
    av_dict_free(&options);
    if (rc < 0) {
        avformat_free_context(*format);
        *format = NULL;
        av_freep(&io->buffer);
        avio_context_free(&io);
    }
    return rc;
}

static int source_view_selected(const AVFormatContext *format, const AVStream *stream,
                                 const PlaybackSourceCase *source)
{
    if (!source->transport)
        return 1;
    if (source->pid_count) {
        int selected = 0;
        for (size_t i = 0; i < source->pid_count; ++i)
            selected |= stream->id == (int)source->pids[i];
        if (!selected)
            return 0;
    }
    for (unsigned p = 0; p < format->nb_programs; ++p) {
        const AVProgram *program = format->programs[p];
        if (program->program_num != 1)
            continue;
        for (unsigned i = 0; i < program->nb_stream_indexes; ++i)
            if (program->stream_index[i] == (unsigned)stream->index)
                return 1;
    }
    return 0;
}

static int source_view_run(const ViewCase *input, ViewWork *work)
{
    const PlaybackSourceCase *source = input->prepared;
    const AVInputFormat *demuxer = av_find_input_format(source->demuxer);
    if (!demuxer)
        return 2;
    PacketInput reader = {
        .data = source->entities ? source->entities : input->data,
        .size = source->entities ? (int64_t)source->size : (int64_t)input->size,
        .quantum = 32768
    };
    AVFormatContext *format = NULL;
    const int ts_flags = AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN;
    int rc = source->ignore_edits ? source_view_open_mp4(&format, &reader, source->seekable)
        : packet_open(&format, &reader, source->seekable, demuxer, source->transport ? ts_flags : 0);
    if (rc == AVERROR(ENOMEM))
        return 2;
    if (rc < 0) {
        view_record(work, 8, NULL, 0, NULL, 0);
        if (work->trace)
            puts("8");
        return 0;
    }
    for (unsigned i = 0; i < format->nb_streams; ++i) {
        if (source_view_selected(format, format->streams[i], source))
            source_view_track(work, format->streams[i], source);
    }
    AVPacket *packet = av_packet_alloc();
    if (!packet) {
        packet_close(&format);
        return 2;
    }
    while ((rc = av_read_frame(format, packet)) >= 0) {
        AVStream *stream = format->streams[packet->stream_index];
        if (source_view_selected(format, stream, source)) {
            int64_t pts = packet->pts, dts = packet->dts, duration = packet->duration;
            if (!source->transport && !source->ignore_edits) {
                const AVRational ns = {1, 1000000000};
                if (pts != AV_NOPTS_VALUE)
                    pts = av_rescale_q(pts, stream->time_base, ns);
                if (dts != AV_NOPTS_VALUE)
                    dts = av_rescale_q(dts, stream->time_base, ns);
                duration = av_rescale_q(duration, stream->time_base, ns);
            }
            fold(&work->digest, 1);
            PacketWork view = {.digest = work->digest, .trace = work->trace};
            if (work->trace)
                printf("1 ");
            packet_fold(&view, source_view_track_id(stream), pts, dts,
                        duration > 0 ? (uint64_t)duration : 0, (uint64_t)packet->flags,
                        packet->data, (size_t)packet->size);
            work->digest = view.digest;
            work->packets += view.packets;
            work->bytes += view.bytes;
        }
        av_packet_unref(packet);
    }
    av_packet_free(&packet);
    packet_close(&format);
    if (rc == AVERROR(ENOMEM))
        return 2;
    uint64_t terminal = rc == AVERROR_EOF ? 7 : 8;
    view_record(work, terminal, NULL, 0, NULL, 0);
    if (work->trace)
        printf("%" PRIu64 "\n", terminal);
    return 0;
}

static void source_view_release(void *opaque)
{
    PlaybackSourceCase *source = opaque;
    free(source->entities);
    free(source);
}

static int source_view_add_entity(const ViewCase *input, PlaybackSourceCase *source)
{
    const char *dir = input->argv[1], *name = input->argv[2];
    if (!*name || name[0] == '-' || strpbrk(name, "/\\"))
        return 2;
    size_t path_size = strlen(dir) + strlen(name) + 2;
    char *path = malloc(path_size);
    if (!path)
        return 2;
    snprintf(path, path_size, "%s/%s", dir, name);
    size_t tail_size = 0;
    uint8_t *tail = view_load(path, &tail_size);
    free(path);
    if (!tail)
        return 2;
    source->size = input->size + tail_size;
    source->entities = malloc(source->size ? source->size : 1);
    if (source->entities) {
        memcpy(source->entities, input->data, input->size);
        memcpy(source->entities + input->size, tail, tail_size);
    }
    free(tail);
    return source->entities ? 0 : 2;
}

static int source_view_parse_pids(const char *pids, PlaybackSourceCase *source)
{
    while (*pids && source->pid_count < 64) {
        char *end = NULL;
        unsigned long pid = strtoul(pids, &end, 10);
        if (pids == end || !pid || pid >= 0x1fff || (*end && *end != ','))
            return 2;
        source->pids[source->pid_count++] = (unsigned)pid;
        if (!*end)
            return 0;
        pids = end + 1;
    }
    return 2;
}

static int playback_source_view_prepare(ViewCase *input)
{
    /* This entry uses the shared row fold and provides its own prefix dispatch. */
    (void)packet_main;
    if (input->argc < 1)
        return 2;
    const char *layout = input->argv[0];
    PlaybackSourceCase source = {0};
    if (!strcmp(layout, "matroska")) {
        source.demuxer = "matroska";
        source.seekable = 1;
    } else if (!strcmp(layout, "mp4") || !strcmp(layout, "mp4-progressive") ||
               !strcmp(layout, "mp4-timing-policy")) {
        source.demuxer = "mov";
        source.seekable = strcmp(layout, "mp4-progressive") != 0;
        source.ignore_edits = 1;
    } else if (!strcmp(layout, "ts188") || !strcmp(layout, "ts192") ||
               !strcmp(layout, "ts-entities")) {
        source.demuxer = "mpegts";
        source.transport = 1;
    } else {
        return 2;
    }
    if (!strcmp(layout, "ts-entities")) {
        if (input->argc != 3 || source_view_add_entity(input, &source))
            return 2;
    } else if (source.transport && input->argc == 2) {
        if (source_view_parse_pids(input->argv[1], &source))
            return 2;
    } else if (input->argc != 1) {
        return 2;
    }
    PlaybackSourceCase *prepared = malloc(sizeof(*prepared));
    if (!prepared) {
        free(source.entities);
        return 2;
    }
    *prepared = source;
    input->prepared = prepared;
    input->release = source_view_release;
    input->name = "playback-source-view";
    input->run = source_view_run;
    return 0;
}
