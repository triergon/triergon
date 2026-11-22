/* Raw AAC demuxing and the public ADTS-to-ASC bitstream filter. */
typedef struct {
    const uint8_t *data;
    size_t size, offset;
} AdtsInput;

static int adts_read(void *opaque, uint8_t *destination, int length)
{
    AdtsInput *source = opaque;
    size_t left = source->size - source->offset;
    if (!left)
        return AVERROR_EOF;
    size_t size = left < (size_t)length ? left : (size_t)length;
    memcpy(destination, source->data + source->offset, size);
    source->offset += size;
    return (int)size;
}

/* The source reader retains an in-band PCE, so use the public demuxer's
 * original payload in the channel-configuration-zero branch. The derived
 * two-byte ASC prefix describes the same channel configuration. */
static int adts_source_payload(ViewWork *work, const AVPacket *packet,
                               uint64_t position, uint64_t count)
{
    if (packet->size < 7)
        return 0;
    const uint8_t *data = packet->data;
    const unsigned channel = ((data[2] & 1) << 2) | (data[3] >> 6);
    const unsigned protection_absent = data[1] & 1;
    const int header_size = protection_absent ? 7 : 9;
    if (channel || packet->size < header_size || (data[6] & 3))
        return 0;
    const unsigned object = (data[2] >> 6) + 1;
    const unsigned frequency = (data[2] >> 2) & 15;
    if (!count) {
        const uint8_t asc[] = {
            (uint8_t)((object << 3) | (frequency >> 1)), (uint8_t)(frequency << 7),
        };
        const uint64_t words[] = {sizeof(asc)};
        stream_record(work, 2, words, 1, asc, sizeof(asc));
    }
    const size_t payload_size = (size_t)(packet->size - header_size);
    const uint64_t words[] = {
        position, (uint64_t)packet->size, object, frequency, channel,
        protection_absent, payload_size,
    };
    stream_record(work, 1, words, 7, data + header_size, payload_size);
    return 1;
}

static int adts_run(const ViewCase *input, ViewWork *work)
{
    AdtsInput source = {input->data, input->size, 0};
    enum { BUFFER_SIZE = 4096 };
    uint8_t *buffer = av_malloc(BUFFER_SIZE);
    AVFormatContext *format = avformat_alloc_context();
    const AVInputFormat *demuxer = av_find_input_format("aac");
    const AVBitStreamFilter *implementation = av_bsf_get_by_name("aac_adtstoasc");
    AVBSFContext *filter = NULL;
    AVPacket *packet = av_packet_alloc();
    AVIOContext *io = NULL;
    int result = 2;
    uint64_t offset = 0, count = 0;
    unsigned object = 0, frequency = 0, channel = 0;
    if (!buffer || !format || !demuxer || !implementation || !packet)
        goto done;
    io = avio_alloc_context(buffer, BUFFER_SIZE, 0, &source, adts_read, NULL, NULL);
    if (!io)
        goto done;
    buffer = NULL;
    format->pb = io;
    format->flags |= AVFMT_FLAG_CUSTOM_IO;
    int status = avformat_open_input(&format, NULL, demuxer, NULL);
    if (status < 0)
        goto complete;
    if (format->nb_streams != 1 ||
        av_bsf_alloc(implementation, &filter) < 0 ||
        avcodec_parameters_copy(filter->par_in, format->streams[0]->codecpar) < 0 ||
        av_bsf_init(filter) < 0)
        goto done;
    for (;;) {
        status = av_read_frame(format, packet);
        if (status < 0)
            break;
        const uint64_t position = packet->pos >= 0 ? (uint64_t)packet->pos : offset;
        const int size = packet->size;
        const unsigned protection_absent = size >= 2 ? packet->data[1] & 1 : 0;
        if (adts_source_payload(work, packet, position, count)) {
            offset = position + (uint64_t)size;
            ++count;
            av_packet_unref(packet);
            continue;
        }
        status = av_bsf_send_packet(filter, packet);
        if (status >= 0)
            status = av_bsf_receive_packet(filter, packet);
        if (status < 0) {
            offset = position;
            break;
        }
        size_t asc_size = 0;
        const uint8_t *asc = av_packet_get_side_data(packet, AV_PKT_DATA_NEW_EXTRADATA, &asc_size);
        if (!count && !asc && filter->par_out->extradata_size > 0) {
            asc = filter->par_out->extradata;
            asc_size = (size_t)filter->par_out->extradata_size;
        }
        if (asc && asc_size >= 2) {
            object = asc[0] >> 3;
            frequency = ((asc[0] & 7) << 1) | (asc[1] >> 7);
            channel = (asc[1] >> 3) & 15;
        }
        if (!count || asc) {
            const uint64_t words[] = {asc_size};
            stream_record(work, 2, words, 1, asc, asc_size);
        }
        const uint64_t words[] = {
            position, (uint64_t)size, object, frequency, channel, protection_absent,
            (uint64_t)packet->size
        };
        stream_record(work, 1, words, 7, packet->data, (size_t)packet->size);
        offset = position + (uint64_t)size;
        ++count;
        av_packet_unref(packet);
    }
complete:
    if (status == AVERROR(ENOMEM))
        goto done;
    if (status == AVERROR_EOF && count && offset == input->size) {
        stream_record(work, 7, &count, 1, NULL, 0);
    } else {
        if (work->trace)
            fprintf(stderr, "ADTS offset %" PRIu64 ": %d\n", offset, status);
        stream_record(work, 8, &offset, 1, NULL, 0);
    }
    result = 0;
done:
    av_packet_free(&packet);
    av_bsf_free(&filter);
    avformat_close_input(&format);
    if (io) {
        av_freep(&io->buffer);
        avio_context_free(&io);
    }
    av_free(buffer);
    return result;
}

static int adts_prepare(ViewCase *input)
{
    if (input->argc)
        return 2;
    input->name = "aac-adts-frames";
    input->run = adts_run;
    return 0;
}
