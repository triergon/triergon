/* The packet data and physical positions published by MPEG-TS raw demux. */
static int transport_packet_grid_run(const ViewCase *input, ViewWork *work)
{
    const int width = atoi(input->argv[0]);
    const int prefix = width - 188;
    size_t window = (size_t)strtoul(input->argv[1], NULL, 10);
    AVDictionary *options = NULL;
    if (av_dict_set_int(&options, "raw_packet_size", width, 0) < 0)
        return 2;
    TransportViewInput source;
    int status = transport_view_open_with_options(&source, input, window, "data", &options);
    if (av_dict_count(options))
        status = AVERROR_OPTION_NOT_FOUND;
    av_dict_free(&options);
    AVPacket *packet = av_packet_alloc();
    size_t offset = 0;
    uint8_t carrier[192];
    size_t filled = 0;
    if (!packet)
        status = AVERROR(ENOMEM);
    if (status >= 0) {
        while ((status = av_read_frame(source.format, packet)) >= 0) {
            if (packet->pos != (int64_t)offset || packet->size <= 0 ||
                (size_t)packet->size > input->size - offset) {
                status = AVERROR_INVALIDDATA;
                break;
            }
            for (size_t at = 0; at < (size_t)packet->size;) {
                size_t take = (size_t)width - filled;
                if (take > (size_t)packet->size - at)
                    take = (size_t)packet->size - at;
                memcpy(carrier + filled, packet->data + at, take);
                at += take;
                filled += take;
                if (filled == (size_t)width) {
                    const uint64_t words[] = {offset + at - filled + (size_t)prefix, 188};
                    transport_view_row(work, 6, words, 2, carrier + prefix, 188);
                    filled = 0;
                }
            }
            offset += (size_t)packet->size;
            av_packet_unref(packet);
        }
    }
    av_packet_free(&packet);
    transport_view_close(&source);
    if (status != AVERROR_EOF || offset != input->size) {
        fprintf(stderr, "raw data reference stopped with %d after %zu of %zu bytes\n",
                status, offset, input->size);
        return 2;
    }
    transport_view_row(work, 7, NULL, 0, NULL, 0);
    return 0;
}

static int transport_packets_run(const ViewCase *input, ViewWork *work)
{
    size_t window = (size_t)strtoul(input->argv[1], NULL, 10);
    TransportViewInput source;
    int status = transport_view_open(&source, input, window, "mpegtsraw");
    AVPacket *packet = NULL;
    const int opened = status >= 0;
    if (opened) {
        packet = av_packet_alloc();
        if (!packet)
            status = AVERROR(ENOMEM);
        else {
            while ((status = av_read_frame(source.format, packet)) >= 0) {
                /* The public pos is the end position of the returned TS bytes. */
                const uint64_t words[] = {(uint64_t)(packet->pos - packet->size),
                                          (uint64_t)packet->size};
                transport_view_row(work, 6, words, 2, packet->data, (size_t)packet->size);
                av_packet_unref(packet);
            }
        }
    }
    const int drained = opened && status == AVERROR_EOF;
    transport_view_row(work, drained ? 7 : 8, NULL, 0, NULL, 0);
    if (!drained)
        fprintf(stderr, "raw reference stopped with %d\n", status);
    av_packet_free(&packet);
    transport_view_close(&source);
    return status == AVERROR(ENOMEM) || status == AVERROR_DEMUXER_NOT_FOUND ? 2 : 0;
}

static int transport_packets_prepare(ViewCase *input)
{
    if ((input->argc != 2 && (input->argc != 3 || strcmp(input->argv[2], "grid"))) ||
        (strcmp(input->argv[0], "188") && strcmp(input->argv[0], "192")) ||
        (strcmp(input->argv[1], "1") && strcmp(input->argv[1], "7") &&
         strcmp(input->argv[1], "4096")))
        return 2;
    input->name = "ts-packets";
    input->run = input->argc == 3 ? transport_packet_grid_run : transport_packets_run;
    return 0;
}
