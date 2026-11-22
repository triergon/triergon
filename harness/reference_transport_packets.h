/* The packet data and physical positions published by MPEG-TS raw demux. */
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
    if (input->argc != 2 ||
        (strcmp(input->argv[0], "188") && strcmp(input->argv[0], "192")) ||
        (strcmp(input->argv[1], "1") && strcmp(input->argv[1], "7") &&
         strcmp(input->argv[1], "4096")))
        return 2;
    input->name = "ts-packets";
    input->run = transport_packets_run;
    return 0;
}
