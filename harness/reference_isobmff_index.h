/* One-track MOV index lookups and regular source seeks. */
#include <errno.h>
#include <limits.h>

#define SEEK_VIEW_MAX_LOOKUPS 64

typedef enum { SEEK_VIEW_TRACK, SEEK_VIEW_PLAN, SEEK_VIEW_LOOKUP } SeekViewKind;
typedef struct {
    SeekViewKind kind;
    int track, window, eof;
    int64_t ticks[SEEK_VIEW_MAX_LOOKUPS];
    int query_count;
    uint64_t ordinal;
    uint16_t preroll;
} SeekView;

static int seek_view_open_source(AVFormatContext **format, PacketInput *source)
{
    AVDictionary *options = NULL;
    int code = av_dict_set(&options, "ignore_editlist", "1", 0);
    if (code < 0)
        return code;
    *format = avformat_alloc_context();
    uint8_t *buffer = av_malloc((size_t)source->quantum);
    AVIOContext *io = *format && buffer
        ? avio_alloc_context(buffer, source->quantum, 0, source,
                             packet_input_read, NULL, packet_input_seek)
        : NULL;
    if (!io) {
        av_free(buffer);
        avformat_free_context(*format);
        av_dict_free(&options);
        *format = NULL;
        return AVERROR(ENOMEM);
    }
    (*format)->pb = io;
    (*format)->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN;
    code = avformat_open_input(format, NULL, av_find_input_format("mov"), &options);
    av_dict_free(&options);
    if (code < 0) {
        av_freep(&io->buffer);
        avio_context_free(&io);
    }
    return code;
}

static int seek_view_has_negative_size(const AVFormatContext *format)
{
    for (unsigned track = 0; track < format->nb_streams; ++track) {
        AVStream *stream = format->streams[track];
        const int count = avformat_index_get_entries_count(stream);
        for (int ordinal = 0; ordinal < count; ++ordinal)
            if (avformat_index_get_entry(stream, ordinal)->size < 0)
                return 1;
    }
    return 0;
}

static int track_seek_main(AVFormatContext *format, AVStream *stream,
                           const SeekView *seek, ViewWork *work)
{
    if (seek->eof) {
        AVPacket *packet = av_packet_alloc();
        if (!packet)
            return AVERROR(ENOMEM);
        int code;
        while ((code = av_read_frame(format, packet)) >= 0)
            av_packet_unref(packet);
        av_packet_free(&packet);
        /* A negative public index size cannot describe a packet. The MOV
         * reader reports ENOMEM when it reaches this malformed size. */
        if (code == AVERROR(ENOMEM) && seek_view_has_negative_size(format))
            return AVERROR_INVALIDDATA;
        if (code != AVERROR_EOF)
            return code;
    }
    const int count = avformat_index_get_entries_count(stream);
    const AVIndexEntry *landing =
        avformat_index_get_entry_from_timestamp(stream, seek->ticks[0], AVSEEK_FLAG_BACKWARD);
    if (!landing) {
        /* SeekJob falls back to the first key when a backward lookup finds none. */
        for (int ordinal = 0; ordinal < count; ++ordinal) {
            const AVIndexEntry *entry = avformat_index_get_entry(stream, ordinal);
            if (entry->flags & AVINDEX_KEYFRAME) {
                landing = entry;
                break;
            }
        }
    }
    if (!landing) {
        const uint64_t absent = 0;
        mov_view_row(work, 3, &absent, 1, NULL, 0);
        return 0;
    }
    int ordinal = 0;
    while (ordinal < count && avformat_index_get_entry(stream, ordinal) != landing)
        ++ordinal;
    if (ordinal == count)
        return AVERROR_BUG;
    const uint64_t fields[] = {
        1, (uint64_t)landing->timestamp, (uint64_t)landing->pos, (uint64_t)landing->size,
        (uint64_t)(int64_t)landing->flags, (uint64_t)(int64_t)landing->min_distance, ordinal,
    };
    mov_view_row(work, 3, fields, sizeof(fields) / sizeof(*fields), NULL, 0);
    return 0;
}

static int source_seek_packet(ViewWork *work, FILE *trace, const AVPacket *packet,
                              int64_t decode_origin)
{
    const int known = packet->dts != AV_NOPTS_VALUE;
    if (known && decode_origin < 0 && packet->dts > INT64_MAX + decode_origin)
        return AVERROR_INVALIDDATA;
    const uint64_t words[] = {
        (uint64_t)packet->pos, (uint64_t)packet->size, known,
        known ? (uint64_t)(packet->dts - decode_origin) : 0,
    };
    const size_t size = (size_t)packet->size;
    view_record(work, 1, words, 3 + known, packet->data, size);
    if (trace) {
        fprintf(trace, "1 %" PRIu64 " %zu %d", words[0], size, known);
        if (known)
            fprintf(trace, " %" PRIu64, words[3]);
        if (size) {
            uint64_t digest = 0;
            fold_bytes(&digest, packet->data, size);
            fprintf(trace, " bytes=%zu digest=%" PRIu64, size, digest);
        }
        fputc('\n', trace);
    }
    return 0;
}

static int source_seek_main(AVFormatContext *format, AVStream *stream,
                            const SeekView *seek, ViewWork *work)
{
    const int count = avformat_index_get_entries_count(stream);
    if (!seek->ordinal || seek->ordinal > (uint64_t)count)
        return AVERROR_INVALIDDATA;
    const int target = (int)seek->ordinal - 1;
    int sync = target;
    while (sync >= 0 && !(avformat_index_get_entry(stream, sync)->flags & AVINDEX_KEYFRAME))
        --sync;
    if (sync < 0)
        return AVERROR_INVALIDDATA;
    const int start = sync > seek->preroll ? sync - seek->preroll : 0;
    /* Save values before seeking, which may change the index. The preroll
     * start can be a non-key entry, so a second keyframe snap would add samples. */
    const AVIndexEntry end = *avformat_index_get_entry(stream, target);
    const int64_t first = avformat_index_get_entry(stream, 0)->timestamp;
    if (first == AV_NOPTS_VALUE)
        return AVERROR_INVALIDDATA;
    /* Regular, unedited stts starts at zero. MOV can shift the public index and
     * packet DTS back for signed composition offsets. The seek uses media time. */
    const int64_t decode_origin = first < 0 ? first : 0;
    const int64_t indexed = avformat_index_get_entry(stream, start)->timestamp;
    if (indexed > INT64_MAX + decode_origin)
        return AVERROR_INVALIDDATA;
    const int64_t timestamp = indexed - decode_origin;
    int code = av_seek_frame(format, stream->index, timestamp,
                             AVSEEK_FLAG_BACKWARD | AVSEEK_FLAG_ANY);
    if (code < 0)
        return code;
    AVPacket *packet = av_packet_alloc();
    if (!packet)
        return AVERROR(ENOMEM);
    /* A failed read must discard the partial range, including its trace rows. */
    char *rows = NULL;
    size_t row_bytes = 0;
    FILE *trace = work->trace ? open_memstream(&rows, &row_bytes) : NULL;
    if (work->trace && !trace) {
        av_packet_free(&packet);
        return AVERROR(ENOMEM);
    }
    int reached = 0;
    while ((code = av_read_frame(format, packet)) >= 0) {
        if (packet->stream_index == stream->index) {
            code = source_seek_packet(work, trace, packet, decode_origin);
            if (code < 0)
                break;
            reached = packet->pos == end.pos && packet->dts == end.timestamp;
        }
        av_packet_unref(packet);
        if (reached)
            break;
    }
    av_packet_free(&packet);
    if (trace) {
        const int closed = fclose(trace);
        const int failed = closed || (reached && fwrite(rows, 1, row_bytes, stdout) != row_bytes);
        free(rows);
        if (failed)
            return AVERROR_BUG;
    }
    if (!reached)
        return code == AVERROR_EOF ? AVERROR_INVALIDDATA : code;
    mov_view_row(work, 7, NULL, 0, NULL, 0);
    return 0;
}

typedef struct {
    uint64_t ordinal, tied;
    int64_t pts, pos;
} SeekViewTarget;

static int source_lookup_main(AVFormatContext *format, AVStream *stream,
                              const SeekView *seek, ViewWork *work)
{
    SeekViewTarget selections[SEEK_VIEW_MAX_LOOKUPS] = {{0}};
    AVPacket *packet = av_packet_alloc();
    if (!packet)
        return AVERROR(ENOMEM);
    uint64_t ordinal = 0, unknown = 0;
    int code;
    while ((code = av_read_frame(format, packet)) >= 0) {
        if (packet->stream_index == stream->index) {
            ++ordinal;
            if (packet->pts == AV_NOPTS_VALUE) {
                if (!unknown)
                    unknown = ordinal;
            } else {
                for (int q = 0; q < seek->query_count; ++q) {
                    SeekViewTarget *best = &selections[q];
                    if (packet->pts > seek->ticks[q])
                        continue;
                    if (!best->ordinal || packet->pts > best->pts)
                        *best = (SeekViewTarget){
                            .ordinal = ordinal, .pts = packet->pts, .pos = packet->pos,
                        };
                    else if (packet->pts == best->pts && !best->tied)
                        best->tied = ordinal;
                }
            }
        }
        av_packet_unref(packet);
    }
    av_packet_free(&packet);
    if (code != AVERROR_EOF)
        return code;

    for (int q = 0; q < seek->query_count; ++q) {
        const SeekViewTarget *best = &selections[q];
        const uint64_t query = (uint64_t)seek->ticks[q];
        if (unknown) {
            const uint64_t words[] = {query, 3, unknown};
            mov_view_row(work, 3, words, 3, NULL, 0);
        } else if (!best->ordinal) {
            const uint64_t words[] = {query, 0};
            mov_view_row(work, 3, words, 2, NULL, 0);
        } else {
            const uint64_t words[] = {
                query, best->tied ? 2 : 1, best->ordinal,
                best->tied ? best->tied : (uint64_t)best->pos, (uint64_t)best->pts,
            };
            mov_view_row(work, 3, words, 5, NULL, 0);
        }
    }
    return 0;
}

static int isobmff_index_run(const ViewCase *input, ViewWork *work)
{
    const SeekView *seek = input->prepared;
    PacketInput source = {
        .data = input->data, .size = (int64_t)input->size, .quantum = seek->window,
    };
    AVFormatContext *format = NULL;
    int code = seek->kind == SEEK_VIEW_TRACK
        ? packet_open(&format, &source, 1, av_find_input_format("mov"),
                      AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN)
        : seek_view_open_source(&format, &source);
    if (code >= 0) {
        AVStream *stream = NULL;
        for (unsigned i = 0; i < format->nb_streams; ++i)
            if (format->streams[i]->id == seek->track) {
                stream = format->streams[i];
                break;
            }
        if (!stream)
            code = AVERROR_INVALIDDATA;
        else if (seek->kind == SEEK_VIEW_TRACK)
            code = track_seek_main(format, stream, seek, work);
        else if (seek->kind == SEEK_VIEW_PLAN)
            code = source_seek_main(format, stream, seek, work);
        else
            code = source_lookup_main(format, stream, seek, work);
    }
    packet_close(&format);
    if (code == AVERROR(ENOMEM) || code == AVERROR_BUG)
        return 2;
    if (code < 0) {
        work->packets = work->bytes = work->digest = 0;
        if (work->trace)
            fprintf(stderr, "MOV seek: %s\n", av_err2str(code));
        mov_view_row(work, 8, NULL, 0, NULL, 0);
    }
    return 0;
}

static int seek_view_signed(const char *text, int64_t *value)
{
    const char *digits = (*text == '-' || *text == '+') ? text + 1 : text;
    if (!*digits || strspn(digits, "0123456789") != strlen(digits))
        return 0;
    errno = 0;
    *value = strtoll(text, NULL, 10);
    return !errno;
}

static int seek_view_unsigned(const char *text, uint64_t *value)
{
    const char *digits = *text == '+' ? text + 1 : text;
    if (!*digits || strspn(digits, "0123456789") != strlen(digits))
        return 0;
    errno = 0;
    *value = strtoull(text, NULL, 10);
    return !errno;
}

static int seek_view_positive_int(const char *text, int *value)
{
    int64_t parsed;
    if (!seek_view_signed(text, &parsed) || parsed < 1 || parsed > INT_MAX)
        return 0;
    *value = (int)parsed;
    return 1;
}

static int isobmff_index_prepare(ViewCase *input)
{
    SeekView seek = {0};
    char *const *args = input->argv;
    int quantum;
    if (!strcmp(input->prefix, "mov-track-seek-")) {
        if (input->argc != 6 || !seek_view_positive_int(args[0], &seek.track) ||
            !seek_view_signed(args[1], &seek.ticks[0]) ||
            (strcmp(args[2], "0") && strcmp(args[2], "1")) ||
            !seek_view_positive_int(args[3], &seek.window) ||
            !seek_view_positive_int(args[4], &quantum) ||
            (strcmp(args[5], "strict") && strcmp(args[5], "recovery")))
            return 2;
        seek.kind = SEEK_VIEW_TRACK;
        seek.eof = args[2][0] == '1';
        input->name = "mov-track-seek";
    } else if (input->argc == 6 && !strcmp(args[0], "plan")) {
        uint64_t preroll;
        if (!seek_view_positive_int(args[1], &seek.track) ||
            !seek_view_unsigned(args[2], &seek.ordinal) ||
            !seek_view_unsigned(args[3], &preroll) || preroll > UINT16_MAX ||
            !seek_view_positive_int(args[4], &seek.window) ||
            !seek_view_positive_int(args[5], &quantum))
            return 2;
        seek.kind = SEEK_VIEW_PLAN;
        seek.preroll = (uint16_t)preroll;
        input->name = "source-seek";
    } else if (input->argc >= 5 && input->argc <= 4 + SEEK_VIEW_MAX_LOOKUPS &&
               !strcmp(args[0], "lookup")) {
        if (!seek_view_positive_int(args[1], &seek.track) ||
            !seek_view_positive_int(args[2], &seek.window) ||
            !seek_view_positive_int(args[3], &quantum))
            return 2;
        seek.kind = SEEK_VIEW_LOOKUP;
        seek.query_count = input->argc - 4;
        for (int q = 0; q < seek.query_count; ++q)
            if (!seek_view_signed(args[q + 4], &seek.ticks[q]))
                return 2;
        input->name = "source-seek";
    } else {
        return 2;
    }
    SeekView *prepared = malloc(sizeof(*prepared));
    if (!prepared)
        return 2;
    *prepared = seek;
    input->prepared = prepared;
    input->release = free;
    input->run = isobmff_index_run;
    return 0;
}
