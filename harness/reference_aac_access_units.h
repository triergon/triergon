/* Configured AAC-LC decoding through the public libavcodec API. */
#include <libavutil/intreadwrite.h>

enum { AAC_PADDING = 64, AAC_SAMPLES = 1024, AAC_MAX_INPUT = 64 << 20 };
_Static_assert(AAC_PADDING >= AV_INPUT_BUFFER_PADDING_SIZE, "AAC packet padding");

static void keep_aac_input(void *opaque, uint8_t *data) {
    (void)opaque;
    (void)data;
}

/* A complete plane is a multiple of 64 bytes, so each lane reads whole words. */
static void fold_aac_plane(struct fold *digest, const uint8_t *samples) {
    const size_t bytes = AAC_SAMPLES * sizeof(float);
    digest->total += bytes;
    for (size_t offset = 0; offset < bytes; offset += 64) {
        for (size_t lane = 0; lane < 8; lane++) {
            const uint64_t word = AV_RL64(samples + offset + lane * 8);
            digest->lane[lane] = (digest->lane[lane] ^ word) * FOLD_K;
        }
    }
}

static void aac_access_units(const uint8_t *data, size_t size, struct out *out) {
    static const uint8_t padding[AAC_PADDING] = {0};
    if (size < 16 || AV_HAVE_BIGENDIAN)
        die("AAC input header", AVERROR_INVALIDDATA);
    const uint32_t rate = AV_RL32(data);
    const uint32_t channels = AV_RL32(data + 4);
    const uint32_t packets = AV_RL32(data + 8);
    const uint32_t config = AV_RL32(data + 12);
    if (!rate || !channels || channels > 8 || !packets || !config ||
        config > 65536 || config > size - 16)
        die("AAC configuration bounds", AVERROR_INVALIDDATA);

    const AVCodec *codec = avcodec_find_decoder_by_name("aac");
    if (!codec) die("AAC decoder", AVERROR_DECODER_NOT_FOUND);
    AVCodecContext *decoder = avcodec_alloc_context3(codec);
    AVPacket *packet = av_packet_alloc();
    AVFrame *frame = av_frame_alloc();
    if (!decoder || !packet || !frame) die("AAC decoder storage", AVERROR(ENOMEM));
    decoder->extradata = av_mallocz(config + AV_INPUT_BUFFER_PADDING_SIZE);
    packet->buf = av_buffer_create((uint8_t *)data, size, keep_aac_input, NULL,
                                   AV_BUFFER_FLAG_READONLY);
    if (!decoder->extradata || !packet->buf) die("AAC input storage", AVERROR(ENOMEM));
    data = packet->buf->data;
    memcpy(decoder->extradata, data + 16, config);
    decoder->extradata_size = (int)config;
    decoder->thread_count = 1;
    decoder->err_recognition = AV_EF_EXPLODE;
    int rc = avcodec_open2(decoder, codec, NULL);
    if (rc < 0) die("AAC open", rc);

    size_t at = 16 + config;
    for (uint32_t index = 0; index < packets; index++) {
        if (size - at < 4 + AAC_PADDING) die("AAC packet header", AVERROR_INVALIDDATA);
        const uint32_t length = AV_RL32(data + at);
        at += 4;
        if (!length || length > 65536 || length > size - at - AAC_PADDING ||
            memcmp(data + at + length, padding, AAC_PADDING))
            die("AAC packet bounds", AVERROR_INVALIDDATA);
        packet->data = (uint8_t *)data + at;
        packet->size = (int)length;
        at += length + AAC_PADDING;
        if ((rc = avcodec_send_packet(decoder, packet)) < 0) die("AAC send", rc);
        if ((rc = avcodec_receive_frame(decoder, frame)) < 0) die("AAC receive", rc);
        if (frame->format != AV_SAMPLE_FMT_FLTP || frame->nb_samples != AAC_SAMPLES ||
            (uint32_t)frame->ch_layout.nb_channels != channels ||
            (uint32_t)frame->sample_rate != rate)
            die("AAC output shape", AVERROR_INVALIDDATA);
        if (out->raw) {
            for (uint32_t channel = 0; channel < channels; channel++) {
                enum AVChannel speaker = av_channel_layout_channel_from_index(&frame->ch_layout, channel);
                if (speaker < 0 || speaker > UINT8_MAX ||
                    (out->aac_channels && out->aac_speakers[channel] != speaker))
                    die("AAC output speaker order", AVERROR_INVALIDDATA);
                out->aac_speakers[channel] = (uint8_t)speaker;
            }
            out->aac_channels = channels;
        }
        const size_t plane = AAC_SAMPLES * sizeof(float);
        for (uint32_t channel = 0; channel < channels; channel++) {
            const uint8_t *samples = frame->extended_data[channel];
            fold_aac_plane(&out->digest, samples);
            if (out->raw && fwrite(samples, 1, plane, out->raw) != plane)
                die("AAC PCM output", AVERROR(EIO));
        }
        out->bytes += plane * channels;
        out->frames++;
        av_frame_unref(frame);
    }
    if (at != size) die("AAC trailing input", AVERROR_INVALIDDATA);
    if ((rc = avcodec_send_packet(decoder, NULL)) < 0) die("AAC flush", rc);
    if (avcodec_receive_frame(decoder, frame) != AVERROR_EOF)
        die("AAC unexpected delayed output", AVERROR_INVALIDDATA);
    av_frame_free(&frame);
    av_packet_free(&packet);
    avcodec_free_context(&decoder);
}
