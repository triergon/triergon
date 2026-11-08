/* Own reference tooling: MP4 or MOV forward playback through public FFmpeg APIs, the
 * same operation as playback-worker's mp4-forward modes.
 *
 * A preloaded input read in 32 KiB windows through the mov demuxer without parsers
 * or timestamp fill-in, every packet of every track through the end of the input.
 * Each packet is folded with its track ID, PTS, DTS and duration in the track's
 * time base, packet flags and payload. LAYOUT regular reads a seekable input,
 * progressive a forward-only one. */

#define MP4_FORWARD_QUANTUM (32 * 1024)

static int mp4_forward_operation(const uint8_t *data, size_t size, const char *layout, PacketWork *work)
{
    const int regular = !strcmp(layout, "regular");
    if (!regular && strcmp(layout, "progressive"))
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
