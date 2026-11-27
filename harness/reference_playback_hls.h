/* TS188 access units, exact AAC clocks and bounded forward input. */
#define now_ns view_now
#include "reference_hls.h"
#undef now_ns

typedef enum { HV_PACKETS, HV_CLOCKS, HV_COMPACT, HV_PUSH } HlsView;
typedef enum { HV_SEGMENT, HV_PROGRESSIVE, HV_MEDIA } HlsReader;
typedef struct {
    HlsView view;
    HlsReader reader;
    HlsProfile profile;
    size_t window;
} HlsViewCase;
typedef struct {
    HlsInput input;
    size_t window;
} HlsViewInput;

static int hls_view_read(void *opaque, uint8_t *buffer, int requested)
{
    HlsViewInput *input = opaque;
    if (requested > (int)input->window)
        requested = (int)input->window;
    return hls_read_body(&input->input, buffer, requested);
}

static void hls_view_record(ViewWork *work, uint64_t kind, const uint64_t *words,
                            size_t count, const AVPacket *packet)
{
    size_t size = packet ? (size_t)packet->size : 0;
    view_record(work, kind, words, count, packet ? packet->data : NULL, size);
    if (!work->trace)
        return;
    printf("%" PRIu64, kind);
    for (size_t i = 0; i < count; ++i)
        printf(" %" PRIu64, words[i]);
    if (size)
        putchar(' ');
    for (size_t i = 0; i < size; ++i)
        printf("%02x", packet->data[i]);
    putchar('\n');
}

#define HLS_VIEW_ROW(work, kind, packet, ...) do { \
    const uint64_t hls_view_words[] = { __VA_ARGS__ }; \
    hls_view_record(work, kind, hls_view_words, \
                    sizeof(hls_view_words) / sizeof(*hls_view_words), packet); \
} while (0)

static uint64_t hls_view_gcd(uint64_t a, uint64_t b)
{
    while (b) {
        uint64_t rem = a % b;
        a = b;
        b = rem;
    }
    return a;
}

static void hls_view_ratio(int64_t numerator, uint64_t denominator, uint64_t *n, uint64_t *d)
{
    uint64_t magnitude = numerator < 0 ? 0 - (uint64_t)numerator : (uint64_t)numerator;
    uint64_t divisor = hls_view_gcd(magnitude, denominator);
    *n = (uint64_t)(numerator / (int64_t)divisor);
    *d = denominator / divisor;
}

static int hls_view_aac_rate(const AVPacket *packet)
{
    static const int rates[] = {
        96000, 88200, 64000, 48000, 44100, 32000, 24000,
        22050, 16000, 12000, 11025, 8000, 7350,
    };
    if (hls_audio_duration_ns((HlsSpan){packet->data, (size_t)packet->size},
                              AV_CODEC_ID_AAC) < 0)
        return 0;
    return rates[(packet->data[2] >> 2) & 15];
}

/* Hold exact timestamps in a common clock for the stream's AAC rate. The wire
 * timestamps use 90 kHz; frame lengths are 1024 samples at that rate. */
static int64_t hls_view_grid_delta(int64_t pts, int rate, int direction)
{
    AVRational clock = {1, 90000}, frame = {1024, rate};
    int64_t index = av_rescale_q(pts, clock, frame);
    return av_rescale_q(index + direction, frame, clock) - av_rescale_q(index, frame, clock);
}

static int hls_view_audio(HlsRetained *retained, HlsViewCase options, int pid,
                          ViewWork *work, int64_t *last90)
{
    if (options.view == HV_PUSH && options.reader == HV_MEDIA) {
        int rc = hls_project_application_audio(retained);
        if (rc < 0)
            return rc;
        for (size_t i = 0; i < retained->count; ++i) {
            const HlsUnit *unit = &retained->data[i];
            if (!unit->video) {
                HLS_VIEW_ROW(work, 1, &unit->packet, 1, (uint64_t)unit->pts,
                             (uint64_t)unit->duration, (uint64_t)unit->packet.size);
                *last90 = unit->pts;  /* Media reader tail is in nanoseconds. */
            }
        }
        return 0;
    }
    int rate = 0;
    size_t leading = 0;
    int64_t first = AV_NOPTS_VALUE;
    for (size_t i = 0; i < retained->count; ++i) {
        const HlsUnit *unit = &retained->data[i];
        if (unit->video)
            continue;
        int frame_rate = hls_view_aac_rate(&unit->packet);
        if (!frame_rate || (rate && rate != frame_rate))
            return AVERROR_INVALIDDATA;
        rate = frame_rate;
        if (first == AV_NOPTS_VALUE) {
            if (unit->pts == AV_NOPTS_VALUE)
                ++leading;
            else
                first = unit->pts;
        }
    }
    if (!rate)
        return AVERROR_INVALIDDATA;

    uint64_t duration_n, duration_d;
    hls_view_ratio(1024, (uint64_t)rate, &duration_n, &duration_d);
    int64_t ticks = 90000 * (int64_t)(duration_d / hls_view_gcd(90000, duration_d));
    int64_t step = (int64_t)duration_n * (ticks / (int64_t)duration_d);
    int64_t factor = ticks / 90000;
    int64_t next = 0, next90 = 0;
    if (first != AV_NOPTS_VALUE) {
        next = first * factor - (int64_t)leading * step;
        next90 = first;
        for (size_t i = 0; i < leading; ++i)
            next90 += hls_view_grid_delta(next90, rate, -1);
    }
    int64_t previous = INT64_MIN;
    unsigned missing_origin = first == AV_NOPTS_VALUE ? 3 : 1;
    for (size_t i = 0; i < retained->count; ++i) {
        const HlsUnit *unit = &retained->data[i];
        if (unit->video)
            continue;
        const AVPacket *packet = &unit->packet;
        if (options.view == HV_COMPACT) {
            HLS_VIEW_ROW(work, 1, packet, 1, duration_n, duration_d, (uint64_t)packet->size);
            continue;
        }
        int anchored = unit->pts != AV_NOPTS_VALUE;
        int64_t pts = anchored ? unit->pts * factor : next;
        int64_t pts90 = anchored ? unit->pts : next90;
        int64_t dts = unit->dts == AV_NOPTS_VALUE ? pts : unit->dts * factor;
        unsigned origin = anchored ? 0 : missing_origin;
        if (pts < previous || (!anchored && previous == INT64_MIN &&
            options.reader == HV_PROGRESSIVE && options.view == HV_PACKETS))
            return AVERROR_INVALIDDATA;
        previous = pts;
        if (anchored)
            missing_origin = 2;
        if (options.view == HV_PACKETS) {
            /* The progressive corpus carries PTS-only audio PES. The parser
             * can mirror the PTS into its DTS without a wire DTS present. */
            int present = options.reader != HV_PROGRESSIVE ||
                          (unit->dts != AV_NOPTS_VALUE && unit->dts != unit->pts);
            HLS_VIEW_ROW(work, 1, packet,
                         1, (uint64_t)pid, 1, (uint64_t)ticks, 1, (uint64_t)pts,
                         (uint64_t)present, present ? (uint64_t)dts : 0, 1, (uint64_t)step,
                         0, (uint64_t)packet->size);
        } else {
            uint64_t pn, pd, dn, dd;
            hls_view_ratio(pts, (uint64_t)ticks, &pn, &pd);
            hls_view_ratio(dts, (uint64_t)ticks, &dn, &dd);
            HLS_VIEW_ROW(work, 3, packet, pn, pd, dn, dd, origin, (uint64_t)pts90,
                         (uint64_t)av_rescale_rnd(1024, 90000, rate, AV_ROUND_DOWN),
                         (uint64_t)packet->size);
        }
        next = pts + step;
        next90 = pts90 + hls_view_grid_delta(pts90, rate, 1);
        *last90 = pts90;
    }
    return 0;
}

static int hls_view_rows(HlsRetained *retained, HlsViewCase options,
                         const AVStream *video, const AVStream *audio, ViewWork *work)
{
    int64_t max_video = INT64_MIN, last_audio = AV_NOPTS_VALUE;
    if (options.view != HV_CLOCKS) {
        for (size_t i = 0; i < retained->count; ++i) {
            const HlsUnit *unit = &retained->data[i];
            if (!unit->video)
                continue;
            const AVPacket *packet = &unit->packet;
            int pts = unit->pts != AV_NOPTS_VALUE, dts = unit->dts != AV_NOPTS_VALUE;
            uint64_t pt = pts ? (uint64_t)unit->pts : 0, dt = dts ? (uint64_t)unit->dts : 0;
            if (options.view == HV_PACKETS)
                HLS_VIEW_ROW(work, 1, packet,
                             0, (uint64_t)video->id, 1, 90000, (uint64_t)pts, pt,
                             (uint64_t)dts, dt, 0, 0, (uint64_t)unit->rap,
                             (uint64_t)packet->size);
            else
                HLS_VIEW_ROW(work, 1, packet,
                             0, (uint64_t)pts, pt, (uint64_t)dts, dt, (uint64_t)unit->rap,
                             (uint64_t)video->codecpar->width, (uint64_t)video->codecpar->height,
                             (uint64_t)packet->size);
            if (unit->pts > max_video)
                max_video = unit->pts;
        }
    }
    int rc = hls_view_audio(retained, options, audio->id, work, &last_audio);
    if (rc < 0)
        return rc;
    if (options.view == HV_PUSH)
        HLS_VIEW_ROW(work, 3, NULL, (uint64_t)max_video, last_audio != AV_NOPTS_VALUE,
                     last_audio == AV_NOPTS_VALUE ? 0 : (uint64_t)last_audio);
    hls_view_record(work, 7, NULL, 0, NULL);
    return 0;
}

static int hls_view_run(const ViewCase *input, ViewWork *work)
{
    const HlsViewCase options = *(const HlsViewCase *)input->prepared;
    HlsViewInput source = {{input->data, input->size, 0}, options.window};
    AVFormatContext *format = NULL;
    AVIOContext *io = NULL;
    AVPacket *packet = NULL;
    uint8_t *buffer = NULL;
    HlsRetained retained = {0};
    int rc = AVERROR_INVALIDDATA, saw_video = 0, saw_audio = 0;
    if (!input->size || input->size > HLS_MAX_INPUT || input->size % HLS_TS_BYTES)
        goto finish;
    rc = AVERROR(ENOMEM);
    format = avformat_alloc_context();
    buffer = av_malloc(4096);
    if (!format || !buffer)
        goto finish;
    io = avio_alloc_context(buffer, 4096, 0, &source, hls_view_read, NULL, NULL);
    if (!io)
        goto finish;
    buffer = NULL;
    format->pb = io;
    format->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_NOFILLIN;
    if (options.reader == HV_PROGRESSIVE) {
        format->probesize = 8192;
        format->max_analyze_duration = 250000;
    }
    rc = avformat_open_input(&format, NULL, av_find_input_format("mpegts"), NULL);
    if (rc < 0)
        goto finish;
    rc = avformat_find_stream_info(format, NULL);
    if (rc < 0)
        goto finish;
    int vi = hls_unique_stream(format, AVMEDIA_TYPE_VIDEO, options.profile.video);
    int ai = hls_unique_stream(format, AVMEDIA_TYPE_AUDIO, options.profile.audio);
    if (vi < 0 || ai < 0 || format->nb_streams != 2) {
        rc = AVERROR_INVALIDDATA;
        goto finish;
    }
    packet = av_packet_alloc();
    if (!packet) {
        rc = AVERROR(ENOMEM);
        goto finish;
    }
    while ((rc = av_read_frame(format, packet)) >= 0) {
        int video = packet->stream_index == vi;
        if ((!video && packet->stream_index != ai) || packet->size <= 0 ||
            !packet->data || !packet->buf || (packet->flags & AV_PKT_FLAG_CORRUPT)) {
            rc = AVERROR_INVALIDDATA;
            goto finish;
        }
        HlsUnit unit = {.video = video, .rap = !!(packet->flags & AV_PKT_FLAG_KEY),
                        .pts = packet->pts, .dts = packet->dts};
        if (video)
            saw_video = 1;
        else {
            saw_audio = 1;
            unit.duration = hls_audio_duration_ns(
                (HlsSpan){packet->data, (size_t)packet->size}, options.profile.audio);
            if (unit.duration <= 0) {
                rc = AVERROR_INVALIDDATA;
                goto finish;
            }
        }
        rc = hls_retain(&retained, unit, packet);
        if (rc < 0)
            goto finish;
    }
    if (rc != AVERROR_EOF)
        goto finish;
    if (!saw_audio || !saw_video || source.input.used != input->size ||
        (io->error < 0 && io->error != AVERROR_EOF)) {
        rc = AVERROR_INVALIDDATA;
        goto finish;
    }

    rc = hls_view_rows(&retained, options, format->streams[vi], format->streams[ai], work);

finish:
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
    if (rc == AVERROR_INVALIDDATA || rc == AVERROR_EOF) {
        *work = (ViewWork){.trace = work->trace};
        hls_view_record(work, 8, NULL, 0, NULL);
        return 0;
    }
    if (rc < 0) {
        fprintf(stderr, "HLS reference error %d\n", rc);
        return 1;
    }
    return 0;
}

static int playback_hls_prepare(ViewCase *input)
{
    /* The existing entry stays in the shared header with the demux helpers. */
    (void)hls_main;
    if (input->argc != 5)
        return 2;
    const char *view = input->argv[0], *reader = input->argv[1], *video = input->argv[2];
    const char *audio = input->argv[3], *window = input->argv[4];
    HlsViewCase options = {0};
    if (!strcmp(view, "packets")) {
        input->name = "hls-unit-packets";
        options.view = HV_PACKETS;
    } else if (!strcmp(view, "clocks")) {
        input->name = "hls-aac-clocks";
        options.view = HV_CLOCKS;
    } else if (!strcmp(view, "compact")) {
        input->name = "hls-compact-push";
        options.view = HV_COMPACT;
    } else if (!strcmp(view, "push")) {
        input->name = "hls-push-input";
        options.view = HV_PUSH;
    } else {
        return 2;
    }
    if (!strcmp(reader, "segment"))
        options.reader = HV_SEGMENT;
    else if (!strcmp(reader, "progressive"))
        options.reader = HV_PROGRESSIVE;
    else if (!strcmp(reader, "media"))
        options.reader = HV_MEDIA;
    else
        return 2;
    options.profile.video = !strcmp(video, "avc") ? AV_CODEC_ID_H264 :
                            !strcmp(video, "hevc") ? AV_CODEC_ID_HEVC : AV_CODEC_ID_NONE;
    options.profile.audio = !strcmp(audio, "aac") ? AV_CODEC_ID_AAC :
                            !strcmp(audio, "ac3") ? AV_CODEC_ID_AC3 :
                            !strcmp(audio, "eac3") ? AV_CODEC_ID_EAC3 : AV_CODEC_ID_NONE;
    options.window = !strcmp(window, "1") ? 1 : !strcmp(window, "188") ? 188 :
                     !strcmp(window, "4096") ? 4096 : 0;
    if (options.profile.video == AV_CODEC_ID_NONE || options.profile.audio == AV_CODEC_ID_NONE ||
        !options.window || (options.reader != HV_MEDIA &&
            (options.profile.video != AV_CODEC_ID_H264 || options.profile.audio != AV_CODEC_ID_AAC)) ||
        (options.reader == HV_MEDIA && (options.view == HV_PACKETS || options.view == HV_COMPACT)) ||
        (options.view == HV_CLOCKS && options.profile.audio != AV_CODEC_ID_AAC) ||
        (options.view == HV_PUSH && options.reader == HV_PROGRESSIVE))
        return 2;
    HlsViewCase *prepared = malloc(sizeof(*prepared));
    if (!prepared)
        return 1;
    *prepared = options;
    input->prepared = prepared;
    input->release = free;
    input->run = hls_view_run;
    return 0;
}
#undef HLS_VIEW_ROW
