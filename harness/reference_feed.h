/* Own reference tooling: Matroska video startup and seek through public FFmpeg APIs.
 *
 * The same operation as playback-worker: preloaded seekable input read in 4 KiB
 * windows, stream discovery, the first video stream, an optional backward seek after
 * discovery or after reading to the end of file, and every packet of that stream
 * folded with its PTS, DTS and duration in nanoseconds. The capsule is SMVCL001,
 * one operation byte (0 startup, 1 seek after discovery, 2 seek after the end), the
 * target in milliseconds as a big-endian int64, then the file. */
#include <libavutil/mathematics.h>
#include <libavutil/mem.h>

#define FEED_QUANTUM 4096

typedef struct {
    const uint8_t *bytes;
    int64_t length, position;
} FeedSource;

typedef struct {
    uint64_t packets, bytes, digest;
} FeedWork;

static int feed_read(void *opaque, uint8_t *buffer, int requested) {
    FeedSource *source = opaque;
    if (requested <= 0) return AVERROR(EINVAL);
    if (source->position >= source->length) return AVERROR_EOF;
    int64_t count = source->length - source->position;
    if (count > requested) count = requested;
    if (count > FEED_QUANTUM) count = FEED_QUANTUM;
    memcpy(buffer, source->bytes + source->position, (size_t)count);
    source->position += count;
    return (int)count;
}

static int64_t feed_seek(void *opaque, int64_t offset, int whence) {
    FeedSource *source = opaque;
    if (whence == AVSEEK_SIZE) return source->length;
    int64_t base;
    switch (whence & ~AVSEEK_FORCE) {
    case SEEK_SET: base = 0; break;
    case SEEK_CUR: base = source->position; break;
    case SEEK_END: base = source->length; break;
    default: return AVERROR(EINVAL);
    }
    if (offset < -base || offset > source->length - base) return AVERROR(EINVAL);
    source->position = base + offset;
    return source->position;
}

static void feed_fold(FeedWork *work, uint64_t value) { work->digest = 31 * work->digest + value; }

static uint64_t feed_ns(int64_t value, AVRational base) {
    if (value == AV_NOPTS_VALUE) return (uint64_t)INT64_MIN;
    return (uint64_t)av_rescale_q(value, base, (AVRational){1, 1000000000});
}

static void feed_packet(FeedWork *work, const AVPacket *pkt, AVRational base) {
    work->packets++;
    work->bytes += (uint64_t)pkt->size;
    feed_fold(work, feed_ns(pkt->pts, base));
    feed_fold(work, feed_ns(pkt->dts, base));
    feed_fold(work, pkt->duration > 0 ? feed_ns(pkt->duration, base) : 0);
    feed_fold(work, (uint64_t)pkt->size);
    const uint8_t *data = pkt->data;
    size_t remaining = (size_t)pkt->size;
    while (remaining >= 8) {
        feed_fold(work, AV_RL64(data));
        data += 8;
        remaining -= 8;
    }
    if (remaining) {
        uint64_t tail = 0;
        for (size_t index = 0; index < remaining; ++index) tail |= (uint64_t)data[index] << (index * 8);
        feed_fold(work, tail);
    }
}

static int feed_run(const AVInputFormat *demux, const uint8_t *input, size_t size, uint8_t mode,
                    int64_t milliseconds, FeedWork *work) {
    FeedSource source = {input, (int64_t)size, 0};
    AVFormatContext *format = avformat_alloc_context();
    AVIOContext *io = NULL;
    AVPacket *pkt = NULL;
    int complete = 0;
    uint8_t *buffer = av_malloc(FEED_QUANTUM);
    if (!format || !buffer) goto done;
    io = avio_alloc_context(buffer, FEED_QUANTUM, 0, &source, feed_read, NULL, feed_seek);
    if (!io) goto done;
    buffer = NULL;
    format->pb = io;
    format->flags |= AVFMT_FLAG_CUSTOM_IO;
    if (avformat_open_input(&format, NULL, demux, NULL) < 0 ||
        avformat_find_stream_info(format, NULL) < 0) goto done;
    int video = -1;
    for (unsigned i = 0; i < format->nb_streams; ++i) {
        if (format->streams[i]->codecpar->codec_type == AVMEDIA_TYPE_VIDEO) {
            video = (int)i;
            break;
        }
    }
    if (video < 0) goto done;
    AVRational base = format->streams[video]->time_base;
    pkt = av_packet_alloc();
    if (!pkt) goto done;
    int result;
    if (mode == 2) {
        while ((result = av_read_frame(format, pkt)) >= 0) av_packet_unref(pkt);
        if (result != AVERROR_EOF) goto done;
    }
    if (mode != 0) {
        int64_t target = av_rescale_q(milliseconds, (AVRational){1, 1000}, base);
        if (av_seek_frame(format, video, target, AVSEEK_FLAG_BACKWARD) < 0) goto done;
    }
    while ((result = av_read_frame(format, pkt)) >= 0) {
        if (pkt->stream_index == video) feed_packet(work, pkt, base);
        av_packet_unref(pkt);
    }
    complete = result == AVERROR_EOF;

done:
    av_packet_free(&pkt);
    avformat_close_input(&format);
    av_free(buffer);
    if (io) av_freep(&io->buffer);
    avio_context_free(&io);
    return complete;
}

/* feed-bench CAPSULE ITERATIONS | feed-fold CAPSULE | feed-memory CAPSULE */
static int feed_main(int argc, char **argv, uint8_t *input, long size) {
    unsigned long iterations = 1;
    int timed = !strcmp(argv[1], "feed-bench"), memory = !strcmp(argv[1], "feed-memory");
    if (timed) {
        char *end = NULL;
        if (argc != 4) return 2;
        iterations = strtoul(argv[3], &end, 10);
        if (*end || !iterations || iterations > 1000000) return 2;
    } else if (argc != 3 || (!memory && strcmp(argv[1], "feed-fold"))) return 2;
    if (size <= 17 || memcmp(input, "SMVCL001", 8) || input[8] > 2 || AV_RB64(input + 9) > INT64_MAX)
        return 2;
    const AVInputFormat *demux = av_find_input_format("matroska");
    if (!demux) return 2;
    if (memory && write(STDOUT_FILENO, "SAFE_MEDIA_BEGIN_V1\n", 20) != 20) return 2;
    uint64_t start = timed ? now_ns() : 0;
    FeedWork total = {0};
    for (unsigned long i = 0; i < iterations; ++i) {
        FeedWork work = {0};
        if (!feed_run(demux, input + 17, (size_t)size - 17, input[8], (int64_t)AV_RB64(input + 9), &work))
            return 1;
        total.packets += work.packets;
        total.bytes += work.bytes;
        feed_fold(&total, work.digest);
    }
    uint64_t elapsed = timed ? now_ns() - start : 0;
    if (memory && write(STDOUT_FILENO, "SAFE_MEDIA_END_V1\n", 18) != 18) return 2;
    printf("{\"schema\":1,\"operation\":\"matroska-video-session\",\"iterations\":%lu,"
           "\"packets\":%" PRIu64 ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64
           ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true}\n",
           iterations, total.packets, total.bytes, total.digest, elapsed);
    return 0;
}
