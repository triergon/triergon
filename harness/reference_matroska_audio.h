/* Own reference tooling: Matroska audio startup and forward clocks through public
 * FFmpeg APIs, the same operation as playback-worker's mkv-audio modes.
 *
 * A preloaded seekable input read in 4 KiB windows, stream discovery with the stock
 * parsers, then every packet through the end of file. Each packet of a selected
 * AAC, AC-3 or E-AC-3 stream is folded with its stream index, PTS, DTS and duration
 * in nanoseconds, key flag and payload. SELECT is aac, dolby (AC-3 and E-AC-3) or
 * all. */

#define MKV_AUDIO_QUANTUM 4096

static int mkv_audio_selected(const AVStream *stream, int aac, int dolby)
{
    const AVCodecParameters *codec = stream->codecpar;
    if (codec->codec_type != AVMEDIA_TYPE_AUDIO)
        return 0;
    if (codec->codec_id == AV_CODEC_ID_AAC)
        return aac;
    return dolby && (codec->codec_id == AV_CODEC_ID_AC3 || codec->codec_id == AV_CODEC_ID_EAC3);
}

static int mkv_audio_operation(const uint8_t *data, size_t size, const char *select, PacketWork *work)
{
    const int all = !strcmp(select, "all");
    const int aac = all || !strcmp(select, "aac"), dolby = all || !strcmp(select, "dolby");
    if (!aac && !dolby)
        return AVERROR(EINVAL);
    PacketInput in = {data, (int64_t)size, 0, MKV_AUDIO_QUANTUM};
    AVFormatContext *format = NULL;
    AVPacket *packet = NULL;
    int rc = packet_open(&format, &in, 1, av_find_input_format("matroska"), 0);
    if (rc < 0)
        goto done;
    rc = avformat_find_stream_info(format, NULL);
    if (rc < 0)
        goto done;
    int selected = 0;
    for (unsigned i = 0; i < format->nb_streams; ++i)
        selected |= mkv_audio_selected(format->streams[i], aac, dolby);
    if (!selected) {
        rc = AVERROR_STREAM_NOT_FOUND;
        goto done;
    }
    packet = av_packet_alloc();
    if (!packet) {
        rc = AVERROR(ENOMEM);
        goto done;
    }
    while ((rc = av_read_frame(format, packet)) >= 0) {
        const AVStream *stream = format->streams[packet->stream_index];
        if (mkv_audio_selected(stream, aac, dolby)) {
            const AVRational base = stream->time_base;
            packet_fold(work, (uint64_t)packet->stream_index, ns(packet->pts, base), ns(packet->dts, base),
                        packet->duration > 0 ? (uint64_t)ns(packet->duration, base) : 0,
                        !!(packet->flags & AV_PKT_FLAG_KEY), packet->data, (size_t)packet->size);
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

static int mkv_audio_main(int argc, char **argv, const uint8_t *data, long size)
{
    return packet_main(argc, argv, data, size, "mkv-audio-", "matroska-audio-clock", mkv_audio_operation);
}
