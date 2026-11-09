/* Source roll groups for regular Opus seeks. Opus in ISO BMFF §4.3.6.2. */
#include <libavutil/mathematics.h>

typedef struct { const uint8_t *data; size_t size; } Mp4View;

static int mp4_box_next(Mp4View *input, uint32_t *kind, Mp4View *body) {
    if (input->size < 8)
        return 0;
    uint64_t size = AV_RB32(input->data);
    size_t header = 8;
    if (size == 1) {
        if (input->size < 16)
            return 0;
        size = AV_RB64(input->data + 8);
        header = 16;
    } else if (size == 0) {
        size = input->size;
    }
    if (size < header || size > input->size)
        return 0;
    *kind = AV_RB32(input->data + 4);
    *body = (Mp4View){input->data + header, (size_t)size - header};
    input->data += (size_t)size;
    input->size -= (size_t)size;
    return 1;
}

static Mp4View mp4_box(Mp4View input, uint32_t wanted) {
    uint32_t kind;
    Mp4View body;
    while (mp4_box_next(&input, &kind, &body))
        if (kind == wanted)
            return body;
    return (Mp4View){0};
}

static Mp4View mp4_roll_table(Mp4View input, uint32_t track_id) {
    Mp4View movie = mp4_box(input, MKBETAG('m','o','o','v'));
    uint32_t kind;
    Mp4View track;
    while (mp4_box_next(&movie, &kind, &track)) {
        if (kind != MKBETAG('t','r','a','k'))
            continue;
        Mp4View header = mp4_box(track, MKBETAG('t','k','h','d'));
        if (header.size < 16 || header.data[0] > 1)
            continue;
        size_t offset = header.data[0] == 0 ? 12 : 20;
        if (header.size < offset + 4 || AV_RB32(header.data + offset) != track_id)
            continue;
        Mp4View media = mp4_box(track, MKBETAG('m','d','i','a'));
        Mp4View info = mp4_box(media, MKBETAG('m','i','n','f'));
        return mp4_box(info, MKBETAG('s','t','b','l'));
    }
    return (Mp4View){0};
}

static Mp4View mp4_roll_group(Mp4View table, uint32_t wanted) {
    uint32_t kind;
    Mp4View body;
    while (mp4_box_next(&table, &kind, &body))
        if (kind == wanted && body.size >= 8 && !memcmp(body.data + 4, "roll", 4))
            return body;
    return (Mp4View){0};
}

/* Returns the number of preceding packets, or zero for an ungrouped sample. */
static int mp4_roll_distance(Mp4View table, uint32_t sample) {
    Mp4View map = mp4_roll_group(table, MKBETAG('s','b','g','p'));
    Mp4View desc = mp4_roll_group(table, MKBETAG('s','g','p','d'));
    if (map.size < 12 || map.data[0] > 1 || desc.size < 12 || desc.data[0] > 2)
        return AVERROR_INVALIDDATA;
    size_t offset = map.data[0] == 0 ? 8 : 12;
    if (map.size < offset + 4)
        return AVERROR_INVALIDDATA;
    uint32_t count = AV_RB32(map.data + offset);
    offset += 4;
    if (count > (map.size - offset) / 8)
        return AVERROR_INVALIDDATA;
    uint32_t group = 0;
    for (uint32_t i = 0; i < count; ++i, offset += 8) {
        uint32_t run = AV_RB32(map.data + offset);
        if (sample < run) {
            group = AV_RB32(map.data + offset + 4);
            break;
        }
        sample -= run;
    }

    uint8_t version = desc.data[0];
    offset = version == 0 ? 8 : version == 1 ? 12 : 16;
    if (desc.size < offset + 4)
        return AVERROR_INVALIDDATA;
    if (!group && version == 2)
        group = AV_RB32(desc.data + 12);
    if (!group)
        return 0;
    uint32_t length = version == 0 ? 2 : AV_RB32(desc.data + 8);
    if (group > AV_RB32(desc.data + offset))
        return AVERROR_INVALIDDATA;
    offset += 4;
    for (uint32_t i = 1; i <= group; ++i) {
        uint32_t entry_size = length;
        if (!length) {
            if (desc.size - offset < 4)
                return AVERROR_INVALIDDATA;
            entry_size = AV_RB32(desc.data + offset);
            offset += 4;
        }
        if (entry_size != 2 || desc.size - offset < entry_size)
            return AVERROR_INVALIDDATA;
        if (i == group) {
            uint16_t word = AV_RB16(desc.data + offset);
            return word >= 32768 ? 65536 - word : AVERROR_INVALIDDATA;
        }
        offset += entry_size;
    }
    return AVERROR_INVALIDDATA;
}

typedef struct {
    int64_t video_position;
    int64_t audio_position;
    int sync;
    int first;
    int64_t lead;
} Mp4RollLanding;

static int64_t mp4_clock_delta(int64_t later, int64_t earlier) {
    uint64_t delta = (uint64_t)later - (uint64_t)earlier;
    return earlier != AV_NOPTS_VALUE && later >= earlier && delta <= INT64_MAX ? (int64_t)delta : -1;
}

static int mp4_seek_opus_roll(AVFormatContext *format, AVPacket *packet, Mp4View input,
                              int video, int audio, Mp4RollLanding *landing) {
    AVStream *v = format->streams[video];
    AVStream *a = format->streams[audio];
    if (a->codecpar->codec_id != AV_CODEC_ID_OPUS)
        return AVERROR_INVALIDDATA;
    int result;
    int64_t video_pts = AV_NOPTS_VALUE;
    while ((result = av_read_frame(format, packet)) >= 0) {
        if (packet->stream_index == video) {
            video_pts = packet->pts;
            landing->video_position = packet->pos;
            av_packet_unref(packet);
            break;
        }
        av_packet_unref(packet);
    }
    if (result < 0 || video_pts == AV_NOPTS_VALUE || landing->video_position < 0)
        return AVERROR_INVALIDDATA;

    Mp4View table = mp4_roll_table(input, (uint32_t)a->id);
    Mp4View sizes = mp4_box(table, MKBETAG('s','t','s','z'));
    if (!sizes.size)
        sizes = mp4_box(table, MKBETAG('s','t','z','2'));
    int entries = avformat_index_get_entries_count(a);
    if (sizes.size < 12 || entries < 1 || (uint32_t)entries != AV_RB32(sizes.data + 8))
        return AVERROR_INVALIDDATA;
    int64_t target = av_rescale_q_rnd(video_pts, v->time_base, a->time_base, AV_ROUND_DOWN);
    int sync = av_index_search_timestamp(a, target, AVSEEK_FLAG_BACKWARD | AVSEEK_FLAG_ANY);
    if (sync < 0)
        sync = 0;
    int distance = mp4_roll_distance(table, (uint32_t)sync);
    if (distance < 0)
        return distance;
    int first = distance > 0 && sync >= distance ? sync - distance : 0;
    const AVIndexEntry *sync_entry = avformat_index_get_entry(a, sync);
    const AVIndexEntry *first_entry = avformat_index_get_entry(a, first);
    if (!sync_entry || !first_entry || first_entry->pos < 0)
        return AVERROR_INVALIDDATA;
    int64_t lead = mp4_clock_delta(sync_entry->timestamp, first_entry->timestamp);
    const AVRational samples = {1, 48000};
    if (lead < 0)
        return AVERROR_INVALIDDATA;
    int minimum = av_compare_ts(lead, a->time_base, 3840, samples);
    if ((!distance && minimum >= 0) || (first && minimum < 0))
        return AVERROR_INVALIDDATA;

    landing->audio_position = first_entry->pos;
    landing->sync = sync + 1;
    landing->first = first + 1;
    landing->lead = av_rescale_q(lead, a->time_base, samples);
    if (av_compare_ts(lead, a->time_base, landing->lead, samples))
        return AVERROR_INVALIDDATA;
    int64_t start = first_entry->timestamp;
    return avformat_seek_file(format, audio, start, start, start, AVSEEK_FLAG_ANY);
}
