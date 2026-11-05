/*
 * Hardware-acceleration evaluation worker (research tooling only; never linked
 * into production Rust). Same command line as harness/capture/reference_capture.c
 * so harness/capture/compare.py can grade it, plus a `phases` mode.
 *
 * CAPTURE_CONV selects the RGBA -> YUV 4:2:0 path in front of the unchanged
 * software mpeg1video encoder and mpegts muxer (no MPEG-1 hardware encoder
 * exists on the evaluated devices; see research/capture/hw/README.md):
 *   sws          Streem's path: sws_scale RGBA -> NV12 (SWS_BILINEAR), U/V split
 *   vulkan       [CPU row flip] hwupload -> scale_vulkan=format=yuv444p:out_range=limited
 *                (BT.601 input colorspace, GPU) -> hwdownload -> swscale
 *                4:4:4 -> 4:2:0 (CPU; the 9.0.2 Vulkan backend cannot
 *                subsample). Frames stay in CPU memory before and after
 *                (required readback for the CPU encoder).
 *   vulkan-copy  hwupload -> hwdownload of the RGBA frame only (transfer cost;
 *                output is NOT a valid conversion, phases mode only)
 *   placebo      [CPU row flip] hwupload -> libplacebo=format=yuv420p (BT.601 limited, no
 *                dithering; matrix and 4:2:0 subsampling both on the GPU) ->
 *                hwdownload. Needs a build with --enable-libplacebo.
 * CAPTURE_VK_DIRECT_FORMAT=<fmt> (vulkan only) is a capability probe: ask
 * scale_vulkan for <fmt> directly instead of 4:4:4 + CPU subsampling.
 * CAPTURE_VK_DEVICE names the Vulkan device; CAPTURE_FFMPEG_LOG=verbose shows
 * the device FFmpeg selected.
 *
 *   hw_capture convert  <w> <h> <flip> <frames> <in.rgba> <out.yuv>
 *   hw_capture pipeline <w> <h> <flip> <frames> <bitrate> <in.rgba> <out.ts>
 *   hw_capture phases   <w> <h> <flip> <frames> <bitrate> <in.rgba> <iterations>
 *   hw_capture memory   <w> <h> <flip> <frames> <bitrate> <in.rgba>
 *
 * phases: one JSON line per iteration. Process-level device creation
 * (startup_device_ns) happens once before the first session; each iteration is
 * open -> frames -> drain -> reset (close + reopen) -> close, with the device
 * released at the end of the last iteration (shutdown_device_ns).
 */
#ifdef __APPLE__
#define _DARWIN_C_SOURCE 1
#else
#define _GNU_SOURCE 1
#endif
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>

#include <libavcodec/avcodec.h>
#include <libavfilter/avfilter.h>
#include <libavfilter/buffersink.h>
#include <libavfilter/buffersrc.h>
#include <libavformat/avformat.h>
#include <libavutil/hwcontext.h>
#include <libavutil/opt.h>
#include <libswscale/swscale.h>

enum conv { CONV_SWS, CONV_VULKAN, CONV_VULKAN_COPY, CONV_PLACEBO };

typedef struct {
    FILE *file;
    uint64_t hash, bytes;
} Sink;

static int sink_write(void *opaque, const uint8_t *data, int n) {
    Sink *s = opaque;
    if (n <= 0) return -1;
    for (int i = 0; i < n; i++) {
        s->hash ^= data[i];
        s->hash *= UINT64_C(1099511628211);
    }
    s->bytes += (uint64_t)n;
    if (s->file && fwrite(data, 1, (size_t)n, s->file) != (size_t)n) return -1;
    return n;
}

static void die(const char *what) {
    fprintf(stderr, "hw_capture: %s\n", what);
    exit(2);
}

static uint64_t now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * UINT64_C(1000000000) + (uint64_t)t.tv_nsec;
}

static uint64_t cpu_ns(int which) {
    struct rusage u;
    getrusage(RUSAGE_SELF, &u);
    struct timeval t = which ? u.ru_stime : u.ru_utime;
    return (uint64_t)t.tv_sec * UINT64_C(1000000000) + (uint64_t)t.tv_usec * 1000;
}

#ifdef __APPLE__
#include <mach/mach.h>
#endif

/* Peak physical footprint (macOS: includes IOSurface/GPU memory attributed to
 * the process); 0 where unavailable. */
static uint64_t footprint_peak(void) {
#ifdef __APPLE__
    task_vm_info_data_t info;
    mach_msg_type_number_t count = TASK_VM_INFO_COUNT;
    if (task_info(mach_task_self(), TASK_VM_INFO, (task_info_t)&info, &count) == KERN_SUCCESS)
        return info.ledger_phys_footprint_peak;
#endif
    return 0;
}

static void marker(const char *text) {
    size_t n = strlen(text);
    if (write(STDOUT_FILENO, text, n) != (ssize_t)n) die("marker");
}

static uint64_t maxrss_bytes(void) {
    struct rusage u;
    getrusage(RUSAGE_SELF, &u);
#ifdef __APPLE__
    return (uint64_t)u.ru_maxrss;
#else
    return (uint64_t)u.ru_maxrss * 1024;
#endif
}

typedef struct {
    int w, h, flip;
    enum conv conv;
    AVBufferRef *device;
    AVCodecContext *ctx;
    AVFrame *frame;   /* encoder input (YUV420P, CPU) */
    AVFrame *rgba;    /* graph input (CPU) */
    AVFrame *out;     /* graph output (CPU) */
    struct SwsContext *sws;
    uint8_t *nv12_uv;
    AVFilterGraph *graph;
    AVFilterContext *src, *sinkf;
    AVFormatContext *oc;
    AVIOContext *avio;
    AVPacket *pkt;
    AVRational st_tb;
    int64_t pts;
    int flushed;      /* graph input closed (end of stream) */
    int64_t in_pts;   /* graph input timestamps (the output may lag) */
    AVBufferPool *flippool; /* bottom-up rows reversed on the CPU for the GPU paths */
} Venc;

static void set(AVCodecContext *ctx, const char *name, const char *value) {
    if (av_opt_set(ctx, name, value, 0) < 0) die(name);
}

/* Create, configure (hardware device before init) and link one filter. */
static AVFilterContext *add(Venc *v, AVFilterContext *prev, const char *name, const char *args) {
    AVFilterContext *f = avfilter_graph_alloc_filter(v->graph, avfilter_get_by_name(name), name);
    if (!f) die(name);
    if (v->device && strncmp(name, "buffer", 6) && strcmp(name, "format"))
        f->hw_device_ctx = av_buffer_ref(v->device);
    if (avfilter_init_str(f, args) < 0) die(name);
    if (prev && avfilter_link(prev, 0, f, 0) < 0) die("link");
    return f;
}

/* The capture contract ignores alpha. swscale and scale_vulkan never read it
 * for RGB -> YUV; libplacebo would composite it, so its input is declared
 * RGB0 (identical bytes, alpha ignored). */
static enum AVPixelFormat in_format(const Venc *v) {
    return v->conv == CONV_PLACEBO ? AV_PIX_FMT_RGB0 : AV_PIX_FMT_RGBA;
}

static void graph_open(Venc *v) {
    char args[256];
    v->graph = avfilter_graph_alloc();
    if (!v->graph) die("graph");
    snprintf(args, sizeof args,
             "video_size=%dx%d:pix_fmt=%d:time_base=1/30:frame_rate=30/1:pixel_aspect=1/1:colorspace=bt470bg:range=pc",
             v->w, v->h, in_format(v));
    v->src = add(v, NULL, "buffer", args);
    AVFilterContext *f = add(v, v->src, "hwupload", NULL);
    const char *direct = getenv("CAPTURE_VK_DIRECT_FORMAT");
    if (v->conv == CONV_VULKAN && direct) {
        /* Capability probe only: ask the GPU for 4:2:0 output directly. */
        char opt[64];
        snprintf(opt, sizeof opt, "format=%s:out_range=limited", direct);
        f = add(v, f, "scale_vulkan", opt);
        f = add(v, f, "hwdownload", NULL);
        snprintf(opt, sizeof opt, "pix_fmts=%s", direct);
        f = add(v, f, "format", opt);
    } else if (v->conv == CONV_PLACEBO) {
        f = add(v, f, "libplacebo", "format=yuv420p:colorspace=bt470bg:range=tv:dithering=none");
        f = add(v, f, "hwdownload", NULL);
        f = add(v, f, "format", "pix_fmts=yuv420p");
    } else if (v->conv == CONV_VULKAN) {
        /* FFmpeg 9.0.2's swscale Vulkan backend has no 4:2:0 subsampling op
         * (yuv420p/nv12 fail with ENOTSUP), so the GPU applies the BT.601
         * limited-range matrix at 4:4:4 and the CPU (swscale) subsamples. */
        f = add(v, f, "scale_vulkan", "format=yuv444p:out_range=limited");
        f = add(v, f, "hwdownload", NULL);
        f = add(v, f, "format", "pix_fmts=yuv444p");
        f = add(v, f, "scale", "flags=bilinear");
        f = add(v, f, "format", "pix_fmts=yuv420p");
    } else {
        f = add(v, f, "hwdownload", NULL);
        f = add(v, f, "format", "pix_fmts=rgba");
    }
    v->sinkf = add(v, f, "buffersink", NULL);
    if (avfilter_graph_config(v->graph, NULL) < 0) die("graph config");
    v->rgba = av_frame_alloc();
    v->out = av_frame_alloc();
    if (!v->rgba || !v->out) die("frames");
}

static void venc_open(Venc *v, int w, int h, int flip, enum conv conv, AVBufferRef *device, int64_t bitrate, Sink *sink) {
    char buf[64];
    memset(v, 0, sizeof *v);
    v->w = w; v->h = h; v->flip = flip; v->conv = conv; v->device = device;
    const AVCodec *codec = avcodec_find_encoder_by_name("mpeg1video");
    if (!codec) die("mpeg1video encoder absent");
    v->ctx = avcodec_alloc_context3(codec);
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
    v->frame->width = w;
    v->frame->height = h;
    v->frame->format = AV_PIX_FMT_YUV420P;
    if (av_frame_get_buffer(v->frame, 32) < 0) die("frame buffer");
    if (conv == CONV_SWS) {
        v->sws = sws_getContext(w, h, AV_PIX_FMT_RGBA, w, h, AV_PIX_FMT_NV12, SWS_BILINEAR, NULL, NULL, NULL);
        v->nv12_uv = malloc((size_t)w * (size_t)h / 2);
        if (!v->sws || !v->nv12_uv) die("sws");
    } else {
        graph_open(v);
        /* Bottom-up input on the GPU paths: hwupload crashes on swscale's
         * negative-stride flip, and 9.0.2's vflip_vulkan shader is off by one
         * row (writes h - y, not h - 1 - y), so the rows are reversed on the
         * CPU before upload; the copy is part of the timed conversion. */
        /* Pooled, reference-counted buffers: a GPU upload may alias host
         * memory (MoltenVK) and libplacebo holds one frame back, so a buffer
         * is reused only after FFmpeg releases it. */
        if (flip && (conv == CONV_VULKAN || conv == CONV_PLACEBO) &&
            !(v->flippool = av_buffer_pool_init((size_t)w * (size_t)h * 4, NULL)))
            die("flip pool");
    }
    if (avformat_alloc_output_context2(&v->oc, NULL, "mpegts", NULL) < 0) die("mpegts");
    uint8_t *iobuf = av_malloc(32768);
    v->avio = avio_alloc_context(iobuf, 32768, 1, sink, NULL, sink_write, NULL);
    v->oc->pb = v->avio;
    AVStream *st = avformat_new_stream(v->oc, NULL);
    if (!st || avcodec_parameters_from_context(st->codecpar, v->ctx) < 0) die("stream");
    if (avformat_write_header(v->oc, NULL) < 0) die("write_header");
    v->st_tb = st->time_base;
    v->pkt = av_packet_alloc();
}

static void borrowed(void *opaque, uint8_t *data) {
    (void)opaque;
    (void)data;
}

/* Graph output -> v->frame. Returns 0 when the graph holds the frame back
 * (vf_libplacebo's queue needs the next input before it emits a frame). */
static int take(Venc *v) {
    av_frame_unref(v->out);
    int got = av_buffersink_get_frame(v->sinkf, v->out);
    if (got == AVERROR(EAGAIN) || got == AVERROR_EOF) return 0;
    if (got < 0) {
        fprintf(stderr, "hw_capture: buffersink get: %s\n", av_err2str(got));
        exit(2);
    }
    if (v->conv == CONV_VULKAN || v->conv == CONV_PLACEBO) {
        for (int p = 0; p < 3; p++) {
            int pw = p ? v->w / 2 : v->w, ph = p ? v->h / 2 : v->h;
            for (int y = 0; y < ph; y++)
                memcpy(v->frame->data[p] + (size_t)y * (size_t)v->frame->linesize[p],
                       v->out->data[p] + (size_t)y * (size_t)v->out->linesize[p], (size_t)pw);
        }
    }
    return 1;
}

/* End of stream for the graph: returns 1 per frame still held back. */
static int convert_flush(Venc *v) {
    if (v->conv == CONV_SWS) return 0;
    if (!v->flushed) {
        if (av_buffersrc_add_frame(v->src, NULL) < 0) die("buffersrc eof");
        v->flushed = 1;
    }
    if (av_frame_make_writable(v->frame) < 0) die("writable");
    return take(v);
}

/* RGBA -> v->frame (YUV420P, CPU). Returns 1 when v->frame holds a converted
 * frame (always for sws; graph paths may emit one frame late). */
static int convert_frame(Venc *v, const uint8_t *rgba) {
    int row = v->w * 4;
    if (av_frame_make_writable(v->frame) < 0) die("writable");
    if (v->conv == CONV_SWS) {
        const uint8_t *src[4] = {v->flip ? rgba + (size_t)(v->h - 1) * (size_t)row : rgba, NULL, NULL, NULL};
        int src_stride[4] = {v->flip ? -row : row, 0, 0, 0};
        uint8_t *dst[4] = {v->frame->data[0], v->nv12_uv, NULL, NULL};
        int dst_stride[4] = {v->frame->linesize[0], v->w, 0, 0};
        sws_scale(v->sws, src, src_stride, 0, v->h, dst, dst_stride);
        for (int y = 0; y < v->h / 2; y++) {
            const uint8_t *s = v->nv12_uv + (size_t)y * (size_t)v->w;
            uint8_t *up = v->frame->data[1] + (size_t)y * (size_t)v->frame->linesize[1];
            uint8_t *vp = v->frame->data[2] + (size_t)y * (size_t)v->frame->linesize[2];
            for (int i = 0; i < v->w / 2; i++) {
                up[i] = s[2 * i];
                vp[i] = s[2 * i + 1];
            }
        }
        return 1;
    }
    /* Wrap the caller's CPU frame without copying (non-owning reference); the
     * graph's hwupload performs the host -> device transfer. */
    av_frame_unref(v->rgba);
    v->rgba->width = v->w;
    v->rgba->height = v->h;
    v->rgba->format = in_format(v);
    v->rgba->colorspace = AVCOL_SPC_BT470BG;
    v->rgba->color_range = AVCOL_RANGE_JPEG;
    v->rgba->pts = v->in_pts++;
    v->rgba->duration = 1;
    if (v->flippool) {
        v->rgba->buf[0] = av_buffer_pool_get(v->flippool);
        if (!v->rgba->buf[0]) die("flip buffer");
        for (int y = 0; y < v->h; y++)
            memcpy(v->rgba->buf[0]->data + (size_t)y * (size_t)row, rgba + (size_t)(v->h - 1 - y) * (size_t)row, (size_t)row);
    } else {
        v->rgba->buf[0] = av_buffer_create((uint8_t *)rgba, (size_t)row * (size_t)v->h, borrowed, NULL, AV_BUFFER_FLAG_READONLY);
        if (!v->rgba->buf[0]) die("rgba buffer");
    }
    v->rgba->data[0] = v->rgba->buf[0]->data;
    v->rgba->linesize[0] = row;
    if (av_buffersrc_add_frame_flags(v->src, v->rgba, AV_BUFFERSRC_FLAG_KEEP_REF) < 0) die("buffersrc add");
    return take(v);
}

static int write_packets(Venc *v) {
    for (;;) {
        int r = avcodec_receive_packet(v->ctx, v->pkt);
        if (r == AVERROR(EAGAIN) || r == AVERROR_EOF) return 1;
        if (r < 0) return 0;
        av_packet_rescale_ts(v->pkt, (AVRational){1, 30}, v->st_tb);
        v->pkt->stream_index = 0;
        int wr = av_write_frame(v->oc, v->pkt);
        av_packet_unref(v->pkt);
        if (wr < 0) return 0;
    }
}

static void encode_frame(Venc *v) {
    v->frame->pts = v->pts++;
    if (avcodec_send_frame(v->ctx, v->frame) < 0 || !write_packets(v)) die("encode");
    avio_flush(v->avio);
}

static void drain(Venc *v) {
    if (avcodec_send_frame(v->ctx, NULL) < 0 || !write_packets(v)) die("drain");
    avio_flush(v->avio);
}

static void venc_close(Venc *v) {
    av_packet_free(&v->pkt);
    av_frame_free(&v->frame);
    av_frame_free(&v->rgba);
    av_frame_free(&v->out);
    sws_freeContext(v->sws);
    avfilter_graph_free(&v->graph);
    avformat_free_context(v->oc);
    if (v->avio) {
        av_freep(&v->avio->buffer);
        avio_context_free(&v->avio);
    }
    avcodec_free_context(&v->ctx);
    free(v->nv12_uv);
    av_buffer_pool_uninit(&v->flippool);
}

static uint8_t *load(const char *path, size_t bytes) {
    FILE *f = fopen(path, "rb");
    if (!f) die("open input");
    uint8_t *data = malloc(bytes);
    if (!data || fread(data, 1, bytes, f) != bytes) die("short input");
    fclose(f);
    return data;
}

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

static void stats(const char *name, uint64_t *v, int n, int last) {
    uint64_t *s = malloc(sizeof *s * (size_t)n), sum = 0;
    memcpy(s, v, sizeof *s * (size_t)n);
    qsort(s, (size_t)n, sizeof *s, cmp_u64);
    for (int i = 0; i < n; i++) sum += s[i];
    printf("\"%s\":{\"sum\":%" PRIu64 ",\"median\":%" PRIu64 ",\"p95\":%" PRIu64 ",\"max\":%" PRIu64 "}%s", name, sum,
           s[n / 2], s[(n * 95) / 100 < n ? (n * 95) / 100 : n - 1], s[n - 1], last ? "" : ",");
    free(s);
}

int main(int argc, char **argv) {
    if (argc < 7) die("usage");
    const char *conv_name = getenv("CAPTURE_CONV") ? getenv("CAPTURE_CONV") : "sws";
    enum conv conv = !strcmp(conv_name, "vulkan") ? CONV_VULKAN
                   : !strcmp(conv_name, "vulkan-copy") ? CONV_VULKAN_COPY
                   : !strcmp(conv_name, "placebo") ? CONV_PLACEBO : CONV_SWS;
    const char *log = getenv("CAPTURE_FFMPEG_LOG");
    av_log_set_level(log && !strcmp(log, "verbose") ? AV_LOG_VERBOSE : AV_LOG_ERROR);
    const char *mode = argv[1];
    int w = atoi(argv[2]), h = atoi(argv[3]), flip = atoi(argv[4]), frames = atoi(argv[5]);
    size_t fb = (size_t)w * (size_t)h * 4;
    int memory = !strcmp(mode, "memory");
    uint8_t *preloaded = NULL;
    if (memory) {
        /* Input frames are resident before the measured window, as for every
         * worker; the window spans device startup to device shutdown. */
        if (argc != 8) die("usage");
        preloaded = load(argv[7], fb * (size_t)frames);
        fflush(stdout);
        marker("SAFE_MEDIA_BEGIN_V1\n");
    }
    uint64_t t_dev0 = now_ns();
    AVBufferRef *device = NULL;
    /* CAPTURE_VK_DEVICE: Vulkan device index or name substring (FFmpeg's device
     * string). The evaluation always names the GPU explicitly so a software
     * rasteriser (e.g. Mesa llvmpipe) can never stand in for it. */
    if (conv != CONV_SWS && strcmp(mode, "load") &&
        av_hwdevice_ctx_create(&device, AV_HWDEVICE_TYPE_VULKAN, getenv("CAPTURE_VK_DEVICE"), NULL, 0) < 0)
        die("vulkan device");
    uint64_t startup_device = now_ns() - t_dev0;
    Sink sink = {NULL, UINT64_C(14695981039346656037), 0};
    if (!strcmp(mode, "convert") && argc == 8) {
        uint8_t *rgba = load(argv[6], fb * (size_t)frames);
        FILE *out = fopen(argv[7], "wb");
        Venc v;
        venc_open(&v, w, h, flip, conv, device, 2500000, &sink);
        for (int i = 0; i <= frames; i++) {
            while (i < frames ? convert_frame(&v, rgba + (size_t)i * fb) : convert_flush(&v)) {
                for (int p = 0; p < 3; p++)
                    for (int y = 0; y < (p ? h / 2 : h); y++)
                        fwrite(v.frame->data[p] + (size_t)y * (size_t)v.frame->linesize[p], 1, (size_t)(p ? w / 2 : w), out);
                if (i < frames) break;
            }
        }
        venc_close(&v);
        fclose(out);
        return 0;
    }
    if (argc < 8) die("usage");
    int64_t bitrate = strtoll(argv[6], NULL, 10);
    uint8_t *rgba = preloaded ? preloaded : load(argv[7], fb * (size_t)frames);
    if (!strcmp(mode, "load")) {
        printf("{\"loaded\":%zu}\n", fb * (size_t)frames);
        return 0;
    }
    if (!strcmp(mode, "pipeline") && argc == 9) {
        sink.file = fopen(argv[8], "wb");
        Venc v;
        venc_open(&v, w, h, flip, conv, device, bitrate, &sink);
        for (int i = 0; i < frames; i++)
            if (convert_frame(&v, rgba + (size_t)i * fb)) encode_frame(&v);
        while (convert_flush(&v)) encode_frame(&v);
        venc_close(&v);
        fclose(sink.file);
        printf("{\"bytes\":%" PRIu64 ",\"fnv1a64\":\"%016" PRIx64 "\"}\n", sink.bytes, sink.hash);
    } else if (!strcmp(mode, "phases") && argc == 9) {
        int iterations = atoi(argv[8]);
        uint64_t *conv_ns = malloc(sizeof *conv_ns * (size_t)frames), *enc_ns = malloc(sizeof *enc_ns * (size_t)frames),
                 *frame_ns = malloc(sizeof *frame_ns * (size_t)frames);
        for (int it = 0; it < iterations; it++) {
            sink.hash = UINT64_C(14695981039346656037);
            sink.bytes = 0;
            uint64_t u0 = cpu_ns(0), s0 = cpu_ns(1), t0 = now_ns();
            Venc v;
            venc_open(&v, w, h, flip, conv, device, bitrate, &sink);
            uint64_t t1 = now_ns();
            for (int i = 0; i < frames; i++) {
                uint64_t a = now_ns();
                int ready = convert_frame(&v, rgba + (size_t)i * fb);
                uint64_t b = now_ns();
                if (ready) encode_frame(&v);
                uint64_t c = now_ns();
                conv_ns[i] = b - a;
                enc_ns[i] = c - b;
                frame_ns[i] = c - a;
            }
            /* Frames a graph held back (libplacebo: the last one) are emitted
             * and encoded here; the cost is charged to the last frame. */
            for (;;) {
                uint64_t a = now_ns();
                int more = convert_flush(&v);
                uint64_t b = now_ns();
                if (more) encode_frame(&v);
                uint64_t c = now_ns();
                conv_ns[frames - 1] += b - a;
                enc_ns[frames - 1] += c - b;
                frame_ns[frames - 1] += c - a;
                if (!more) break;
            }
            uint64_t t2 = now_ns();
            drain(&v);
            uint64_t t3 = now_ns();
            venc_close(&v);
            venc_open(&v, w, h, flip, conv, device, bitrate, &sink);
            uint64_t t4 = now_ns();
            venc_close(&v);
            uint64_t t5 = now_ns(), shutdown_device = 0;
            if (it == iterations - 1 && device) {
                av_buffer_unref(&device);
                shutdown_device = now_ns() - t5;
            }
            uint64_t u1 = cpu_ns(0), s1 = cpu_ns(1);
            printf("{\"conv\":\"%s\",\"startup_device_ns\":%" PRIu64 ",\"open_ns\":%" PRIu64 ",\"frames_ns\":%" PRIu64
                   ",\"drain_ns\":%" PRIu64 ",\"reset_ns\":%" PRIu64 ",\"close_ns\":%" PRIu64 ",\"shutdown_device_ns\":%" PRIu64
                   ",\"session_ns\":%" PRIu64 ",\"cpu_user_ns\":%" PRIu64 ",\"cpu_sys_ns\":%" PRIu64 ",\"maxrss\":%" PRIu64
                   ",\"footprint_peak\":%" PRIu64 ",\"bytes\":%" PRIu64 ",\"fnv1a64\":\"%016" PRIx64 "\",",
                   conv_name, it == 0 ? startup_device : 0, t1 - t0, t2 - t1, t3 - t2, t4 - t3, t5 - t4, shutdown_device,
                   t5 - t0 + shutdown_device, u1 - u0, s1 - s0, maxrss_bytes(), footprint_peak(), sink.bytes, sink.hash);
            stats("convert", conv_ns, frames, 0);
            stats("encode_mux", enc_ns, frames, 0);
            stats("frame", frame_ns, frames, 1);
            printf("}\n");
            fflush(stdout);
        }
    } else if (!strcmp(mode, "memory") && argc == 8) {
        Venc v;
        venc_open(&v, w, h, flip, conv, device, bitrate, &sink);
        for (int i = 0; i < frames; i++)
            if (convert_frame(&v, rgba + (size_t)i * fb)) encode_frame(&v);
        while (convert_flush(&v)) encode_frame(&v);
        drain(&v);
        venc_close(&v);
        av_buffer_unref(&device);
        marker("SAFE_MEDIA_END_V1\n");
        char line[192];
        int n = snprintf(line, sizeof line, "{\"bytes\":%" PRIu64 ",\"fnv1a64\":\"%016" PRIx64 "\",\"maxrss\":%" PRIu64
                         ",\"footprint_peak\":%" PRIu64 "}\n", sink.bytes, sink.hash, maxrss_bytes(), footprint_peak());
        if (write(STDOUT_FILENO, line, (size_t)n) != n) die("result");
    } else {
        die("usage");
    }
    return 0;
}
