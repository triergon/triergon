/* Own reference tooling: the packet fold and command line shared by the per-input
 * playback profile references (reference_matroska_audio.h, reference_mp4_forward.h,
 * reference_ts_program.h and reference_raw_video.h), the same as playback-worker's
 * packet_fold.rs.
 *
 * Every operation reads one preloaded input through a custom AVIO context and folds
 * each selected AVPacket as track, PTS, DTS, duration, flags, size and every payload
 * byte. An unknown time folds as INT64_MIN and an unknown duration as zero. */
#include <libavutil/mathematics.h>
#include <libavutil/mem.h>

typedef struct {
    uint64_t packets, bytes, digest;
    int trace;
} PacketWork;

typedef struct {
    const uint8_t *data;
    int64_t size, at;
    int quantum;
} PacketInput;

typedef int (*PacketOperation)(const uint8_t *data, size_t size, const char *argument, PacketWork *work);

static int packet_input_read(void *opaque, uint8_t *buffer, int requested)
{
    PacketInput *in = opaque;
    if (requested <= 0)
        return AVERROR(EINVAL);
    if (in->at >= in->size)
        return AVERROR_EOF;
    int64_t count = in->size - in->at;
    if (count > requested)
        count = requested;
    if (count > in->quantum)
        count = in->quantum;
    memcpy(buffer, in->data + in->at, (size_t)count);
    in->at += count;
    return (int)count;
}

static int64_t packet_input_seek(void *opaque, int64_t offset, int whence)
{
    PacketInput *in = opaque;
    if (whence == AVSEEK_SIZE)
        return in->size;
    int64_t base;
    switch (whence & ~AVSEEK_FORCE) {
    case SEEK_SET: base = 0; break;
    case SEEK_CUR: base = in->at; break;
    case SEEK_END: base = in->size; break;
    default: return AVERROR(EINVAL);
    }
    if (offset < -base || offset > in->size - base)
        return AVERROR(EINVAL);
    in->at = base + offset;
    return in->at;
}

/* Open `in` with `demuxer` and `flags`; a forward-only input has no seek callback. */
static int packet_open(AVFormatContext **format, PacketInput *in, int seekable,
                       const AVInputFormat *demuxer, int flags)
{
    *format = avformat_alloc_context();
    uint8_t *buffer = av_malloc((size_t)in->quantum);
    AVIOContext *io = NULL;
    if (*format && buffer)
        io = avio_alloc_context(buffer, in->quantum, 0, in, packet_input_read, NULL,
                                seekable ? packet_input_seek : NULL);
    if (!io) {
        av_free(buffer);
        avformat_free_context(*format);
        *format = NULL;
        return AVERROR(ENOMEM);
    }
    (*format)->pb = io;
    (*format)->flags |= AVFMT_FLAG_CUSTOM_IO | flags;
    int rc = avformat_open_input(format, NULL, demuxer, NULL);
    if (rc < 0) {
        av_freep(&io->buffer);
        avio_context_free(&io);
    }
    return rc;
}

static void packet_close(AVFormatContext **format)
{
    if (!*format)
        return;
    AVIOContext *io = (*format)->pb;
    avformat_close_input(format);
    if (io) {
        av_freep(&io->buffer);
        avio_context_free(&io);
    }
}

static void packet_fold(PacketWork *work, uint64_t track, int64_t pts, int64_t dts, uint64_t duration,
                        uint64_t flags, const uint8_t *data, size_t size)
{
    const uint64_t words[] = {track, (uint64_t)pts, (uint64_t)dts, duration, flags, (uint64_t)size};
    for (size_t i = 0; i < sizeof(words) / sizeof(*words); ++i)
        fold(&work->digest, words[i]);
    fold_bytes(&work->digest, data, size);
    work->packets++;
    work->bytes += size;
    if (work->trace) {
        printf("%" PRIu64 " ", track);
        if (pts == INT64_MIN) printf("- "); else printf("%" PRId64 " ", pts);
        if (dts == INT64_MIN) printf("- "); else printf("%" PRId64 " ", dts);
        printf("%" PRIu64 " %" PRIu64 " %zu\n", duration, flags, size);
    }
}

/* {prefix}fold FILE ARG | {prefix}bench FILE ITERATIONS ARG | {prefix}memory FILE ARG |
 * {prefix}trace FILE ARG */
static int packet_main(int argc, char **argv, const uint8_t *data, long size, const char *prefix,
                       const char *operation_name, PacketOperation operation)
{
    const char *mode = argv[1] + strlen(prefix);
    const int bench = !strcmp(mode, "bench"), memory = !strcmp(mode, "memory"), trace = !strcmp(mode, "trace");
    if (argc != (bench ? 5 : 4) || (!bench && !memory && !trace && strcmp(mode, "fold")) || size <= 0)
        return 2;
    unsigned long iterations = 1;
    if (bench) {
        char *end = NULL;
        iterations = strtoul(argv[3], &end, 10);
        if (*end || !iterations || iterations > 1000000)
            return 2;
    }
    const char *argument = argv[argc - 1];
    if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18)
        return 2;
    PacketWork total = {0};
    const uint64_t start = bench ? now_ns() : 0;
    for (unsigned long i = 0; i < iterations; ++i) {
        PacketWork once = {.trace = trace};
        int rc = operation(data, (size_t)size, argument, &once);
        if (rc < 0) {
            char error[AV_ERROR_MAX_STRING_SIZE];
            av_strerror(rc, error, sizeof(error));
            fprintf(stderr, "reference: %s (%d)\n", error, rc);
            return 1;
        }
        total.packets += once.packets;
        total.bytes += once.bytes;
        fold(&total.digest, once.digest);
    }
    const uint64_t elapsed = bench ? now_ns() - start : 0;
    if (memory && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16)
        return 2;
    printf("{\"schema\":1,\"operation\":\"%s\",\"iterations\":%lu"
           ",\"packets\":%" PRIu64 ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64
           ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true}\n",
           operation_name, iterations, total.packets, total.bytes, total.digest, elapsed);
    return 0;
}
