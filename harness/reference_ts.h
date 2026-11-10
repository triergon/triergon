/* Own reference tooling: the packets FFmpeg's MPEG-TS demuxer emits for one
 * program, the same operation as playback-worker's ts modes.
 *
 * The input is read from memory through custom AVIO (32 KiB buffer, reads of at
 * most 64 KiB), opened as mpegts with AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN and
 * no stream discovery, and every packet of the selected program is folded in
 * demuxer order: stream index, PID, PTS, DTS, duration, position, flags, payload and
 * every side-data element. The operation reaches EOF only when av_read_frame
 * returns AVERROR_EOF. FFmpeg probes the packet size itself, and its demuxer has no
 * caller-declared descriptor namespace, so the size and namespace arguments describe
 * the candidate's request only. Both modes use the same AVIO adapter for ordered
 * media entities and the same program filter and packet fold. The ts-au modes
 * enable codec parsing and retain NOFILLIN. */
#include <libavformat/avformat.h>

#define TS_OPERATION "mpegts-pes-packets"
#define TS_AU_OPERATION "mpegts-access-unit-packets"
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

static void ts_fold_packet(const AVFormatContext *format, const AVPacket *packet,
                           int program, TsWork *work)
{
    if (!ts_in_program(format, packet->stream_index, program))
        return;
    const AVStream *stream = format->streams[packet->stream_index];
    uint64_t fields[] = {
        (uint64_t)packet->stream_index, (uint64_t)stream->id, (uint64_t)packet->pts,
        (uint64_t)packet->dts, (uint64_t)packet->duration, (uint64_t)packet->pos,
        (uint64_t)packet->flags, (uint64_t)packet->size,
    };
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
}

/* One operation. Returns 1 at AVERROR_EOF, or the negative error that stopped the
 * demuxer before it. */
static int ts_operation(const uint8_t *bytes, size_t size, int program,
                        int access_units, TsWork *work)
{
    TsInput in = {bytes, size, 0};
    AVFormatContext *format = avformat_alloc_context();
    AVIOContext *io = NULL;
    AVPacket *packet = NULL;
    uint8_t *buffer = av_malloc(TS_AVIO_BYTES);
    int rc = AVERROR(ENOMEM);
    if (!format || !buffer) {
        av_free(buffer);
        goto done;
    }
    io = avio_alloc_context(buffer, TS_AVIO_BYTES, 0, &in, ts_read, NULL, ts_seek);
    if (!io) {
        av_free(buffer);
        goto done;
    }
    format->pb = io;
    format->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_NOFILLIN;
    if (!access_units)
        format->flags |= AVFMT_FLAG_NOPARSE;
    rc = avformat_open_input(&format, NULL, av_find_input_format("mpegts"), NULL);
    if (rc < 0)
        goto done;
    packet = av_packet_alloc();
    if (!packet) {
        rc = AVERROR(ENOMEM);
        goto done;
    }
    while ((rc = av_read_frame(format, packet)) >= 0) {
        ts_fold_packet(format, packet, program, work);
        av_packet_unref(packet);
    }
    rc = rc == AVERROR_EOF ? 1 : rc;
done:
    av_packet_free(&packet);
    avformat_close_input(&format);
    if (io) {
        av_freep(&io->buffer);
        avio_context_free(&io);
    }
    return rc;
}

#include "reference_ts_sequence.h"

/* ts-fold FILE SIZE PROGRAM [OPTIONS] | ts-bench FILE N SIZE PROGRAM [OPTIONS] |
 * ts-memory FILE SIZE PROGRAM [OPTIONS]; OPTIONS are --dvb or --atsc and further entities.
 * ts-au-fold, ts-au-bench and ts-au-memory use the same arguments with parsing. */
static int ts_main(int argc, char **argv, const uint8_t *bytes, long size)
{
    const int access_units = !strncmp(argv[1], "ts-au-", 6);
    const char *mode = argv[1] + (access_units ? 6 : 3);
    const int bench = !strcmp(mode, "bench");
    const int memory = !strcmp(mode, "memory");
    if (!bench && !memory && strcmp(mode, "fold"))
        return 2;
    int at = bench ? 4 : 3;
    if (argc < at + 2)
        return 2;
    unsigned long iterations = 1;
    char *end = NULL;
    if (bench) {
        iterations = strtoul(argv[3], &end, 10);
        if (*end || !iterations || iterations > 1000000)
            return 2;
    }
    if (strcmp(argv[at], "188") && strcmp(argv[at], "192"))
        return 2;
    long program = strtol(argv[at + 1], &end, 10);
    if (*end || program < 1 || program > 65535)
        return 2;
    int first_entity = at + 2;
    if (first_entity < argc
        && (!strcmp(argv[first_entity], "--dvb") || !strcmp(argv[first_entity], "--atsc")))
        ++first_entity;
    TsEntities source = {0};
    int result = 2;
    if (first_entity < argc
        && ts_entities_load(&source, argc, argv, at, bytes, (size_t)size, access_units) < 0)
        goto done;
    if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18)
        goto done;
    TsWork total = {0, 0, 0};
    const char *refusal = NULL;
    int eof = 0, rc = 0;
    const uint64_t start = bench ? now_ns() : 0;
    for (unsigned long i = 0; i < iterations; ++i) {
        TsWork work = {0, 0, 0};
        rc = source.count ? ts_entities_operation(&source, (int)program, access_units, &work)
            : ts_operation(bytes, (size_t)size, (int)program, access_units, &work);
        fold(&total.digest, work.digest);
        total.packets += work.packets;
        total.bytes += work.bytes;
        eof = rc == 1;
        if (!eof) {
            refusal = "demuxer stopped before EOF";
            break;
        }
    }
    const uint64_t elapsed = bench ? now_ns() - start : 0;
    if (memory && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16)
        goto done;
    printf("{\"schema\":1,\"operation\":\"%s\",\"iterations\":%lu,\"packets\":%" PRIu64
           ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 ",\"elapsed_ns\":%" PRIu64 ",\"eof\":%s",
           access_units ? TS_AU_OPERATION : TS_OPERATION,
           iterations, total.packets, total.bytes, total.digest, elapsed, eof ? "true" : "false");
    if (refusal) {
        printf(",\"refusal\":");
        quote(refusal);
        if (rc < 0) {
            char error[256];
            av_strerror(rc, error, sizeof(error));
            printf(",\"error\":");
            quote(error);
        }
    }
    printf("}\n");
    result = 0;
done:
    ts_entities_free(&source);
    return result;
}
