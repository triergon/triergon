/* Development oracle: decode an MPEG audio file with FFmpeg's libavformat/libavcodec and
 * write every decoded frame interleaved f32le (each frame with its own channel count),
 * plus one line per frame to OUTPUT.frames: "samples channels rate skip_start skip_end".
 *
 * usage: reference [--trim] [--codec NAME] [--flush-at N] INPUT OUTPUT.f32
 *   --trim      let the decoder apply skip-samples side data (FFmpeg's default); without
 *               it AV_CODEC_FLAG2_SKIP_MANUAL keeps the untrimmed decoder output.
 *   --flush-at  flush the decoder (avcodec_flush_buffers) before packet N.
 *   --codec     decoder name; "default" for FFmpeg's default decoder of the codec (the
 *               fixed-point mp1 and mp2, and mp3float). Without it, the float decoder of
 *               the codec: mp1float, mp2float or mp3float.
 *
 * Build: cc -O1 reference.c $(pkg-config --cflags --libs libavformat libavcodec libavutil)
 */
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/intreadwrite.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int drain(AVCodecContext *dec, AVFrame *frame, FILE *out, FILE *log) {
    int ret;
    while ((ret = avcodec_receive_frame(dec, frame)) >= 0) {
        int ch = frame->ch_layout.nb_channels;
        int skip_start = 0, skip_end = 0;
        AVFrameSideData *sd = av_frame_get_side_data(frame, AV_FRAME_DATA_SKIP_SAMPLES);
        if (sd && sd->size >= 10) {
            skip_start = AV_RL32(sd->data);
            skip_end = AV_RL32(sd->data + 4);
        }
        fprintf(log, "%d %d %d %d %d\n", frame->nb_samples, ch, frame->sample_rate, skip_start,
                skip_end);
        for (int i = 0; i < frame->nb_samples; i++)
            for (int c = 0; c < ch; c++) {
                float v;
                switch (frame->format) {
                case AV_SAMPLE_FMT_FLTP: v = ((float *)frame->extended_data[c])[i]; break;
                case AV_SAMPLE_FMT_FLT: v = ((float *)frame->extended_data[0])[i * ch + c]; break;
                /* The fixed-point decoders: scaled as swresample converts s16 to float. */
                case AV_SAMPLE_FMT_S16P: v = ((int16_t *)frame->extended_data[c])[i] / 32768.0f; break;
                case AV_SAMPLE_FMT_S16: v = ((int16_t *)frame->extended_data[0])[i * ch + c] / 32768.0f; break;
                default:
                    fprintf(stderr, "unexpected sample format %d\n", frame->format);
                    return -1;
                }
                fwrite(&v, 4, 1, out);
            }
        av_frame_unref(frame);
    }
    return ret == AVERROR(EAGAIN) || ret == AVERROR_EOF ? 0 : ret;
}

int main(int argc, char **argv) {
    int trim = 0;
    const char *codec_name = NULL;
    int flush_at = -1;
    int a = 1;
    for (; a < argc && argv[a][0] == '-' && argv[a][1] == '-'; a++) {
        if (!strcmp(argv[a], "--trim"))
            trim = 1;
        else if (!strcmp(argv[a], "--codec") && a + 1 < argc)
            codec_name = argv[++a];
        else if (!strcmp(argv[a], "--flush-at") && a + 1 < argc)
            flush_at = atoi(argv[++a]);
    }
    if (argc - a != 2) {
        fprintf(stderr, "usage: reference [--trim] [--codec NAME] INPUT OUTPUT.f32\n");
        return 2;
    }
    av_log_set_level(AV_LOG_ERROR);
    AVFormatContext *fmt = NULL;
    const AVInputFormat *mp3 = av_find_input_format("mp3");
    if (avformat_open_input(&fmt, argv[a], mp3, NULL) < 0) {
        fprintf(stderr, "cannot open %s\n", argv[a]);
        return 1;
    }
    if (avformat_find_stream_info(fmt, NULL) < 0)
        return 1;
    int si = av_find_best_stream(fmt, AVMEDIA_TYPE_AUDIO, -1, -1, NULL, 0);
    if (si < 0)
        return 1;
    AVStream *st = fmt->streams[si];
    char float_name[64];
    if (codec_name && !strcmp(codec_name, "default")) {
        const AVCodec *any = avcodec_find_decoder(st->codecpar->codec_id);
        codec_name = any ? any->name : "";
    } else if (!codec_name) {
        const AVCodec *any = avcodec_find_decoder(st->codecpar->codec_id);
        const char *base = any ? any->name : "";
        size_t n = strlen(base);
        snprintf(float_name, sizeof float_name, n > 5 && !strcmp(base + n - 5, "float") ? "%s" : "%sfloat", base);
        codec_name = float_name;
    }
    const AVCodec *codec = avcodec_find_decoder_by_name(codec_name);
    if (!codec) {
        fprintf(stderr, "no decoder %s\n", codec_name);
        return 1;
    }
    AVCodecContext *dec = avcodec_alloc_context3(codec);
    avcodec_parameters_to_context(dec, st->codecpar);
    dec->thread_count = 1;
    if (!trim)
        dec->flags2 |= AV_CODEC_FLAG2_SKIP_MANUAL;
    if (avcodec_open2(dec, codec, NULL) < 0)
        return 1;
    char logname[4096];
    snprintf(logname, sizeof logname, "%s.frames", argv[a + 1]);
    FILE *out = fopen(argv[a + 1], "wb"), *log = fopen(logname, "w");
    AVPacket *pkt = av_packet_alloc();
    AVFrame *frame = av_frame_alloc();
    int packets = 0, errors = 0;
    while (av_read_frame(fmt, pkt) >= 0) {
        if (pkt->stream_index == si) {
            if (packets == flush_at)
                avcodec_flush_buffers(dec);
            packets++;
            if (avcodec_send_packet(dec, pkt) < 0)
                errors++;
            if (drain(dec, frame, out, log) < 0)
                errors++;
        }
        av_packet_unref(pkt);
    }
    avcodec_send_packet(dec, NULL);
    drain(dec, frame, out, log);
    fprintf(stderr, "{\"decoder\":\"%s\",\"packets\":%d,\"errors\":%d}\n", codec->name, packets,
            errors);
    fclose(out);
    fclose(log);
    return 0;
}
