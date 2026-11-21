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
            const uint64_t words[] = {
                count++, packet->pts != AV_NOPTS_VALUE,
                packet->pts == AV_NOPTS_VALUE ? 0 : packet->pts,
                packet->dts != AV_NOPTS_VALUE,
                packet->dts == AV_NOPTS_VALUE ? 0 : packet->dts,
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
