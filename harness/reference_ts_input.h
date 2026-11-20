enum { TS_AVIO_BYTES = 32 * 1024, TS_MAXIMUM_READ = 64 * 1024 };

typedef struct { const uint8_t *data; size_t size, used; } TsInput;
typedef struct { uint64_t packets, bytes, digest; } TsWork;

static int ts_read(void *opaque, uint8_t *buffer, int requested)
{
    TsInput *in = opaque;
    size_t left = in->size - in->used;
    if (!left)
        return AVERROR_EOF;
    size_t count = requested > 0 ? (size_t)requested : 0;
    if (count > left)
        count = left;
    if (count > TS_MAXIMUM_READ)
        count = TS_MAXIMUM_READ;
    memcpy(buffer, in->data + in->used, count);
    in->used += count;
    return (int)count;
}

static int64_t ts_seek(void *opaque, int64_t offset, int origin)
{
    TsInput *in = opaque;
    if (origin == AVSEEK_SIZE)
        return (int64_t)in->size;
    origin &= ~AVSEEK_FORCE;
    int64_t base = origin == SEEK_SET ? 0 : origin == SEEK_CUR ? (int64_t)in->used
        : origin == SEEK_END ? (int64_t)in->size : -1;
    if (base < 0 || offset < -base || offset > (int64_t)in->size - base)
        return AVERROR(EINVAL);
    in->used = (size_t)(base + offset);
    return (int64_t)in->used;
}

static int ts_in_program(const AVFormatContext *format, int index, int program)
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
