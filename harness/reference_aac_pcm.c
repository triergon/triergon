/* Floating-point AAC correctness reference with initialized output planes.
 * Used for the pinned single-SCE stereo vector. The decoder receives the
 * usual FFmpeg allocation and may write every sample. */

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/intreadwrite.h>

static int initialize_planes(AVCodecContext *decoder, AVFrame *frame, int flags) {
    const AVChannelLayout stereo = AV_CHANNEL_LAYOUT_STEREO;
    if (frame->format != AV_SAMPLE_FMT_FLTP || frame->nb_samples <= 0 ||
        av_channel_layout_compare(&frame->ch_layout, &stereo) != 0)
        return AVERROR_INVALIDDATA;

    int r = avcodec_default_get_buffer2(decoder, frame, flags);
    if (r < 0) return r;
    if (frame->nb_samples > frame->linesize[0] / 4) return AVERROR_INVALIDDATA;

    for (int ch = 0; ch < stereo.nb_channels; ch++)
        memset(frame->extended_data[ch], 0, (size_t)frame->nb_samples * 4);
    return 0;
}

static int drain(AVCodecContext *decoder, AVFrame *frame, int sample_rate, FILE *out) {
    const AVChannelLayout stereo = AV_CHANNEL_LAYOUT_STEREO;
    int r;
    while ((r = avcodec_receive_frame(decoder, frame)) == 0) {
        if (frame->format != AV_SAMPLE_FMT_FLTP || frame->sample_rate != sample_rate ||
            av_channel_layout_compare(&frame->ch_layout, &stereo) != 0 || frame->nb_samples <= 0)
            return AVERROR_INVALIDDATA;
        for (int i = 0; i < frame->nb_samples; i++) {
            uint8_t sample[8];
            for (int ch = 0; ch < stereo.nb_channels; ch++) {
                uint32_t bits;
                memcpy(&bits, frame->extended_data[ch] + (size_t)i * 4, sizeof bits);
                AV_WL32(sample + ch * 4, bits);
            }
            if (fwrite(sample, sizeof sample, 1, out) != 1) return AVERROR(EIO);
        }
        av_frame_unref(frame);
    }
    return r == AVERROR(EAGAIN) || r == AVERROR_EOF ? 0 : r;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fputs("usage: ffmpeg-aac-initialized-pcm INPUT.adts OUTPUT.f32\n", stderr);
        return 2;
    }
    av_log_set_level(AV_LOG_ERROR);
    AVFormatContext *input = NULL;
    AVCodecContext *decoder = NULL;
    AVPacket *packet = NULL;
    AVFrame *frame = NULL;
    FILE *output = NULL;
    int r = avformat_open_input(&input, argv[1], av_find_input_format("aac"), NULL);
    if (r < 0 || (r = avformat_find_stream_info(input, NULL)) < 0) goto done;
    const AVCodec *codec = NULL;
    int stream = av_find_best_stream(input, AVMEDIA_TYPE_AUDIO, -1, -1, &codec, 0);
    if (stream < 0 || codec->id != AV_CODEC_ID_AAC) {
        r = stream < 0 ? stream : AVERROR_INVALIDDATA;
        goto done;
    }
    decoder = avcodec_alloc_context3(codec);
    packet = av_packet_alloc();
    frame = av_frame_alloc();
    if (!decoder || !packet || !frame) {
        r = AVERROR(ENOMEM);
        goto done;
    }
    r = avcodec_parameters_to_context(decoder, input->streams[stream]->codecpar);
    if (r < 0) goto done;
    const int sample_rate = decoder->sample_rate;
    decoder->thread_count = 1;
    decoder->get_buffer2 = initialize_planes;
    if ((r = avcodec_open2(decoder, codec, NULL)) < 0) goto done;
    output = fopen(argv[2], "wb");
    if (!output) {
        r = AVERROR(errno);
        goto done;
    }

    while ((r = av_read_frame(input, packet)) == 0) {
        if (packet->stream_index == stream) {
            r = avcodec_send_packet(decoder, packet);
            if (r >= 0) r = drain(decoder, frame, sample_rate, output);
        }
        av_packet_unref(packet);
        if (r < 0) goto done;
    }
    if (r == AVERROR_EOF && (r = avcodec_send_packet(decoder, NULL)) >= 0)
        r = drain(decoder, frame, sample_rate, output);

done:
    if (output && fclose(output) != 0 && r >= 0) r = AVERROR(EIO);
    if (r < 0)
        fprintf(stderr, "AAC PCM reference: %s\n", av_err2str(r));
    av_frame_free(&frame);
    av_packet_free(&packet);
    avcodec_free_context(&decoder);
    avformat_close_input(&input);
    return r < 0;
}
