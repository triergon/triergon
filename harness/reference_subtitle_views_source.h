/* Public FFmpeg subtitle source views. */
static int subtitle_view_vobsub(const ViewCase *input, ViewWork *work)
{
    SubtitleViewDemux demux = {0};
    AVDictionary *options = NULL;
    if (av_dict_set(&options, "sub_name", input->argv[1], 0) < 0)
        return 2;
    const int opened = subtitle_view_open(&demux, input, input->argv[0], &options);
    av_dict_free(&options);
    if (opened < 0) {
        subtitle_view_refusal(work);
        subtitle_view_close(&demux);
        return 0;
    }
    uint64_t comments = 0, blank = 0;
    for (size_t start = 0; start < input->size;) {
        size_t end = start;
        while (end < input->size && input->data[end] != '\n')
            ++end;
        size_t first = start;
        while (first < end && strchr(" \t\r\v\f", input->data[first]) && input->data[first])
            ++first;
        if (first == end)
            ++blank;
        else if (input->data[start] == '#')
            ++comments;
        start = end + 1;
    }
    const uint64_t counts[] = {comments, blank};
    SUBTITLE_ROW(work, 3, counts, NULL, 0);
    for (unsigned i = 0; i < demux.format->nb_streams; ++i) {
        const AVStream *stream = demux.format->streams[i];
        const char *language = subtitle_view_tag(stream, "language");
        const uint64_t track[] = {i, stream->id, !!(stream->disposition & AV_DISPOSITION_DEFAULT),
                                  strlen(language)};
        SUBTITLE_ROW(work, 2, track, (const uint8_t *)language, track[3]);
        const uint64_t header[] = {i, (uint64_t)stream->codecpar->extradata_size};
        SUBTITLE_ROW(work, 2, header, stream->codecpar->extradata, header[1]);
    }
    AVPacket *packet = av_packet_alloc();
    if (!packet) {
        subtitle_view_close(&demux);
        return 2;
    }
    int status;
    while ((status = av_read_frame(demux.format, packet)) >= 0) {
        const uint64_t words[] = {packet->stream_index, packet->pts, packet->pos};
        SUBTITLE_ROW(work, 1, words, NULL, 0);
        av_packet_unref(packet);
    }
    if (status == AVERROR_EOF)
        subtitle_view_row(work, 7, NULL, 0, NULL, 0);
    else
        subtitle_view_refusal(work);
    av_packet_free(&packet);
    subtitle_view_close(&demux);
    return 0;
}

/* The packet's DTS as its source codes it, which the candidate reports. A TS
 * packet keeps DTS only when the PES starting at its carrier has PTS_DTS_flags
 * 11 (H.222.0 §2.4.3.7); a Matroska block carries one timestamp and no DTS
 * (RFC 9559 §§10.1, 10.2 and 11.2). Returns -1 when the packet does not start
 * a PES of its PID on the TS188 grid. */
static int subtitle_view_dvb_dts(const ViewCase *input, int matroska, const AVStream *stream,
                                 const AVPacket *packet, int64_t *dts)
{
    *dts = AV_NOPTS_VALUE;
    if (matroska)
        return 0;
    if (packet->pos < 0 || packet->pos % 188 || (uint64_t)packet->pos >= input->size)
        return -1;
    /* A packet the demuxer cuts from a PES continuation takes that PES's header. */
    size_t start = (size_t)packet->pos;
    for (;;) {
        const uint8_t *ts = input->data + start;
        if (ts[0] == 0x47 && (ts[1] & 0x40) &&
            (((unsigned)ts[1] & 0x1f) << 8 | ts[2]) == (unsigned)stream->id)
            break;
        if (start < 188)
            return -1;
        start -= 188;
    }
    uint8_t header[8];
    size_t have = 0;
    int started = 0;
    for (size_t at = start; input->size - at >= 188; at += 188) {
        const uint8_t *ts = input->data + at;
        if (ts[0] != 0x47)
            return -1;
        if ((((unsigned)ts[1] & 0x1f) << 8 | ts[2]) != (unsigned)stream->id) {
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
        if (have < sizeof(header))
            continue;
        /* PTS is compared as the demuxer reports it, including a PTS it carries over
         * to a PES that codes none; only DTS presence is projected. */
        const int flags = header[7] >> 6;
        if (header[0] || header[1] || header[2] != 1 || flags == 1 ||
            (flags == 3 && packet->dts == AV_NOPTS_VALUE))
            return -1;
        if (flags == 3)
            *dts = packet->dts;
        return 0;
    }
    return -1;
}

static int subtitle_view_dvb(const ViewCase *input, ViewWork *work)
{
    SubtitleViewDemux demux = {0};
    const char *file = !strcmp(input->argv[0], "mkv") ? "input.mkv" : "input.ts";
    if (subtitle_view_open(&demux, input, file, NULL) < 0) {
        subtitle_view_refusal(work);
        subtitle_view_close(&demux);
        return 0;
    }
    AVPacket *packet = av_packet_alloc();
    if (!packet) {
        subtitle_view_close(&demux);
        return 2;
    }
    unsigned count = 0;
    int track = -1;
    int status;
    do {
        for (unsigned i = 0; track < 0 && i < demux.format->nb_streams; ++i) {
            const AVStream *stream = demux.format->streams[i];
            if (stream->codecpar->codec_id != AV_CODEC_ID_DVB_SUBTITLE)
                continue;
            track = (int)i;
            const char *language = subtitle_view_tag(stream, "language");
            const uint64_t config[] = {0, stream->time_base.num, stream->time_base.den,
                                       (uint64_t)stream->codecpar->extradata_size};
            SUBTITLE_ROW(work, 2, config, stream->codecpar->extradata, config[3]);
            const uint64_t label[] = {1, strlen(language)};
            SUBTITLE_ROW(work, 2, label, (const uint8_t *)language, label[1]);
        }
        if (track >= 0 && packet->stream_index == track && packet->data) {
            int64_t dts;
            if (subtitle_view_dvb_dts(input, !strcmp(input->argv[0], "mkv"),
                                      demux.format->streams[track], packet, &dts) < 0) {
                av_packet_free(&packet);
                subtitle_view_close(&demux);
                return 2;
            }
            const uint64_t words[] = {
                count++, packet->pts != AV_NOPTS_VALUE,
                packet->pts == AV_NOPTS_VALUE ? 0 : packet->pts,
                dts != AV_NOPTS_VALUE, dts == AV_NOPTS_VALUE ? 0 : dts,
                packet->duration, (uint64_t)packet->size
            };
            SUBTITLE_ROW(work, 1, words, packet->data, words[6]);
        }
        av_packet_unref(packet);
    } while ((status = av_read_frame(demux.format, packet)) >= 0);
    if (track >= 0 && status == AVERROR_EOF)
        subtitle_view_row(work, 7, NULL, 0, NULL, 0);
    else
        subtitle_view_refusal(work);
    av_packet_free(&packet);
    subtitle_view_close(&demux);
    return 0;
}

static int subtitle_views_source_prepare(ViewCase *input)
{
    if (!strcmp(input->prefix, "vobsub-lines-") && input->argc == 2) {
        input->name = "vobsub-index";
        input->run = subtitle_view_vobsub;
        return 0;
    }
    if (!strcmp(input->prefix, "dvb-source-") && input->argc == 1 &&
        (!strcmp(input->argv[0], "mkv") || !strcmp(input->argv[0], "ts"))) {
        input->name = "dvb-source";
        input->run = subtitle_view_dvb;
        return 0;
    }
    fprintf(stderr, "usage: %s operation is unavailable\n", input->prefix);
    return 2;
}
