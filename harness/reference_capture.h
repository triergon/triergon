/*
 * Capture reference: the capture build of harness/reference.c (tooling only;
 * never linked into the library). It reproduces an application's capture call
 * sequence against a pinned FFmpeg build:
 *
 *   sws_getContext(w,h,RGBA -> w,h,NV12, SWS_BILINEAR), negative source stride
 *   for bottom-up input, NV12 UV deinterleaved into YUV420P U/V by the caller;
 *   mpeg1video: video_size, pixel_format=yuv420p, time_base=1/30, b, maxrate=b*7/5,
 *   bufsize=b/7, g=30, bf=0, dct=fastint; frame pts 0,1,2,... in 1/30;
 *   mpegts muxer, default options, custom AVIO (32768-byte buffer, write callback),
 *   av_packet_rescale_ts(1/30 -> stream tb), av_write_frame, avio_flush per frame,
 *   no trailer.
 *
 * Usage:
 *   reference convert  <w> <h> <flip> <frames> <in.rgba> <out.yuv>
 *   reference pipeline <w> <h> <flip> <frames> <bitrate> <in.rgba> <out.ts>
 *   reference memory   <w> <h> <flip> <frames> <bitrate> <in.rgba>
 *   reference bench    <in.rgba> <iterations> <w> <h> <flip> <frames> <bitrate>
 *
 * `bench` takes the harness's timing command line (input and iterations first)
 * and times `iterations` complete sessions (open, every frame
 * converted+encoded+muxed+flushed, close), each into a fresh sink; every
 * session must produce the same stream, whose {iterations, elapsed_ns,
 * packets (frames), bytes, digest} it prints.
 * `memory` performs the same session once between the shared observer's stdout
 * markers (harness/allocations-linux.c); input frames are loaded before BEGIN.
 * Output bytes go to a sink that hashes (FNV-1a 64) and counts, never retains.
 */
#define _POSIX_C_SOURCE 200809L
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/opt.h>
#include <libswscale/swscale.h>

typedef struct {
    FILE *file;      /* pipeline: retained output file; NULL otherwise */
    uint64_t hash;
    uint64_t bytes;
    uint64_t writes;
} Sink;

static int sink_write(void *opaque, const uint8_t *data, int n) {
    Sink *s = opaque;
    if (n <= 0) return -1;
    for (int i = 0; i < n; i++) {
        s->hash ^= data[i];
        s->hash *= UINT64_C(1099511628211);
    }
    s->bytes += (uint64_t)n;
    s->writes++;
    if (s->file && fwrite(data, 1, (size_t)n, s->file) != (size_t)n) return -1;
    return n;
}

typedef struct {
    int w, h;
    AVCodecContext *ctx;
    AVFrame *frame;
    struct SwsContext *sws;
    AVFormatContext *oc;
    AVStream *st;
    AVIOContext *avio;
    AVPacket *pkt;
    AVRational st_tb;
    int64_t pts;
    uint8_t *nv12_uv;
} Venc;

static void die(const char *what) {
    fprintf(stderr, "reference_capture: %s\n", what);
    exit(2);
}

static void set(AVCodecContext *ctx, const char *name, const char *value) {
    if (av_opt_set(ctx, name, value, 0) < 0) die(name);
}

static void venc_open(Venc *v, int w, int h, int64_t bitrate, Sink *sink) {
    char buf[64];
    memset(v, 0, sizeof *v);
    v->w = w;
    v->h = h;
    const AVCodec *codec = avcodec_find_encoder_by_name("mpeg1video");
    if (!codec) die("mpeg1video encoder absent");
    v->ctx = avcodec_alloc_context3(codec);
    if (!v->ctx) die("alloc context");
    snprintf(buf, sizeof buf, "%dx%d", w, h);
    set(v->ctx, "video_size", buf);
    set(v->ctx, "pixel_format", "yuv420p");
    set(v->ctx, "time_base", "1/30");
    snprintf(buf, sizeof buf, "%" PRId64, bitrate);
    set(v->ctx, "b", buf);
    snprintf(buf, sizeof buf, "%" PRId64, bitrate * 7 / 5);
    set(v->ctx, "maxrate", buf);
    snprintf(buf, sizeof buf, "%" PRId64, bitrate / 7);
    set(v->ctx, "bufsize", buf);
    set(v->ctx, "g", "30");
    set(v->ctx, "bf", "0");
    set(v->ctx, "dct", "fastint");
    if (avcodec_open2(v->ctx, codec, NULL) < 0) die("avcodec_open2");
    v->frame = av_frame_alloc();
    if (!v->frame) die("frame alloc");
    v->frame->width = w;
    v->frame->height = h;
    v->frame->format = AV_PIX_FMT_YUV420P;
    if (av_frame_get_buffer(v->frame, 32) < 0) die("frame buffer");
    v->sws = sws_getContext(w, h, AV_PIX_FMT_RGBA, w, h, AV_PIX_FMT_NV12, SWS_BILINEAR,
                            NULL, NULL, NULL);
    if (!v->sws) die("sws_getContext");
    v->nv12_uv = malloc((size_t)w * (size_t)h / 2);
    if (!v->nv12_uv) die("nv12 scratch");
    if (avformat_alloc_output_context2(&v->oc, NULL, "mpegts", NULL) < 0 || !v->oc)
        die("mpegts muxer");
    uint8_t *iobuf = av_malloc(32768);
    if (!iobuf) die("avio buffer");
    v->avio = avio_alloc_context(iobuf, 32768, 1, sink, NULL, sink_write, NULL);
    if (!v->avio) die("avio");
    v->oc->pb = v->avio;
    v->st = avformat_new_stream(v->oc, NULL);
    if (!v->st) die("new stream");
    if (avcodec_parameters_from_context(v->st->codecpar, v->ctx) < 0) die("codecpar");
    if (avformat_write_header(v->oc, NULL) < 0) die("write_header");
    v->st_tb = v->st->time_base;
    v->pkt = av_packet_alloc();
    if (!v->pkt) die("packet");
}

static void convert_frame(Venc *v, const uint8_t *rgba, int flip) {
    int row = v->w * 4;
    const uint8_t *src[4] = {flip ? rgba + (size_t)(v->h - 1) * (size_t)row : rgba, NULL, NULL, NULL};
    int src_stride[4] = {flip ? -row : row, 0, 0, 0};
    uint8_t *dst[4] = {v->frame->data[0], v->nv12_uv, NULL, NULL};
    int dst_stride[4] = {v->frame->linesize[0], v->w, 0, 0};
    sws_scale(v->sws, src, src_stride, 0, v->h, dst, dst_stride);
    int cw = v->w / 2, ch = v->h / 2;
    for (int y = 0; y < ch; y++) {
        const uint8_t *s = v->nv12_uv + (size_t)y * (size_t)v->w;
        uint8_t *up = v->frame->data[1] + (size_t)y * (size_t)v->frame->linesize[1];
        uint8_t *vp = v->frame->data[2] + (size_t)y * (size_t)v->frame->linesize[2];
        for (int i = 0; i < cw; i++) {
            up[i] = s[2 * i];
            vp[i] = s[2 * i + 1];
        }
    }
}

static int venc_encode(Venc *v, const uint8_t *rgba, int flip) {
    if (av_frame_make_writable(v->frame) < 0) return 0;
    convert_frame(v, rgba, flip);
    v->frame->pts = v->pts++;
    if (avcodec_send_frame(v->ctx, v->frame) < 0) return 0;
    for (;;) {
        int r = avcodec_receive_packet(v->ctx, v->pkt);
        if (r == AVERROR(EAGAIN) || r == AVERROR_EOF) break;
        if (r < 0) return 0;
        av_packet_rescale_ts(v->pkt, (AVRational){1, 30}, v->st_tb);
        v->pkt->stream_index = 0;
        int wr = av_write_frame(v->oc, v->pkt);
        av_packet_unref(v->pkt);
        if (wr < 0) return 0;
    }
    avio_flush(v->avio);
    return 1;
}

static void venc_close(Venc *v) {
    av_packet_free(&v->pkt);
    av_frame_free(&v->frame);
    sws_freeContext(v->sws);
    avformat_free_context(v->oc);
    if (v->avio) {
        av_freep(&v->avio->buffer);
        avio_context_free(&v->avio);
    }
    avcodec_free_context(&v->ctx);
    free(v->nv12_uv);
}

static uint8_t *load(const char *path, size_t bytes) {
    FILE *f = fopen(path, "rb");
    if (!f) die("open input");
    uint8_t *data = malloc(bytes);
    if (!data) die("input alloc");
    if (fread(data, 1, bytes, f) != bytes) die("short input");
    fclose(f);
    return data;
}

static uint64_t now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * UINT64_C(1000000000) + (uint64_t)t.tv_nsec;
}

static void session(int w, int h, int flip, int frames, int64_t bitrate, const uint8_t *rgba,
                    Sink *sink) {
    Venc v;
    size_t frame_bytes = (size_t)w * (size_t)h * 4;
    venc_open(&v, w, h, bitrate, sink);
    for (int i = 0; i < frames; i++)
        if (!venc_encode(&v, rgba + (size_t)i * frame_bytes, flip)) die("encode");
    venc_close(&v);
}

static void write_marker(const char *text) {
    size_t n = strlen(text);
    if (write(STDOUT_FILENO, text, n) != (ssize_t)n) die("marker");
}

static int bench(char **argv) {
    int iterations = atoi(argv[3]);
    int w = atoi(argv[4]), h = atoi(argv[5]), flip = atoi(argv[6]), frames = atoi(argv[7]);
    int64_t bitrate = strtoll(argv[8], NULL, 10);
    if (iterations <= 0) die("usage");
    if (w <= 0 || h <= 0 || (w & 1) || (h & 1) || frames <= 0) die("geometry");
    size_t frame_bytes = (size_t)w * (size_t)h * 4;
    uint8_t *rgba = load(argv[2], frame_bytes * (size_t)frames);
    uint64_t bytes = 0, digest = 0;
    uint64_t start = now_ns();
    for (int i = 0; i < iterations; i++) {
        Sink sink = {NULL, UINT64_C(14695981039346656037), 0, 0};
        session(w, h, flip, frames, bitrate, rgba, &sink);
        if (i == 0) {
            bytes = sink.bytes;
            digest = sink.hash;
        } else if (sink.bytes != bytes || sink.hash != digest) {
            die("output differs between iterations");
        }
    }
    uint64_t elapsed = now_ns() - start;
    printf("{\"iterations\":%d,\"elapsed_ns\":%" PRIu64 ",\"packets\":%d,\"bytes\":%" PRIu64
           ",\"digest\":%" PRIu64 "}\n",
           iterations, elapsed, frames, bytes, digest);
    free(rgba);
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 9 && !strcmp(argv[1], "bench")) return bench(argv);
    if (argc < 7) die("usage");
    const char *mode = argv[1];
    int w = atoi(argv[2]), h = atoi(argv[3]), flip = atoi(argv[4]), frames = atoi(argv[5]);
    if (w <= 0 || h <= 0 || (w & 1) || (h & 1) || frames <= 0) die("geometry");
    size_t frame_bytes = (size_t)w * (size_t)h * 4;
    if (!strcmp(mode, "convert") && argc == 8) {
        uint8_t *rgba = load(argv[6], frame_bytes * (size_t)frames);
        FILE *out = fopen(argv[7], "wb");
        if (!out) die("open output");
        Sink sink = {NULL, UINT64_C(14695981039346656037), 0, 0};
        Venc v;
        venc_open(&v, w, h, 2500000, &sink);
        for (int i = 0; i < frames; i++) {
            if (av_frame_make_writable(v.frame) < 0) die("writable");
            convert_frame(&v, rgba + (size_t)i * frame_bytes, flip);
            for (int p = 0; p < 3; p++) {
                int pw = p ? w / 2 : w, ph = p ? h / 2 : h;
                for (int y = 0; y < ph; y++)
                    fwrite(v.frame->data[p] + (size_t)y * (size_t)v.frame->linesize[p], 1,
                           (size_t)pw, out);
            }
        }
        venc_close(&v);
        fclose(out);
        free(rgba);
        return 0;
    }
    if (argc < 8) die("usage");
    int64_t bitrate = strtoll(argv[6], NULL, 10);
    uint8_t *rgba = load(argv[7], frame_bytes * (size_t)frames);
    if (!strcmp(mode, "pipeline") && argc == 9) {
        Sink sink = {fopen(argv[8], "wb"), UINT64_C(14695981039346656037), 0, 0};
        if (!sink.file) die("open output");
        session(w, h, flip, frames, bitrate, rgba, &sink);
        fclose(sink.file);
        printf("{\"bytes\":%" PRIu64 ",\"writes\":%" PRIu64 ",\"fnv1a64\":\"%016" PRIx64 "\"}\n",
               sink.bytes, sink.writes, sink.hash);
    } else if (!strcmp(mode, "memory") && argc == 8) {
        Sink sink = {NULL, UINT64_C(14695981039346656037), 0, 0};
        fflush(stdout);
        write_marker("TRIERGON_BEGIN_V1\n");
        session(w, h, flip, frames, bitrate, rgba, &sink);
        write_marker("TRIERGON_END_V1\n");
        char line[128];
        int n = snprintf(line, sizeof line, "{\"bytes\":%" PRIu64 ",\"fnv1a64\":\"%016" PRIx64 "\"}\n",
                         sink.bytes, sink.hash);
        if (write(STDOUT_FILENO, line, (size_t)n) != n) die("result");
    } else {
        die("usage");
    }
    free(rgba);
    return 0;
}
