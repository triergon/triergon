/* Selected transport access units through public demuxer and framer APIs. */
#include <errno.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include "reference_ts_input.h"
#include "reference_digest.h"

#ifdef TRIERGON_REFERENCE_MPG123
#include <mpg123.h>
#define TS_MATCH_PREFIX "ts-free-audio-"
#define TS_MATCH_OPERATION "mpegts-free-format-frames"
#elif defined(TRIERGON_REFERENCE_AUDIO_SEEK)
#define TS_MATCH_PREFIX "ts-audio-seek-"
#define TS_MATCH_OPERATION "mpegts-previous-audio"
#else
#define TS_MATCH_PREFIX "ts-seek-"
#define TS_MATCH_OPERATION "mpegts-previous-picture"
#endif

typedef struct {
    TsInput input;
    AVFormatContext *format;
    AVIOContext *io;
    AVPacket *packet;
    int program, pid, carrier_size;
} TsReader;

static void reader_close(TsReader *reader)
{
    av_packet_free(&reader->packet);
    avformat_close_input(&reader->format);
    if (reader->io) {
        av_freep(&reader->io->buffer);
        avio_context_free(&reader->io);
    }
}

static int reader_open(TsReader *reader, int parse)
{
    reader->input.used = 0;
    reader->format = avformat_alloc_context();
    reader->packet = av_packet_alloc();
    if (!reader->format || !reader->packet)
        return AVERROR(ENOMEM);
    uint8_t *buffer = av_malloc(TS_AVIO_BYTES);
    if (!buffer)
        return AVERROR(ENOMEM);
    reader->io = avio_alloc_context(buffer, TS_AVIO_BYTES, 0, &reader->input,
                                    ts_read, NULL, ts_seek);
    if (!reader->io) {
        av_free(buffer);
        return AVERROR(ENOMEM);
    }
    reader->format->pb = reader->io;
    reader->format->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_NOFILLIN;
    if (!parse)
        reader->format->flags |= AVFMT_FLAG_NOPARSE;
    return avformat_open_input(&reader->format, NULL, av_find_input_format("mpegts"), NULL);
}

static int reader_next(TsReader *reader)
{
    av_packet_unref(reader->packet);
    for (;;) {
        int rc = av_read_frame(reader->format, reader->packet);
        if (rc < 0)
            return rc;
        int index = reader->packet->stream_index;
        const AVStream *stream = reader->format->streams[index];
        if (stream->id == reader->pid && ts_in_program(reader->format, index, reader->program))
            return stream->time_base.num == 1 && stream->time_base.den == 90000
                ? 0 : AVERROR_INVALIDDATA;
        av_packet_unref(reader->packet);
    }
}

static void fold_clock(TsWork *out, int64_t pts, int64_t dts)
{
    fold(&out->digest, (uint64_t)pts);
    fold(&out->digest, (uint64_t)dts);
}

#ifndef TRIERGON_REFERENCE_MPG123
static int match(TsReader *reader, int64_t target, int preroll, TsWork *out)
{
    int rc = reader_open(reader, 1);
    if (rc < 0)
        return rc;
    int64_t landing = AV_NOPTS_VALUE;
    uint64_t ordinal = 0, chosen = 0;
    while ((rc = reader_next(reader)) == 0) {
        const AVPacket *p = reader->packet;
#ifdef TRIERGON_REFERENCE_AUDIO_SEEK
        if (p->size <= 0)
            return AVERROR_INVALIDDATA;
        if (p->pts != AV_NOPTS_VALUE) {
#else
        if (p->flags & AV_PKT_FLAG_KEY) {
            if (p->pts == AV_NOPTS_VALUE)
                return AVERROR_INVALIDDATA;
#endif
            if (p->pts <= target && p->pts >= landing) {
                landing = p->pts;
                chosen = ordinal;
            }
        }
        ++ordinal;
    }
    if (rc != AVERROR_EOF || landing == AV_NOPTS_VALUE)
        return AVERROR_INVALIDDATA;
    reader_close(reader);
    if ((rc = reader_open(reader, 1)) < 0)
        return rc;
    fold(&out->digest, (uint64_t)target);
    fold(&out->digest, (uint64_t)landing);
    fold(&out->digest, (uint64_t)(preroll ? target : landing));
    for (ordinal = 0; (rc = reader_next(reader)) == 0; ++ordinal) {
        if (ordinal < chosen)
            continue;
        const AVPacket *p = reader->packet;
#ifdef TRIERGON_REFERENCE_AUDIO_SEEK
        if (ordinal == chosen && p->pts != landing)
            return AVERROR_INVALIDDATA;
#else
        int key = !!(p->flags & AV_PKT_FLAG_KEY);
        if (ordinal == chosen && (!key || p->pts != landing))
            return AVERROR_INVALIDDATA;
#endif
        fold_clock(out, p->pts, p->dts == AV_NOPTS_VALUE ? p->pts : p->dts);
#ifndef TRIERGON_REFERENCE_AUDIO_SEEK
        fold(&out->digest, (uint64_t)key);
#endif
        fold(&out->digest, (uint64_t)p->size);
        fold_bytes(&out->digest, p->data, (size_t)p->size);
        out->packets++;
        out->bytes += (uint64_t)p->size;
    }
    return rc == AVERROR_EOF && out->packets ? 0 : AVERROR_INVALIDDATA;
}
#else
typedef struct { uint64_t offset; int64_t pts, dts; } PesClock;

typedef struct {
    mpg123_handle *framer;
    uint64_t fed;
    PesClock *clocks;
    size_t clock_count, clock_capacity;
    uint8_t *raw;
    size_t raw_head, raw_end, raw_capacity;
    int layer_one_slots;
} AudioFeed;

static int pes_timestamp(const uint8_t *bytes, unsigned prefix, int64_t *time)
{
    if (bytes[0] >> 4 != prefix || !(bytes[0] & bytes[2] & bytes[4] & 1))
        return AVERROR_INVALIDDATA;
    *time = ((int64_t)(bytes[0] & 14) << 29) | ((int64_t)bytes[1] << 22)
        | ((int64_t)(bytes[2] & 254) << 14) | ((int64_t)bytes[3] << 7)
        | (bytes[4] >> 1);
    return 0;
}

/* The demuxer may populate DTS when the PES only declares PTS. Locate the
 * original header from its packet position to reproduce the source clock. */
static int source_clock(const TsReader *reader, PesClock *clock)
{
    clock->pts = clock->dts = AV_NOPTS_VALUE;
    int64_t position = reader->packet->pos;
    size_t carrier = (size_t)reader->carrier_size;
    if (position < 0 || (uint64_t)position >= reader->input.size)
        return AVERROR_INVALIDDATA;
    size_t offset = (size_t)position / carrier * carrier;
    uint8_t header[19];
    size_t used = 0, wanted = 9;
    int start = 1;
    for (; carrier <= reader->input.size - offset && used < wanted; offset += carrier) {
        const uint8_t *p = reader->input.data + offset + carrier - 188;
        if (p[0] != 0x47)
            return AVERROR_INVALIDDATA;
        int pid = ((p[1] & 31) << 8) | p[2];
        if (pid != reader->pid) {
            if (start)
                return AVERROR_INVALIDDATA;
            continue;
        }
        int pusi = !!(p[1] & 0x40);
        if (pusi != start || (p[1] & 0x80) || (p[3] & 0xc0))
            return AVERROR_INVALIDDATA;
        int control = (p[3] >> 4) & 3;
        if (!(control & 1)) {
            if (start || control == 0)
                return AVERROR_INVALIDDATA;
            continue;
        }
        start = 0;
        size_t at = control == 3 ? 5u + p[4] : 4u;
        for (; at < 188 && used < wanted; ++at) {
            header[used++] = p[at];
            if (used == 9) {
                if (header[0] || header[1] || header[2] != 1
                    || header[3] < 0xc0 || header[3] > 0xdf || (header[6] & 0xc0) != 0x80
                    || (header[7] & 0xc0) == 0x40)
                    return AVERROR_INVALIDDATA;
                if (header[7] & 0x80)
                    wanted = header[7] & 0x40 ? 19 : 14;
                if (header[8] < wanted - 9)
                    return AVERROR_INVALIDDATA;
            }
        }
    }
    if (used != wanted)
        return AVERROR_INVALIDDATA;
    if (wanted > 9 && pes_timestamp(header + 9, wanted == 19 ? 3u : 2u, &clock->pts) < 0)
        return AVERROR_INVALIDDATA;
    return wanted == 19 ? pes_timestamp(header + 14, 1, &clock->dts) : 0;
}

/* mpg123 reports frame offsets in the fed elementary stream. Each frame takes
 * the latest unclaimed PES clock at or before its start, including untimed PES. */
static void frame_clock(TsWork *out, PesClock *clocks, size_t *count, uint64_t start)
{
    size_t used = 0;
    PesClock anchor = {0, AV_NOPTS_VALUE, AV_NOPTS_VALUE};
    while (used < *count && clocks[used].offset <= start)
        anchor = clocks[used++];
    if (used) {
        *count -= used;
        memmove(clocks, clocks + used, *count * sizeof(*clocks));
    }
    fold_clock(out, anchor.pts, anchor.dts);
}

static int feed_next(TsReader *reader, AudioFeed *audio)
{
    int rc = reader_next(reader);
    if (rc < 0)
        return rc;
    const AVPacket *p = reader->packet;
    if (p->size <= 0)
        return AVERROR_INVALIDDATA;
    if (audio->clock_count == audio->clock_capacity) {
        size_t next = audio->clock_capacity ? audio->clock_capacity * 2 : 8;
        PesClock *more = av_realloc_array(audio->clocks, next, sizeof(*audio->clocks));
        if (!more)
            return AVERROR(ENOMEM);
        audio->clocks = more;
        audio->clock_capacity = next;
    }
    PesClock clock = {.offset = audio->fed};
    if ((rc = source_clock(reader, &clock)) < 0)
        return rc;
    audio->clocks[audio->clock_count++] = clock;
    if (audio->layer_one_slots) {
        size_t size = (size_t)p->size;
        if (audio->raw_capacity - audio->raw_end < size) {
            size_t retained = audio->raw_end - audio->raw_head;
            if (retained)
                memmove(audio->raw, audio->raw + audio->raw_head, retained);
            audio->raw_head = 0;
            audio->raw_end = retained;
            if (audio->raw_capacity - retained < size) {
                size_t capacity = retained + size;
                uint8_t *more = av_realloc(audio->raw, capacity);
                if (!more)
                    return AVERROR(ENOMEM);
                audio->raw = more;
                audio->raw_capacity = capacity;
            }
        }
        memcpy(audio->raw + audio->raw_end, p->data, size);
        audio->raw_end += size;
    }
    audio->fed += (uint64_t)p->size;
    return mpg123_feed(audio->framer, p->data, (size_t)p->size) == MPG123_OK
        ? 0 : AVERROR_INVALIDDATA;
}

static int emit_audio(TsReader *reader, AudioFeed *audio, TsWork *out)
{
    mpg123_handle *framer = audio->framer;
    unsigned long header;
    unsigned char *data;
    size_t size;
    struct mpg123_frameinfo2 info;
    const int64_t start = mpg123_framepos64(framer);
    if (start < 0 || (uint64_t)start != out->bytes
        || mpg123_framedata(framer, &header, &data, &size) != MPG123_OK
        || mpg123_info2(framer, &info) != MPG123_OK
        || info.vbr != MPG123_CBR || (header & 0xf000) != 0)
        return AVERROR_INVALIDDATA;

    const uint8_t prefix[4] = {
        (uint8_t)(header >> 24), (uint8_t)(header >> 16),
        (uint8_t)(header >> 8), (uint8_t)header
    };
    size_t extra = 0;
    if (audio->layer_one_slots) {
        if (info.layer != 1)
            return AVERROR_INVALIDDATA;
        /* mpg123 locates the Layer I headers but returns one byte for a
         * free-format padding slot. Check the complete four-byte source
         * extent and include the three bytes absent from its body view. */
        extra = (header & 0x200) ? 3 : 0;
        size_t extent = sizeof(prefix) + size + extra;
        while (audio->raw_end - audio->raw_head < extent) {
            int rc = feed_next(reader, audio);
            if (rc < 0)
                return rc == AVERROR_EOF ? AVERROR_INVALIDDATA : rc;
        }
        size_t parsed_size;
        if (mpg123_framedata(framer, NULL, &data, &parsed_size) != MPG123_OK
            || parsed_size != size)
            return AVERROR_INVALIDDATA;
        const uint8_t *raw = audio->raw + audio->raw_head;
        if (memcmp(raw, prefix, sizeof(prefix))
            || memcmp(raw + sizeof(prefix), data, size))
            return AVERROR_INVALIDDATA;
    }

    frame_clock(out, audio->clocks, &audio->clock_count, (uint64_t)start);
    fold(&out->digest, (uint64_t)start);
    fold(&out->digest, (uint64_t)info.rate);
    fold(&out->digest, (uint64_t)mpg123_spf(framer));
    size_t extent = sizeof(prefix) + size + extra;
    fold(&out->digest, extent);
    fold_bytes(&out->digest, prefix, sizeof(prefix));
    fold_bytes(&out->digest, data, size);
    if (audio->layer_one_slots) {
        fold_bytes(&out->digest, audio->raw + audio->raw_head + sizeof(prefix) + size, extra);
        audio->raw_head += extent;
    }
    out->packets++;
    out->bytes += extent;
    return 0;
}

static int free_audio(TsReader *reader, AudioFeed *audio, TsWork *out)
{
    int rc = reader_open(reader, 0);
    if (rc < 0)
        return rc;
    for (;;) {
        int status = mpg123_framebyframe_next(audio->framer);
        if (status == MPG123_NEED_MORE) {
            rc = feed_next(reader, audio);
            if (rc == AVERROR_EOF)
                return audio->fed == out->bytes && out->packets ? 0 : AVERROR_INVALIDDATA;
        } else if (status == MPG123_OK || status == MPG123_NEW_FORMAT) {
            rc = emit_audio(reader, audio, out);
        } else {
            return AVERROR_INVALIDDATA;
        }
        if (rc < 0)
            return rc;
    }
}

static int match(TsReader *reader, int64_t target, int layer_one_slots, TsWork *out)
{
    (void)target;
    mpg123_handle *framer = mpg123_new(NULL, NULL);
    if (!framer)
        return AVERROR(ENOMEM);
    AudioFeed audio = {.framer = framer, .layer_one_slots = layer_one_slots};
    long flags = MPG123_QUIET | MPG123_IGNORE_INFOFRAME | (layer_one_slots ? 0 : MPG123_NO_RESYNC);
    int rc = AVERROR_INVALIDDATA;
    if (mpg123_param(framer, MPG123_ADD_FLAGS, flags, 0) == MPG123_OK
        && mpg123_open_feed(framer) == MPG123_OK)
        rc = free_audio(reader, &audio, out);
    av_free(audio.clocks);
    av_free(audio.raw);
    mpg123_delete(framer);
    return rc;
}
#endif

static int number(const char *text, long long min, long long max, long long *value)
{
    char *end;
    errno = 0;
    *value = strtoll(text, &end, 10);
    return !errno && end != text && !*end && *value >= min && *value <= max;
}

static uint64_t monotonic_ns(void)
{
    struct timespec time;
    clock_gettime(CLOCK_MONOTONIC, &time);
    return (uint64_t)time.tv_sec * UINT64_C(1000000000) + (uint64_t)time.tv_nsec;
}

int main(int argc, char **argv)
{
    const size_t prefix = sizeof(TS_MATCH_PREFIX) - 1;
    if (argc < 3 || strncmp(argv[1], TS_MATCH_PREFIX, prefix))
        return 2;
    const char *mode = argv[1] + prefix;
    const int bench = !strcmp(mode, "bench"), memory = !strcmp(mode, "memory");
    if (!bench && !memory && strcmp(mode, "fold"))
        return 2;
    const int start = bench ? 4 : 3;
    long long iterations = 1, program, pid, target = 0;
    int option = 0, track = start + 2;
#ifdef TRIERGON_REFERENCE_AUDIO_SEEK
    if (argc >= start + 6 && (!strcmp(argv[track], "--dvb") || !strcmp(argv[track], "--atsc")))
        ++track;
#endif
#ifdef TRIERGON_REFERENCE_MPG123
    option = argc == start + 4 && !strcmp(argv[start+3], "layer1-padding");
    if (!option && argc != start + 3)
        return 2;
#else
    const int repeat = argc == track + 4;
    long long repeat_target = 0;
    if ((!repeat && argc != track + 3) || !number(argv[track+1], INT64_MIN, INT64_MAX, &target)
        || (repeat && !number(argv[track+3], INT64_MIN, INT64_MAX, &repeat_target)))
        return 2;
    option = !strcmp(argv[track+2], "preroll");
    if (!option && strcmp(argv[track+2], "previous"))
        return 2;
#endif
    if ((bench && !number(argv[3], 1, 1000000, &iterations))
        || (strcmp(argv[start], "188") && strcmp(argv[start], "192"))
        || !number(argv[start+1], 1, 65535, &program)
        || !number(argv[track], 0x10, 0x1ffe, &pid))
        return 2;
    FILE *file = fopen(argv[2], "rb");
    if (!file)
        return 2;
    long length = fseek(file, 0, SEEK_END) ? -1 : ftell(file);
    if (length < 1 || length > 64 * 1024 * 1024 || fseek(file, 0, SEEK_SET)) {
        fclose(file);
        return 2;
    }
    uint8_t *bytes = malloc((size_t)length);
    size_t read = bytes ? fread(bytes, 1, (size_t)length, file) : 0;
    fclose(file);
    int result = 2;
    if (read != (size_t)length)
        goto done;
    av_log_set_level(bench || memory ? AV_LOG_QUIET : AV_LOG_ERROR);
#ifdef TRIERGON_REFERENCE_MPG123
    if (mpg123_init() != MPG123_OK)
        goto done;
#endif
    if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18)
        goto done;
    uint64_t before = bench ? monotonic_ns() : 0;
    TsWork totals = {0};
    long long runs = 0;
    int rc = 0;
    for (long long i = 0; i < iterations; ++i) {
        TsReader reader = {
            .input = {bytes, (size_t)length, 0}, .program = (int)program,
            .pid = (int)pid, .carrier_size = atoi(argv[start])
        };
        TsWork work = {0};
        rc = match(&reader, (int64_t)target, option, &work);
        reader_close(&reader);
#ifndef TRIERGON_REFERENCE_MPG123
        if (!rc && repeat) {
            rc = match(&reader, (int64_t)repeat_target, option, &work);
            reader_close(&reader);
        }
#endif
        runs++;
        fold(&totals.digest, work.digest);
        totals.packets += work.packets;
        totals.bytes += work.bytes;
        if (rc < 0)
            break;
    }
    uint64_t elapsed = bench ? monotonic_ns() - before : 0;
    if (memory && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16)
        goto done;
    if (rc < 0 && rc != AVERROR_INVALIDDATA) {
        char error[128];
        av_strerror(rc, error, sizeof(error));
        fprintf(stderr, "%s: %s\n", TS_MATCH_OPERATION, error);
        goto done;
    }
    printf("{\"schema\":1,\"operation\":\"%s\",\"iterations\":%lld,\"packets\":%" PRIu64
           ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 ",\"elapsed_ns\":%" PRIu64 ",\"eof\":%s",
           TS_MATCH_OPERATION, runs, totals.packets, totals.bytes, totals.digest, elapsed,
           rc < 0 ? "false" : "true");
    if (rc < 0)
        printf(",\"refusal\":\"invalid reference frame or incomplete coverage\"");
    puts("}");
    result = 0;
done:
    free(bytes);
    return result;
}
