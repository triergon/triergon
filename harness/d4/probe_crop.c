/* D4 crop probe (research tooling): decodes a stream with libavcodec twice, once with
 * apply_cropping = 0 (prints the conformance crop fields) and once with the default
 * apply_cropping = 1 (prints the delivered frame size and how many samples of the left crop
 * were kept), i.e. what reference_decode hands its filter graph.
 * usage: probe_crop FILE [N]   prints one line per picture for the first N (default 3) pictures */
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/pixdesc.h>
#include <stdio.h>
#include <stdlib.h>

static void run(const char *path, int apply, int n) {
    AVFormatContext *fmt = NULL;
    if (avformat_open_input(&fmt, path, NULL, NULL) < 0 || avformat_find_stream_info(fmt, NULL) < 0) { puts("open failed"); exit(1); }
    int si = av_find_best_stream(fmt, AVMEDIA_TYPE_VIDEO, -1, -1, NULL, 0);
    const AVCodec *codec = avcodec_find_decoder(fmt->streams[si]->codecpar->codec_id);
    AVCodecContext *dec = avcodec_alloc_context3(codec);
    avcodec_parameters_to_context(dec, fmt->streams[si]->codecpar);
    dec->thread_count = 1;
    dec->apply_cropping = apply;
    avcodec_open2(dec, codec, NULL);
    AVPacket *pkt = av_packet_alloc();
    AVFrame *f = av_frame_alloc();
    int shown = 0;
    for (int eof = 0; shown < n && eof < 2;) {
        if (!eof && av_read_frame(fmt, pkt) >= 0) {
            if (pkt->stream_index == si) avcodec_send_packet(dec, pkt);
            av_packet_unref(pkt);
        } else {
            avcodec_send_packet(dec, NULL); /* drain */
            eof++;
        }
        while (shown < n && avcodec_receive_frame(dec, f) == 0) {
            if (apply)
                printf("apply=1 frame %d: %dx%d fmt=%s linesize=%d,%d data0%%64=%lu data1%%64=%lu\n", shown, f->width, f->height,
                       av_get_pix_fmt_name(f->format), f->linesize[0], f->linesize[1], (unsigned long)((uintptr_t)f->data[0] % 64),
                       (unsigned long)((uintptr_t)f->data[1] % 64));
            else
                printf("apply=0 frame %d: coded %dx%d crop l=%zu r=%zu t=%zu b=%zu\n", shown, f->width, f->height, f->crop_left, f->crop_right,
                       f->crop_top, f->crop_bottom);
            shown++;
            av_frame_unref(f);
        }
    }
    avcodec_free_context(&dec);
    avformat_close_input(&fmt);
    av_frame_free(&f);
    av_packet_free(&pkt);
}

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    av_log_set_level(AV_LOG_ERROR);
    int n = argc > 2 ? atoi(argv[2]) : 3;
    run(argv[1], 0, n);
    run(argv[1], 1, n);
    return 0;
}
