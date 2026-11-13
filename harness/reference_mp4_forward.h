/* Own reference tooling: MP4 or MOV forward playback through public FFmpeg APIs, the
 * same operation as playback-worker's mp4-forward modes.
 *
 * A preloaded input read in 32 KiB windows through the mov demuxer without parsers
 * or timestamp fill-in, every packet of every track through the end of the input.
 * Each packet is folded with its track ID, PTS, DTS and duration in the track's
 * time base, packet flags and payload. LAYOUT regular reads a seekable input,
 * progressive and progressive-edited read a forward-only input. The edited
 * operation also folds the source offset, normalized time base and skip samples. */

#include <libavutil/intreadwrite.h>

#define MP4_FORWARD_QUANTUM (32 * 1024)

static int mp4_forward_operation(const uint8_t *data, size_t size, const char *layout, PacketWork *work)
{
    const int regular = !strcmp(layout, "regular");
    const int edited = !strcmp(layout, "progressive-edited");
    if (!regular && !edited && strcmp(layout, "progressive"))
        return AVERROR(EINVAL);
    PacketInput in = {data, (int64_t)size, 0, MP4_FORWARD_QUANTUM};
    AVFormatContext *format = NULL;
    AVPacket *packet = NULL;
    int rc = packet_open(&format, &in, regular, av_find_input_format("mov"),
                         AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN);
    if (rc < 0)
        goto done;
    packet = av_packet_alloc();
    if (!packet) {
        rc = AVERROR(ENOMEM);
        goto done;
    }
    while ((rc = av_read_frame(format, packet)) >= 0) {
        const AVStream *stream = format->streams[packet->stream_index];
        packet_fold(work, (uint64_t)(uint32_t)stream->id,
                    packet->pts == AV_NOPTS_VALUE ? INT64_MIN : packet->pts,
                    packet->dts == AV_NOPTS_VALUE ? INT64_MIN : packet->dts,
                    packet->duration > 0 ? (uint64_t)packet->duration : 0,
                    (uint64_t)packet->flags, packet->data, (size_t)packet->size);
        if (edited) {
            int time_num, time_den;
            if (packet->pos < 0 || stream->time_base.num <= 0 || stream->time_base.den <= 0 ||
                !av_reduce(&time_num, &time_den, stream->time_base.num, stream->time_base.den, INT_MAX)) {
                rc = AVERROR_INVALIDDATA;
                goto done;
            }
            fold(&work->digest, (uint64_t)packet->pos);
            fold(&work->digest, (uint64_t)time_num);
            fold(&work->digest, (uint64_t)time_den);
            if (work->trace)
                printf("source %" PRId64 " %d/%d\n", packet->pos, time_num, time_den);
            size_t skip_len = 0;
            const uint8_t *skip = av_packet_get_side_data(packet, AV_PKT_DATA_SKIP_SAMPLES, &skip_len);
            if (skip && skip_len != 10) {
                rc = AVERROR_INVALIDDATA;
                goto done;
            }
            fold(&work->digest, skip != NULL);
            if (skip) {
                fold_bytes(&work->digest, skip, skip_len);
                if (work->trace)
                    printf("skip %" PRIu32 " %" PRIu32 " %u %u\n",
                           AV_RL32(skip), AV_RL32(skip + 4),
                           (unsigned)skip[8], (unsigned)skip[9]);
            } else if (work->trace) {
                puts("skip -");
            }
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

static int mp4_forward_main(int argc, char **argv, const uint8_t *data, long size)
{
    return packet_main(argc, argv, data, size, "mp4-forward-", "mp4-forward-playback", mp4_forward_operation);
}
