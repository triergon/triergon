/* Public demux outcomes for the player entry points. */
#include <errno.h>
#define now_ns view_now
#include "reference_packet_fold.h"
#undef now_ns
/* These headers also define the command adapters of the standalone references. */
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wunused-function"
#include "reference_mp4_forward.h"
#include "reference_ts_program.h"
#pragma GCC diagnostic pop
#include "reference_mp4_roll.h"

typedef enum { PLAYER_MOV, PLAYER_MKV, PLAYER_TS } PlayerFormat;
typedef enum { PLAYER_SEEK, PLAYER_CATALOGUE, PLAYER_DELIVERY, PLAYER_SELECT,
               PLAYER_INDEX, PLAYER_PES_OPEN } PlayerOperation;
typedef enum { PLAYER_OPEN, PLAYER_SYNC, PLAYER_DEMUX, PLAYER_MOVIE,
               PLAYER_DISCARD, PLAYER_OPUS_ROLL } PlayerSeek;

typedef struct {
    PlayerOperation operation;
    PlayerFormat format;
    PlayerSeek seek;
    int seekable, details, window;
    int64_t video, timestamp, count;
    int64_t selection[3], replacement[3];
    int switching, capsule;
    uint8_t *other;
    size_t other_size;
    const char *pes_selection, *mov_layout;
    char *media_selection;
} PlaybackPlayerCase;

static void player_release(void *opaque)
{
    PlaybackPlayerCase *player = opaque;
    free(player->other);
    av_free(player->media_selection);
    free(player);
}

static void player_field(PacketWork *work, uint64_t kind, uint64_t value)
{
    fold(&work->digest, kind);
    fold(&work->digest, value);
    ++work->packets;
    if (work->trace)
        printf("%" PRIu64 " %" PRIu64 "\n", kind, value);
}

static uint64_t player_track(PlayerFormat format, const AVStream *stream)
{
    if (format == PLAYER_MKV && stream->id <= 0)
        return (uint64_t)stream->index + 1;
    return (uint64_t)stream->id;
}

static int player_packet(PacketWork *work, const PlaybackPlayerCase *player,
                         const AVStream *stream, const AVPacket *packet)
{
    AVRational time = stream->time_base;
    int64_t pts = packet->pts, dts = packet->dts, duration = packet->duration;
    uint64_t flags = (uint64_t)packet->flags;
    if (player->format == PLAYER_MKV) {
        const AVRational ns = {1, 1000000000};
        pts = pts == AV_NOPTS_VALUE ? INT64_MIN : av_rescale_q(pts, time, ns);
        dts = dts == AV_NOPTS_VALUE ? INT64_MIN : av_rescale_q(dts, time, ns);
        duration = av_rescale_q(duration, time, ns);
        flags &= AV_PKT_FLAG_KEY;
    }
    packet_fold(work, player_track(player->format, stream), pts, dts,
                duration > 0 ? (uint64_t)duration : 0, flags, packet->data,
                (size_t)packet->size);
    if (player->details) {
        int num, den;
        if (!av_reduce(&num, &den, time.num, time.den, INT_MAX))
            return AVERROR_INVALIDDATA;
        fold(&work->digest, (uint64_t)packet->pos);
        fold(&work->digest, (uint64_t)num);
        fold(&work->digest, (uint64_t)den);
        size_t size = 0;
        const uint8_t *skip = av_packet_get_side_data(packet, AV_PKT_DATA_SKIP_SAMPLES, &size);
        if (skip && size != 10)
            return AVERROR_INVALIDDATA;
        fold(&work->digest, skip != NULL);
        if (skip)
            fold_bytes(&work->digest, skip, size);
        if (work->trace) {
            printf("source %" PRId64 " %d/%d ", packet->pos, num, den);
            if (skip)
                printf("%" PRIu32 " %" PRIu32 " %u %u\n",
                       AV_RL32(skip), AV_RL32(skip + 4), (unsigned)skip[8], (unsigned)skip[9]);
            else
                puts("-");
        }
    }
    return 0;
}

static int player_open(AVFormatContext **format, PacketInput *reader,
                       const PlaybackPlayerCase *player)
{
    const char *name = player->format == PLAYER_MOV ? "mov"
        : player->format == PLAYER_MKV ? "matroska" : "mpegts";
    int flags = player->format == PLAYER_MKV ? 0 : AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN;
    const AVInputFormat *demux = av_find_input_format(name);
    if (!demux)
        return AVERROR_DEMUXER_NOT_FOUND;
    return packet_open(format, reader, player->seekable, demux, flags);
}

static int player_stream(AVFormatContext *format, PlayerFormat container,
                         enum AVMediaType type, int64_t id)
{
    for (unsigned i = 0; i < format->nb_streams; ++i) {
        const AVStream *stream = format->streams[i];
        if (stream->codecpar->codec_type == type &&
            (id < 0 || player_track(container, stream) == (uint64_t)id))
            return (int)i;
    }
    return AVERROR_STREAM_NOT_FOUND;
}

static void player_selection(AVFormatContext *format, PlayerFormat container,
                             const int64_t *selection)
{
    for (unsigned i = 0; i < format->nb_streams; ++i) {
        AVStream *stream = format->streams[i];
        int64_t id = (int64_t)player_track(container, stream);
        enum AVMediaType type = stream->codecpar->codec_type;
        int active = (type == AVMEDIA_TYPE_VIDEO && selection[0] == id)
            || (type == AVMEDIA_TYPE_AUDIO && selection[1] == id)
            || (type == AVMEDIA_TYPE_SUBTITLE && selection[2] == id);
        stream->discard = active ? AVDISCARD_DEFAULT : AVDISCARD_ALL;
    }
}

static int player_read(AVFormatContext *format, AVPacket *packet,
                       const PlaybackPlayerCase *player, PacketWork *work,
                       int64_t count, int only, int landing, int project_index)
{
    int rc = 0;
    while (count != 0 && (rc = av_read_frame(format, packet)) >= 0) {
        int index = packet->stream_index;
        if (landing == index) {
            player_field(work, 4, (uint64_t)packet->pos);
            landing = -1;
        }
        if (index == only || only == -1) {
            if (project_index && format->streams[index]->discard == AVDISCARD_ALL) {
                av_packet_unref(packet);
                continue;
            }
            if (work) {
                rc = player_packet(work, player, format->streams[index], packet);
                if (rc < 0)
                    return rc;
            }
            if (count > 0)
                --count;
        }
        av_packet_unref(packet);
    }
    return rc == AVERROR_EOF && count <= 0 ? 0 : rc;
}

/* Range playback uses a usable mvhd duration or the latest media packet end
 * on the presentation clock. The demuxer's estimated duration remains visible. */
static int player_range_duration(const ViewCase *input, const PlaybackPlayerCase *player,
                                int64_t *duration)
{
    Mp4View movie = mp4_box((Mp4View){input->data, input->size}, MKBETAG('m','o','o','v'));
    Mp4View header = mp4_box(movie, MKBETAG('m','v','h','d'));
    if (header.size < 20 || header.data[0] > 1 || (header.data[0] == 1 && header.size < 32))
        return AVERROR_INVALIDDATA;
    const int wide = header.data[0] == 1;
    uint32_t scale = AV_RB32(header.data + (wide ? 20 : 12));
    uint64_t ticks = wide ? AV_RB64(header.data + 24) : AV_RB32(header.data + 16);
    if (!scale)
        return AVERROR_INVALIDDATA;
    if (ticks && ticks != (wide ? UINT64_MAX : UINT32_MAX)) {
        uint64_t seconds = ticks / scale;
        int64_t fraction = av_rescale_rnd((int64_t)(ticks % scale), AV_TIME_BASE,
                                         scale, AV_ROUND_NEAR_INF);
        if ((seconds || fraction) && seconds <= (uint64_t)((INT64_MAX - fraction) / AV_TIME_BASE)) {
            *duration = (int64_t)seconds * AV_TIME_BASE + fraction;
            return 0;
        }
    }

    PacketInput reader = {input->data, (int64_t)input->size, 0, player->window};
    AVFormatContext *format = NULL;
    AVPacket *packet = av_packet_alloc();
    if (!packet)
        return AVERROR(ENOMEM);
    *duration = 0;
    int rc = player_open(&format, &reader, player);
    if (rc < 0)
        goto done;
    while ((rc = av_read_frame(format, packet)) >= 0) {
        const AVStream *stream = format->streams[packet->stream_index];
        if (stream->codecpar->codec_type != AVMEDIA_TYPE_DATA && packet->pts != AV_NOPTS_VALUE) {
            if (packet->duration < 0 || packet->pts > INT64_MAX - packet->duration) {
                rc = AVERROR_INVALIDDATA;
                goto done;
            }
            int64_t end = av_rescale_q(packet->pts + packet->duration, stream->time_base,
                                       AV_TIME_BASE_Q);
            if (end > *duration)
                *duration = end;
        }
        av_packet_unref(packet);
    }
    if (rc == AVERROR_EOF)
        rc = 0;
done:
    av_packet_free(&packet);
    packet_close(&format);
    return rc;
}

/* ISO BMFF edit durations are on the movie clock. An empty interval has
 * no packet landing, even if the demuxer can seek past it. */
static int player_movie_empty(Mp4View input, uint32_t track_id, int64_t timestamp)
{
    Mp4View movie = mp4_box(input, MKBETAG('m','o','o','v'));
    Mp4View mvhd = mp4_box(movie, MKBETAG('m','v','h','d'));
    uint32_t kind;
    Mp4View track;
    while (mp4_box_next(&movie, &kind, &track)) {
        if (kind != MKBETAG('t','r','a','k'))
            continue;
        Mp4View tkhd = mp4_box(track, MKBETAG('t','k','h','d'));
        size_t offset = tkhd.size && tkhd.data[0] == 1 ? 20 : 12;
        if (tkhd.size < offset + 4 || AV_RB32(tkhd.data + offset) != track_id)
            continue;
        Mp4View edits = mp4_box(mp4_box(track, MKBETAG('e','d','t','s')), MKBETAG('e','l','s','t'));
        if (!edits.data)
            return 0;
        Mp4View mdhd = mp4_box(mp4_box(track, MKBETAG('m','d','i','a')), MKBETAG('m','d','h','d'));
        const size_t movie_scale_at = mvhd.size && mvhd.data[0] == 1 ? 20 : 12;
        const size_t media_scale_at = mdhd.size && mdhd.data[0] == 1 ? 20 : 12;
        if (mvhd.size < movie_scale_at + 4 || mdhd.size < media_scale_at + 4 ||
            mvhd.data[0] > 1 || mdhd.data[0] > 1 || edits.size < 8 || edits.data[0] > 1)
            return AVERROR_INVALIDDATA;
        uint32_t movie_scale = AV_RB32(mvhd.data + movie_scale_at);
        uint32_t media_scale = AV_RB32(mdhd.data + media_scale_at);
        if (!movie_scale || !media_scale)
            return AVERROR_INVALIDDATA;
        int64_t target = av_rescale_rnd(timestamp, movie_scale, media_scale, AV_ROUND_DOWN);
        const int wide = edits.data[0] == 1;
        const unsigned size = wide ? 20 : 12;
        const uint32_t count = AV_RB32(edits.data + 4);
        if (count > (edits.size - 8) / size)
            return AVERROR_INVALIDDATA;
        if (target < 0)
            return 0;
        uint64_t remaining = (uint64_t)target;
        const uint8_t *row = edits.data + 8;
        for (uint32_t n = 0; n < count; ++n, row += size) {
            uint64_t length = wide ? AV_RB64(row) : AV_RB32(row);
            if (remaining < length)
                return wide ? AV_RB64(row + 8) == UINT64_MAX : AV_RB32(row + 4) == UINT32_MAX;
            remaining -= length;
        }
        return 0;
    }
    return AVERROR_STREAM_NOT_FOUND;
}

/* Use the backward seek to identify the edited video occurrence, then replay
 * it in a fresh demux session. The seek's per-stream indices can skip audio
 * that is delivered after this occurrence in the selected movie replay. */
static int player_movie_read(AVFormatContext *format, AVPacket *landing,
                             const ViewCase *input, const PlaybackPlayerCase *player,
                             int video, PacketWork *work)
{
    int rc;
    while ((rc = av_read_frame(format, landing)) >= 0 && landing->stream_index != video)
        av_packet_unref(landing);
    if (rc < 0)
        return rc;
    PacketInput reader = {input->data, (int64_t)input->size, 0, player->window};
    AVFormatContext *replay = NULL;
    AVPacket *packet = av_packet_alloc();
    if (!packet)
        return AVERROR(ENOMEM);
    rc = player_open(&replay, &reader, player);
    if (rc < 0)
        goto done;
    const int track = player_stream(replay, PLAYER_MOV, AVMEDIA_TYPE_VIDEO, player->video);
    if (track < 0) {
        rc = track;
        goto done;
    }
    while ((rc = av_read_frame(replay, packet)) >= 0) {
        if (packet->stream_index == track && packet->pos == landing->pos &&
            packet->pts == landing->pts && packet->dts == landing->dts &&
            packet->duration == landing->duration && packet->flags == landing->flags &&
            packet->size == landing->size &&
            (!packet->size || !memcmp(packet->data, landing->data, (size_t)packet->size))) {
            player_field(work, 4, (uint64_t)packet->pos);
            rc = player_packet(work, player, replay->streams[track], packet);
            av_packet_unref(packet);
            if (rc >= 0)
                rc = player_read(replay, packet, player, work, -1, -1, -1, 1);
            goto done;
        }
        av_packet_unref(packet);
    }
    if (rc == AVERROR_EOF)
        rc = AVERROR_INVALIDDATA;
done:
    av_packet_free(&packet);
    packet_close(&replay);
    return rc;
}

typedef struct {
    int64_t position, pts, dts, duration;
    size_t order;
    uint32_t description;
    int stream, size, key, disposable;
} PlayerSourceSample;

typedef struct {
    PlayerSourceSample *rows;
    size_t count, used;
} PlayerSourceTrack;

static int player_source_track_order(const void *a, const void *b)
{
    const PlayerSourceSample *left = a, *right = b;
    if (left->stream != right->stream)
        return left->stream < right->stream ? -1 : 1;
    return (left->order > right->order) - (left->order < right->order);
}

static int player_source_file_order(const void *a, const void *b)
{
    const PlayerSourceSample *left = a, *right = b;
    if (left->position != right->position)
        return left->position < right->position ? -1 : 1;
    return (left->order > right->order) - (left->order < right->order);
}

static int player_source_descriptions(Mp4View table, PlayerSourceTrack *track)
{
    if (!track->used)
        return 0;
    Mp4View map = mp4_box(table, MKBETAG('s','t','s','c'));
    Mp4View chunks = mp4_box(table, MKBETAG('s','t','c','o'));
    if (!chunks.data)
        chunks = mp4_box(table, MKBETAG('c','o','6','4'));
    if (map.size < 8 || chunks.size < 8)
        return AVERROR_INVALIDDATA;
    const uint64_t last_chunk = (uint64_t)AV_RB32(chunks.data + 4) + 1;
    const uint32_t runs = AV_RB32(map.data + 4);
    if (runs > (map.size - 8) / 12)
        return AVERROR_INVALIDDATA;
    size_t written = 0;
    uint64_t first = 1;
    for (uint32_t i = 0; i < runs; ++i) {
        const uint8_t *row = map.data + 8 + (size_t)i * 12;
        uint64_t next = i + 1 < runs ? AV_RB32(row + 12) : last_chunk;
        uint32_t samples = AV_RB32(row + 4), description = AV_RB32(row + 8);
        if (AV_RB32(row) != first || next <= first || next > last_chunk ||
            !samples || !description || next - first > (track->used - written) / samples)
            return AVERROR_INVALIDDATA;
        size_t end = written + (size_t)(next - first) * samples;
        while (written < end)
            track->rows[written++].description = description;
        first = next;
    }
    return written == track->used ? 0 : AVERROR_INVALIDDATA;
}

/* Sample deltas and sync signalling are table facts. For example, demux can
 * present all hint or text packets as keys and infer a duration for a coded
 * zero delta. The source seek uses the declared media values. */
static int player_source_sample_table(Mp4View table, PlayerSourceTrack *track)
{
    Mp4View timing = mp4_box(table, MKBETAG('s','t','t','s'));
    Mp4View sync = mp4_box(table, MKBETAG('s','t','s','s'));
    uint32_t keys = sync.data && sync.size >= 8 ? AV_RB32(sync.data + 4) : 0;
    if ((sync.data && sync.size < 8) || (keys && keys > (sync.size - 8) / 4))
        return AVERROR_INVALIDDATA;
    if (!timing.data)
        return 0;
    if (timing.size < 8)
        return AVERROR_INVALIDDATA;
    uint32_t runs = AV_RB32(timing.data + 4);
    if (runs > (timing.size - 8) / 8)
        return AVERROR_INVALIDDATA;
    uint32_t key = 0;
    for (uint32_t i = 0; i < runs; ++i) {
        const uint8_t *entry = timing.data + 8 + (size_t)i * 8;
        const uint32_t count = AV_RB32(entry), delta = AV_RB32(entry + 4);
        if (count > track->count - track->used)
            return AVERROR_INVALIDDATA;
        for (uint32_t sample = 0; sample < count; ++sample) {
            PlayerSourceSample *row = &track->rows[track->used++];
            row->duration = delta;
            row->key = !sync.data ||
                       (key < keys && AV_RB32(sync.data + 8 + (size_t)key * 4) == track->used);
            if (sync.data && row->key)
                ++key;
        }
    }
    if (key != keys)
        return AVERROR_INVALIDDATA;
    return player_source_descriptions(table, track);
}

static int player_source_fragment(Mp4View root, Mp4View traf, uint32_t id,
                                  PlayerSourceTrack *track)
{
    Mp4View tfhd = mp4_box(traf, MKBETAG('t','f','h','d'));
    if (tfhd.size < 8 || AV_RB32(tfhd.data + 4) != id)
        return 0;
    Mp4View defaults = mp4_box(mp4_box(root, MKBETAG('m','o','o','v')), MKBETAG('m','v','e','x'));
    uint32_t kind, duration = 0, flags = 0, description = 0;
    Mp4View body;
    while (mp4_box_next(&defaults, &kind, &body)) {
        if (kind == MKBETAG('t','r','e','x') && body.size >= 24 && AV_RB32(body.data + 4) == id) {
            description = AV_RB32(body.data + 8);
            duration = AV_RB32(body.data + 12);
            flags = AV_RB32(body.data + 20);
            break;
        }
    }
    const uint32_t fields = AV_RB24(tfhd.data + 1);
    size_t at = fields & 1 ? 16 : 8;
    size_t end = at + (fields & 2 ? 4 : 0) + (fields & 8 ? 4 : 0) +
                      (fields & 16 ? 4 : 0) + (fields & 32 ? 4 : 0);
    if (end > tfhd.size)
        return AVERROR_INVALIDDATA;
    if (fields & 2) {
        description = AV_RB32(tfhd.data + at);
        at += 4;
    }
    if (fields & 8) {
        duration = AV_RB32(tfhd.data + at);
        at += 4;
    }
    if (fields & 16)
        at += 4;
    if (fields & 32)
        flags = AV_RB32(tfhd.data + at);
    while (mp4_box_next(&traf, &kind, &body)) {
        if (kind != MKBETAG('t','r','u','n'))
            continue;
        if (body.size < 8)
            return AVERROR_INVALIDDATA;
        const uint32_t layout = AV_RB24(body.data + 1), count = AV_RB32(body.data + 4);
        at = layout & 1 ? 12 : 8;
        uint32_t first = flags;
        if (layout & 4) {
            if (at + 4 > body.size)
                return AVERROR_INVALIDDATA;
            first = AV_RB32(body.data + at);
            at += 4;
        }
        const unsigned stride = 4 * (!!(layout & 0x100) + !!(layout & 0x200) +
                                     !!(layout & 0x400) + !!(layout & 0x800));
        if (at > body.size || count > track->count - track->used ||
            (stride && count > (body.size - at) / stride))
            return AVERROR_INVALIDDATA;
        for (uint32_t i = 0; i < count; ++i, at += stride) {
            PlayerSourceSample *row = &track->rows[track->used++];
            unsigned offset = (layout & 0x100 ? 4 : 0) + (layout & 0x200 ? 4 : 0);
            uint32_t coded = (layout & 0x400) ? AV_RB32(body.data + at + offset)
                                             : i == 0 ? first : flags;
            row->description = description;
            row->duration = layout & 0x100 ? AV_RB32(body.data + at) : duration;
            row->key = !(coded & 0x00010000);
        }
    }
    return 0;
}

static int player_source_track(Mp4View input, uint32_t id, PlayerSourceTrack *track)
{
    int result = player_source_sample_table(mp4_sample_table(input, id), track);
    if (result < 0)
        return result;
    Mp4View roots = input, root;
    uint32_t kind;
    while (track->used < track->count && mp4_box_next(&roots, &kind, &root)) {
        if (kind != MKBETAG('m','o','o','f'))
            continue;
        Mp4View traf;
        while (mp4_box_next(&root, &kind, &traf)) {
            if (kind != MKBETAG('t','r','a','f'))
                continue;
            result = player_source_fragment(input, traf, id, track);
            if (result < 0)
                return result;
        }
    }
    return track->used == track->count ? 0 : AVERROR_INVALIDDATA;
}

/* The source seek selects in composition time, carries the preceding sync
 * of that sample's description, and replays in physical sample order. */
static int player_source_seek(const ViewCase *input, const PlaybackPlayerCase *player,
                              PacketWork *work)
{
    const size_t maximum = 2000000;
    PacketInput reader = {input->data, (int64_t)input->size, 0, player->window};
    AVFormatContext *format = NULL;
    AVPacket *packet = av_packet_alloc();
    PlayerSourceSample *rows = NULL;
    size_t count = 0, capacity = 0;
    if (!packet)
        return AVERROR(ENOMEM);
    int rc = source_view_open_mp4(&format, &reader, player->seekable);
    if (rc < 0)
        goto done;
    int video = player_stream(format, PLAYER_MOV, AVMEDIA_TYPE_VIDEO, player->video);
    if (video < 0) {
        rc = video;
        goto done;
    }
    while ((rc = av_read_frame(format, packet)) >= 0) {
        if (count == maximum || packet->pos < 0 || packet->pos > reader.size ||
            packet->size < 0 || packet->size > reader.size - packet->pos ||
            (packet->size && memcmp(packet->data, input->data + packet->pos, (size_t)packet->size))) {
            rc = AVERROR_INVALIDDATA;
            goto done;
        }
        if (count == capacity) {
            capacity = capacity ? FFMIN(capacity * 2, maximum) : 256;
            PlayerSourceSample *expanded = av_realloc_array(rows, capacity, sizeof(*rows));
            if (!expanded) {
                rc = AVERROR(ENOMEM);
                goto done;
            }
            rows = expanded;
        }
        rows[count] = (PlayerSourceSample){
            .position = packet->pos, .pts = packet->pts, .dts = packet->dts,
            .order = count, .stream = packet->stream_index, .size = packet->size,
            .disposable = !!(packet->flags & AV_PKT_FLAG_DISPOSABLE),
        };
        ++count;
        av_packet_unref(packet);
    }
    if (rc != AVERROR_EOF)
        goto done;
    if (!count) {
        rc = AVERROR_INVALIDDATA;
        goto done;
    }
    qsort(rows, count, sizeof(*rows), player_source_track_order);
    for (size_t at = 0; at < count;) {
        size_t end = at + 1;
        int stream = rows[at].stream;
        while (end < count && rows[end].stream == stream)
            ++end;
        PlayerSourceTrack track = {rows + at, end - at, 0};
        rc = player_source_track((Mp4View){input->data, input->size},
                                 (uint32_t)format->streams[stream]->id, &track);
        if (rc < 0)
            goto done;
        at = end;
    }
    size_t previous_sync = SIZE_MAX, landing = SIZE_MAX;
    uint32_t description = 0;
    int64_t nearest = INT64_MIN;
    for (size_t i = 0; i < count; ++i) {
        const PlayerSourceSample *sample = &rows[i];
        if (sample->stream != video)
            continue;
        if (sample->description != description) {
            description = sample->description;
            previous_sync = SIZE_MAX;
        }
        if (sample->key)
            previous_sync = sample->order;
        if (sample->pts != AV_NOPTS_VALUE && sample->pts <= player->timestamp && sample->pts > nearest) {
            nearest = sample->pts;
            landing = previous_sync;
        }
    }
    if (landing == SIZE_MAX) {
        rc = AVERROR_INVALIDDATA;
        goto done;
    }
    qsort(rows, count, sizeof(*rows), player_source_file_order);
    size_t sync = 0;
    while (rows[sync].order != landing)
        ++sync;
    const int64_t cutoff = FFMIN(rows[sync].pts, player->timestamp);
    const AVRational video_base = format->streams[video]->time_base;
    size_t start = 0;
    while (start < sync) {
        const PlayerSourceSample *sample = &rows[start];
        const AVStream *track = format->streams[sample->stream];
        if (sample->stream != video) {
            int64_t end;
            if (track->codecpar->codec_type != AVMEDIA_TYPE_AUDIO ||
                sample->pts == AV_NOPTS_VALUE || sample->pts != sample->dts ||
                sample->duration <= 0 ||
                __builtin_add_overflow(sample->pts, sample->duration, &end) ||
                av_compare_ts(end, track->time_base, cutoff, video_base) > 0)
                break;
        }
        ++start;
    }
    rc = 0;
    for (size_t i = start; i < count; ++i) {
        const PlayerSourceSample *sample = &rows[i];
        if (i < sync && sample->stream == video)
            continue;
        if (i == sync)
            player_field(work, 4, (uint64_t)sample->position);
        const AVStream *track = format->streams[sample->stream];
        if (work->trace && sample->disposable &&
            (track->codecpar->codec_id == AV_CODEC_ID_AV1 ||
             track->codecpar->codec_tag == MKTAG('d','a','v','1') ||
             track->codecpar->codec_tag == MKTAG('a','v','0','1')))
            fprintf(stderr, "AV1 demux marked sample at %" PRId64 " disposable\n", sample->position);
        AVPacket source = {.pts = sample->pts, .dts = sample->dts,
                           .duration = sample->duration, .flags = sample->key ? AV_PKT_FLAG_KEY : 0,
                           .data = (uint8_t *)input->data + sample->position,
                           .size = sample->size, .pos = sample->position};
        rc = player_packet(work, player, track, &source);
        if (rc < 0)
            break;
    }
done:
    av_free(rows);
    av_packet_free(&packet);
    packet_close(&format);
    return rc;
}

static int player_seek(AVFormatContext *format, AVPacket *packet,
                       const ViewCase *input, const PlaybackPlayerCase *player,
                       PacketWork *work)
{
    const int range = player->seek == PLAYER_OPEN || player->seek == PLAYER_DISCARD ||
                      player->seek == PLAYER_OPUS_ROLL;
    if (range) {
        if (format->duration == AV_NOPTS_VALUE) {
            int result = avformat_find_stream_info(format, NULL);
            if (result < 0)
                return result;
        }
        for (unsigned i = 0; i < format->nb_streams; ++i)
            if (format->streams[i]->codecpar->codec_type == AVMEDIA_TYPE_DATA)
                format->streams[i]->discard = AVDISCARD_ALL;
        int64_t duration;
        int result = player_range_duration(input, player, &duration);
        if (result < 0)
            return result;
        if (work->trace)
            fprintf(stderr, "demux duration estimate: %" PRId64 " us\n", format->duration);
        player_field(work, 3, (uint64_t)duration);
    }
    if (player->seek == PLAYER_OPEN)
        return player_read(format, packet, player, work, -1, -1, -1, 1);
    if (player->seek == PLAYER_SYNC)
        return player_source_seek(input, player, work);
    int video = player_stream(format, PLAYER_MOV, AVMEDIA_TYPE_VIDEO, player->video);
    if (video < 0)
        return video;
    int result = player_read(format, packet, player, NULL, 1, -1, -1, 1);
    if (result < 0)
        return result;
    if (player->seek == PLAYER_MOVIE) {
        result = player_movie_empty((Mp4View){input->data, input->size},
                                    (uint32_t)format->streams[video]->id, player->timestamp);
        if (result < 0)
            return result;
        if (result) {
            player_field(work, 3, 0);
            return 0;
        }
    }
    if (player->seek == PLAYER_OPUS_ROLL) {
        Mp4RollLanding landing = {0};
        int audio = player_stream(format, PLAYER_MOV, AVMEDIA_TYPE_AUDIO, -1);
        result = mp4_seek_opus_roll(format, (Mp4View){input->data, input->size},
                                   video, audio, player->timestamp, &landing);
        if (result < 0)
            return result;
        player_field(work, 5, (uint64_t)landing.sync);
        player_field(work, 5, (uint64_t)landing.first);
        player_field(work, 5, (uint64_t)landing.lead);
    } else {
        result = av_seek_frame(format, video, player->timestamp, AVSEEK_FLAG_BACKWARD);
        if (result < 0)
            return result;
        if (player->seek == PLAYER_MOVIE)
            return player_movie_read(format, packet, input, player, video, work);
    }
    return player_read(format, packet, player, work, -1, -1, video, 1);
}

/* EBML identifiers retain their marker bit. A size with every data bit
 * set extends to the enclosing end for the Segment used by this view. */
static int player_ebml_vint(const uint8_t *bytes, size_t end, size_t *at,
                           unsigned maximum, int identifier, uint64_t *value)
{
    if (*at >= end || !bytes[*at])
        return AVERROR_INVALIDDATA;
    unsigned width = 1;
    uint8_t marker = 0x80;
    while (!(bytes[*at] & marker)) {
        ++width;
        marker >>= 1;
    }
    if (width > maximum || width > end - *at)
        return AVERROR_INVALIDDATA;
    uint64_t word = identifier ? bytes[*at] : bytes[*at] & (marker - 1);
    for (unsigned i = 1; i < width; ++i)
        word = (word << 8) | bytes[*at + i];
    *at += width;
    *value = !identifier && word == (UINT64_C(1) << (7 * width)) - 1 ? UINT64_MAX : word;
    return 0;
}

static int player_matroska_delivery(AVFormatContext *format, AVPacket *packet,
                                    const PlaybackPlayerCase *player, PacketWork *work)
{
    int video = player_stream(format, PLAYER_MKV, AVMEDIA_TYPE_VIDEO, -1);
    if (video < 0)
        return video;
    if (player->capsule != 0) {
        int rc = player_read(format, packet, player, NULL, player->capsule == 1 ? 1 : -1,
                             video, -1, 0);
        if (rc < 0)
            return rc;
        int64_t target = av_rescale_q(player->timestamp, (AVRational){1, 1000},
                                     format->streams[video]->time_base);
        /* The reference's own backward seek: its video packets from the landing on. */
        rc = av_seek_frame(format, video, target, AVSEEK_FLAG_BACKWARD);
        if (rc < 0)
            return rc;
    }
    return player_read(format, packet, player, work, -1, video, -1, 0);
}

static int player_select(AVFormatContext *format, AVPacket *packet,
                         const PlaybackPlayerCase *player, PacketWork *work)
{
    if (player->format == PLAYER_TS) {
        int rc = avformat_find_stream_info(format, NULL);
        if (rc < 0)
            return rc;
    }
    player_selection(format, player->format, player->selection);
    if (player->switching) {
        int rc = player_read(format, packet, player, work, player->count, -1, -1, 1);
        if (rc < 0)
            return rc;
        int video = player_stream(format, player->format, AVMEDIA_TYPE_VIDEO, player->video);
        if (video < 0)
            return video;
        player_selection(format, player->format, player->replacement);
        int64_t target = player->timestamp;
        if (player->format == PLAYER_MKV)
            target = av_rescale_q(target, (AVRational){1, 1000}, format->streams[video]->time_base);
        rc = av_seek_frame(format, video, target, AVSEEK_FLAG_BACKWARD);
        if (rc < 0)
            return rc;
    }
    return player_read(format, packet, player, work, -1, -1, -1, 1);
}

static int player_demux(const ViewCase *input, const PlaybackPlayerCase *player, PacketWork *work)
{
    size_t header = player->format == PLAYER_MKV && player->operation == PLAYER_DELIVERY ? 17 : 0;
    PacketInput reader = {input->data + header, (int64_t)(input->size - header), 0, player->window};
    AVFormatContext *format = NULL;
    AVPacket *packet = NULL;
    int result = player_open(&format, &reader, player);
    if (result < 0)
        goto done;
    if (player->operation == PLAYER_INDEX) {
        /* Match the public stream discovery performed by the terminal matrix. */
        result = avformat_find_stream_info(format, NULL);
        if (result < 0)
            goto done;
    }
    packet = av_packet_alloc();
    if (!packet) {
        result = AVERROR(ENOMEM);
        goto done;
    }
    switch (player->operation) {
    case PLAYER_SEEK:
        result = player_seek(format, packet, input, player, work);
        break;
    case PLAYER_CATALOGUE:
        result = player_read(format, packet, player, work, player->count, -1, -1, 0);
        if (result < 0)
            break;
        packet_close(&format);
        reader = (PacketInput){player->other, (int64_t)player->other_size, 0, player->window};
        result = player_open(&format, &reader, player);
        if (result < 0)
            break;
        player_field(work, 3, (uint64_t)player->count);
        result = player_read(format, packet, player, work, -1, -1, -1, 0);
        break;
    case PLAYER_SELECT:
        result = player_select(format, packet, player, work);
        break;
    case PLAYER_DELIVERY:
        if (player->format == PLAYER_MKV) {
            result = player_matroska_delivery(format, packet, player, work);
            break;
        }
        /* MP4 delivery and index comparison use the same packet reader. */
        /* fall through */
    case PLAYER_INDEX:
        result = player_read(format, packet, player, work, -1, -1, -1, 0);
        break;
    case PLAYER_PES_OPEN:
        result = AVERROR(EINVAL);
        break;
    }
done:
    av_packet_free(&packet);
    packet_close(&format);
    return result;
}

/* The independent segment API emits video and audio PES. DVB subtitles
 * remain transport source events and have their own PES operation. */
static int player_prepare_pes(const ViewCase *input, PlaybackPlayerCase *player)
{
    TsProgramSelection selection;
    if (input->argc != 2 || (strcmp(input->argv[0], "segment") &&
        strcmp(input->argv[0], "private-audio") && strcmp(input->argv[0], "startup")) ||
        ts_program_parse(input->argv[1], &selection) < 0)
        return 0;
    const char *argument = input->argv[1];
    player->pes_selection = argument;
    if (strcmp(input->argv[0], "segment") || !strstr(argument, "=dvbsub"))
        return 1;

    char *media = av_strdup(argument);
    if (!media)
        return 0;
    player->media_selection = media;
    size_t prefix = (size_t)(strchr(argument, ':') - argument) + 1;
    const char *entry = argument + prefix;
    char *out = media + prefix;
    while (*entry) {
        size_t len = strcspn(entry, ",");
        const char dvb[] = "=dvbsub";
        const size_t suffix = sizeof(dvb) - 1;
        if (len <= suffix || memcmp(entry + len - suffix, dvb, suffix)) {
            if (out != media + prefix)
                *out++ = ',';
            memcpy(out, entry, len);
            out += len;
        }
        entry += len;
        if (*entry == ',')
            ++entry;
    }
    *out = 0;
    player->pes_selection = media;
    return ts_program_parse(media, &selection) == 0;
}

static int player_run(const ViewCase *input, ViewWork *work)
{
    const PlaybackPlayerCase *player = input->prepared;
    PacketWork packets = {.trace = work->trace};
    int result;
    if (player->operation == PLAYER_PES_OPEN)
        result = ts_program_operation(input->data, input->size, player->pes_selection, &packets);
    else if (player->mov_layout)
        result = mp4_forward_operation(input->data, input->size, player->mov_layout, &packets);
    else
        result = player_demux(input, player, &packets);
    if (result == AVERROR(ENOMEM) || result == AVERROR_BUG)
        return 2;
    if (result < 0 && packets.trace) {
        char error[AV_ERROR_MAX_STRING_SIZE];
        av_strerror(result, error, sizeof(error));
        fprintf(stderr, "reference refusal: %s\n", error);
    }
    player_field(&packets, result < 0 ? 8 : 7, 0);
    work->packets = packets.packets;
    work->bytes = packets.bytes;
    work->digest = packets.digest;
    return 0;
}

static int player_number(const char *text, int64_t *value)
{
    char *end;
    errno = 0;
    long long number = strtoll(text, &end, 10);
    if (errno || end == text || *end)
        return 0;
    *value = (int64_t)number;
    return 1;
}

static int player_track_list(const char *text, int64_t *list)
{
    list[0] = list[1] = list[2] = -1;
    while (*text) {
        int slot;
        if (!strncmp(text, "video=", 6)) { slot = 0; text += 6; }
        else if (!strncmp(text, "audio=", 6)) { slot = 1; text += 6; }
        else if (!strncmp(text, "subtitle=", 9)) { slot = 2; text += 9; }
        else return 0;
        char *end;
        errno = 0;
        long long id = strtoll(text, &end, 10);
        if (errno || id <= 0 || end == text || (*end && *end != ',') || list[slot] != -1)
            return 0;
        list[slot] = (int64_t)id;
        text = *end ? end + 1 : end;
    }
    return list[0] >= 0 || list[1] >= 0 || list[2] >= 0;
}

static int player_layout(PlaybackPlayerCase *player, const char *layout)
{
    if (!strcmp(layout, "regular") || !strcmp(layout, "progressive") ||
        !strcmp(layout, "progressive-edited") || !strcmp(layout, "progressive-edited-recovery")) {
        player->mov_layout = layout;
        return 1;
    }
    return 0;
}

static int player_prepare_seek(ViewCase *input, PlaybackPlayerCase *player)
{
    if (input->argc != 3 || !player_number(input->argv[1], &player->video) ||
        !player_number(input->argv[2], &player->timestamp) || player->video <= 0)
        return 0;
    const char *mode = input->argv[0];
    if (!strcmp(mode, "open")) player->seek = PLAYER_OPEN;
    else if (!strcmp(mode, "sync") || !strcmp(mode, "progressive-sync")) player->seek = PLAYER_SYNC;
    else if (!strcmp(mode, "demux") || !strcmp(mode, "progressive-demux")) player->seek = PLAYER_DEMUX;
    else if (!strcmp(mode, "movie")) player->seek = PLAYER_MOVIE;
    else if (!strcmp(mode, "discard")) player->seek = PLAYER_DISCARD;
    else if (!strcmp(mode, "opus-roll")) player->seek = PLAYER_OPUS_ROLL;
    else return 0;
    player->details = 1;
    return 1;
}

static int player_prepare_catalogue(ViewCase *input, PlaybackPlayerCase *player)
{
    if (input->argc != 4 || (strcmp(input->argv[0], "prepare") && strcmp(input->argv[0], "replace")) ||
        !player_number(input->argv[1], &player->count) || player->count < 0)
        return 0;
    const char *directory = input->argv[2], *file = input->argv[3];
    size_t count = strlen(directory) + strlen(file) + 2;
    char *path = malloc(count);
    if (!path)
        return 0;
    snprintf(path, count, "%s/%s", directory, file);
    player->other = view_load(path, &player->other_size);
    free(path);
    return player->other && player->other_size <= 64U * 1024U * 1024U;
}

static int player_prepare_delivery(ViewCase *input, PlaybackPlayerCase *player)
{
    if (input->argc != 2 || (strcmp(input->argv[0], "fifo") &&
        strcmp(input->argv[0], "ready") && strcmp(input->argv[0], "pump") && strcmp(input->argv[0], "direct")))
        return 0;
    if (strcmp(input->argv[1], "matroska"))
        return player_layout(player, input->argv[1]);
    if (input->size <= 17 || memcmp(input->data, "SMVCL001", 8) || input->data[8] > 2)
        return 0;
    player->format = PLAYER_MKV;
    player->window = 4096;
    player->capsule = input->data[8];
    player->timestamp = (int64_t)AV_RB64(input->data + 9);
    return 1;
}

static int player_prepare_selection(ViewCase *input, PlaybackPlayerCase *player)
{
    if (input->argc != 2 && input->argc != 6)
        return 0;
    const char *format = input->argv[0];
    if (!strcmp(format, "mp4")) player->format = PLAYER_MOV;
    else if (!strcmp(format, "matroska")) player->format = PLAYER_MKV;
    else if (!strcmp(format, "ts188")) player->format = PLAYER_TS;
    else return 0;
    if (!player_track_list(input->argv[1], player->selection))
        return 0;
    if (input->argc == 2)
        return 1;
    player->switching = 1;
    return player->format != PLAYER_TS
        && player_number(input->argv[2], &player->count) && player->count >= 0
        && player_number(input->argv[3], &player->video) && player->video > 0
        && player_number(input->argv[4], &player->timestamp)
        && player_track_list(input->argv[5], player->replacement);
}

static int player_prepare_index(ViewCase *input, PlaybackPlayerCase *player)
{
    if (input->argc == 2) {
        if (!strcmp(input->argv[0], "index"))
            return player_layout(player, input->argv[1]);
        if (!strcmp(input->argv[0], "index-encoded-range") && !strcmp(input->argv[1], "regular"))
            return player_layout(player, input->argv[1]);
        return 0;
    }
    if (input->argc != 5 || strcmp(input->argv[0], "terminal"))
        return 0;
    if (strcmp(input->argv[1], "file") && strcmp(input->argv[1], "pipe"))
        return 0;
    if (strcmp(input->argv[2], "known") && strcmp(input->argv[2], "unknown"))
        return 0;
    int64_t chunk, grant;
    if (!player_number(input->argv[3], &chunk) || chunk <= 0 || chunk > 4096 ||
        !player_number(input->argv[4], &grant) || grant <= 0)
        return 0;
    player->seekable = !strcmp(input->argv[1], "file");
    /* FFprobe's file and pipe results are the same reference for every
     * candidate read and work quantum of that transport. */
    player->window = MP4_FORWARD_QUANTUM;
    player->details = 1;
    return 1;
}

static int playback_player_prepare(ViewCase *input)
{
    PlaybackPlayerCase *player = calloc(1, sizeof(*player));
    if (!player)
        return 2;
    player->format = PLAYER_MOV;
    player->seekable = 1;
    player->window = 32768;
    input->prepared = player;
    input->release = player_release;
    input->run = player_run;
    int valid;
    if (!strcmp(input->prefix, "mp4-seek-")) {
        input->name = "playback-mp4-seek";
        player->operation = PLAYER_SEEK;
        valid = player_prepare_seek(input, player);
    } else if (!strcmp(input->prefix, "catalogue-")) {
        input->name = "playback-catalogue-replacement";
        player->operation = PLAYER_CATALOGUE;
        valid = player_prepare_catalogue(input, player);
    } else if (!strcmp(input->prefix, "delivery-")) {
        input->name = "playback-delivery";
        player->operation = PLAYER_DELIVERY;
        valid = player_prepare_delivery(input, player);
    } else if (!strcmp(input->prefix, "select-")) {
        input->name = "playback-track-selection";
        player->operation = PLAYER_SELECT;
        valid = player_prepare_selection(input, player);
    } else if (!strcmp(input->prefix, "mp4-index-")) {
        input->name = "playback-mov-index";
        player->operation = PLAYER_INDEX;
        valid = player_prepare_index(input, player);
    } else {
        input->name = "playback-ts-open";
        player->operation = PLAYER_PES_OPEN;
        valid = player_prepare_pes(input, player);
    }
    return valid && input->size <= 64U * 1024U * 1024U ? 0 : 2;
}
