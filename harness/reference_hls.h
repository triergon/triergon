/* Own reference tooling: one independently addressable TS188 HLS segment through
 * public FFmpeg APIs, the same operation as playback-worker's hls modes.
 *
 * The segment is read from the preloaded input as a forward-only source. Stream
 * discovery selects one video and one audio stream with the stock parsers and no
 * decoders for these codecs, every unit is retained through the end of the segment,
 * the application audio clock is resolved, and the units are folded video first and
 * audio second, followed by the last video and audio presentation times. */
#include <errno.h>
#include <libavcodec/avcodec.h>
#include <libavutil/log.h>
#include <libavutil/mathematics.h>
#include <libavutil/mem.h>

#define HLS_OPERATION "hls-segment-feed"
enum { HLS_AVIO_BYTES = 64 * 1024, HLS_TS_BYTES = 188, HLS_MAX_INPUT = 64 * 1024 * 1024 };

typedef struct { const uint8_t *data; size_t size, used; } HlsInput;
typedef struct { const uint8_t *data; size_t size; } HlsSpan;
typedef struct {
    AVPacket packet;
    int video, rap;
    int64_t pts, dts, duration;
} HlsUnit;
typedef struct { HlsUnit *data; size_t count, capacity; } HlsRetained;
typedef struct { uint64_t packets, bytes, digest; } HlsWork;
typedef struct { enum AVCodecID video, audio; } HlsProfile;

static int hls_read_body(void *opaque, uint8_t *buffer, int requested)
{
    HlsInput *in = opaque;
    if (requested <= 0)
        return AVERROR(EINVAL);
    size_t length = in->size - in->used;
    if (!length)
        return AVERROR_EOF;
    if (length > (size_t)requested)
        length = (size_t)requested;
    memcpy(buffer, in->data + in->used, length);
    in->used += length;
    return (int)length;
}

static int hls_video_wire(HlsSpan bytes, enum AVCodecID codec, int rap)
{
    if (!rap)
        return 0;
    /* The additive corpus gives every RAP its in-band parameter sets. The
     * historical worker retains the separate cached-parameter-set adapter. */
    unsigned seen = 0;
    for (size_t i = 0; i + 3 < bytes.size; ++i) {
        if (bytes.data[i] || bytes.data[i + 1] || bytes.data[i + 2] != 1)
            continue;
        unsigned header = bytes.data[i + 3];
        if (codec == AV_CODEC_ID_H264) {
            unsigned type = header & 31;
            if (type == 7) seen |= 1;
            if (type == 8) seen |= 2;
            if (type == 5) seen |= 4;
        } else {
            unsigned type = (header >> 1) & 63;
            if (type == 32) seen |= 1;
            if (type == 33) seen |= 2;
            if (type == 34) seen |= 4;
            if (type >= 16 && type <= 23) seen |= 8;
        }
    }
    return seen == (codec == AV_CODEC_ID_H264 ? 7u : 15u) ? 0 : AVERROR_INVALIDDATA;
}

static int64_t hls_audio_duration_ns(HlsSpan wire, enum AVCodecID codec)
{
    static const unsigned aac_rate[] = {
        96000, 88200, 64000, 48000, 44100, 32000, 24000,
        22050, 16000, 12000, 11025, 8000, 7350,
    };
    static const unsigned dolby_rate[] = {48000, 44100, 32000};
    static const unsigned ac3_kbps[] = {
        32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192,
        224, 256, 320, 384, 448, 512, 576, 640,
    };
    const uint8_t *p = wire.data;
    if (wire.size < 7)
        return AVERROR_INVALIDDATA;
    unsigned rate, samples;
    if (codec == AV_CODEC_ID_AAC) {
        size_t length = ((size_t)(p[3] & 3) << 11) | ((size_t)p[4] << 3) | (p[5] >> 5);
        unsigned index = (p[2] >> 2) & 15;
        if (p[0] != 0xff || (p[1] & 0xf6) != 0xf0 || p[2] >> 6 != 1 ||
            (p[6] & 3) || index >= sizeof(aac_rate) / sizeof(*aac_rate) || length != wire.size)
            return AVERROR_INVALIDDATA;
        rate = aac_rate[index];
        samples = 1024;
    } else {
        if (p[0] != 0x0b || p[1] != 0x77)
            return AVERROR_INVALIDDATA;
        unsigned bsid = p[5] >> 3;
        unsigned fscod = p[4] >> 6;
        if (codec == AV_CODEC_ID_AC3) {
            unsigned size_code = p[4] & 63;
            if (bsid > 8 || fscod >= 3 || size_code >= 2 * (sizeof(ac3_kbps) / sizeof(*ac3_kbps)))
                return AVERROR_INVALIDDATA;
            rate = dolby_rate[fscod];
            unsigned words = ac3_kbps[size_code / 2] * 96000 / rate;
            if (rate == 44100)
                words += size_code % 2;
            if (wire.size != words * 2)
                return AVERROR_INVALIDDATA;
            samples = 1536;
        } else {
            size_t length = ((((size_t)p[2] & 7) << 8 | p[3]) + 1) * 2;
            if (codec != AV_CODEC_ID_EAC3 || bsid < 11 || bsid > 16 || p[2] >> 6 || length != wire.size)
                return AVERROR_INVALIDDATA;
            unsigned code = (p[4] >> 4) & 3;
            if (fscod == 3) {
                if (code >= 3)
                    return AVERROR_INVALIDDATA;
                rate = dolby_rate[code] / 2;
                samples = 1536;
            } else {
                static const unsigned blocks[] = {1, 2, 3, 6};
                rate = dolby_rate[fscod];
                samples = 256 * blocks[code];
            }
        }
    }
    return (int64_t)samples * 1000000000 / rate;
}

static void hls_mix(uint64_t *fold, uint64_t word)
{
    *fold = 31 * *fold + word;
}

/* Explicit application projection over retained parser-only AAC and Dolby packets.
 * Source clocks are still available in each original AVPacket. */
static int64_t hls_ns_from_90k(int64_t ticks)
{
    return av_rescale_rnd(ticks, 1000000000, 90000, AV_ROUND_NEAR_INF);
}

static int hls_project_application_audio(HlsRetained *retained)
{
    int64_t leading = 0, first_ns = 0;
    int anchored = 0;
    for (size_t i = 0; i < retained->count; ++i) {
        HlsUnit *unit = &retained->data[i];
        if (unit->video)
            continue;
        int64_t anchor = unit->pts == AV_NOPTS_VALUE ? unit->dts : unit->pts;
        int64_t duration = unit->duration;
        if (duration <= 0)
            return AVERROR(ERANGE);
        if (anchor != AV_NOPTS_VALUE) {
            int64_t ns = hls_ns_from_90k(anchor);
            if (ns == INT64_MIN)
                return AVERROR(ERANGE);
            unit->pts = ns;
            if (!anchored) {
                if (ns < INT64_MIN + leading)
                    return AVERROR(ERANGE);
                first_ns = ns - leading;
                anchored = 1;
            }
        } else if (!anchored) {
            if (leading > INT64_MAX - duration)
                return AVERROR(ERANGE);
            leading += duration;
        }
    }

    int seen = 0;
    int64_t previous = 0, previous_duration = 0;
    for (size_t i = 0; i < retained->count; ++i) {
        HlsUnit *unit = &retained->data[i];
        if (unit->video)
            continue;
        if (unit->pts == AV_NOPTS_VALUE) {
            if (seen && previous > INT64_MAX - previous_duration)
                return AVERROR(ERANGE);
            unit->pts = seen ? previous + previous_duration : first_ns;
        }
        if (seen && unit->pts < previous)
            return AVERROR_INVALIDDATA;
        seen = 1;
        previous = unit->pts;
        previous_duration = unit->duration;
    }
    return 0;
}

static void hls_fold_segment(const HlsRetained *retained, int width, int height, HlsWork *work, int trace)
{
    int64_t max_video = INT64_MIN, final_audio = -1;
    for (int video_pass = 1; video_pass >= 0; --video_pass) {
        for (size_t i = 0; i < retained->count; ++i) {
            const HlsUnit *unit = &retained->data[i];
            if (unit->video != video_pass)
                continue;
            uint64_t *digest = &work->digest;
            size_t size = (size_t)unit->packet.size;
            hls_mix(digest, unit->video ? 1 : 2);
            hls_mix(digest, (uint64_t)unit->pts);
            if (unit->video) {
                hls_mix(digest, (uint64_t)unit->dts);
                if (unit->pts > max_video)
                    max_video = unit->pts;
                hls_mix(digest, (unsigned)unit->rap);
                hls_mix(digest, (unsigned)width);
                hls_mix(digest, (unsigned)height);
                if (trace)
                    printf("v %" PRId64 " %" PRId64 " %d %d %d ", unit->pts, unit->dts, unit->rap, width, height);
            } else {
                final_audio = unit->pts;
                hls_mix(digest, (uint64_t)unit->duration);
                if (trace)
                    printf("a %" PRId64 " %" PRId64 " ", unit->pts, unit->duration);
            }
            hls_mix(digest, size);
            work->bytes += size;
            fold_bytes(digest, unit->packet.data, size);
            if (trace) {
                for (size_t n = 0; n < size; ++n)
                    printf("%02x", (unsigned)unit->packet.data[n]);
                putchar('\n');
            }
        }
    }
    hls_mix(&work->digest, (uint64_t)max_video);
    hls_mix(&work->digest, (uint64_t)final_audio);
    work->packets = retained->count;
}

static int hls_retain(HlsRetained *queue, HlsUnit unit, AVPacket *packet)
{
    if (queue->count == queue->capacity) {
        size_t capacity = queue->capacity ? queue->capacity * 2 : 32;
        if (capacity < queue->capacity)
            return AVERROR(ERANGE);
        HlsUnit *data = av_realloc_array(queue->data, capacity, sizeof(*data));
        if (!data)
            return AVERROR(ENOMEM);
        queue->data = data;
        queue->capacity = capacity;
    }
    queue->data[queue->count] = unit;
    av_packet_move_ref(&queue->data[queue->count].packet, packet);
    ++queue->count;
    return 0;
}

static int hls_unique_stream(const AVFormatContext *format, enum AVMediaType type, enum AVCodecID codec)
{
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

static int hls_operation(const uint8_t *bytes, size_t size, HlsProfile profile, HlsWork *work, int trace)
{
    HlsInput in = {bytes, size, 0};
    AVFormatContext *format = avformat_alloc_context();
    AVIOContext *io = NULL;
    AVPacket *packet = NULL;
    uint8_t *buffer = NULL;
    HlsRetained retained = {0};
    int rc = AVERROR(ENOMEM), saw_video = 0, saw_audio = 0;
    const char *stage = "allocate custom AVIO";
    if (!format)
        goto done;
    buffer = av_malloc(HLS_AVIO_BYTES);
    if (!buffer)
        goto done;
    io = avio_alloc_context(buffer, HLS_AVIO_BYTES, 0, &in, hls_read_body, NULL, NULL);
    if (!io)
        goto done;
    buffer = NULL;
    format->pb = io;
    format->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_NOFILLIN;
    stage = "probe/open TS";
    rc = avformat_open_input(&format, NULL, NULL, NULL);
    if (rc < 0)
        goto done;
    if (strcmp(format->iformat->name, "mpegts")) {
        rc = AVERROR_INVALIDDATA;
        goto done;
    }
    stage = "find stream info";
    rc = avformat_find_stream_info(format, NULL);
    if (rc < 0)
        goto done;
    stage = "select negotiated video and audio";
    int vi = hls_unique_stream(format, AVMEDIA_TYPE_VIDEO, profile.video);
    int ai = hls_unique_stream(format, AVMEDIA_TYPE_AUDIO, profile.audio);
    if (vi < 0 || ai < 0 || format->nb_streams != 2) {
        rc = AVERROR(ENOSYS);
        goto done;
    }
    int width = format->streams[vi]->codecpar->width;
    int height = format->streams[vi]->codecpar->height;
    if (width <= 0 || height <= 0) {
        rc = AVERROR_INVALIDDATA;
        goto done;
    }
    packet = av_packet_alloc();
    if (!packet) {
        rc = AVERROR(ENOMEM);
        goto done;
    }
    while ((rc = av_read_frame(format, packet)) >= 0) {
        stage = "validate parsed packet";
        int si = packet->stream_index;
        if ((si != vi && si != ai) || packet->size <= 0 || !packet->data || !packet->buf ||
            (packet->flags & AV_PKT_FLAG_CORRUPT)) {
            rc = AVERROR_INVALIDDATA;
            goto done;
        }
        HlsUnit unit = {.video = si == vi, .pts = packet->pts, .dts = packet->dts};
        HlsSpan wire = {packet->data, (size_t)packet->size};
        if (unit.video) {
            stage = "video clock and in-band random access";
            unit.rap = !!(packet->flags & AV_PKT_FLAG_KEY);
            rc = hls_video_wire(wire, profile.video, unit.rap);
            if (rc < 0 || (!saw_video && !unit.rap) || unit.pts == AV_NOPTS_VALUE || unit.dts == AV_NOPTS_VALUE) {
                rc = AVERROR_INVALIDDATA;
                goto done;
            }
            saw_video = 1;
        } else {
            stage = "selected audio frame and exact duration";
            unit.duration = hls_audio_duration_ns(wire, profile.audio);
            if (unit.duration <= 0) {
                rc = AVERROR_INVALIDDATA;
                goto done;
            }
            saw_audio = 1;
        }
        stage = "hls_retain output packet";
        rc = hls_retain(&retained, unit, packet);
        if (rc < 0)
            goto done;
        stage = "read next frame";
    }
    if (rc != AVERROR_EOF)
        goto done;
    stage = "validate clean EOF and full physical body";
    if (in.used != size || (io->error < 0 && io->error != AVERROR_EOF) ||
        !saw_video || !saw_audio) {
        rc = AVERROR_INVALIDDATA;
        goto done;
    }
    stage = "project application audio timing";
    rc = hls_project_application_audio(&retained);
    if (rc < 0)
        goto done;
    hls_fold_segment(&retained, width, height, work, trace);
    rc = 0;

done:
    for (size_t i = 0; i < retained.count; ++i)
        av_packet_unref(&retained.data[i].packet);
    av_free(retained.data);
    av_packet_free(&packet);
    avformat_close_input(&format);
    if (io) {
        av_freep(&io->buffer);
        avio_context_free(&io);
    }
    av_free(buffer);
    if (rc < 0)
        fprintf(stderr, "reference stage: %s\n", stage);
    return rc;
}

/* hls-bench FILE ITERATIONS VIDEO AUDIO | hls-fold FILE VIDEO AUDIO | hls-memory FILE VIDEO AUDIO */
static int hls_main(int argc, char **argv, const uint8_t *bytes, long size)
{
    const int bench = argc == 6 && !strcmp(argv[1], "hls-bench");
    const int memory = argc == 5 && !strcmp(argv[1], "hls-memory");
    if (!bench && !memory && !(argc == 5 && !strcmp(argv[1], "hls-fold")))
        return 2;
    unsigned long iterations = 1;
    if (bench) {
        char *end = NULL;
        iterations = strtoul(argv[3], &end, 10);
        if (*end || !iterations || iterations > 1000000)
            return 2;
    }
    const char *video = argv[argc - 2], *audio = argv[argc - 1];
    HlsProfile profile = {
        .video = !strcmp(video, "avc") ? AV_CODEC_ID_H264 : !strcmp(video, "hevc") ? AV_CODEC_ID_HEVC : AV_CODEC_ID_NONE,
        .audio = !strcmp(audio, "aac") ? AV_CODEC_ID_AAC : !strcmp(audio, "ac3") ? AV_CODEC_ID_AC3 :
                 !strcmp(audio, "eac3") ? AV_CODEC_ID_EAC3 : AV_CODEC_ID_NONE,
    };
    if (profile.video == AV_CODEC_ID_NONE || profile.audio == AV_CODEC_ID_NONE || size <= 0 ||
        size > HLS_MAX_INPUT || size % HLS_TS_BYTES)
        return 2;
    if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18)
        return 2;
    HlsWork total = {0};
    const uint64_t start = bench ? now_ns() : 0;
    for (unsigned long i = 0; i < iterations; ++i) {
        HlsWork once = {0};
        int rc = hls_operation(bytes, (size_t)size, profile, &once, 0);
        if (rc < 0) {
            char error[AV_ERROR_MAX_STRING_SIZE];
            av_strerror(rc, error, sizeof(error));
            fprintf(stderr, "reference: %s (%d)\n", error, rc);
            return 1;
        }
        total.packets += once.packets;
        total.bytes += once.bytes;
        hls_mix(&total.digest, once.digest);
    }
    const uint64_t elapsed = bench ? now_ns() - start : 0;
    if (memory && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16)
        return 2;
    printf("{\"schema\":1,\"operation\":\"" HLS_OPERATION "\",\"iterations\":%lu"
           ",\"packets\":%" PRIu64 ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64
           ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true}\n",
           iterations, total.packets, total.bytes, total.digest, elapsed);
    return 0;
}
