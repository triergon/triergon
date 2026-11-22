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
} PlaybackPlayerCase;

static void player_release(void *opaque)
{
    PlaybackPlayerCase *player = opaque;
    free(player->other);
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
        player_field(work, 3, (uint64_t)format->duration);
    }
    if (player->seek == PLAYER_OPEN)
        return player_read(format, packet, player, work, -1, -1, -1, 1);
    int video = player_stream(format, PLAYER_MOV, AVMEDIA_TYPE_VIDEO, player->video);
    if (video < 0)
        return video;
    int result = player_read(format, packet, player, NULL, 1, -1, -1, 1);
    if (result < 0)
        return result;
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

    }
    return player_read(format, packet, player, work, -1, -1, video, 1);
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
        TsProgramSelection selection;
        input->name = "playback-ts-open";
        player->operation = PLAYER_PES_OPEN;
        valid = input->argc == 2 && (!strcmp(input->argv[0], "segment") ||
                  !strcmp(input->argv[0], "private-audio") || !strcmp(input->argv[0], "startup"))
                  && ts_program_parse(input->argv[1], &selection) == 0;
        if (valid)
            player->pes_selection = input->argv[1];
    }
    return valid && input->size <= 64U * 1024U * 1024U ? 0 : 2;
}
