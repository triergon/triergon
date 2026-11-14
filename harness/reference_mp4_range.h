/* Seekable MP4 playback through the public MOV demuxer API. */
#include <errno.h>
#include "reference_mp4_roll.h"

typedef struct {
    uint64_t packets;
    uint64_t bytes;
    uint64_t digest;
} Mp4RangeWork;

static int mp4_range_read(const uint8_t *data, size_t size, int seek, int64_t target,
                          int recover, Mp4RangeWork *work) {
    Source source = {data, size, 0};
    AVFormatContext *format = avformat_alloc_context();
    AVIOContext *io = NULL;
    AVPacket *packet = NULL;
    int result = AVERROR(ENOMEM);
    if (!format)
        goto done;
    uint8_t *buffer = av_malloc(32768);
    if (!buffer)
        goto done;
    io = avio_alloc_context(buffer, 32768, 0, &source, read_bytes, NULL, seek_bytes);
    if (!io) {
        av_free(buffer);
        goto done;
    }
    format->pb = io;
    format->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN;
    const AVInputFormat *mov = av_find_input_format("mov");
    result = mov ? avformat_open_input(&format, NULL, mov, NULL) : AVERROR_DEMUXER_NOT_FOUND;
    if (result < 0)
        goto done;
    int video = -1;
    int audio = -1;
    result = AVERROR_INVALIDDATA;
    if (format->nb_streams < 1 || format->nb_streams > 64)
        goto done;
    for (unsigned i = 0; i < format->nb_streams; ++i) {
        AVStream *stream = format->streams[i];
        const AVCodecParameters *codec = stream->codecpar;
        if (codec->codec_type == AVMEDIA_TYPE_DATA) {
            stream->discard = AVDISCARD_ALL;
            continue;
        }
        const uint32_t tag = codec->codec_tag;
        const int video_codec =
            (codec->codec_id == AV_CODEC_ID_H264 && tag == MKTAG('a', 'v', 'c', '1')) ||
            (codec->codec_id == AV_CODEC_ID_HEVC &&
             (tag == MKTAG('d', 'v', 'h', '1') || tag == MKTAG('d', 'v', 'h', 'e'))) ||
            /* The MOV demuxer can emit dav1 packets and configuration with no codec ID. */
            (tag == MKTAG('d', 'a', 'v', '1') &&
             (codec->codec_id == AV_CODEC_ID_AV1 || codec->codec_id == AV_CODEC_ID_NONE));
        if (video < 0 && codec->codec_type == AVMEDIA_TYPE_VIDEO && video_codec)
            video = (int)i;
        else if (audio < 0 && codec->codec_type == AVMEDIA_TYPE_AUDIO &&
                 (codec->codec_id == AV_CODEC_ID_AAC ||
                  (codec->codec_id == AV_CODEC_ID_OPUS && tag == MKTAG('O', 'p', 'u', 's'))))
            audio = (int)i;
        else if (codec->codec_type == AVMEDIA_TYPE_SUBTITLE &&
                 ((codec->codec_id == AV_CODEC_ID_MOV_TEXT && tag == MKTAG('t', 'x', '3', 'g')) ||
                  (codec->codec_id == AV_CODEC_ID_DVD_SUBTITLE && tag == MKTAG('m', 'p', '4', 's'))))
            continue;
        else
            goto done;
    }
    if (video < 0)
        goto done;
    const AVCodecParameters *codec = format->streams[video]->codecpar;
    if (audio < 0 && codec->codec_tag != MKTAG('d', 'a', 'v', '1'))
        goto done;
    const AVDictionaryEntry *brand = av_dict_get(format->metadata, "major_brand", NULL, 0);
    const int quicktime = brand && !strcmp(brand->value, "qt  ");
    for (unsigned i = 0; i < format->nb_streams; ++i) {
        const AVCodecParameters *config = format->streams[i]->codecpar;
        const int compare_configuration = (int)i == video
            ? config->codec_tag != MKTAG('a', 'v', 'c', '1')
            : config->codec_id == AV_CODEC_ID_OPUS ||
              (quicktime && config->codec_id == AV_CODEC_ID_AAC);
        if (!compare_configuration)
            continue;
        if (config->extradata_size <= 0 || !config->extradata)
            goto done;
        fold(&work->digest, config->codec_tag);
        fold(&work->digest, (uint64_t)config->extradata_size);
        fold_bytes(&work->digest, config->extradata, (size_t)config->extradata_size);
    }
    packet = av_packet_alloc();
    if (!packet) {
        result = AVERROR(ENOMEM);
        goto done;
    }
    Mp4RollLanding landing = {0};
    int waiting_video = recover;
    int waiting_audio = recover;
    if (seek) {
        result = av_read_frame(format, packet);
        if (result < 0)
            goto done;
        av_packet_unref(packet);
        if (recover) {
            result = mp4_seek_opus_roll(format, (Mp4View){data, size}, video, audio, target, &landing);
            if (result < 0)
                goto done;
            fold(&work->digest, (uint64_t)landing.video_position);
            fold(&work->digest, (uint64_t)landing.sync);
            fold(&work->digest, (uint64_t)landing.first);
            fold(&work->digest, (uint64_t)landing.lead);
        } else {
            result = av_seek_frame(format, video, target, AVSEEK_FLAG_BACKWARD);
            if (result < 0)
                goto done;
        }
    }
    while ((result = av_read_frame(format, packet)) >= 0) {
        if (packet->pos < 0 || packet->size < 0 || packet->stream_index < 0 ||
            (unsigned)packet->stream_index >= format->nb_streams ||
            (packet->stream_index != video && packet->stream_index != audio &&
             format->streams[packet->stream_index]->codecpar->codec_type != AVMEDIA_TYPE_SUBTITLE)) {
            result = AVERROR_INVALIDDATA;
            goto done;
        }
        if (waiting_video && packet->stream_index == video) {
            if (packet->pos != landing.video_position || packet->pts != landing.video_pts) {
                result = AVERROR_INVALIDDATA;
                goto done;
            }
            waiting_video = 0;
        }
        if (waiting_audio && packet->stream_index == audio) {
            if (packet->pos != landing.audio_position) {
                result = AVERROR_INVALIDDATA;
                goto done;
            }
            waiting_audio = 0;
        }
        size_t skip_len = 0;
        const uint8_t *skip = av_packet_get_side_data(packet, AV_PKT_DATA_SKIP_SAMPLES, &skip_len);
        if (skip && skip_len != 10) {
            result = AVERROR_INVALIDDATA;
            goto done;
        }
        fold(&work->digest, (uint64_t)format->streams[packet->stream_index]->id);
        fold(&work->digest, (uint64_t)packet->pts);
        fold(&work->digest, (uint64_t)packet->dts);
        fold(&work->digest, (uint64_t)packet->duration);
        fold(&work->digest, (uint64_t)packet->pos);
        fold(&work->digest, (uint64_t)packet->size);
        fold(&work->digest, (uint64_t)packet->flags);
        fold(&work->digest, skip ? AV_RL32(skip) : 0);
        fold(&work->digest, skip ? AV_RL32(skip + 4) : 0);
        fold(&work->digest, skip ? skip[8] : 0);
        fold(&work->digest, skip ? skip[9] : 0);
        fold_bytes(&work->digest, packet->data, (size_t)packet->size);
        ++work->packets;
        work->bytes += (uint64_t)packet->size;
        av_packet_unref(packet);
    }
    if (result == AVERROR_EOF)
        result = waiting_video || waiting_audio ? AVERROR_INVALIDDATA : 0;
done:
    av_packet_free(&packet);
    avformat_close_input(&format);
    if (io) {
        av_freep(&io->buffer);
        avio_context_free(&io);
    }
    return result;
}

static int mp4_range_main(int argc, char **argv, const uint8_t *data, size_t size) {
    int bench = !strcmp(argv[1], "mp4-range-bench");
    int memory = !strcmp(argv[1], "mp4-range-memory");
    int next = 3;
    uint64_t iterations = 1;
    char *end;
    if (!bench && !memory && strcmp(argv[1], "mp4-range-fold"))
        return 2;
    if (bench) {
        if (argc <= next)
            return 2;
        errno = 0;
        iterations = strtoull(argv[next], &end, 10);
        if (errno || end == argv[next] || *end || iterations == 0 || iterations > 1000000)
            return 2;
        ++next;
    }
    int seek = argc > next;
    int recover = argc == next + 2 && !strcmp(argv[next + 1], "opus-roll");
    int64_t target = 0;
    if (seek) {
        errno = 0;
        target = strtoll(argv[next], &end, 10);
        if (argc != next + 1 + recover || errno || end == argv[next] || *end)
            return 2;
    }
    if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18)
        return 2;
    Mp4RangeWork total = {0};
    uint64_t start = now_ns();
    for (uint64_t i = 0; i < iterations; ++i) {
        Mp4RangeWork work = {0};
        int result = mp4_range_read(data, size, seek, target, recover, &work);
        if (result < 0) {
            fprintf(stderr, "MP4 range playback: %s\n", av_err2str(result));
            return 1;
        }
        total.packets += work.packets;
        total.bytes += work.bytes;
        fold(&total.digest, work.digest);
    }
    uint64_t elapsed = bench ? now_ns() - start : 0;
    if (memory && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16)
        return 2;
    printf("{\"schema\":1,\"operation\":\"mp4-range-playback\",\"iterations\":%" PRIu64
           ",\"packets\":%" PRIu64 ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64
           ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true}\n",
           iterations, total.packets, total.bytes, total.digest, elapsed);
    return 0;
}
