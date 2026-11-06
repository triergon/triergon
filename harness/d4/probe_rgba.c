/* D4 RGBA black-box probe (research tooling, never production).
 *
 * Feeds crafted planar 4:2:0 frames through the exact reference video chain
 *   scale=1920:1080:force_original_aspect_ratio=decrease,
 *   pad=1920:1080:(ow-iw)/2:(oh-ih)/2,format=rgba
 * built the same way as harness/d4/reference_decode.c (buffer source carrying
 * colorspace and range), using only the public libavfilter API.
 *
 * usage: probe_rgba W H BITS COLORSPACE RANGE < frames > rgba
 *   frames: N back-to-back yuv420p (BITS=8) or yuv420p10le (BITS=10) pictures
 *   COLORSPACE: AVColorSpace number (2 = unspecified), RANGE: AVColorRange (1 tv, 2 pc)
 *   output: N canvases of 1920x1080x4 bytes (PROBE_CHAIN overrides the chain; any output
 *   format is written as its tightly packed planes).
 */
#include <libavfilter/avfilter.h>
#include <libavfilter/buffersink.h>
#include <libavfilter/buffersrc.h>
#include <libavutil/frame.h>
#include <libavutil/imgutils.h>
#include <libavutil/pixfmt.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000u + (uint64_t)ts.tv_nsec;
}

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

static void die(const char *what, int err) {
    char buf[128] = "";
    av_strerror(err, buf, sizeof buf);
    fprintf(stderr, "%s: %s\n", what, buf);
    exit(1);
}

static int bench_mode;

static void drain(AVFilterContext *sink) {
    AVFrame *f = av_frame_alloc();
    for (;;) {
        int r = av_buffersink_get_frame(sink, f);
        if (r == AVERROR(EAGAIN) || r == AVERROR_EOF) break;
        if (r < 0) die("sink", r);
        if (bench_mode) { av_frame_unref(f); continue; }
        /* Every plane, tightly packed (rgba: one plane of width*4 rows). */
        int n = av_image_get_buffer_size(f->format, f->width, f->height, 1);
        uint8_t *packed = malloc((size_t)n);
        av_image_copy_to_buffer(packed, n, (const uint8_t *const *)f->data, f->linesize, f->format, f->width, f->height, 1);
        fwrite(packed, 1, (size_t)n, stdout);
        free(packed);
        av_frame_unref(f);
    }
    av_frame_free(&f);
}

int main(int argc, char **argv) {
    if (argc < 6) return 2;
    int w = atoi(argv[1]), h = atoi(argv[2]), bits = atoi(argv[3]), cs = atoi(argv[4]), range = atoi(argv[5]);
    int fmt = bits == 8 ? AV_PIX_FMT_YUV420P : AV_PIX_FMT_YUV420P10LE;
    int bps = bits == 8 ? 1 : 2;
    int cw = (w + 1) / 2, ch = (h + 1) / 2;
    size_t frame_bytes = (size_t)bps * ((size_t)w * h + 2 * (size_t)cw * ch);
    av_log_set_level(AV_LOG_ERROR);
    AVFilterGraph *g = avfilter_graph_alloc();
    g->nb_threads = 1; /* as reference_decode.c */
    AVFilterContext *src, *sink;
    char args[512];
    snprintf(args, sizeof args, "video_size=%dx%d:pix_fmt=%d:time_base=1/90000:pixel_aspect=1/1:colorspace=%d:range=%d", w, h, fmt, cs, range);
    int r;
    if ((r = avfilter_graph_create_filter(&src, avfilter_get_by_name("buffer"), "in", args, NULL, g)) < 0) die("buffer", r);
    if ((r = avfilter_graph_create_filter(&sink, avfilter_get_by_name("buffersink"), "out", NULL, NULL, g)) < 0) die("sink", r);
    AVFilterInOut *outputs = avfilter_inout_alloc(), *inputs = avfilter_inout_alloc();
    outputs->name = av_strdup("in"); outputs->filter_ctx = src; outputs->pad_idx = 0; outputs->next = NULL;
    inputs->name = av_strdup("out"); inputs->filter_ctx = sink; inputs->pad_idx = 0; inputs->next = NULL;
    const char *chain = getenv("PROBE_CHAIN") ? getenv("PROBE_CHAIN")
        : "scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,format=rgba";
    if ((r = avfilter_graph_parse_ptr(g, chain, &inputs, &outputs, NULL)) < 0) die("parse", r);
    if ((r = avfilter_graph_config(g, NULL)) < 0) die("config", r);
    uint8_t *buf = malloc(frame_bytes);
    /* PROBE_BENCH=N: convert the first frame N times (frame copy outside the timed part) and
     * print the median conversion time (buffersrc add + buffersink get), no output. */
    long reps = getenv("PROBE_BENCH") ? strtol(getenv("PROBE_BENCH"), NULL, 10) : 0;
    bench_mode = reps > 0;
    for (int64_t n = 0;; n++) {
        if (fread(buf, 1, frame_bytes, stdin) != frame_bytes) break;
        AVFrame *f = av_frame_alloc();
        f->width = w; f->height = h; f->format = fmt; f->pts = n * 3000;
        f->colorspace = cs; f->color_range = range;
        f->sample_aspect_ratio = (AVRational){1, 1};
        if ((r = av_frame_get_buffer(f, 0)) < 0) die("alloc", r);
        const uint8_t *p = buf;
        int pw[3] = {w, cw, cw}, ph[3] = {h, ch, ch};
        for (int c = 0; c < 3; c++)
            for (int y = 0; y < ph[c]; y++) {
                memcpy(f->data[c] + (ptrdiff_t)y * f->linesize[c], p, (size_t)pw[c] * bps);
                p += (size_t)pw[c] * bps;
            }
        if (bench_mode) {
            uint64_t *t = malloc(sizeof(uint64_t) * (size_t)reps);
            for (long i = 0; i < reps; i++) {
                AVFrame *c = av_frame_clone(f);
                c->pts = n * 3000 + i * 3000;
                uint64_t t0 = now_ns();
                if ((r = av_buffersrc_add_frame_flags(src, c, 0)) < 0) die("add", r);
                drain(sink);
                t[i] = now_ns() - t0;
                av_frame_free(&c);
            }
            qsort(t, (size_t)reps, sizeof *t, cmp_u64);
            printf("{\"median_ns\":%llu,\"min_ns\":%llu,\"reps\":%ld}\n", (unsigned long long)t[reps / 2], (unsigned long long)t[0], reps);
            free(t);
            av_frame_free(&f);
            break;
        }
        if ((r = av_buffersrc_add_frame_flags(src, f, 0)) < 0) die("add", r);
        av_frame_free(&f);
        drain(sink);
    }
    if ((r = av_buffersrc_add_frame_flags(src, NULL, 0)) < 0) die("eof", r);
    drain(sink);
    avfilter_graph_free(&g);
    free(buf);
    return 0;
}
