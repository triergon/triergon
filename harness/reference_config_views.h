/* Configuration exposed by the MOV demuxer, AAC/E-AC-3 decoders and HEVC parser. */
#include <libavutil/downmix_info.h>
#include <libavutil/pixdesc.h>

typedef struct {
    AVFormatContext *format;
    AVIOContext *io;
    AdtsInput input;
} ConfigDemux;

static int64_t config_seek(void *opaque, int64_t offset, int whence)
{
    AdtsInput *input = opaque;
    if (whence & AVSEEK_SIZE)
        return (int64_t)input->size;
    whence &= ~AVSEEK_FORCE;
    int64_t base;
    switch (whence) {
    case SEEK_SET: base = 0; break;
    case SEEK_CUR: base = (int64_t)input->offset; break;
    case SEEK_END: base = (int64_t)input->size; break;
    default: return AVERROR(EINVAL);
    }
    if (offset < -base || offset > (int64_t)input->size - base)
        return AVERROR(EINVAL);
    input->offset = (size_t)(base + offset);
    return (int64_t)input->offset;
}

static int config_demux_open(ConfigDemux *demux, const char *name,
                             const uint8_t *data, size_t size)
{
    demux->input = (AdtsInput){data, size, 0};
    uint8_t *buffer = av_malloc(4096);
    if (!buffer)
        return AVERROR(ENOMEM);
    demux->io = avio_alloc_context(buffer, 4096, 0, &demux->input,
                                  adts_read, NULL, config_seek);
    if (!demux->io) {
        av_free(buffer);
        return AVERROR(ENOMEM);
    }
    const AVInputFormat *implementation = av_find_input_format(name);
    demux->format = avformat_alloc_context();
    if (!implementation)
        return AVERROR_DEMUXER_NOT_FOUND;
    if (!demux->format)
        return AVERROR(ENOMEM);
    demux->format->pb = demux->io;
    demux->format->flags |= AVFMT_FLAG_CUSTOM_IO;
    return avformat_open_input(&demux->format, NULL, implementation, NULL);
}

static void config_demux_close(ConfigDemux *demux)
{
    avformat_close_input(&demux->format);
    if (demux->io) {
        av_freep(&demux->io->buffer);
        avio_context_free(&demux->io);
    }
}

static void config_words(ViewWork *work, uint64_t kind, const uint64_t *words, size_t count)
{
    stream_record(work, kind, words, count, NULL, 0);
}

typedef struct {
    uint8_t *data;
    size_t used;
} ConfigBoxes;

static void config_put(ConfigBoxes *boxes, const void *data, size_t size)
{
    if (size)
        memcpy(boxes->data + boxes->used, data, size);
    boxes->used += size;
}

static void config_u32(ConfigBoxes *boxes, uint32_t value)
{
    const uint8_t bytes[] = {value >> 24, value >> 16, value >> 8, value};
    config_put(boxes, bytes, sizeof(bytes));
}

static size_t config_box(ConfigBoxes *boxes, const char *type)
{
    size_t start = boxes->used;
    config_u32(boxes, 0);
    config_put(boxes, type, 4);
    return start;
}

static void config_end_box(ConfigBoxes *boxes, size_t start)
{
    ConfigBoxes header = {boxes->data, start};
    config_u32(&header, (uint32_t)(boxes->used - start));
}

static void config_zeros(ConfigBoxes *boxes, size_t size)
{
    memset(boxes->data + boxes->used, 0, size);
    boxes->used += size;
}

/* Fixed four-byte descriptor lengths keep the physical and declared sizes separate. */
static void config_descriptor(ConfigBoxes *boxes, uint8_t tag, size_t length)
{
    const uint8_t header[] = {
        tag, (length >> 21) | 128, (length >> 14) | 128,
        (length >> 7) | 128, length & 127,
    };
    config_put(boxes, header, sizeof(header));
}

static void config_esds(ConfigBoxes *boxes, const ViewCase *input, size_t declared)
{
    size_t esds = config_box(boxes, "esds");
    config_u32(boxes, 0);
    config_descriptor(boxes, 3, 3 + 5 + 13 + 5 + input->size);
    config_put(boxes, (const uint8_t[]){0, 1, 0}, 3);
    config_descriptor(boxes, 4, 13 + 5 + input->size);
    config_put(boxes, (const uint8_t[]){0x40, 0x15}, 2);
    config_zeros(boxes, 11);
    config_descriptor(boxes, 5, declared);
    config_put(boxes, input->data, input->size);
    config_end_box(boxes, esds);
}

static ConfigBoxes wrap_mp4(const ViewCase *input, int audio, size_t declared)
{
    /* All authoring overhead is under 512 bytes; payload length is bounded by prepare. */
    ConfigBoxes boxes = {malloc(input->size + 512), 0};
    if (!boxes.data)
        return boxes;

    size_t ftyp = config_box(&boxes, "ftyp");
    config_put(&boxes, "isom\0\0\0\0isom", 12);
    config_end_box(&boxes, ftyp);
    size_t moov = config_box(&boxes, "moov");
    size_t trak = config_box(&boxes, "trak");
    size_t tkhd = config_box(&boxes, "tkhd");
    config_u32(&boxes, 7);
    config_zeros(&boxes, 8);
    config_u32(&boxes, 1);
    config_zeros(&boxes, 68);
    config_end_box(&boxes, tkhd);
    size_t mdia = config_box(&boxes, "mdia");
    size_t mdhd = config_box(&boxes, "mdhd");
    config_zeros(&boxes, 12);
    config_u32(&boxes, audio ? 48000 : 1000);
    config_zeros(&boxes, 8);
    config_end_box(&boxes, mdhd);
    size_t hdlr = config_box(&boxes, "hdlr");
    config_zeros(&boxes, 8);
    config_put(&boxes, audio ? "soun" : "vide", 4);
    config_zeros(&boxes, 13);
    config_end_box(&boxes, hdlr);
    size_t minf = config_box(&boxes, "minf");
    size_t stbl = config_box(&boxes, "stbl");
    size_t stsd = config_box(&boxes, "stsd");
    config_u32(&boxes, 0);
    config_u32(&boxes, 1);
    size_t entry = config_box(&boxes, audio ? "mp4a" : "vp09");
    config_put(&boxes, (const uint8_t[]){0, 0, 0, 0, 0, 0, 0, 1}, 8);
    if (audio) {
        config_zeros(&boxes, 8);
        config_put(&boxes, (const uint8_t[]){0, 2, 0, 16}, 4);
        config_u32(&boxes, 0);
        config_u32(&boxes, 48000U << 16);
        config_esds(&boxes, input, declared);
    } else {
        config_zeros(&boxes, 16);
        config_put(&boxes, (const uint8_t[]){0, 2, 0, 2}, 4);
        config_u32(&boxes, 72U << 16);
        config_u32(&boxes, 72U << 16);
        config_u32(&boxes, 0);
        config_put(&boxes, (const uint8_t[]){0, 1}, 2);
        config_zeros(&boxes, 32);
        config_put(&boxes, (const uint8_t[]){0, 24, 255, 255}, 4);
        size_t config = config_box(&boxes, "vpcC");
        config_put(&boxes, input->data, input->size);
        config_end_box(&boxes, config);
    }
    config_end_box(&boxes, entry);
    config_end_box(&boxes, stsd);
    const char *table_names[] = {"stts", "stsc", "stsz", "stco"};
    for (size_t i = 0; i < 4; ++i) {
        size_t table = config_box(&boxes, table_names[i]);
        config_zeros(&boxes, i == 2 ? 12 : 8);
        config_end_box(&boxes, table);
    }
    config_end_box(&boxes, stbl);
    config_end_box(&boxes, minf);
    config_end_box(&boxes, mdia);
    config_end_box(&boxes, trak);
    config_end_box(&boxes, moov);
    return boxes;
}

static int vpcc_main(const ViewCase *input, ViewWork *work)
{
    ConfigBoxes movie = wrap_mp4(input, 0, 0);
    if (!movie.data)
        return 2;
    ConfigDemux demux = {0};
    int status = config_demux_open(&demux, "mov", movie.data, movie.used);
    if (status >= 0 && demux.format->nb_streams == 1) {
        const AVCodecParameters *par = demux.format->streams[0]->codecpar;
        if (par->codec_id != AV_CODEC_ID_VP9) {
            status = AVERROR_BUG;
        } else {
            const uint64_t words[] = {
                par->color_range, par->color_primaries, par->color_trc, par->color_space,
            };
            config_words(work, 3, words, 4);
        }
    } else if (status >= 0) {
        status = AVERROR_BUG;
    }
    if (status < 0 && status != AVERROR(ENOMEM) && status != AVERROR_BUG &&
        status != AVERROR_DEMUXER_NOT_FOUND)
        config_words(work, 8, NULL, 0);
    config_demux_close(&demux);
    free(movie.data);
    return status == AVERROR(ENOMEM) || status == AVERROR_BUG ||
        status == AVERROR_DEMUXER_NOT_FOUND ? 2 : 0;
}

static int hvcc_written_main(const ViewCase *input, ViewWork *work)
{
    ConfigDemux demux = {0};
    AVCodecContext *context = avcodec_alloc_context3(NULL);
    AVCodecParserContext *parser = av_parser_init(AV_CODEC_ID_HEVC);
    AVPacket *packet = av_packet_alloc();
    int result = 2;
    if (!context || !parser || !packet ||
        config_demux_open(&demux, "mov", input->data, input->size) < 0 ||
        demux.format->nb_streams != 1 ||
        avcodec_parameters_to_context(context, demux.format->streams[0]->codecpar) < 0 ||
        context->codec_id != AV_CODEC_ID_HEVC ||
        av_read_frame(demux.format, packet) < 0)
        goto done;
    parser->flags |= PARSER_FLAG_COMPLETE_FRAMES;
    uint8_t *parsed = NULL;
    int length = 0;
    if (av_parser_parse2(parser, context, &parsed, &length, packet->data, packet->size,
                         packet->pts, packet->dts, packet->pos) < 0 || length <= 0)
        goto done;
    const AVPixFmtDescriptor *format = av_pix_fmt_desc_get(parser->format);
    if (!format || !format->nb_components || (format->flags & AV_PIX_FMT_FLAG_RGB))
        goto done;
    unsigned chroma = format->nb_components == 1 ? 0 :
                      format->log2_chroma_h ? 1 : format->log2_chroma_w ? 2 : 3;
    if (format->comp[0].depth < 8 || (chroma && format->comp[1].depth < 8))
        goto done;
    const uint64_t words[] = {
        (uint64_t)context->profile, (uint64_t)context->level, chroma,
        (uint64_t)(format->comp[0].depth - 8),
        chroma ? (uint64_t)(format->comp[1].depth - 8) : 0,
    };
    config_words(work, 3, words, 5);
    result = 0;
done:
    av_packet_free(&packet);
    if (parser)
        av_parser_close(parser);
    avcodec_free_context(&context);
    config_demux_close(&demux);
    return result;
}

static int config_decoder_open(AVCodecContext **decoder, const char *name,
                               const uint8_t *data, size_t bits)
{
    const AVCodec *implementation = avcodec_find_decoder_by_name(name);
    if (!implementation)
        return AVERROR_DECODER_NOT_FOUND;
    *decoder = avcodec_alloc_context3(implementation);
    if (!*decoder)
        return AVERROR(ENOMEM);
    size_t size = (bits + 7) / 8;
    if (size) {
        uint8_t *extra = av_mallocz(size + AV_INPUT_BUFFER_PADDING_SIZE);
        if (!extra)
            return AVERROR(ENOMEM);
        memcpy(extra, data, size);
        if (bits % 8)
            extra[size - 1] &= (uint8_t)(255 << (8 - bits % 8));
        (*decoder)->extradata = extra;
        (*decoder)->extradata_size = (int)size;
    }
    return avcodec_open2(*decoder, implementation, NULL);
}

static int asc_main(const ViewCase *input, ViewWork *work)
{
    uint64_t declared = strtoul(input->argv[1], NULL, 10);
    uint64_t physical = input->size * UINT64_C(8);
    const uint64_t header[] = {declared, physical, declared == physical};
    config_words(work, 2, header, 3);
    for (uint64_t i = 0; i < 2; ++i) {
        uint64_t bits = i ? physical : declared;
        AVCodecContext *decoder = NULL;
        int status = bits <= physical
            ? config_decoder_open(&decoder, "aac", input->data, (size_t)bits)
            : AVERROR_INVALIDDATA;
        if (status == AVERROR(ENOMEM) || status == AVERROR_DECODER_NOT_FOUND) {
            avcodec_free_context(&decoder);
            return 2;
        }
        if (status < 0 || (!bits && decoder->ch_layout.nb_channels == 0)) {
            config_words(work, 8, &i, 1);
        } else {
            const uint64_t words[] = {i, (uint64_t)decoder->ch_layout.nb_channels};
            config_words(work, 3, words, 2);
            if (work->trace)
                fprintf(stderr, "ASC open: sample_rate=%d profile=%d frame_size=%d\n",
                        decoder->sample_rate, decoder->profile, decoder->frame_size);
        }
        avcodec_free_context(&decoder);
    }
    return 0;
}

static int asc_mov_main(const ViewCase *input, ViewWork *work)
{
    size_t bits = strtoul(input->argv[1], NULL, 10);
    ConfigBoxes movie = wrap_mp4(input, 1, bits / 8);
    if (!movie.data)
        return 2;
    ConfigDemux demux = {0};
    int status = config_demux_open(&demux, "mov", movie.data, movie.used);
    if (status >= 0 && demux.format->nb_streams == 1) {
        const AVCodecParameters *par = demux.format->streams[0]->codecpar;
        if (par->codec_id != AV_CODEC_ID_AAC) {
            status = AVERROR_BUG;
        } else {
            const uint64_t size = (uint64_t)par->extradata_size;
            stream_record(work, 2, &size, 1, par->extradata, (size_t)size);
            const uint64_t facts[] = {
                (uint64_t)par->sample_rate, (uint64_t)par->ch_layout.nb_channels,
                (uint64_t)par->frame_size,
            };
            config_words(work, 3, facts, 3);
        }
    } else if (status >= 0) {
        status = AVERROR_BUG;
    }
    if (status < 0 && status != AVERROR(ENOMEM) && status != AVERROR_BUG &&
        status != AVERROR_DEMUXER_NOT_FOUND)
        config_words(work, 8, NULL, 0);
    config_demux_close(&demux);
    free(movie.data);
    return status == AVERROR(ENOMEM) || status == AVERROR_BUG ||
        status == AVERROR_DEMUXER_NOT_FOUND ? 2 : 0;
}

static int asc_frame_main(const ViewCase *input, ViewWork *work)
{
    const char *hex = input->argv[1];
    uint8_t raw[32];
    size_t bytes = strlen(hex) / 2;
    for (size_t i = 0; i < bytes; ++i) {
        char pair[] = {hex[i * 2], hex[i * 2 + 1], 0};
        raw[i] = (uint8_t)strtoul(pair, NULL, 16);
    }
    const uint8_t *data = input->data;
    size_t header = (data[1] & 1) ? 7 : 9;
    size_t size = ((size_t)(data[3] & 3) << 11) | ((size_t)data[4] << 3) | (data[5] >> 5);
    if (data[0] != 255 || (data[1] & 0xf6) != 0xf0 ||
        size < header || size > input->size || (data[6] & 3))
        return 2;
    AVCodecContext *decoder = NULL;
    AVPacket *packet = av_packet_alloc();
    AVFrame *frame = av_frame_alloc();
    int result = 2;
    if (!packet || !frame || av_new_packet(packet, (int)(size - header)) < 0)
        goto done;
    memcpy(packet->data, data + header, size - header);
    int status = config_decoder_open(&decoder, "aac", raw, bytes * 8);
    if (status >= 0)
        status = avcodec_send_packet(decoder, packet);
    if (status >= 0)
        status = avcodec_receive_frame(decoder, frame);
    if (status >= 0) {
        static const unsigned aac_rates[] = {
            96000, 88200, 64000, 48000, 44100, 32000, 24000,
            22050, 16000, 12000, 11025, 8000, 7350,
        };
        unsigned rate = (unsigned)frame->sample_rate;
        unsigned samples = (unsigned)frame->nb_samples;
        if (work->trace)
            fprintf(stderr, "AAC decoded frame: %u Hz, %u samples\n", rate, samples);
        /* The LC ASC alone predicts the core clock. In-band dual-rate SBR
         * doubles rate and samples while leaving the decoded duration intact.
         * Keep explicitly signaled extension configurations in their own clock. */
        if (bytes == 2 && (raw[0] >> 3) == 2) {
            const unsigned index = ((raw[0] & 7) << 1) | (raw[1] >> 7);
            if (index < sizeof(aac_rates) / sizeof(*aac_rates) &&
                rate == 2 * aac_rates[index] && !(samples & 1)) {
                rate /= 2;
                samples /= 2;
            }
        }
        const uint64_t words[] = {
            rate, (uint64_t)frame->ch_layout.nb_channels, samples,
        };
        config_words(work, 3, words, 3);
    } else if (status == AVERROR(ENOMEM) || status == AVERROR_DECODER_NOT_FOUND) {
        goto done;
    } else {
        config_words(work, 8, NULL, 0);
    }
    result = 0;
done:
    av_frame_free(&frame);
    av_packet_free(&packet);
    avcodec_free_context(&decoder);
    return result;
}

static uint64_t config_gain(double gain)
{
    return gain > 0.0 ? (uint64_t)(gain * 100000.0 + 0.5) : 0;
}

static int eac3_views_main(const ViewCase *input, ViewWork *work)
{
    ConfigDemux demux = {0};
    AVCodecContext *decoder = NULL;
    AVPacket *packet = av_packet_alloc();
    AVFrame *frame = av_frame_alloc();
    uint64_t count = 0, offset = 0;
    int result = 2;
    if (!packet || !frame ||
        config_demux_open(&demux, "eac3", input->data, input->size) < 0 ||
        config_decoder_open(&decoder, "eac3", NULL, 0) < 0)
        goto done;
    int status;
    while ((status = av_read_frame(demux.format, packet)) >= 0) {
        status = avcodec_send_packet(decoder, packet);
        if (status >= 0)
            status = avcodec_receive_frame(decoder, frame);
        if (status < 0)
            break;
        if (frame->ch_layout.order != AV_CHANNEL_ORDER_NATIVE)
            goto done;
        const AVFrameSideData *matrix = av_frame_get_side_data(frame, AV_FRAME_DATA_MATRIXENCODING);
        enum AVMatrixEncoding encoding = AV_MATRIX_ENCODING_NONE;
        if (matrix) {
            if (matrix->size != sizeof(encoding))
                goto done;
            memcpy(&encoding, matrix->data, sizeof(encoding));
        }
        const uint64_t position = packet->pos >= 0 ? (uint64_t)packet->pos : offset;
        const uint64_t words[] = {
            position, (uint64_t)packet->size, (uint64_t)frame->sample_rate,
            (uint64_t)frame->nb_samples, (uint64_t)frame->ch_layout.nb_channels,
            frame->ch_layout.u.mask, decoder->audio_service_type, encoding,
        };
        config_words(work, 1, words, 8);
        const AVFrameSideData *entry = av_frame_get_side_data(frame, AV_FRAME_DATA_DOWNMIX_INFO);
        if (entry) {
            AVDownmixInfo mix;
            if (entry->size != sizeof(mix))
                goto done;
            memcpy(&mix, entry->data, sizeof(mix));
            const uint64_t fields[] = {
                mix.preferred_downmix_type,
                config_gain(mix.center_mix_level), config_gain(mix.center_mix_level_ltrt),
                config_gain(mix.surround_mix_level), config_gain(mix.surround_mix_level_ltrt),
                config_gain(mix.lfe_mix_level),
            };
            config_words(work, 3, fields, 6);
        }
        offset = position + (uint64_t)packet->size;
        ++count;
        av_packet_unref(packet);
        av_frame_unref(frame);
    }
    if (status == AVERROR(ENOMEM))
        goto done;
    if (status == AVERROR_EOF && offset == input->size)
        config_words(work, 7, &count, 1);
    else {
        if (work->trace)
            fprintf(stderr, "E-AC-3 offset %" PRIu64 ": %d\n", offset, status);
        config_words(work, 8, &offset, 1);
    }
    result = 0;
done:
    av_frame_free(&frame);
    av_packet_free(&packet);
    avcodec_free_context(&decoder);
    config_demux_close(&demux);
    return result;
}

static int config_views_prepare(ViewCase *input)
{
    if (!input->argc)
        return 2;
    const char *name = input->argv[0];
    input->name = "codec-config-views";
    if (input->argc == 1) {
        if (!strcmp(name, "vpcc") && input->size <= 65547)
            input->run = vpcc_main;
        else if (!strcmp(name, "hvcc-written"))
            input->run = hvcc_written_main;
        else if (!strcmp(name, "eac3"))
            input->run = eac3_views_main;
    } else if (input->argc == 2 && input->size <= 4096 &&
               (!strcmp(name, "asc") || !strcmp(name, "asc-truncated") || !strcmp(name, "asc-mov"))) {
        const char *argument = input->argv[1];
        if (!*argument || strspn(argument, "0123456789") != strlen(argument) ||
            strtoul(argument, NULL, 10) > 32768)
            return 2;
        if (!strcmp(name, "asc") || !strcmp(name, "asc-truncated"))
            input->run = asc_main;
        else if (strtoul(argument, NULL, 10) % 8 == 0)
            input->run = asc_mov_main;
    } else if (input->argc == 2 && input->size >= 7 && !strcmp(name, "asc-frame")) {
        const char *argument = input->argv[1];
        size_t length = strlen(argument);
        if (!length || length % 2 || length > 64 ||
            strspn(argument, "0123456789abcdefABCDEF") != length)
            return 2;
        input->run = asc_frame_main;
    }
    return input->run ? 0 : 2;
}
