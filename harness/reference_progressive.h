/* AVC/AAC TS188 through public FFmpeg APIs, with bounded forward reads and
 * two retained units per stream. Files are opened and read inside each operation. */
#ifndef TRIERGON_REFERENCE_PROGRESSIVE_H
#define TRIERGON_REFERENCE_PROGRESSIVE_H
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/log.h>
#include <libavutil/mem.h>

#define PROGRESSIVE_OPERATION "ProgressiveAvcAacTsOwnedFeedV1"
enum {
    PROGRESSIVE_WINDOW = 4096,
    PROGRESSIVE_DEPTH = 2,
    PROGRESSIVE_MAX_TS_BYTES = 64 * 1024 * 1024
};

typedef struct {
    int fd, eof;
    uint64_t size, bytes, reads, max_read;
} ProgressiveInput;
typedef struct {
    int64_t n, d;
} ProgressiveRatio;
typedef struct {
    AVPacket packet;
    int64_t fields[12];
    unsigned nfields;
    uint64_t acquired_at_read;
} ProgressiveHeld;
typedef struct {
    ProgressiveHeld queue[PROGRESSIVE_DEPTH];
    unsigned head, high;
    uint64_t count, bytes, digest, first, across_read, retired_before_eof;
} ProgressiveStream;
typedef struct {
    uint64_t packets, bytes, digest;
} ProgressiveWork;

static int progressive_read_body(void *opaque, uint8_t *buffer, int requested) {
    ProgressiveInput *in = opaque;
    if (requested <= 0)
        return AVERROR(EINVAL);
    if (in->eof)
        return AVERROR_EOF;
    ssize_t count;
    do {
        count = read(in->fd, buffer,
                     requested < PROGRESSIVE_WINDOW ? (size_t)requested : PROGRESSIVE_WINDOW);
    } while (count < 0 && errno == EINTR);
    if (count < 0)
        return AVERROR(errno);
    if (!count) {
        if (in->bytes != in->size)
            return AVERROR_INVALIDDATA;
        in->eof = 1;
        return AVERROR_EOF;
    }
    in->bytes += (uint64_t)count;
    ++in->reads;
    if (in->max_read < (uint64_t)count)
        in->max_read = (uint64_t)count;
    return in->bytes <= in->size ? (int)count : AVERROR_INVALIDDATA;
}

static ProgressiveRatio progressive_ratio(int64_t n, int64_t d) {
    uint64_t a = n < 0 ? 0 - (uint64_t)n : (uint64_t)n;
    uint64_t b = (uint64_t)d;
    while (b) {
        uint64_t remainder = a % b;
        a = b;
        b = remainder;
    }
    return (ProgressiveRatio){n / (int64_t)a, d / (int64_t)a};
}

static int progressive_audio_fields(const AVPacket *p, ProgressiveRatio *previous,
                                    ProgressiveRatio *next, int64_t fields[12]) {
    static const unsigned rates[] = {
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    };
    static const unsigned channels[] = {0, 1, 2, 3, 4, 5, 6, 8};
    const uint8_t *b = p->data;
    if (p->size < 7 || b[0] != 0xff || (b[1] & 0xf6) != 0xf0 || b[2] >> 6 != 1 || (b[6] & 3))
        return AVERROR_INVALIDDATA;
    unsigned index = (b[2] >> 2) & 15;
    unsigned config = ((b[2] & 1) << 2) | (b[3] >> 6);
    int length = ((b[3] & 3) << 11) | (b[4] << 3) | (b[5] >> 5);
    if (index >= sizeof(rates) / sizeof(*rates) || !config || length != p->size ||
        length <= ((b[1] & 1) ? 7 : 9))
        return AVERROR_INVALIDDATA;

    /* The fixture admission scan proves every audio PES is PTS-only.
     * d32 may mirror its PTS into DTS; that is not an explicit wire DTS. */
    if (p->dts != AV_NOPTS_VALUE && p->dts != p->pts)
        return AVERROR_INVALIDDATA;
    ProgressiveRatio pts =
        p->pts == AV_NOPTS_VALUE ? (ProgressiveRatio){0, 0} : progressive_ratio(p->pts, 90000);
    ProgressiveRatio duration = progressive_ratio(1024, rates[index]);
    ProgressiveRatio present = pts.d ? pts : *next;
    if (!present.d ||
        (previous->d && (__int128)present.n * previous->d < (__int128)previous->n * present.d))
        return AVERROR_INVALIDDATA;
    __int128 n = (__int128)present.n * duration.d + (__int128)duration.n * present.d;
    __int128 d = (__int128)present.d * duration.d;
    if (n < INT64_MIN || n > INT64_MAX || d > INT64_MAX)
        return AVERROR(ERANGE);
    *previous = present;
    *next = progressive_ratio((int64_t)n, (int64_t)d);
    const int64_t values[] = {
        rates[index], channels[config], 1024,       pts.n,      pts.d,  0, 0,
        present.n,    present.d,        duration.n, duration.d, length,
    };
    memcpy(fields, values, sizeof(values));
    return 0;
}

static int progressive_video_fields(const AVPacket *p, const AVCodecParameters *codec, int has_dts,
                                    int first, int64_t fields[12]) {
    int rap = !!(p->flags & AV_PKT_FLAG_KEY);
    if ((!rap && first) || p->pts == AV_NOPTS_VALUE || p->dts == AV_NOPTS_VALUE ||
        (!has_dts && p->pts != p->dts) || codec->width <= 0 || codec->height <= 0)
        return AVERROR_INVALIDDATA;
    if (rap) {
        unsigned seen = 0;
        for (int i = 0; i + 3 < p->size; ++i) {
            if (p->data[i] || p->data[i + 1] || p->data[i + 2] != 1)
                continue;
            unsigned type = p->data[i + 3] & 31;
            if (type == 7)
                seen |= 1;
            if (type == 8)
                seen |= 2;
            if (type == 5)
                seen |= 4;
        }
        if (seen != 7)
            return AVERROR_INVALIDDATA;
    }
    const uint64_t mask = (UINT64_C(1) << 33) - 1;
    const int64_t values[] = {
        p->pts,
        p->dts,
        (int64_t)((uint64_t)p->pts & mask),
        has_dts ? (int64_t)((uint64_t)p->dts & mask) : -1,
        rap,
        codec->width,
        codec->height,
        p->size,
    };
    memcpy(fields, values, sizeof(values));
    return 0;
}

static void progressive_mix(uint64_t *value, uint64_t word) {
    *value = *value * 31 + word;
}

static void progressive_retire(ProgressiveStream *stream, ProgressiveHeld *held,
                               const ProgressiveInput *in, int tag, int trace) {
    AVPacket *packet = &held->packet;
    if (!packet->buf)
        return;
    stream->across_read += in->reads > held->acquired_at_read;
    stream->retired_before_eof += !in->eof;
    progressive_mix(&stream->digest, (unsigned)tag);
    if (trace)
        putchar(tag);
    for (unsigned i = 0; i < held->nfields; ++i) {
        progressive_mix(&stream->digest, (uint64_t)held->fields[i]);
        if (trace)
            printf(" %" PRId64, held->fields[i]);
    }
    if (trace)
        putchar(' ');
    for (int i = 0; i < packet->size; ++i) {
        progressive_mix(&stream->digest, packet->data[i]);
        if (trace)
            printf("%02x", (unsigned)packet->data[i]);
    }
    if (trace)
        putchar('\n');
    stream->bytes += (uint64_t)packet->size;
    av_packet_unref(packet);
}

static int progressive_unique_stream(const AVFormatContext *format, enum AVMediaType type,
                                     enum AVCodecID codec) {
    int selected = -1;
    for (unsigned i = 0; i < format->nb_streams; ++i) {
        const AVStream *stream = format->streams[i];
        if (stream->codecpar->codec_type != type)
            continue;
        if (stream->codecpar->codec_id != codec || selected != -1 || i > INT32_MAX ||
            stream->time_base.num != 1 || stream->time_base.den != 90000)
            return -1;
        selected = (int)i;
    }
    return selected;
}

static int progressive_operation(const char *path, int has_dts, ProgressiveWork *work, int trace,
                                 int require_streaming) {
    ProgressiveInput in = {.fd = -1};
    ProgressiveStream streams[2] = {0};
    AVFormatContext *format = NULL;
    AVIOContext *io = NULL;
    AVPacket *packet = NULL;
    uint8_t *buffer = NULL;
    ProgressiveRatio previous = {0}, next = {0};
    const char *stage = "open forward body";
    int rc = 0;
    in.fd = open(path, O_RDONLY);
    if (in.fd < 0) {
        rc = AVERROR(errno);
        goto done;
    }
    struct stat facts;
    if (fstat(in.fd, &facts) || !S_ISREG(facts.st_mode) || facts.st_size <= 0 ||
        facts.st_size > PROGRESSIVE_MAX_TS_BYTES || facts.st_size % 188) {
        rc = AVERROR_INVALIDDATA;
        goto done;
    }
    in.size = (uint64_t)facts.st_size;
    stage = "allocate custom AVIO";
    rc = AVERROR(ENOMEM);
    format = avformat_alloc_context();
    buffer = av_malloc(PROGRESSIVE_WINDOW);
    if (!format || !buffer)
        goto done;
    io = avio_alloc_context(buffer, PROGRESSIVE_WINDOW, 0, &in, progressive_read_body, NULL, NULL);
    if (!io)
        goto done;
    buffer = NULL;
    io->seekable = 0;
    format->pb = io;
    format->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_NOFILLIN;
    format->probesize = 8192;
    format->max_analyze_duration = 250000;
    stage = "open bounded MPEG-TS";
    rc = avformat_open_input(&format, NULL, av_find_input_format("mpegts"), NULL);
    if (rc < 0)
        goto done;
    stage = "find bounded stream info";
    rc = avformat_find_stream_info(format, NULL);
    if (rc < 0)
        goto done;
    stage = "select AVC and ADTS AAC";
    int vi = progressive_unique_stream(format, AVMEDIA_TYPE_VIDEO, AV_CODEC_ID_H264);
    int ai = progressive_unique_stream(format, AVMEDIA_TYPE_AUDIO, AV_CODEC_ID_AAC);
    if (vi < 0 || ai < 0 || format->nb_streams != 2) {
        rc = AVERROR_INVALIDDATA;
        goto done;
    }
    packet = av_packet_alloc();
    if (!packet) {
        rc = AVERROR(ENOMEM);
        goto done;
    }
    stage = "read progressive packet";
    while ((rc = av_read_frame(format, packet)) >= 0) {
        const int si = packet->stream_index;
        if ((si != vi && si != ai) || packet->size <= 0 || !packet->data || !packet->buf ||
            (packet->flags & AV_PKT_FLAG_CORRUPT)) {
            rc = AVERROR_INVALIDDATA;
            goto done;
        }
        ProgressiveStream *stream = &streams[si == ai];
        ProgressiveHeld *slot = &stream->queue[stream->head];
        int tag = si == vi ? 'v' : 'a';
        int64_t fields[12] = {0};
        rc = si == vi ? progressive_video_fields(packet, format->streams[vi]->codecpar, has_dts,
                                                 !stream->count, fields)
                      : progressive_audio_fields(packet, &previous, &next, fields);
        if (rc < 0) {
            stage = si == vi ? "AVC payload or clock" : "AAC payload or clock";
            goto done;
        }
        if (!stream->count)
            stream->first = in.bytes;
        if (trace)
            printf("o %c\n", tag);
        progressive_retire(stream, slot, &in, tag, trace);
        memcpy(slot->fields, fields, sizeof(fields));
        slot->nfields = si == vi ? 8 : 12;
        slot->acquired_at_read = in.reads;
        av_packet_move_ref(&slot->packet, packet);
        stream->head = (stream->head + 1) % PROGRESSIVE_DEPTH;
        ++stream->count;
        if (stream->high < PROGRESSIVE_DEPTH)
            ++stream->high;
    }
    if (rc != AVERROR_EOF)
        goto done;
    stage = "verify clean EOF and owned streaming retention";
    if (!in.eof || in.bytes != in.size || (io->error < 0 && io->error != AVERROR_EOF)) {
        rc = AVERROR_INVALIDDATA;
        goto done;
    }
    for (unsigned i = 0; i < 2; ++i) {
        ProgressiveStream *stream = &streams[i];
        for (unsigned j = 0; j < PROGRESSIVE_DEPTH; ++j)
            progressive_retire(stream, &stream->queue[(stream->head + j) % PROGRESSIVE_DEPTH], &in,
                               i ? 'a' : 'v', trace);
        if (stream->high != PROGRESSIVE_DEPTH ||
            (require_streaming &&
             (stream->first >= in.size || !stream->across_read || !stream->retired_before_eof))) {
            rc = AVERROR_INVALIDDATA;
            goto done;
        }
        work->packets += stream->count;
        work->bytes += stream->bytes;
        progressive_mix(&work->digest, stream->count);
        progressive_mix(&work->digest, stream->digest);
    }
    rc = 0;

done:
    for (unsigned i = 0; i < 2; ++i)
        for (unsigned j = 0; j < PROGRESSIVE_DEPTH; ++j)
            av_packet_unref(&streams[i].queue[j].packet);
    av_packet_free(&packet);
    avformat_close_input(&format);
    if (io) {
        av_freep(&io->buffer);
        avio_context_free(&io);
    }
    av_free(buffer);
    if (in.fd >= 0 && close(in.fd) && !rc)
        rc = AVERROR(errno);
    if (rc < 0)
        fprintf(stderr, "reference stage: %s\n", stage);
    else if (trace)
        fprintf(stderr,
                "STREAM {\"input_bytes\":%" PRIu64 ",\"reads\":%" PRIu64 ",\"max_read\":%" PRIu64
                ",\"first\":[%" PRIu64 ",%" PRIu64 "],\"high\":[%u,%u]"
                ",\"across_read\":[%" PRIu64 ",%" PRIu64 "],\"retired_before_eof\":[%" PRIu64
                ",%" PRIu64 "],\"retired\":true}\n",
                in.bytes, in.reads, in.max_read, streams[0].first, streams[1].first,
                streams[0].high, streams[1].high, streams[0].across_read, streams[1].across_read,
                streams[0].retired_before_eof, streams[1].retired_before_eof);
    return rc;
}

static uint64_t progressive_now_ns(void) {
    struct timespec now;
    return clock_gettime(CLOCK_MONOTONIC, &now)
               ? 0
               : (uint64_t)now.tv_sec * UINT64_C(1000000000) + (uint64_t)now.tv_nsec;
}

static int progressive_main(int argc, char **argv) {
    const int trace = argc == 5 && !strcmp(argv[1], "progressive-trace");
    const int fold = argc == 5 && !strcmp(argv[1], "progressive-fold");
    const int mem = argc == 5 && !strcmp(argv[1], "progressive-memory");
    const int bench = argc == 6 && !strcmp(argv[1], "progressive-bench") && !strcmp(argv[3], "50");
    if ((!trace && !fold && !mem && !bench) ||
        (strcmp(argv[argc - 2], "pts-only") && strcmp(argv[argc - 2], "pts-dts")) ||
        (strcmp(argv[argc - 1], "before-eof") && strcmp(argv[argc - 1], "at-eof"))) {
        fputs("usage: reference progressive-trace|progressive-fold|progressive-memory FILE CLOCK "
              "PROGRESS | "
              "progressive-bench FILE 50 CLOCK PROGRESS\n",
              stderr);
        return 2;
    }
#if !defined(__linux__) || !defined(__aarch64__)
    if (mem || bench) {
        fputs("numeric modes require native Linux AArch64\n", stderr);
        return 2;
    }
#endif
    av_log_set_level(AV_LOG_ERROR);
    const int has_dts = !strcmp(argv[argc - 2], "pts-dts");
    const unsigned iterations = bench ? 50 : 1;
    const int require_streaming = !strcmp(argv[argc - 1], "before-eof");
    ProgressiveWork total = {0};
    if (mem && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18)
        return 1;
    const uint64_t start = bench ? progressive_now_ns() : 0;
    if (bench && !start)
        return 1;
    for (unsigned i = 0; i < iterations; ++i) {
        ProgressiveWork once = {0};
        int rc = progressive_operation(argv[2], has_dts, &once, trace, require_streaming);
        if (rc < 0) {
            char error[AV_ERROR_MAX_STRING_SIZE];
            av_strerror(rc, error, sizeof(error));
            fprintf(stderr, "reference: %s (%d)\n", error, rc);
            return 1;
        }
        total.packets += once.packets;
        total.bytes += once.bytes;
        progressive_mix(&total.digest, once.digest);
    }
    uint64_t end = bench ? progressive_now_ns() : 0;
    if (bench && end <= start)
        return 1;
    if (mem && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16)
        return 1;
    printf("{\"schema\":1,\"operation\":\"" PROGRESSIVE_OPERATION "\",\"iterations\":%u"
           ",\"packets\":%" PRIu64 ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64
           ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true}\n",
           iterations, total.packets, total.bytes, total.digest, end - start);
    return fflush(stdout) == EOF;
}

#endif
