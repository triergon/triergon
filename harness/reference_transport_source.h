/* FFmpeg's public packet view of the same selected program and entities as the
 * source, assembler and observer operations. The packet fold is mpegts-pes's. */
#include <errno.h>

enum { TS_SOURCE_AVIO_BYTES = 32 * 1024, TS_SOURCE_MAXIMUM_READ = 64 * 1024 };

enum { TS_SOURCE_MAX_ENTITIES = 64, TS_SOURCE_MAX_INPUT_BYTES = 64 * 1024 * 1024 };

typedef struct {
    const uint8_t *bytes;
    uint8_t *allocation;
    size_t size, base;
    int discontinuity;
} TsViewsEntity;

typedef struct {
    TsViewsEntity *entities;
    size_t count, size;
} TsViewsEntities;

typedef struct {
    const TsViewsEntities *source;
    size_t at, entity, boundary;
    int waiting;
} TsViewsEntityInput;

/* Return the timing flags at the PES start associated with the public packet
 * position. A header may occupy several carriers or contiguous entities. */
static int ts_views_wire_start(const TsViewsEntities *source, int width, int pid,
                               int64_t *position)
{
    if (*position < 0)
        return -1;
    size_t entity = 0;
    size_t start = (size_t)*position;
    while (entity < source->count &&
           start >= source->entities[entity].base + source->entities[entity].size)
        ++entity;
    if (entity == source->count)
        return -1;
    const size_t prefix = (size_t)width - 188;
    size_t offset = start - source->entities[entity].base;
    if (offset % (size_t)width != 0 && offset % (size_t)width != prefix)
        return -1;
    offset -= offset % (size_t)width;
    *position = (int64_t)(source->entities[entity].base + offset);
    uint8_t header[8];
    size_t have = 0;
    int started = 0;
    for (; entity < source->count; ++entity, offset = 0) {
        const TsViewsEntity *part = &source->entities[entity];
        if (started && part->discontinuity)
            return -1;
        for (; part->size - offset >= (size_t)width; offset += (size_t)width) {
            const uint8_t *ts = part->bytes + offset + prefix;
            if (ts[0] != 0x47)
                return -1;
            if (((ts[1] & 0x1f) * 256 + ts[2]) != pid) {
                if (!started)
                    return -1;
                continue;
            }
            if (!!(ts[1] & 0x40) == started)
                return -1;
            started = 1;
            if (!(ts[3] & 0x10))
                continue;
            size_t payload = ts[3] & 0x20 ? 5u + ts[4] : 4u;
            if (payload >= 188)
                return -1;
            size_t take = 188 - payload;
            if (take > sizeof(header) - have)
                take = sizeof(header) - have;
            memcpy(header + have, ts + payload, take);
            have += take;
            if (have == sizeof(header))
                return !header[0] && !header[1] && header[2] == 1
                    ? header[7] >> 6 : -1;
        }
    }
    return -1;
}

static void ts_views_entities_free(TsViewsEntities *source)
{
    for (size_t i = 0; i < source->count; ++i)
        free(source->entities[i].allocation);
    free(source->entities);
}

static int ts_views_in_program(const AVFormatContext *format, int index, int program)
{
    for (unsigned i = 0; i < format->nb_programs; ++i) {
        const AVProgram *item = format->programs[i];
        if (item->program_num != program)
            continue;
        for (unsigned j = 0; j < item->nb_stream_indexes; ++j)
            if ((int)item->stream_index[j] == index)
                return 1;
    }
    return 0;
}

static int ts_views_fold_packet(const AVFormatContext *format, const AVPacket *packet,
                                int program, const TsViewsEntities *source, int width,
                                ViewWork *work)
{
    if (!ts_views_in_program(format, packet->stream_index, program))
        return 0;
    const AVStream *stream = format->streams[packet->stream_index];
    int64_t pts = packet->pts, dts = packet->dts, position = packet->pos;
    if (width) {
        int flags = ts_views_wire_start(source, width, stream->id, &position);
        if (flags < 0 || flags == 1 || (flags == 0 && pts != AV_NOPTS_VALUE) ||
            (flags != 0 && pts == AV_NOPTS_VALUE) || (flags == 3 && dts == AV_NOPTS_VALUE))
            return AVERROR_INVALIDDATA;
        const uint64_t mask = (UINT64_C(1) << 33) - 1;
        if (pts != AV_NOPTS_VALUE)
            pts = (int64_t)((uint64_t)pts & mask);
        if (flags == 3)
            dts = (int64_t)((uint64_t)dts & mask);
        else
            dts = AV_NOPTS_VALUE;
    }
    uint64_t fields[] = {
        (uint64_t)packet->stream_index, (uint64_t)stream->id, (uint64_t)pts,
        (uint64_t)dts, (uint64_t)packet->duration, (uint64_t)position,
        (uint64_t)packet->flags, (uint64_t)packet->size,
    };
    if (work->trace) {
        uint64_t payload = 0;
        fold_bytes(&payload, packet->data, (size_t)packet->size);
        printf("packet %d %d %" PRId64 " %" PRId64 " %" PRId64 " %" PRId64
               " %d %d %" PRIu64, packet->stream_index, stream->id,
               pts, dts, packet->duration, position,
               packet->flags, packet->size, payload);
        for (int i = 0; i < packet->side_data_elems; ++i) {
            const AVPacketSideData *side = &packet->side_data[i];
            uint64_t digest = 0;
            fold_bytes(&digest, side->data, side->size);
            printf(" side %d %zu %" PRIu64, side->type, side->size, digest);
        }
        putchar('\n');
    }
    for (size_t i = 0; i < sizeof(fields) / sizeof(*fields); ++i)
        fold(&work->digest, fields[i]);
    fold_bytes(&work->digest, packet->data, (size_t)packet->size);
    fold(&work->digest, (uint64_t)packet->side_data_elems);
    for (int i = 0; i < packet->side_data_elems; ++i) {
        const AVPacketSideData *side = &packet->side_data[i];
        fold(&work->digest, (uint64_t)side->type);
        fold(&work->digest, (uint64_t)side->size);
        fold_bytes(&work->digest, side->data, side->size);
    }
    work->packets++;
    work->bytes += (uint64_t)packet->size;
    return 0;
}

static size_t ts_views_next_boundary(const TsViewsEntities *source, size_t index)
{
    while (index < source->count && !source->entities[index].discontinuity)
        ++index;
    return index;
}

static int ts_views_entity_read(void *opaque, uint8_t *buffer, int requested)
{
    TsViewsEntityInput *input = opaque;
    const TsViewsEntities *source = input->source;
    if (requested <= 0)
        return AVERROR(EINVAL);
    if (input->boundary < source->count
        && input->at == source->entities[input->boundary].base) {
        input->waiting = 1;
        return AVERROR(EAGAIN);
    }
    if (input->at == source->size)
        return AVERROR_EOF;
    while (input->entity < source->count
           && input->at >= source->entities[input->entity].base
                           + source->entities[input->entity].size)
        ++input->entity;
    if (input->entity == source->count)
        return AVERROR(EINVAL);
    const TsViewsEntity *entity = &source->entities[input->entity];
    size_t offset = input->at - entity->base;
    size_t count = entity->size - offset;
    if (count > (size_t)requested)
        count = (size_t)requested;
    if (count > TS_SOURCE_MAXIMUM_READ)
        count = TS_SOURCE_MAXIMUM_READ;
    memcpy(buffer, entity->bytes + offset, count);
    input->at += count;
    return (int)count;
}

static int64_t ts_views_entity_seek(void *opaque, int64_t offset, int origin)
{
    TsViewsEntityInput *input = opaque;
    int64_t size = (int64_t)input->source->size;
    if (origin == AVSEEK_SIZE)
        return size;
    origin &= ~AVSEEK_FORCE;
    int64_t base = origin == SEEK_SET ? 0 : origin == SEEK_CUR ? (int64_t)input->at
        : origin == SEEK_END ? size : -1;
    if (base < 0 || offset < -base || offset > size - base)
        return AVERROR(EINVAL);
    size_t at = (size_t)(base + offset);
    if (input->boundary < input->source->count
        && at > input->source->entities[input->boundary].base)
        return AVERROR(EAGAIN);
    input->at = at;
    input->entity = 0;
    return (int64_t)at;
}

static int ts_views_entity_resume(AVFormatContext *format, AVIOContext **io,
                            TsViewsEntityInput *input)
{
    const TsViewsEntities *source = input->source;
    if (!input->waiting || input->boundary == source->count
        || input->at != source->entities[input->boundary].base)
        return AVERROR(EINVAL);
    avio_flush(*io);
    int rc = avformat_flush(format);
    if (rc < 0)
        return rc;
    /* A seek does not clear the public AVIO error left by EAGAIN. Reinitialize
     * the IO context with its existing byte allocation; no media is reread. */
    AVIOContext *next = avio_alloc_context((*io)->buffer, (*io)->buffer_size, 0,
                                          input, ts_views_entity_read, NULL, ts_views_entity_seek);
    if (!next)
        return AVERROR(ENOMEM);
    AVIOContext *previous = *io;
    format->pb = next;
    *io = next;
    avio_context_free(&previous);
    /* The documented direct mode makes this position update invoke seek rather
     * than consume input to skip forward in a newly initialized IO context. */
    next->direct = 1;
    int64_t position = avio_seek(next, (int64_t)input->at, SEEK_SET);
    next->direct = 0;
    if (position != (int64_t)input->at)
        return position < 0 ? (int)position : AVERROR(EINVAL);
    input->boundary = ts_views_next_boundary(source, input->boundary + 1);
    input->waiting = 0;
    return 0;
}

enum { TS_SCOPE_ALL, TS_SCOPE_AVC, TS_SCOPE_COMPACT };

static int ts_views_scope_refused(const AVFormatContext *format, int program, int scope)
{
    for (unsigned i = 0; i < format->nb_programs; ++i) {
        const AVProgram *item = format->programs[i];
        if (item->program_num != program || item->nb_stream_indexes != 1)
            continue;
        const AVCodecParameters *codec = format->streams[item->stream_index[0]]->codecpar;
        if (scope == TS_SCOPE_AVC)
            return codec->codec_tag && codec->codec_tag != 0x1b;
        return codec->codec_id == AV_CODEC_ID_AC3 || codec->codec_id == AV_CODEC_ID_EAC3 ||
               codec->codec_id == AV_CODEC_ID_DVB_SUBTITLE;
    }
    return 0;
}

static int ts_views_entities_operation(const TsViewsEntities *source, int program, int width,
                                       int scope, ViewWork *work)
{
    TsViewsEntityInput input = {source, 0, 0, ts_views_next_boundary(source, 1), 0};
    AVFormatContext *format = avformat_alloc_context();
    AVIOContext *io = NULL;
    AVPacket *packet = NULL;
    uint8_t *buffer = av_malloc(TS_SOURCE_AVIO_BYTES);
    int rc = AVERROR(ENOMEM);
    if (!format || !buffer) {
        av_free(buffer);
        goto done;
    }
    io = avio_alloc_context(buffer, TS_SOURCE_AVIO_BYTES, 0, &input,
                            ts_views_entity_read, NULL, ts_views_entity_seek);
    if (!io) {
        av_free(buffer);
        goto done;
    }
    format->pb = io;
    format->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_NOFILLIN;
    format->flags |= AVFMT_FLAG_NOPARSE;
    rc = avformat_open_input(&format, NULL, av_find_input_format("mpegts"), NULL);
    if (rc < 0)
        goto done;
    packet = av_packet_alloc();
    if (!packet) {
        rc = AVERROR(ENOMEM);
        goto done;
    }
    for (;;) {
        rc = av_read_frame(format, packet);
        if (rc == AVERROR(EAGAIN) && input.waiting) {
            rc = ts_views_entity_resume(format, &io, &input);
            if (rc < 0)
                break;
            continue;
        }
        if (rc < 0)
            break;
        if (scope == TS_SCOPE_ALL)
            rc = ts_views_fold_packet(format, packet, program, source, width, work);
        av_packet_unref(packet);
        if (rc < 0)
            break;
    }
    if (rc == AVERROR_EOF && input.at == source->size && input.boundary == source->count) {
        if (scope != TS_SCOPE_ALL) {
            if (!ts_views_scope_refused(format, program, scope)) {
                rc = AVERROR_INVALIDDATA;
                goto done;
            }
            transport_view_row(work, 8, NULL, 0, NULL, 0);
        }
        rc = 1;
    } else if (rc == AVERROR_EOF) {
        rc = AVERROR(EIO);
    }
done:
    av_packet_free(&packet);
    avformat_close_input(&format);
    if (io) {
        av_freep(&io->buffer);
        avio_context_free(&io);
    }
    return rc;
}

typedef struct {
    TsViewsEntities source;
    int program, wire_clock_width, scope;
} TsViewsSourceRequest;

static void ts_views_release(void *opaque)
{
    TsViewsSourceRequest *request = opaque;
    ts_views_entities_free(&request->source);
    free(request);
}

static int ts_views_run(const ViewCase *input, ViewWork *work)
{
    const TsViewsSourceRequest *request = input->prepared;
    int rc = ts_views_entities_operation(&request->source, request->program,
                                        request->wire_clock_width, request->scope, work);
    if (rc == 1)
        return 0;
    char error[AV_ERROR_MAX_STRING_SIZE];
    av_strerror(rc, error, sizeof(error));
    fprintf(stderr, "MPEG-TS demux stopped before EOF: %s\n", error);
    return 2;
}

static int transport_source_prepare(ViewCase *input)
{
    static const struct {
        const char *prefix;
        const char *name;
        const char *forms[6];
    } operations[] = {
        {"ts-source-", "ts-source-poll", {"poll", "feed", "feed-carriers", "compact", "compact-observed"}},
        {"ts-pes-assembler-", "ts-pes-assembler", {"1", "7", "184", "whole"}},
        {"ts-observer-", "ts-observer-forms", {"unregistered", "input-bound", "compact"}},
    };
    if (input->argc < 3 || !input->size || input->size > TS_SOURCE_MAX_INPUT_BYTES)
        return 2;
    if (strcmp(input->argv[0], "188") && strcmp(input->argv[0], "192"))
        return 2;
    const char *number = input->argv[1];
    if (!*number || strspn(number, "0123456789") != strlen(number))
        return 2;
    unsigned long program = strtoul(number, NULL, 10);
    if (!program || program > UINT16_MAX)
        return 2;

    for (size_t i = 0; i < sizeof(operations) / sizeof(*operations); ++i) {
        if (!strcmp(input->prefix, operations[i].prefix)) {
            const char *const *forms = operations[i].forms;
            for (; *forms; ++forms) {
                if (!strcmp(input->argv[2], *forms)) {
                    input->name = operations[i].name;
                    break;
                }
            }
            break;
        }
    }
    if (!input->name)
        return 2;

    TsViewsSourceRequest *request = calloc(1, sizeof(*request));
    if (!request)
        return 2;
    input->prepared = request;
    input->release = ts_views_release;
    request->program = (int)program;
    TsViewsEntities *source = &request->source;
    source->entities = calloc(TS_SOURCE_MAX_ENTITIES, sizeof(*source->entities));
    if (!source->entities)
        return 2;
    source->entities[0] = (TsViewsEntity){input->data, NULL, input->size, 0, 0};
    source->count = 1;
    source->size = input->size;
    int index = 3;
    if (index < input->argc && !strcmp(input->argv[index], "--source-clocks")) {
        if (!strcmp(input->prefix, "ts-observer-"))
            return 2;
        request->wire_clock_width = atoi(input->argv[0]);
        ++index;
    }
    if (index < input->argc && !strcmp(input->argv[index], "--scope-refusal")) {
        if (strcmp(input->prefix, "ts-observer-"))
            return 2;
        if (!strcmp(input->argv[2], "unregistered"))
            request->scope = TS_SCOPE_AVC;
        else if (!strcmp(input->argv[2], "compact"))
            request->scope = TS_SCOPE_COMPACT;
        else
            return 2;
        ++index;
    }
    if (index < input->argc
        && (!strcmp(input->argv[index], "--dvb") || !strcmp(input->argv[index], "--atsc")))
        ++index;
    while (index < input->argc) {
        int discontinuity = !strcmp(input->argv[index], "--discontinuity");
        if (discontinuity)
            ++index;
        if (index == input->argc || input->argv[index][0] == '-'
            || source->count == TS_SOURCE_MAX_ENTITIES)
            return 2;
        size_t size;
        uint8_t *data = view_load(input->argv[index++], &size);
        if (!data)
            return 2;
        if (!size || size > TS_SOURCE_MAX_INPUT_BYTES - source->size) {
            free(data);
            return 2;
        }
        source->entities[source->count++] = (TsViewsEntity){data, data, size, source->size, discontinuity};
        source->size += size;
    }
    av_log_set_level(AV_LOG_ERROR);
    input->run = ts_views_run;
    return 0;
}
