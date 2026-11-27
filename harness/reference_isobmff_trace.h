/* Public MOV observations for index landings and comparison models. */
#include <stdarg.h>

#define TRACE_BUFFER_SIZE 32768
#define TRACE_MAX_LANDINGS 64

typedef enum {
    TRACE_RECEIVER, TRACE_DIAGNOSTICS, TRACE_CONFIGURATION, TRACE_HEADER, TRACE_INITIAL,
    TRACE_TRANSPORT, TRACE_SIDX, TRACE_TFRA
} MovTraceKind;

typedef struct {
    uint64_t start, end;
    int64_t tick;
} MovLanding;

typedef struct {
    MovTraceKind kind;
    int read_max, track;
    int64_t initial_rewind;
    size_t count;
    MovLanding landings[TRACE_MAX_LANDINGS];
} MovTrace;

typedef struct {
    PacketInput input;
    ViewWork *work;
    int64_t first_seek;
} MovTraceIO;

static void mov_trace_row(ViewWork *work, uint64_t kind, const uint64_t *words,
                          size_t count, const uint8_t *data, size_t size)
{
    view_record(work, kind, words, count, data, size);
    if (!work->trace)
        return;
    printf("%" PRIu64, kind);
    for (size_t i = 0; i < count; ++i)
        printf(" %" PRIu64, words[i]);
    if (size) {
        fputs(" hex=", stdout);
        for (size_t i = 0; i < size; ++i)
            printf("%02x", data[i]);
    }
    putchar('\n');
}

static int mov_trace_read(void *opaque, uint8_t *buf, int requested)
{
    MovTraceIO *source = opaque;
    const uint64_t offset = (uint64_t)source->input.at;
    const int result = packet_input_read(&source->input, buf, requested);
    if (source->work) {
        const uint64_t fields[] = {offset, (uint64_t)requested, (uint64_t)(int64_t)result};
        mov_trace_row(source->work, 1, fields, 3, NULL, 0);
    }
    return result;
}

static int64_t mov_trace_seek(void *opaque, int64_t offset, int whence)
{
    MovTraceIO *source = opaque;
    const int64_t result = packet_input_seek(&source->input, offset, whence);
    if (whence == SEEK_SET && result >= 0 && source->first_seek < 0)
        source->first_seek = result;
    if (source->work && !(whence & AVSEEK_SIZE)) {
        const uint64_t fields[] = {(uint64_t)whence, (uint64_t)offset, (uint64_t)result};
        mov_trace_row(source->work, 3, fields, 3, NULL, 0);
    }
    return result;
}

static void mov_trace_result(ViewWork *work, int result)
{
    const uint64_t word = (uint64_t)(int64_t)result;
    mov_trace_row(work, result >= 0 || result == AVERROR_EOF ? 7 : 8, &word, 1, NULL, 0);
}

static ViewWork *mov_trace_log_work;
static int mov_trace_log_error;

static void mov_trace_log(void *opaque, int level, const char *fmt, va_list args)
{
    (void)opaque;
    if (level > AV_LOG_WARNING || mov_trace_log_error)
        return;
    char local[512], *text = local;
    va_list copy;
    va_copy(copy, args);
    const int size = vsnprintf(local, sizeof(local), fmt, copy);
    va_end(copy);
    if (size < 0) {
        mov_trace_log_error = AVERROR_BUG;
        return;
    }
    if ((size_t)size >= sizeof(local)) {
        text = av_malloc((size_t)size + 1);
        if (!text) {
            mov_trace_log_error = AVERROR(ENOMEM);
            return;
        }
        if (vsnprintf(text, (size_t)size + 1, fmt, args) != size) {
            av_free(text);
            mov_trace_log_error = AVERROR_BUG;
            return;
        }
    }
    const size_t length = (size_t)size - (size > 0 && text[size - 1] == '\n');
    const uint64_t fields[] = {(uint64_t)level, length};
    mov_trace_row(mov_trace_log_work, 3, fields, 2, (const uint8_t *)text, length);
    if (text != local)
        av_free(text);
}

static int mov_trace_open(AVFormatContext **format, AVIOContext **io, MovTraceIO *source,
                          const MovTrace *operation, ViewWork *work)
{
    *format = avformat_alloc_context();
    uint8_t *buffer = av_malloc(TRACE_BUFFER_SIZE);
    *io = *format && buffer
        ? avio_alloc_context(buffer, TRACE_BUFFER_SIZE, 0, source, mov_trace_read, NULL,
                             mov_trace_seek)
        : NULL;
    if (!*io) {
        av_free(buffer);
        return AVERROR(ENOMEM);
    }
    (*format)->pb = *io;
    (*format)->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN;
    AVDictionary *options = NULL;
    int code = 0;
    if (operation->kind == TRACE_TFRA)
        code = av_dict_set(&options, "use_mfra_for", "dts", 0);
    if (code >= 0) {
        if (operation->kind == TRACE_DIAGNOSTICS) {
            mov_trace_log_work = work;
            mov_trace_log_error = 0;
            av_log_set_callback(mov_trace_log);
        }
        code = avformat_open_input(format, NULL, av_find_input_format("mov"), &options);
        if (operation->kind == TRACE_DIAGNOSTICS) {
            av_log_set_callback(av_log_default_callback);
            mov_trace_log_work = NULL;
            if (mov_trace_log_error)
                code = mov_trace_log_error;
        }
    }
    av_dict_free(&options);
    return code;
}

static void mov_trace_close(AVFormatContext **format, AVIOContext **io)
{
    avformat_close_input(format);
    if (*io) {
        av_freep(&(*io)->buffer);
        avio_context_free(io);
    }
}

static int mov_trace_models(const ViewCase *input, ViewWork *work)
{
    const MovTrace *operation = input->prepared;
    MovTraceIO source = {
        .input = {.data = input->data, .size = (int64_t)input->size, .quantum = operation->read_max},
        .work = operation->kind == TRACE_TRANSPORT ? work : NULL,
        .first_seek = -1,
    };
    AVFormatContext *format = NULL;
    AVIOContext *io = NULL;
    int result = mov_trace_open(&format, &io, &source, operation, work);
    if (result < 0 || operation->kind == TRACE_DIAGNOSTICS) {
        mov_trace_result(work, result);
        goto done;
    }
    if (operation->kind == TRACE_INITIAL && operation->initial_rewind >= 0 &&
        source.first_seek != operation->initial_rewind) {
        result = AVERROR_BUG;
        goto done;
    }
    switch (operation->kind) {
    case TRACE_CONFIGURATION:
    case TRACE_HEADER: {
        if (format->nb_streams != 1) {
            result = AVERROR_INVALIDDATA;
            mov_trace_result(work, result);
            break;
        }
        const AVCodecParameters *par = format->streams[0]->codecpar;
        const uint64_t size = par->extradata_size;
        mov_trace_row(work, 2, &size, 1, par->extradata, size);
        mov_trace_result(work, result);
        break;
    }
    case TRACE_TRANSPORT: {
        const uint64_t endpoint = (uint64_t)avio_tell(io);
        mov_trace_row(work, 7, &endpoint, 1, NULL, 0);
        break;
    }
    case TRACE_INITIAL:
    case TRACE_RECEIVER: {
        AVPacket *packet = av_packet_alloc();
        if (!packet) {
            result = AVERROR(ENOMEM);
            break;
        }
        while ((result = av_read_frame(format, packet)) >= 0) {
            if (operation->kind == TRACE_INITIAL)
                break;
            const uint64_t fields[] = {
                (uint64_t)(int64_t)format->streams[packet->stream_index]->id,
                (uint64_t)packet->pts, (uint64_t)packet->dts,
            };
            mov_trace_row(work, 1, fields, 3, NULL, 0);
            av_packet_unref(packet);
        }
        mov_trace_result(work, result);
        av_packet_free(&packet);
        break;
    }
    default:
        result = AVERROR_BUG;
        break;
    }
done:
    mov_trace_close(&format, &io);
    return result == AVERROR(ENOMEM) || result == AVERROR_BUG ? 1 : 0;
}

static int mov_trace_stream(const AVFormatContext *format, int track)
{
    for (unsigned i = 0; i < format->nb_streams; ++i)
        if (format->streams[i]->id == track)
            return (int)i;
    return -1;
}

static int mov_trace_landing(const ViewCase *input, ViewWork *work)
{
    const MovTrace *operation = input->prepared;
    AVPacket *packet = av_packet_alloc();
    if (!packet)
        return 1;
    AVFormatContext *format = NULL;
    AVIOContext *io = NULL;
    MovTraceIO source = {0};
    size_t i = 0;
    int result = 0, stream = -1;
    while (i < operation->count) {
        if (!format) {
            source.input = (PacketInput){
                .data = input->data, .size = (int64_t)input->size, .quantum = operation->read_max,
            };
            result = mov_trace_open(&format, &io, &source, operation, work);
            if (result < 0)
                break;
            stream = mov_trace_stream(format, operation->track);
            if (stream < 0) {
                result = AVERROR_STREAM_NOT_FOUND;
                break;
            }
        }
        const MovLanding *target = &operation->landings[i];
        if (operation->kind == TRACE_SIDX) {
            result = av_seek_frame(format, stream, target->tick, AVSEEK_FLAG_BACKWARD);
            if (result < 0)
                break;
        }
        result = av_read_frame(format, packet);
        if (result < 0)
            break;
        const uint64_t position = (uint64_t)packet->pos;
        const int within = packet->stream_index == stream
            && position >= target->start && position < target->end;
        if (operation->kind == TRACE_TFRA && !within && position < target->start) {
            av_packet_unref(packet);
            continue;
        }
        if (work->trace)
            fprintf(stderr, "first packet: pos=%" PRId64 " pts=%" PRId64 " dts=%" PRId64 "\n",
                    packet->pos, packet->pts, packet->dts);
        const uint64_t track = (uint64_t)(int64_t)operation->track;
        if (operation->kind == TRACE_SIDX) {
            const uint64_t fields[] = {track, target->start, target->end, (uint64_t)packet->pts,
                                      (uint64_t)within};
            mov_trace_row(work, 3, fields, 5, NULL, 0);
            mov_trace_close(&format, &io);
        } else {
            if (!within) {
                result = AVERROR_INVALIDDATA;
                break;
            }
            const uint64_t fields[] = {track, target->start, (uint64_t)packet->dts};
            mov_trace_row(work, 3, fields, 3, NULL, 0);
        }
        ++i;
        av_packet_unref(packet);
    }
    mov_trace_close(&format, &io);
    av_packet_free(&packet);
    mov_trace_row(work, i == operation->count ? 7 : 8, NULL, 0, NULL, 0);
    return result == AVERROR(ENOMEM) ? 1 : 0;
}

static int mov_trace_number(const char *text, uint64_t max, uint64_t *value)
{
    if (!*text || *text == '-')
        return 0;
    char *end;
    errno = 0;
    unsigned long long v = strtoull(text, &end, 10);
    if (errno || *end || v > max)
        return 0;
    *value = (uint64_t)v;
    return 1;
}

static int isobmff_trace_prepare(ViewCase *input)
{
    MovTrace operation = {.read_max = 65536, .initial_rewind = -1};
    uint64_t number;
    if (!strcmp(input->prefix, "index-landings-")) {
        if (input->argc < 8 || (input->argc - 5) % 3
            || (input->argc - 5) / 3 > TRACE_MAX_LANDINGS)
            return 2;
        if (!strcmp(input->argv[0], "sidx"))
            operation.kind = TRACE_SIDX;
        else if (!strcmp(input->argv[0], "tfra"))
            operation.kind = TRACE_TFRA;
        else
            return 2;
        if (!mov_trace_number(input->argv[3], INT_MAX, &number) || !number)
            return 2;
        operation.track = (int)number;
        for (int argv = 5; argv < input->argc; argv += 3) {
            MovLanding *target = &operation.landings[operation.count++];
            if (!mov_trace_number(input->argv[argv], input->size, &target->start)
                || !mov_trace_number(input->argv[argv + 1], input->size, &target->end)
                || target->start >= target->end
                || !mov_trace_number(input->argv[argv + 2], INT64_MAX, &number))
                return 2;
            target->tick = (int64_t)number;
        }
        input->name = "isobmff-index-landings";
        input->run = mov_trace_landing;
    } else {
        if (input->argc < 3 || !mov_trace_number(input->argv[1], 65536, &number) || !number)
            return 2;
        operation.read_max = (int)number;
        const char *model = input->argv[0];
        if (!strcmp(model, "receiver"))
            operation.kind = TRACE_RECEIVER;
        else if (!strcmp(model, "diagnostics"))
            operation.kind = TRACE_DIAGNOSTICS;
        else if (!strcmp(model, "configuration"))
            operation.kind = TRACE_CONFIGURATION;
        else if (!strcmp(model, "header"))
            operation.kind = TRACE_HEADER;
        else if (!strcmp(model, "initial"))
            operation.kind = TRACE_INITIAL;
        else if (!strcmp(model, "transport"))
            operation.kind = TRACE_TRANSPORT;
        else
            return 2;
        if (operation.kind == TRACE_INITIAL && input->argc == 4) {
            if (!mov_trace_number(input->argv[3], input->size, &number))
                return 2;
            operation.initial_rewind = (int64_t)number;
        } else if (input->argc != (operation.kind == TRACE_TRANSPORT ? 4 : 3))
            return 2;
        input->name = "mov-reference-trace";
        input->run = mov_trace_models;
    }
    MovTrace *prepared = malloc(sizeof(*prepared));
    if (!prepared)
        return 1;
    *prepared = operation;
    input->prepared = prepared;
    input->release = free;
    return 0;
}
