/* The AAC parser can emit skipped bytes as opaque packets. Only packets
 * beginning with a complete ADTS fixed and variable header carry frame facts. */
static int codec_adts_packet(AVCodecContext *context, const AVCodecParserContext *parser,
                             const uint8_t *data, size_t size, void *opaque)
{
    (void)context;
    ViewWork *work = opaque;
    if (size < 7 || data[0] != 0xff || (data[1] & 0xf0) != 0xf0)
        return 0;
    if (parser->frame_offset < 0)
        return 2;
    const unsigned length = ((data[3] & 3u) << 11) | ((unsigned)data[4] << 3) | (data[5] >> 5);
    const unsigned profile = data[2] >> 6;
    const unsigned rate = (data[2] >> 2) & 15u;
    const unsigned channels = ((data[2] & 1u) << 2) | (data[3] >> 6);
    const uint64_t words[] = {(uint64_t)parser->frame_offset, length, profile, rate, channels, size};
    view_record(work, 1, words, 6, data, size);
    if (work->trace) {
        uint64_t digest = 0;
        fold_bytes(&digest, data, size);
        printf("1 %" PRId64 " %u %u %u %u %zu %" PRIu64 "\n",
               parser->frame_offset, length, profile, rate, channels, size, digest);
    }
    return 0;
}

static int codec_adts_run(const ViewCase *input, ViewWork *work)
{
    AVCodecContext *context = avcodec_alloc_context3(avcodec_find_decoder(AV_CODEC_ID_AAC));
    if (!context)
        return 2;
    const int status = codec_parse_packets(input, context, AV_CODEC_ID_AAC, codec_adts_packet, work);
    avcodec_free_context(&context);
    return status;
}


#include <libavcodec/ac3_parser.h>
#include <libavutil/crc.h>
#include <libavutil/downmix_info.h>
#include <math.h>

typedef struct {
    uint8_t bsid;
    uint16_t extent;
    unsigned acmod, lfe, dialnorm;
} DolbyHeader;

static int codec_dolby_header(const uint8_t *data, size_t count, DolbyHeader *header)
{
    if (count < 7 || data[0] != 0x0b || data[1] != 0x77 ||
        av_ac3_parse_header(data, count, &header->bsid, &header->extent) < 0)
        return 0;
    if (header->bsid > 10) {
        header->acmod = (data[4] >> 1) & 7u;
        header->lfe = data[4] & 1u;
        header->dialnorm = data[6] >> 3;
    } else {
        const unsigned acmod = data[6] >> 5;
        const unsigned mix_bits = 2u * ((acmod == 2) + (acmod >= 4) +
                                        ((acmod & 1) && acmod != 1));
        const unsigned bit = 3 + mix_bits;
        if (count < 8 && bit + 6 > 8)
            return 0;
        const unsigned bsi = ((unsigned)data[6] << 8) | (count >= 8 ? data[7] : 0);
        header->acmod = acmod;
        header->lfe = (bsi >> (15 - bit)) & 1u;
        header->dialnorm = (bsi >> (10 - bit)) & 31u;
    }
    return 1;
}

static int codec_ac3_header_packet(AVCodecContext *context, const AVCodecParserContext *parser,
                                   const uint8_t *data, size_t size, void *opaque)
{
    ViewWork *work = opaque;
    if (size < 2 || data[0] != 0x0b || data[1] != 0x77)
        return 0;
    for (size_t offset = 0; offset < size;) {
        const uint64_t position = (uint64_t)parser->frame_offset + offset;
        DolbyHeader header;
        if (!codec_dolby_header(data + offset, size - offset, &header)) {
            reader_record(work, 8, &position, 1, NULL, 0);
            return 0;
        }
        const size_t actual = header.extent < size - offset ? header.extent : size - offset;
        if (!header.extent)
            return 2;
        /* Groups can carry more than one dependent header. Context facts are
         * for the independent syncframe; dependent rate/blocks use Annex E. */
        unsigned rate = (unsigned)context->sample_rate;
        unsigned blocks = (unsigned)parser->duration / 256;
        if (offset && header.bsid > 10) {
            const unsigned rates[] = {48000, 44100, 32000, 0};
            const unsigned block_counts[] = {1, 2, 3, 6};
            const uint8_t code = data[offset + 4];
            const unsigned fscod = code >> 6, second = (code >> 4) & 3u;
            rate = fscod == 3 ? rates[second] / 2 : rates[fscod];
            blocks = fscod == 3 ? 6 : block_counts[second];
        }
        const uint64_t fields[] = {position, header.bsid > 10, header.bsid,
                                   header.extent, rate, blocks, header.acmod, header.lfe, actual};
        reader_record(work, 3, fields, sizeof(fields) / sizeof(*fields), NULL, 0);
        offset += actual;
    }
    return 0;
}

static int codec_ac3_header_run(const ViewCase *input, ViewWork *work)
{
    AVCodecContext *context = avcodec_alloc_context3(avcodec_find_decoder(AV_CODEC_ID_AC3));
    if (!context)
        return 2;
    const int status = codec_parse_packets(input, context, AV_CODEC_ID_AC3, codec_ac3_header_packet, work);
    avcodec_free_context(&context);
    return status;
}

static uint64_t codec_audio_gain(double gain)
{
    return gain < 0.0 ? UINT64_MAX : (uint64_t)llround(gain * 100000.0);
}

static void codec_audio_permutations(ViewWork *work, const enum AVChannel *coded, unsigned count)
{
    uint64_t mask = 0;
    for (unsigned i = 0; i < count; ++i) {
        const enum AVChannel channel = coded[i];
        if (channel < 0 || channel >= 63 || (mask & (UINT64_C(1) << channel))) {
            count = 0;
            break;
        }
        mask |= UINT64_C(1) << channel;
    }
    uint64_t fields[257] = {count};
    AVChannelLayout native = {0};
    if (count && av_channel_layout_from_mask(&native, mask) == 0) {
        for (unsigned i = 0; i < count; ++i)
            fields[i + 1] = (uint64_t)av_channel_layout_index_from_channel(&native, coded[i]);
    }
    reader_record(work, 3, fields, count + 1, NULL, 0);
    av_channel_layout_uninit(&native);
}

static int codec_audio_layout(ViewWork *work, AVFrame *frame)
{
    const AVChannelLayout *layout = &frame->ch_layout;
    const int channels = layout->nb_channels;
    char name[512];
    const int described = av_channel_layout_describe(layout, name, sizeof(name));
    if (channels < 0 || channels > 256 || described < 0 || described >= (int)sizeof(name))
        return 2;
    size_t name_len = strlen(name);
    uint64_t fields[5 + 256] = {(uint64_t)frame->sample_rate, (uint64_t)channels, layout->order,
                              layout->order == AV_CHANNEL_ORDER_NATIVE ? layout->u.mask : 0};
    enum AVChannel speakers[256];
    for (int i = 0; i < channels; ++i) {
        speakers[i] = av_channel_layout_channel_from_index(layout, (unsigned)i);
        fields[4 + i] = (uint64_t)speakers[i];
    }
    fields[4 + channels] = name_len;
    reader_record(work, 4, fields, (size_t)channels + 5, (const uint8_t *)name, name_len);
    codec_audio_permutations(work, speakers, (unsigned)channels);
    /* Vorbis I §4.3.9 and Opus mapping family 1 (RFC 7845 §5.1.1.2). */
    const enum AVChannel vorbis[][8] = {
        {AV_CHAN_FRONT_CENTER},
        {AV_CHAN_FRONT_LEFT, AV_CHAN_FRONT_RIGHT},
        {AV_CHAN_FRONT_LEFT, AV_CHAN_FRONT_CENTER, AV_CHAN_FRONT_RIGHT},
        {AV_CHAN_FRONT_LEFT, AV_CHAN_FRONT_RIGHT, AV_CHAN_BACK_LEFT, AV_CHAN_BACK_RIGHT},
        {AV_CHAN_FRONT_LEFT, AV_CHAN_FRONT_CENTER, AV_CHAN_FRONT_RIGHT, AV_CHAN_BACK_LEFT, AV_CHAN_BACK_RIGHT},
        {AV_CHAN_FRONT_LEFT, AV_CHAN_FRONT_CENTER, AV_CHAN_FRONT_RIGHT, AV_CHAN_BACK_LEFT, AV_CHAN_BACK_RIGHT, AV_CHAN_LOW_FREQUENCY},
        {AV_CHAN_FRONT_LEFT, AV_CHAN_FRONT_CENTER, AV_CHAN_FRONT_RIGHT, AV_CHAN_SIDE_LEFT, AV_CHAN_SIDE_RIGHT, AV_CHAN_BACK_CENTER, AV_CHAN_LOW_FREQUENCY},
        {AV_CHAN_FRONT_LEFT, AV_CHAN_FRONT_CENTER, AV_CHAN_FRONT_RIGHT, AV_CHAN_SIDE_LEFT, AV_CHAN_SIDE_RIGHT, AV_CHAN_BACK_LEFT, AV_CHAN_BACK_RIGHT, AV_CHAN_LOW_FREQUENCY},
    };
    if (channels >= 1 && channels <= 8)
        codec_audio_permutations(work, vorbis[channels - 1], (unsigned)channels);
    else
        codec_audio_permutations(work, NULL, 0);

    const AVFrameSideData *side = av_frame_get_side_data(frame, AV_FRAME_DATA_DOWNMIX_INFO);
    if (side) {
        AVDownmixInfo info;
        if (side->size != sizeof(info))
            return 2;
        memcpy(&info, side->data, sizeof(info));
        const unsigned encoding = info.preferred_downmix_type == AV_DOWNMIX_TYPE_LTRT ? 1 :
                                  info.preferred_downmix_type == AV_DOWNMIX_TYPE_DPLII ? 2 : 0;
        const uint64_t levels[] = {
            codec_audio_gain(encoding ? info.center_mix_level_ltrt : info.center_mix_level),
            codec_audio_gain(encoding ? info.surround_mix_level_ltrt : info.surround_mix_level),
            codec_audio_gain(info.lfe_mix_level), encoding,
        };
        reader_record(work, 3, levels, 4, NULL, 0);
        const uint64_t gains[] = {
            info.preferred_downmix_type,
            codec_audio_gain(info.center_mix_level), codec_audio_gain(info.center_mix_level_ltrt),
            codec_audio_gain(info.surround_mix_level), codec_audio_gain(info.surround_mix_level_ltrt),
            codec_audio_gain(info.lfe_mix_level),
        };
        reader_record(work, 3, gains, 6, NULL, 0);
    }
    return 0;
}

typedef struct {
    AVPacket *packet;
    AVFrame *frame;
    ViewWork *work;
    int info;
    int refused;
} CodecAudio;

static int codec_audio_info(AVCodecContext *context, CodecAudio *state)
{
    const uint8_t *data = state->packet->data;
    size_t count = (size_t)state->packet->size;
    DolbyHeader first;
    if (!codec_dolby_header(data, count, &first)) {
        reader_record(state->work, 8, NULL, 0, NULL, 0);
        return 0;
    }
    enum AVMatrixEncoding matrix = AV_MATRIX_ENCODING_NONE;
    const AVFrameSideData *matrix_side = av_frame_get_side_data(state->frame, AV_FRAME_DATA_MATRIXENCODING);
    if (matrix_side) {
        if (matrix_side->size != sizeof(matrix))
            return 2;
        memcpy(&matrix, matrix_side->data, sizeof(matrix));
    }
    const AVCRC *crc16 = av_crc_get_table(AV_CRC_16_ANSI);
    unsigned substreams = 0;
    int crc_ok = 1;
    while (count) {
        DolbyHeader header;
        if (!codec_dolby_header(data, count, &header) || header.extent < 8 || header.extent > count) {
            crc_ok = 0;
            break;
        }
        size_t extent = header.extent;
        if (header.bsid <= 10) {
            const size_t words = extent / 2;
            const size_t crc1_end = 2 * (words / 2 + words / 8);
            crc_ok &= av_crc(crc16, 0, data + 2, crc1_end - 2) == 0;
        }
        crc_ok &= av_crc(crc16, 0, data + 2, extent - 2) == 0;
        ++substreams;
        count -= extent;
        data += extent;
    }
    const AVFrame *frame = state->frame;
    const uint64_t fields[] = {
        first.bsid > 10, first.bsid, first.acmod, first.lfe, first.dialnorm,
        (uint64_t)frame->sample_rate, (uint64_t)frame->nb_samples, substreams,
        (uint64_t)crc_ok, frame->decode_error_flags != 0 || (frame->flags & AV_FRAME_FLAG_CORRUPT),
        context->audio_service_type, matrix,
    };
    reader_record(state->work, 4, fields, sizeof(fields) / sizeof(*fields), NULL, 0);
    return 0;
}

static int codec_audio_receive(AVCodecContext *context, CodecAudio *state)
{
    for (;;) {
        const int result = avcodec_receive_frame(context, state->frame);
        if (result == AVERROR_EOF || result == AVERROR(EAGAIN))
            return 0;
        if (result == AVERROR_INVALIDDATA) {
            state->refused = 1;
            return 0;
        }
        if (result < 0)
            return 2;
        if ((state->info && codec_audio_info(context, state)) ||
            codec_audio_layout(state->work, state->frame))
            return 2;
        av_frame_unref(state->frame);
    }
}

static int codec_audio_send(AVCodecContext *context, CodecAudio *state)
{
    const int status = avcodec_send_packet(context, state->packet);
    if (status == AVERROR_INVALIDDATA) {
        state->refused = 1;
        return 0;
    }
    return status < 0 ? 2 : codec_audio_receive(context, state);
}

static int codec_audio_packet(AVCodecContext *context, const AVCodecParserContext *parser,
                              const uint8_t *data, size_t size, void *opaque)
{
    (void)parser;
    CodecAudio *state = opaque;
    av_packet_unref(state->packet);
    if (state->info) {
        if (av_new_packet(state->packet, (int)size) < 0)
            return 2;
        memcpy(state->packet->data, data, size);
    } else {
        state->packet->data = (uint8_t *)data;
        state->packet->size = (int)size;
    }
    return codec_audio_send(context, state);
}

static int codec_audio_decode(const ViewCase *input, ViewWork *work)
{
    const int info = !strcmp(input->prefix, "ac3-frame-info-");
    const char *lane = input->argv[0];
    enum AVCodecID id = AV_CODEC_ID_NONE;
    const char *demuxer = NULL;
    if (!strcmp(lane, "ac3"))
        id = AV_CODEC_ID_AC3;
    else if (!strcmp(lane, "eac3"))
        id = AV_CODEC_ID_EAC3;
    else if (!strcmp(lane, "aac"))
        id = AV_CODEC_ID_AAC;
    else if (!strcmp(lane, "dts"))
        id = AV_CODEC_ID_DTS;
    else if (!strcmp(lane, "flac"))
        demuxer = input->size >= 4 && !memcmp(input->data, "OggS", 4) ? "ogg" : "flac";
    else if (!strcmp(lane, "mp1") || !strcmp(lane, "mp2") || !strcmp(lane, "mp3"))
        demuxer = "mp3";
    else if (input->size >= 4 && !memcmp(input->data, "\x1a\x45\xdf\xa3", 4))
        demuxer = "matroska";
    else
        demuxer = "ogg";

    CodecAudio state = {.packet = av_packet_alloc(), .frame = av_frame_alloc(),
                        .work = work, .info = info};
    ReaderInput reader = {0};
    const AVCodec *codec = NULL;
    AVCodecContext *context = NULL;
    int status = 2, track = -1;
    if (!state.packet || !state.frame)
        goto done;
    if (demuxer) {
        const int opened = reader_open(&reader, input, demuxer);
        if (opened < 0 || avformat_find_stream_info(reader.format, NULL) < 0) {
            state.refused = 1;
            status = 0;
            goto done;
        }
        track = av_find_best_stream(reader.format, AVMEDIA_TYPE_AUDIO, -1, -1, &codec, 0);
        if (track < 0 || !codec) {
            state.refused = 1;
            status = 0;
            goto done;
        }
    } else {
        codec = avcodec_find_decoder(id);
    }
    context = avcodec_alloc_context3(codec);
    if (!codec || !context)
        goto done;
    if (track >= 0 && avcodec_parameters_to_context(context, reader.format->streams[track]->codecpar) < 0)
        goto done;
    context->thread_count = 1;
    if (avcodec_open2(context, codec, NULL) < 0) {
        state.refused = 1;
        status = 0;
        goto done;
    }
    if (demuxer) {
        int read;
        while ((read = av_read_frame(reader.format, state.packet)) >= 0) {
            if (state.packet->stream_index == track && codec_audio_send(context, &state))
                goto done;
            av_packet_unref(state.packet);
        }
        if (read == AVERROR(ENOMEM))
            goto done;
        state.refused |= read != AVERROR_EOF;
        status = 0;
    } else {
        const enum AVCodecID parser = id == AV_CODEC_ID_EAC3 ? AV_CODEC_ID_AC3 : id;
        status = codec_parse_packets(input, context, parser, codec_audio_packet, &state);
    }
    if (status == 0) {
        const int end = avcodec_send_packet(context, NULL);
        if (end < 0 && end != AVERROR_EOF && end != AVERROR_INVALIDDATA)
            status = 2;
        else
            status = codec_audio_receive(context, &state);
    }

done:
    if (status == 0 && state.refused && !work->packets)
        reader_record(work, 8, NULL, 0, NULL, 0);
    avcodec_free_context(&context);
    av_frame_free(&state.frame);
    av_packet_free(&state.packet);
    reader_close(&reader);
    return status;
}

static int codec_audio_prepare(ViewCase *input)
{
    if (!strcmp(input->prefix, "adts-sync-") && !input->argc) {
        input->name = "adts-sync";
        input->run = codec_adts_run;
        return codec_parser_prepare(input, input->size + 1);
    }
    if (!strcmp(input->prefix, "ac3-header-") && !input->argc) {
        input->name = "ac3-frame-header";
        input->run = codec_ac3_header_run;
        return codec_parser_prepare(input, input->size + 1);
    }
    if (!strcmp(input->prefix, "ac3-frame-info-") && input->argc == 1 &&
        (!strcmp(input->argv[0], "ac3") || !strcmp(input->argv[0], "eac3"))) {
        input->name = "ac3-frame-info";
        input->run = codec_audio_decode;
        return codec_parser_prepare(input, input->size + 1);
    }
    if (!strcmp(input->prefix, "layouts-") && input->argc == 1) {
        const char *lanes[] = {"aac", "ac3", "eac3", "dts", "flac", "mp1", "mp2", "mp3", "opus", "vorbis"};
        for (size_t i = 0; i < sizeof(lanes) / sizeof(*lanes); ++i)
            if (!strcmp(input->argv[0], lanes[i])) {
                input->name = "codec-layouts";
                input->run = codec_audio_decode;
                return i < 4 ? codec_parser_prepare(input, input->size + 1) : 0;
            }
    }
    fprintf(stderr, "usage: %s operation is unavailable\n", input->prefix);
    return 2;
}
