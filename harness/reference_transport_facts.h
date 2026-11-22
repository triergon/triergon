/* Compare the recovery projection to packets published by libavformat. */
static void transport_fact_header(const ViewCase *input, ViewWork *work, const AVPacket *packet)
{
    if (packet->pos < 0 || (uint64_t)packet->pos > input->size ||
        input->size - (size_t)packet->pos < 188)
        return;
    const size_t first = (size_t)packet->pos;
    const unsigned pid = ((unsigned)input->data[first + 1] & 31) << 8 | input->data[first + 2];
    uint8_t header[9 + 255];
    size_t used = 0, length = 9;
    for (size_t offset = first; offset + 188 <= input->size && used < length; offset += 188) {
        const uint8_t *carrier = input->data + offset;
        if (carrier[0] != 0x47)
            return;
        if ((((unsigned)carrier[1] & 31) << 8 | carrier[2]) != pid || !(carrier[3] & 16))
            continue;
        if (!!(carrier[1] & 64) != (offset == first))
            return;
        size_t start = carrier[3] & 32 ? 5u + carrier[4] : 4u;
        while (start < 188 && used < length) {
            header[used++] = carrier[start++];
            if (used == 9)
                length = 9u + header[8];
        }
    }
    if (used != length || header[0] || header[1] || header[2] != 1)
        return;
    unsigned flags = header[7] >> 6;
    int present = flags >= 2 && length >= 14;
    uint64_t pts = 0;
    if (present) {
        const uint8_t *p = header + 9;
        pts = ((uint64_t)(p[0] & 14) << 29) | ((uint64_t)p[1] << 22) |
              ((uint64_t)(p[2] & 254) << 14) | ((uint64_t)p[3] << 7) | (p[4] >> 1);
    }
    const uint64_t words[] = {flags, (uint64_t)present, pts, length};
    transport_view_row(work, 3, words, 4, header, length);
}

static int transport_recovery_trace(const ViewCase *input, ViewWork *work)
{
    const int pid = atoi(input->argv[1]);
    const int opaque = !strcmp(input->argv[2], "opaque");
    TransportViewInput source;
    int status = transport_view_open(&source, input, 32768, "mpegts");
    AVPacket *packet = av_packet_alloc();
    if (status >= 0 && !packet)
        status = AVERROR(ENOMEM);
    if (status >= 0) {
        if (opaque)
            source.format->flags &= ~AVFMT_FLAG_NOPARSE;
        while ((status = av_read_frame(source.format, packet)) >= 0) {
            const AVStream *stream = source.format->streams[packet->stream_index];
            if (stream->id == pid) {
                if (!strcmp(input->argv[2], "faults"))
                    transport_fact_header(input, work, packet);
                const uint64_t words[] = {
                    (uint64_t)pid, (uint64_t)packet->pts, (uint64_t)packet->dts,
                    (uint64_t)packet->pos, !!(packet->flags & AV_PKT_FLAG_CORRUPT),
                    (uint64_t)packet->size,
                };
                transport_view_row(work, 1, words, 6, packet->data, (size_t)packet->size);
            }
            av_packet_unref(packet);
        }
    }
    if (status == AVERROR_EOF)
        transport_view_row(work, 7, NULL, 0, NULL, 0);
    else if (status != AVERROR(ENOMEM))
        transport_view_row(work, 8, NULL, 0, NULL, 0);
    av_packet_free(&packet);
    transport_view_close(&source);
    return status == AVERROR(ENOMEM) ? 2 : 0;
}

static int transport_facts_prepare(ViewCase *input)
{
    if (input->argc != 3)
        return 2;
    input->name = !strcmp(input->prefix, "ts-facts-") ? "ts-fault-facts" : "ts-recovery-trace";
    input->run = transport_recovery_trace;
    return 0;
}
