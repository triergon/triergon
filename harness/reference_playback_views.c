/* Public FFmpeg operations for playback views. */
#include "reference_views_protocol.h"
#ifdef TRIERGON_PLAYBACK_SUBTITLE_WRAP
#include "reference_subtitles_host.h"
#endif
#include "reference_playback_probe.h"
#include "reference_playback_source_view.h"
#include "reference_playback_player.h"
#include "reference_playback_hls.h"
#include "reference_playback_bridge.h"
#include "reference_playback_subtitles.h"

/* Default-limits facade admission and the formats-profile MPEG-TS parser. */
#include <libavutil/crc.h>
#include "reference_ts_input.h"

/* Admission controls supply complete, single-carrier PAT sections. Check the
 * current table's coverage and CRC before using it to count media programs. */
static int facade_ts_pat_programs(const ViewCase *input, size_t width)
{
    uint8_t seen[256] = {0};
    uint8_t media_programs[UINT16_MAX / 8 + 1] = {0};
    unsigned sections = 0, programs = 0;
    int last = -1, version = -1, identity = -1;
    const AVCRC *crc = av_crc_get_table(AV_CRC_32_IEEE);
    for (size_t at = 0; input->size - at >= width; at += width) {
        const uint8_t *ts = input->data + at + width - 188;
        if (ts[0] != 0x47 || (ts[1] & 0x9f) || ts[2] || !(ts[1] & 0x40) ||
            !(ts[3] & 0x10) || (ts[3] & 0xc0))
            continue;
        size_t offset = 4;
        if (ts[3] & 0x20)
            offset += (size_t)ts[4] + 1;
        if (offset >= 188)
            return -1;
        offset += (size_t)ts[offset] + 1;
        if (offset > 188 - 12)
            return -1;
        const uint8_t *section = ts + offset;
        const size_t length = (AV_RB16(section + 1) & 0x0fff) + 3;
        if (section[0] || (section[1] & 0xf0) != 0xb0 || !(section[5] & 1) ||
            length < 12 || length > 188 - offset || (length - 12) % 4 ||
            section[6] > section[7] || av_crc(crc, UINT32_MAX, section, length))
            return -1;
        if (!sections) {
            last = section[7];
            version = section[5] >> 1;
            identity = AV_RB16(section + 3);
        }
        if (section[7] != last || section[5] >> 1 != version ||
            AV_RB16(section + 3) != identity || seen[section[6]]++)
            return -1;
        ++sections;
        for (size_t i = 8; i < length - 4; i += 4) {
            const uint16_t program = AV_RB16(section + i);
            const unsigned bit = 1u << (program % 8);
            if ((section[i + 2] & 0xe0) != 0xe0 || (media_programs[program / 8] & bit))
                return -1;
            media_programs[program / 8] |= bit;
            programs += program != 0;
        }
    }
    return last >= 0 && sections == (unsigned)last + 1 ? (int)programs : -1;
}

static void facade_ts_fold(const AVStream *stream, const AVPacket *packet, ViewWork *work)
{
    const uint64_t fields[] = {stream->index, stream->id, packet->pts, packet->dts,
        packet->duration, packet->pos, packet->flags, packet->size};
    for (size_t i = 0; i < sizeof(fields) / sizeof(*fields); ++i)
        fold(&work->digest, fields[i]);
    fold_bytes(&work->digest, packet->data, (size_t)packet->size);
    fold(&work->digest, packet->side_data_elems);
    for (int i = 0; i < packet->side_data_elems; ++i) {
        const AVPacketSideData *side = &packet->side_data[i];
        fold(&work->digest, side->type);
        fold(&work->digest, side->size);
        fold_bytes(&work->digest, side->data, side->size);
    }
    ++work->packets;
    work->bytes += (uint64_t)packet->size;
    if (work->trace) {
        uint8_t hash[16];
        av_md5_sum(hash, packet->data, (size_t)packet->size);
        printf("%d %d %" PRId64 " %" PRId64 " %" PRId64 " %" PRId64 " %d %d",
               stream->index, stream->id, packet->pts, packet->dts, packet->duration,
               packet->pos, packet->flags, packet->size);
        for (int i = 0; i < packet->side_data_elems; ++i) {
            const AVPacketSideData *side = &packet->side_data[i];
            if (side->type == AV_PKT_DATA_MPEGTS_STREAM_ID && side->size == 1)
                printf(" %u", side->data[0]);
        }
        putchar(' ');
        for (size_t i = 0; i < sizeof(hash); ++i)
            printf("%02x", hash[i]);
        putchar('\n');
    }
}

/* PTS_DTS_flags (H.222.0 §2.4.3.7) of the PES that starts in the carrier at
 * `carrier`, read across the PID's following carriers, or -1 when that carrier
 * does not start a PES of `pid`. */
static int facade_ts_wire_flags(const ViewCase *input, size_t width, unsigned pid, size_t carrier)
{
    const size_t prefix = width - 188;
    uint8_t header[8];
    size_t have = 0;
    int started = 0;
    for (size_t at = carrier; input->size - at >= width; at += width) {
        const uint8_t *ts = input->data + at + prefix;
        if (ts[0] != 0x47)
            return -1;
        if ((((unsigned)ts[1] & 0x1f) << 8 | ts[2]) != pid) {
            if (!started)
                return -1;
            continue;
        }
        if (!!(ts[1] & 0x40) == started)
            return -1;
        started = 1;
        if (!(ts[3] & 0x10))
            continue;
        const size_t payload = ts[3] & 0x20 ? 5u + ts[4] : 4u;
        if (payload >= 188)
            return -1;
        size_t take = 188 - payload;
        if (take > sizeof(header) - have)
            take = sizeof(header) - have;
        memcpy(header + have, ts + payload, take);
        have += take;
        if (have == sizeof(header))
            return !header[0] && !header[1] && header[2] == 1 ? header[7] >> 6 : -1;
    }
    return -1;
}

/* The duration in 90 kHz ticks that an ADTS (ISO/IEC 13818-7 §6.2.1) or MPEG
 * audio (ISO/IEC 11172-3 §2.4.2.3, ISO/IEC 13818-3 §2.4.2.3) frame header gives,
 * when it is a whole number of ticks; zero otherwise. */
static int64_t facade_ts_frame_duration(const AVStream *stream, const AVPacket *packet)
{
    const uint8_t *data = packet->data;
    unsigned samples = 0, rate = 0;
    if (stream->codecpar->codec_id == AV_CODEC_ID_AAC) {
        static const unsigned rates[] = {96000, 88200, 64000, 48000, 44100, 32000, 24000,
                                         22050, 16000, 12000, 11025, 8000, 7350};
        if (packet->size < 7 || data[0] != 0xff || (data[1] & 0xf6) != 0xf0 ||
            ((data[2] >> 2) & 15u) >= sizeof(rates) / sizeof(*rates))
            return 0;
        const unsigned index = (data[2] >> 2) & 15u;
        samples = 1024u * ((data[6] & 3u) + 1u);
        rate = rates[index];
    } else if (stream->codecpar->codec_id == AV_CODEC_ID_MP1 ||
               stream->codecpar->codec_id == AV_CODEC_ID_MP2 ||
               stream->codecpar->codec_id == AV_CODEC_ID_MP3) {
        static const unsigned rates[] = {44100, 48000, 32000};
        if (packet->size < 4 || data[0] != 0xff || (data[1] & 0xe0) != 0xe0)
            return 0;
        const unsigned version = (data[1] >> 3) & 3u, layer = (data[1] >> 1) & 3u;
        const unsigned index = (data[2] >> 2) & 3u;
        if (version == 1 || !layer || index == 3)
            return 0;
        rate = rates[index] >> (version == 3 ? 0 : version == 2 ? 1 : 2);
        samples = layer == 3 ? 384u : layer == 2 || version == 3 ? 1152u : 576u;
    }
    if (!rate || (UINT64_C(90000) * samples) % rate)
        return 0;
    return (int64_t)(UINT64_C(90000) * samples / rate);
}

/* Project a demuxed packet onto the coded transport facts the strict reader
 * publishes, as the source-poll projection does: the position of the packet's
 * source carrier (including an M2TS prefix), PTS and DTS as their coded 33-bit
 * values, DTS only where the PES header codes it, and, where the demuxer reports
 * no duration, the duration the coded audio frame header gives. A packet that
 * does not start at a PES carries neither clock. */
static int facade_ts_project(const ViewCase *input, size_t width, const AVStream *stream,
                             AVPacket *packet)
{
    const uint64_t mask = (UINT64_C(1) << 33) - 1;
    if (packet->pos >= 0) {
        /* The demuxer reports either the carrier start or its sync byte. */
        const size_t position = (size_t)packet->pos;
        if (position % width && position % width != width - 188)
            return AVERROR_INVALIDDATA;
        const size_t carrier = position - position % width;
        const int flags = facade_ts_wire_flags(input, width, (unsigned)stream->id, carrier);
        /* PTS_DTS_flags 01 is forbidden and codes no timestamp. */
        if (flags < 0 || (flags < 2 && packet->pts != AV_NOPTS_VALUE) ||
            (flags == 3 && packet->dts == AV_NOPTS_VALUE))
            return AVERROR_INVALIDDATA;
        if (flags != 3)
            packet->dts = AV_NOPTS_VALUE;
        packet->pos = (int64_t)carrier;
    } else if (packet->pts != AV_NOPTS_VALUE || packet->dts != AV_NOPTS_VALUE) {
        return AVERROR_INVALIDDATA;
    }
    if (packet->pts != AV_NOPTS_VALUE)
        packet->pts = (int64_t)((uint64_t)packet->pts & mask);
    if (packet->dts != AV_NOPTS_VALUE)
        packet->dts = (int64_t)((uint64_t)packet->dts & mask);
    if (!packet->duration)
        packet->duration = facade_ts_frame_duration(stream, packet);
    return 0;
}

typedef struct {
    uint8_t data[8194];
    size_t used, size;
    int64_t pts, dts, position;
    uint8_t stream_id;
    int rate;
    const AVStream *stream;
} FacadeAudioFrame;

static int facade_ts_audio_header(FacadeAudioFrame *frame, int layer1)
{
    const uint8_t *data = frame->data;
    if (!layer1) {
        if ((AV_RB16(data) >> 5) != 0x2b7)
            return 2;
        frame->size = 3 + (((data[1] & 31u) << 8) | data[2]);
        return frame->size == 3 ? 2 : 0;
    }
    const unsigned bitrate = data[2] >> 4;
    const unsigned rate = (data[2] >> 2) & 3;
    const int rates[] = {44100, 48000, 32000};
    if (data[0] != 0xff || (data[1] & 0xfe) != 0xfe || !bitrate || bitrate == 15 || rate == 3)
        return 2;
    frame->rate = rates[rate];
    frame->size = 4 * (12 * bitrate * 32000 / (unsigned)frame->rate + ((data[2] >> 1) & 1));
    return frame->size < 4 || frame->size > sizeof(frame->data) ? 2 : 0;
}

static int facade_ts_audio_feed(FacadeAudioFrame *frame, const ViewCase *input,
                                const AVStream *stream, const AVPacket *packet, ViewWork *work)
{
    const size_t width = (size_t)atoi(input->argv[0]);
    const int layer1 = !strcmp(input->argv[2], "layer1");
    int64_t pts, dts;
    int id = source_pes_clock((CatalogueField){input->data, input->size}, width,
                              (unsigned)stream->id, packet, &pts, &dts);
    if (id < 0 || (frame->stream && stream != frame->stream))
        return 2;
    frame->stream = stream;
    int anchor = 1;
    const int64_t position = packet->pos / (int64_t)width * (int64_t)width;
    for (size_t i = 0; i < (size_t)packet->size; ++i) {
        if (!frame->used) {
            frame->pts = anchor ? pts : AV_NOPTS_VALUE;
            frame->dts = anchor ? dts : AV_NOPTS_VALUE;
            frame->position = (layer1 ? anchor : i == 0) ? position : -1;
            frame->stream_id = (uint8_t)id;
            anchor = 0;
        }
        frame->data[frame->used++] = packet->data[i];
        if (frame->used == (layer1 ? 4u : 3u) && facade_ts_audio_header(frame, layer1))
            return 2;
        if (frame->used == frame->size) {
            int num = 0, den = 0;
            if (layer1 && !av_reduce(&num, &den, 384 * 90000, frame->rate, INT_MAX))
                return 2;
            AVPacketSideData side = {.data = &frame->stream_id, .size = 1, .type = AV_PKT_DATA_MPEGTS_STREAM_ID};
            const AVPacket projected = {.data = frame->data, .size = (int)frame->size,
                .pts = frame->pts, .dts = frame->dts, .pos = frame->position,
                .duration = den == 1 ? num : 0, .flags = layer1 ? AV_PKT_FLAG_KEY : 0,
                .side_data = &side, .side_data_elems = 1};
            facade_ts_fold(stream, &projected, work);
            if (layer1) {
                fold(&work->digest, (uint64_t)num);
                fold(&work->digest, (uint64_t)den);
                if (work->trace)
                    printf("duration %d/%d\n", num, den);
            }
            frame->size = frame->used = 0;
        }
    }
    return 0;
}

static int facade_ts_native(const ViewCase *input, int program, ViewWork *work)
{
    TsInput source = {input->data, input->size, 0};
    AVFormatContext *format = avformat_alloc_context();
    uint8_t *buffer = av_malloc(TS_AVIO_BYTES);
    AVIOContext *io = NULL;
    AVPacket *packet = NULL;
    FacadeAudioFrame audio = {0};
    const int frame_audio = input->argc == 3 && strcmp(input->argv[2], "admission");
    int rc = AVERROR(ENOMEM);
    if (format && buffer)
        io = avio_alloc_context(buffer, TS_AVIO_BYTES, 0, &source, ts_read, NULL, ts_seek);
    if (!io) {
        av_free(buffer);
        goto done;
    }
    format->pb = io;
    format->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_NOFILLIN;
    if (frame_audio)
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
        if (ts_in_program(format, packet->stream_index, program)) {
            const AVStream *stream = format->streams[packet->stream_index];
            if (!frame_audio) {
                if (facade_ts_project(input, (size_t)atoi(input->argv[0]), stream, packet) < 0) {
                    rc = AVERROR_INVALIDDATA;
                    goto done;
                }
                facade_ts_fold(stream, packet, work);
            } else if (facade_ts_audio_feed(&audio, input, stream, packet, work)) {
                rc = AVERROR_INVALIDDATA;
                goto done;
            }
        }
        av_packet_unref(packet);
    }
done:
    av_packet_free(&packet);
    avformat_close_input(&format);
    if (io) {
        av_freep(&io->buffer);
        avio_context_free(&io);
    }
    return rc == AVERROR_EOF && !audio.used && (!frame_audio || audio.stream) ? 0 : 2;
}

static int facade_ts_run(const ViewCase *input, ViewWork *work)
{
    const int program = atoi(input->argv[1]);
    if (facade_ts_native(input, program, work))
        return 2;
    if (input->argc == 3 && !strcmp(input->argv[2], "admission")) {
        const size_t width = (size_t)atoi(input->argv[0]);
        if (work->packets)
            return 2;
        uint64_t reason;
        if (input->size < 3 * width && !playback_probe_width(input->data, input->size))
            reason = 1;
        else if (playback_probe_width(input->data, input->size) == width &&
                 facade_ts_pat_programs(input, width) > 1)
            reason = 2;
        else
            return 2;
        view_record(work, 9, &reason, 1, NULL, 0);
        if (work->trace)
            printf("9 %" PRIu64 "\n", reason);
    }
    return 0;
}

static int facade_ts_prepare(ViewCase *input)
{
    size_t program;
    if ((input->argc != 2 && input->argc != 3) ||
        (strcmp(input->argv[0], "188") && strcmp(input->argv[0], "192")) ||
        !playback_probe_size(input->argv[1], UINT16_MAX, &program) ||
        (input->argc == 3 && strcmp(input->argv[2], "admission") && strcmp(input->argv[2], "loas") &&
         strcmp(input->argv[2], "layer1")))
        return 2;
    input->name = "mpegts-access-unit-packets";
    input->run = facade_ts_run;
    return 0;
}

int main(int argc, char **argv)
{
    if (argc > 1 && !strncmp(argv[1], "subtitle-bridge-", sizeof("subtitle-bridge-") - 1))
        return playback_subtitle_command(argc, argv);
    const ViewDispatch operations[] = {
        {"views-smoke-", view_smoke_prepare},
        {"probe-", playback_probe_prepare},
        {"facade-probe-", playback_probe_prepare},
        {"source-view-", playback_source_view_prepare},
        {"any-ts-au-", facade_ts_prepare},
        {"mp4-seek-", playback_player_prepare},
        {"catalogue-", playback_player_prepare},
        {"delivery-", playback_player_prepare},
        {"select-", playback_player_prepare},
        {"mp4-index-", playback_player_prepare},
        {"ts-pes-open-", playback_player_prepare},
        {"hls-views-", playback_hls_prepare},
        {"access-units-", playback_bridge_prepare},
        {"mp4-bridges-", playback_bridge_prepare},
        {"mkv-discovery-", playback_bridge_prepare},
        {"text-bridge-", playback_subtitles_prepare},
    };
    return view_main(argc, argv, operations, sizeof(operations) / sizeof(*operations));
}
