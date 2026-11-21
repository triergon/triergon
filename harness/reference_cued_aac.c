/* Black-box comparison using the public libavformat API. */
#include <inttypes.h>
#include <stdio.h>
#include <libavformat/avformat.h>
#include <libavutil/mathematics.h>

static void timestamp(const char *name, int64_t value, AVRational base) {
    if (value == AV_NOPTS_VALUE) printf(" %s=None", name);
    else printf(" %s=Some(%" PRId64 ")", name, av_rescale_q(value, base, (AVRational){1, 1000000000}));
}
int main(int argc, char **argv) {
    AVFormatContext *ctx = NULL;
    AVPacket *pkt = NULL;
    int result;
    if (argc != 2) return 2;
    av_log_set_level(AV_LOG_QUIET);
    if ((result = avformat_open_input(&ctx, argv[1], NULL, NULL)) < 0) {
        fprintf(stderr, "avformat_open_input=%d\n", result); return 1;
    }
    if ((result = avformat_find_stream_info(ctx, NULL)) < 0) {
        fprintf(stderr, "avformat_find_stream_info=%d\n", result); avformat_close_input(&ctx); return 1;
    }
    if (ctx->nb_streams != 1) { fprintf(stderr, "stream_count=%u\n", ctx->nb_streams); avformat_close_input(&ctx); return 1; }
    AVRational base = ctx->streams[0]->time_base;
    int64_t target = av_rescale_q(1010000000, (AVRational){1, 1000000000}, base);
    result = av_seek_frame(ctx, 0, target, AVSEEK_FLAG_BACKWARD);
    if (result < 0) { fprintf(stderr, "av_seek_frame=%d\n", result); avformat_close_input(&ctx); return 1; }
    pkt = av_packet_alloc();
    if (!pkt) { avformat_close_input(&ctx); return 1; }
    int first = 1;
    while ((result = av_read_frame(ctx, pkt)) >= 0) {
        if (first) { printf("landing_ns=%" PRId64 "\n", av_rescale_q(pkt->pts, base, (AVRational){1, 1000000000})); first = 0; }
        printf("packet track=%d", pkt->stream_index);
        timestamp("pts_ns", pkt->pts, base);
        timestamp("dts_ns", pkt->dts, base);
        /* libavformat uses zero for an unknown packet duration. */
        timestamp("duration_ns", pkt->duration ? pkt->duration : AV_NOPTS_VALUE, base);
        printf(" bytes=");
        for (int i = 0; i < pkt->size; ++i) printf("%02x", pkt->data[i]);
        putchar('\n');
        av_packet_unref(pkt);
    }
    av_packet_free(&pkt);
    avformat_close_input(&ctx);
    if (result != AVERROR_EOF) { fprintf(stderr, "av_read_frame=%d\n", result); return 1; }
    return 0;
}
