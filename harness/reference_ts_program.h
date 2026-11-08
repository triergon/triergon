/* Own reference tooling: one TS188 or M2TS192 program through public FFmpeg APIs,
 * the same operation as playback-worker's ts-program modes.
 *
 * A preloaded input read in 32 KiB windows through the mpegts demuxer without
 * parsers or timestamp fill-in, every packet through the end of the input. Each
 * packet of a selected PID is folded with its PID, PTS, DTS and duration in 90 kHz
 * ticks, packet flags and payload. SELECTION is FORMAT:PID=CARRIAGE,... as in the
 * worker; the reference uses the format and PIDs, and the demuxer resolves the
 * carriage from the program map. TS188 input is seekable, M2TS192 forward-only. */

#define TS_PROGRAM_QUANTUM (32 * 1024)
#define TS_PROGRAM_MAX_PIDS 64

typedef struct {
    int m2ts;
    unsigned count;
    int pids[TS_PROGRAM_MAX_PIDS];
} TsProgramSelection;

static int ts_program_parse(const char *argument, TsProgramSelection *selection)
{
    const char *list;
    if (!strncmp(argument, "ts188:", 6)) {
        selection->m2ts = 0;
        list = argument + 6;
    } else if (!strncmp(argument, "m2ts192:", 8)) {
        selection->m2ts = 1;
        list = argument + 8;
    } else {
        return AVERROR(EINVAL);
    }
    selection->count = 0;
    while (*list) {
        char *end = NULL;
        long pid = strtol(list, &end, 10);
        if (end == list || *end != '=' || pid < 1 || pid > 0x1ffe || selection->count == TS_PROGRAM_MAX_PIDS)
            return AVERROR(EINVAL);
        selection->pids[selection->count++] = (int)pid;
        const char *next = strchr(end, ',');
        if (next == end + 1)
            return AVERROR(EINVAL);
        list = next ? next + 1 : end + strlen(end);
    }
    return selection->count ? 0 : AVERROR(EINVAL);
}

static int ts_program_selected(const TsProgramSelection *selection, const AVStream *stream)
{
    for (unsigned i = 0; i < selection->count; ++i)
        if (selection->pids[i] == stream->id)
            return 1;
    return 0;
}

static int ts_program_operation(const uint8_t *data, size_t size, const char *argument, PacketWork *work)
{
    TsProgramSelection selection;
    int rc = ts_program_parse(argument, &selection);
    if (rc < 0)
        return rc;
    PacketInput in = {data, (int64_t)size, 0, TS_PROGRAM_QUANTUM};
    AVFormatContext *format = NULL;
    AVPacket *packet = NULL;
    rc = packet_open(&format, &in, !selection.m2ts, av_find_input_format("mpegts"),
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
        if (ts_program_selected(&selection, stream)) {
            if (stream->time_base.num != 1 || stream->time_base.den != 90000) {
                rc = AVERROR_INVALIDDATA;
                goto done;
            }
            packet_fold(work, (uint64_t)stream->id,
                        packet->pts == AV_NOPTS_VALUE ? INT64_MIN : packet->pts,
                        packet->dts == AV_NOPTS_VALUE ? INT64_MIN : packet->dts,
                        packet->duration > 0 ? (uint64_t)packet->duration : 0,
                        (uint64_t)packet->flags, packet->data, (size_t)packet->size);
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

static int ts_program_main(int argc, char **argv, const uint8_t *data, long size)
{
    return packet_main(argc, argv, data, size, "ts-program-", "ts-native-program", ts_program_operation);
}
