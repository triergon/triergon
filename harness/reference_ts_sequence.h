/* MPEG-TS entity input through AVIO. Ordinary boundaries keep the parser;
 * an explicit discontinuity blocks reads until buffered parse state is flushed. */
#include <errno.h>

typedef struct {
    const uint8_t *bytes;
    uint8_t *allocation;
    size_t size, base;
    int discontinuity;
} TsEntity;

typedef struct {
    TsEntity *entities;
    size_t count, size;
} TsEntities;

typedef struct {
    const TsEntities *source;
    size_t at, entity, boundary;
    int waiting;
} TsEntityInput;

static void ts_entities_free(TsEntities *source)
{
    for (size_t i = 0; i < source->count; ++i)
        free(source->entities[i].allocation);
    free(source->entities);
}

static int ts_entity_name(const char *name)
{
    return name[0] && name[0] != '-' && strcmp(name, "..")
        && !strchr(name, '/') && !strchr(name, '\\');
}

static int ts_entities_load(TsEntities *source, int argc, char **argv, int at,
                            const uint8_t *bytes, size_t size)
{
    source->entities = calloc((size_t)argc, sizeof(*source->entities));
    if (!source->entities)
        return AVERROR(ENOMEM);
    source->entities[0] = (TsEntity){bytes, NULL, size, 0, 0};
    source->count = 1;
    source->size = size;
    const char *slash = strrchr(argv[2], '/');
    size_t directory = slash ? (size_t)(slash + 1 - argv[2]) : 0;
    int discontinuity = 0;
    for (int i = at + 2; i < argc; ++i) {
        if (i == at + 2 && !strcmp(argv[i], "--dvb"))
            continue;
        if (!strcmp(argv[i], "--discontinuity") && !discontinuity) {
            discontinuity = 1;
            continue;
        }
        if (!ts_entity_name(argv[i]))
            return AVERROR(EINVAL);
        size_t length = strlen(argv[i]);
        if (directory > SIZE_MAX - length - 1)
            return AVERROR(EINVAL);
        char *path = malloc(directory + length + 1);
        if (!path)
            return AVERROR(ENOMEM);
        memcpy(path, argv[2], directory);
        memcpy(path + directory, argv[i], length + 1);
        FILE *file = fopen(path, "rb");
        free(path);
        if (!file)
            return AVERROR(errno);
        int ok = !fseek(file, 0, SEEK_END);
        long extent = ok ? ftell(file) : -1;
        if (extent <= 0 || extent > 64 * 1024 * 1024
            || (uint64_t)extent > (uint64_t)SIZE_MAX - source->size
            || (uint64_t)extent > (uint64_t)INT64_MAX - source->size) {
            fclose(file);
            return AVERROR(EINVAL);
        }
        rewind(file);
        size_t count = (size_t)extent;
        uint8_t *data = malloc(count);
        if (!data) {
            fclose(file);
            return AVERROR(ENOMEM);
        }
        ok = fread(data, 1, count, file) == count;
        fclose(file);
        if (!ok) {
            free(data);
            return AVERROR(EIO);
        }
        source->entities[source->count++] =
            (TsEntity){data, data, count, source->size, discontinuity};
        source->size += count;
        discontinuity = 0;
    }
    return discontinuity || source->count < 2 ? AVERROR(EINVAL) : 0;
}

static size_t ts_next_boundary(const TsEntities *source, size_t index)
{
    while (index < source->count && !source->entities[index].discontinuity)
        ++index;
    return index;
}

static int ts_entity_read(void *opaque, uint8_t *buffer, int requested)
{
    TsEntityInput *input = opaque;
    const TsEntities *source = input->source;
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
    const TsEntity *entity = &source->entities[input->entity];
    size_t offset = input->at - entity->base;
    size_t count = entity->size - offset;
    if (count > (size_t)requested)
        count = (size_t)requested;
    if (count > TS_MAXIMUM_READ)
        count = TS_MAXIMUM_READ;
    memcpy(buffer, entity->bytes + offset, count);
    input->at += count;
    return (int)count;
}

static int64_t ts_entity_seek(void *opaque, int64_t offset, int origin)
{
    TsEntityInput *input = opaque;
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

static int ts_entity_resume(AVFormatContext *format, AVIOContext **io,
                            TsEntityInput *input)
{
    const TsEntities *source = input->source;
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
                                          input, ts_entity_read, NULL, ts_entity_seek);
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
    input->boundary = ts_next_boundary(source, input->boundary + 1);
    input->waiting = 0;
    return 0;
}

static int ts_entities_operation(const TsEntities *source, int program, TsWork *work)
{
    TsEntityInput input = {source, 0, 0, ts_next_boundary(source, 1), 0};
    AVFormatContext *format = avformat_alloc_context();
    AVIOContext *io = NULL;
    AVPacket *packet = NULL;
    uint8_t *buffer = av_malloc(TS_AVIO_BYTES);
    int rc = AVERROR(ENOMEM);
    if (!format || !buffer) {
        av_free(buffer);
        goto done;
    }
    io = avio_alloc_context(buffer, TS_AVIO_BYTES, 0, &input,
                            ts_entity_read, NULL, ts_entity_seek);
    if (!io) {
        av_free(buffer);
        goto done;
    }
    format->pb = io;
    format->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_NOFILLIN;
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
            rc = ts_entity_resume(format, &io, &input);
            if (rc < 0)
                break;
            continue;
        }
        if (rc < 0)
            break;
        ts_fold_packet(format, packet, program, work);
        av_packet_unref(packet);
    }
    if (rc == AVERROR_EOF && input.at == source->size && input.boundary == source->count)
        rc = 1;
    else if (rc == AVERROR_EOF)
        rc = AVERROR(EIO);
done:
    av_packet_free(&packet);
    avformat_close_input(&format);
    if (io) {
        av_freep(&io->buffer);
        avio_context_free(&io);
    }
    return rc;
}
