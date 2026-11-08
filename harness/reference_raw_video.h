/* Own reference tooling: raw AVC or HEVC Annex B through public FFmpeg APIs, the
 * same operation as playback-worker's raw-video modes.
 *
 * A preloaded forward-only input read in 4 KiB windows through the h264 or hevc
 * demuxer and its stock parser, every access unit through the end of the input.
 * Each packet is folded with stream index, PTS, DTS and duration in the demuxer's
 * 1/1,200,000 s time base, key flag and payload. CODEC is avc or hevc. */

#define RAW_VIDEO_QUANTUM 4096

static int raw_video_operation(const uint8_t *data, size_t size, const char *codec, PacketWork *work)
{
    const char *name = !strcmp(codec, "avc") ? "h264" : !strcmp(codec, "hevc") ? "hevc" : NULL;
    if (!name)
        return AVERROR(EINVAL);
    PacketInput in = {data, (int64_t)size, 0, RAW_VIDEO_QUANTUM};
    AVFormatContext *format = NULL;
    AVPacket *packet = NULL;
    int rc = packet_open(&format, &in, 0, av_find_input_format(name), 0);
    if (rc < 0)
        goto done;
    if (format->nb_streams != 1 || format->streams[0]->time_base.num != 1 ||
        format->streams[0]->time_base.den != 1200000) {
        rc = AVERROR_INVALIDDATA;
        goto done;
    }
    packet = av_packet_alloc();
    if (!packet) {
        rc = AVERROR(ENOMEM);
        goto done;
    }
    while ((rc = av_read_frame(format, packet)) >= 0) {
        packet_fold(work, (uint64_t)packet->stream_index,
                    packet->pts == AV_NOPTS_VALUE ? INT64_MIN : packet->pts,
                    packet->dts == AV_NOPTS_VALUE ? INT64_MIN : packet->dts,
                    packet->duration > 0 ? (uint64_t)packet->duration : 0,
                    !!(packet->flags & AV_PKT_FLAG_KEY), packet->data, (size_t)packet->size);
        av_packet_unref(packet);
    }
    if (rc == AVERROR_EOF)
        rc = in.at == in.size ? 0 : AVERROR_INVALIDDATA;

done:
    av_packet_free(&packet);
    packet_close(&format);
    return rc;
}

static int raw_video_main(int argc, char **argv, const uint8_t *data, long size)
{
    return packet_main(argc, argv, data, size, "raw-video-", "raw-annex-b-feed", raw_video_operation);
}
