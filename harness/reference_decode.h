/* Decode reference: the decode build of harness/reference.c (tooling only).
 *
 * Reproduces the simulator's external ffmpeg work in-process with the same
 * libraries: an elementary-stream parser splits access units, the decoder
 * decodes them and the exact filter chain converts the output:
 *   video: scale=1920:1080:force_original_aspect_ratio=decrease,
 *          pad=1920:1080:(ow-iw)/2:(oh-ih)/2,format=rgba   (passthrough cadence)
 *   audio: aformat=sample_fmts=s16:channel_layouts=stereo  (what -ac 2 to a
 *          pulse/alsa s16 sink negotiates; libavfilter inserts the resampler)
 *
 * usage: reference KIND-CODEC-bench FILE ITERATIONS | KIND-CODEC-memory FILE
 *        | KIND-CODEC-raw FILE OUT   (KIND-CODEC: video-h264 video-hevc audio-ac3
 *        audio-eac3 audio-aac audio-dts audio-flac audio-mp3 audio-vorbis audio-opus);
 *        the same protocol as crates/codec-worker.
 * FLAC, MPEG audio, Vorbis and Opus come in a file format (native FLAC, raw MPEG
 * audio, Ogg), so their lanes read it through the demuxer the CLI uses and decode
 * with the decoder it picks for the stream, except MPEG audio Layer I and II, which
 * use the float decoders (rule mpeg-audio-float-decoders in
 * harness/codecs/numeric-rules.json); packet trims (skip samples, end padding) apply
 * as in the CLI. The audio chain is the CLI's, so it mixes whatever sample format the
 * decoder outputs: FLAC of up to 16 bits arrives as s16 and is mixed in integer
 * (rule flac-integer-device-mix; harness/codecs/downmix-integer.md).
 * The whole operation (parser/decoder/filter setup, decode, convert, teardown)
 * is inside the timed interval; file loading is outside, as in the other workers.
 */
#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <libavcodec/avcodec.h>
#include <libavfilter/avfilter.h>
#include <libavfilter/buffersink.h>
#include <libavfilter/buffersrc.h>
#include <libavformat/avformat.h>
#include <libavutil/channel_layout.h>
#include <libavutil/opt.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000u + (uint64_t)ts.tv_nsec;
}

/* Output digest, identical in crates/codec-worker: eight independent lanes, each
 * lane_i <- (lane_i ^ le64(word 8k+i)) * K over the concatenated output in 64-byte
 * blocks, a zero-padded final block, then h <- 31*h + lane_i and the byte count.
 * Every output byte enters it, at about 1% of the decode cost (a serial byte
 * fold would cost 9 ms per 1080p RGBA picture on both sides and swamp the work). */
#define FOLD_K 0x9E3779B97F4A7C15ull
struct fold {
    uint64_t lane[8], total;
    uint8_t pending[64];
    size_t npending;
};

static void fold_block(struct fold *d, const uint8_t *p) {
    for (int i = 0; i < 8; i++) {
        uint64_t w = 0;
        for (int b = 7; b >= 0; b--) w = w << 8 | p[i * 8 + b];
        d->lane[i] = (d->lane[i] ^ w) * FOLD_K;
    }
}

static void fold(struct fold *d, const uint8_t *p, size_t n) {
    d->total += n;
    if (d->npending) {
        size_t take = 64 - d->npending < n ? 64 - d->npending : n;
        memcpy(d->pending + d->npending, p, take);
        d->npending += take, p += take, n -= take;
        if (d->npending < 64) return;
        fold_block(d, d->pending);
        d->npending = 0;
    }
    for (; n >= 64; p += 64, n -= 64) fold_block(d, p);
    memcpy(d->pending, p, n);
    d->npending = n;
}

static uint64_t fold_finish(struct fold d) {
    if (d.npending) {
        memset(d.pending + d.npending, 0, 64 - d.npending);
        fold_block(&d, d.pending);
    }
    uint64_t h = 0;
    for (int i = 0; i < 8; i++) h = h * 31u + d.lane[i];
    return h * 31u + d.total;
}

static void die(const char *what, int err) {
    char buf[128] = "";
    av_strerror(err, buf, sizeof buf);
    fprintf(stderr, "%s: %s\n", what, buf);
    exit(1);
}

struct out {
    uint64_t frames, bytes;
    struct fold digest;
    FILE *raw;
    uint8_t aac_speakers[8];
    unsigned aac_channels;
};

#include "reference_aac_access_units.h"

static void sink_frames(AVFilterContext *sink, struct out *o, int video) {
    AVFrame *f = av_frame_alloc();
    for (;;) {
        int r = av_buffersink_get_frame(sink, f);
        if (r == AVERROR(EAGAIN) || r == AVERROR_EOF) break;
        if (r < 0) die("buffersink", r);
        if (video) {
            for (int y = 0; y < f->height; y++) {
                const uint8_t *row = f->data[0] + (ptrdiff_t)y * f->linesize[0];
                fold(&o->digest, row, (size_t)f->width * 4);
                if (o->raw) fwrite(row, 1, (size_t)f->width * 4, o->raw);
            }
            o->bytes += (uint64_t)f->width * 4 * f->height;
        } else {
            size_t n = (size_t)f->nb_samples * 2 * sizeof(int16_t);
            fold(&o->digest, f->data[0], n);
            if (o->raw) fwrite(f->data[0], 1, n, o->raw);
            o->bytes += n;
        }
        o->frames++;
        av_frame_unref(f);
    }
    av_frame_free(&f);
}

static AVFilterGraph *graph_for(AVCodecContext *dec, const AVFrame *first, int video,
                                AVFilterContext **src, AVFilterContext **sink) {
    AVFilterGraph *g = avfilter_graph_alloc();
    /* One thread, like the decoder: the comparison is single-core equivalent work
     * (the CLI would otherwise slice-thread scale/aresample across all CPUs). */
    g->nb_threads = 1;
    char args[512];
    int r;
    if (video) {
        snprintf(args, sizeof args, "video_size=%dx%d:pix_fmt=%d:time_base=1/90000:pixel_aspect=%d/%d:colorspace=%d:range=%d",
                 first->width, first->height, first->format,
                 first->sample_aspect_ratio.num ? first->sample_aspect_ratio.num : 1,
                 first->sample_aspect_ratio.den ? first->sample_aspect_ratio.den : 1,
                 first->colorspace, first->color_range);
        if ((r = avfilter_graph_create_filter(src, avfilter_get_by_name("buffer"), "in", args, NULL, g)) < 0) die("buffer", r);
        if ((r = avfilter_graph_create_filter(sink, avfilter_get_by_name("buffersink"), "out", NULL, NULL, g)) < 0) die("sink", r);
    } else {
        char layout[64];
        av_channel_layout_describe(&first->ch_layout, layout, sizeof layout);
        snprintf(args, sizeof args, "sample_rate=%d:sample_fmt=%s:channel_layout=%s:time_base=1/%d",
                 first->sample_rate, av_get_sample_fmt_name(first->format), layout, first->sample_rate);
        if ((r = avfilter_graph_create_filter(src, avfilter_get_by_name("abuffer"), "in", args, NULL, g)) < 0) die("abuffer", r);
        if ((r = avfilter_graph_create_filter(sink, avfilter_get_by_name("abuffersink"), "out", NULL, NULL, g)) < 0) die("asink", r);
    }
    (void)dec;
    /* As the ffmpeg CLI does, hand the first frame's side data (e.g. the AC-3
     * downmix levels) to the source before configuring, so auto-inserted
     * conversions see the same parameters. */
    AVBufferSrcParameters *par = av_buffersrc_parameters_alloc();
    par->side_data = first->side_data;
    par->nb_side_data = first->nb_side_data;
    if ((r = av_buffersrc_parameters_set(*src, par)) < 0) die("buffersrc parameters", r);
    av_free(par);
    AVFilterInOut *outputs = avfilter_inout_alloc(), *inputs = avfilter_inout_alloc();
    outputs->name = av_strdup("in"); outputs->filter_ctx = *src; outputs->pad_idx = 0; outputs->next = NULL;
    inputs->name = av_strdup("out"); inputs->filter_ctx = *sink; inputs->pad_idx = 0; inputs->next = NULL;
    const char *chain = video
        ? "scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,format=rgba"
        : "aformat=sample_fmts=s16:channel_layouts=stereo";
    if ((r = avfilter_graph_parse_ptr(g, chain, &inputs, &outputs, NULL)) < 0) die("graph parse", r);
    if ((r = avfilter_graph_config(g, NULL)) < 0) die("graph config", r);
    avfilter_inout_free(&inputs);
    avfilter_inout_free(&outputs);
    return g;
}

/* The simulator's ffmpeg reads the piped elementary stream through its demuxer and
 * probes it (avformat_find_stream_info) before decoding, then copies the probed codec
 * parameters into the decoder; for H.264/HEVC that includes the reorder delay, without
 * which the decoder drops out-of-order pictures it only discovers mid-stream. Probe
 * once at load, outside the timed interval, and apply the same parameters to every
 * pass (video lanes; see main). */
struct memory_input {
    const uint8_t *data;
    size_t size, at;
};

static int memory_read(void *opaque, uint8_t *buf, int n) {
    struct memory_input *m = opaque;
    size_t left = m->size - m->at;
    if (!left) return AVERROR_EOF;
    if ((size_t)n > left) n = (int)left;
    memcpy(buf, m->data + m->at, (size_t)n);
    m->at += (size_t)n;
    return n;
}

static AVCodecParameters *probe_parameters(const char *demuxer, const uint8_t *data, size_t size) {
    struct memory_input m = {data, size, 0};
    unsigned char *buffer = av_malloc(65536);
    AVIOContext *io = avio_alloc_context(buffer, 65536, 0, &m, memory_read, NULL, NULL);
    AVFormatContext *fmt = avformat_alloc_context();
    fmt->pb = io;
    int r = avformat_open_input(&fmt, NULL, av_find_input_format(demuxer), NULL);
    if (r < 0) die("probe open", r);
    if ((r = avformat_find_stream_info(fmt, NULL)) < 0) die("probe", r);
    if (fmt->nb_streams < 1) die("probe streams", AVERROR_STREAM_NOT_FOUND);
    AVCodecParameters *par = avcodec_parameters_alloc();
    if ((r = avcodec_parameters_copy(par, fmt->streams[0]->codecpar)) < 0) die("probe copy", r);
    avformat_close_input(&fmt);
    av_freep(&io->buffer);
    avio_context_free(&io);
    return par;
}

static void one_pass(const char *codec_name, const AVCodecParameters *par, const uint8_t *data, size_t size, int video,
                     struct out *o) {
    const AVCodec *codec = avcodec_find_decoder_by_name(codec_name);
    if (!codec) die("decoder", AVERROR_DECODER_NOT_FOUND);
    AVCodecParserContext *parser = av_parser_init(codec->id);
    AVCodecContext *dec = avcodec_alloc_context3(codec);
    int r;
    if (par && (r = avcodec_parameters_to_context(dec, par)) < 0) die("parameters", r);
    dec->thread_count = 1;
    if ((r = avcodec_open2(dec, codec, NULL)) < 0) die("open", r);
    AVPacket *pkt = av_packet_alloc();
    AVFrame *frame = av_frame_alloc();
    AVFilterGraph *graph = NULL;
    AVFilterContext *src = NULL, *sink = NULL;
    int64_t pts = 0;
    size_t at = 0;
    for (int flushing = 0; flushing < 2;) {
        int sent = 0;
        if (!flushing) {
            uint8_t *out_data = NULL;
            int out_size = 0;
            int used = av_parser_parse2(parser, dec, &out_data, &out_size, data + at, (int)(size - at),
                                        AV_NOPTS_VALUE, AV_NOPTS_VALUE, 0);
            if (used < 0) die("parse", used);
            at += (size_t)used;
            if (out_size) {
                pkt->data = out_data;
                pkt->size = out_size;
                pkt->pts = pts++;
                if ((r = avcodec_send_packet(dec, pkt)) < 0 && r != AVERROR_INVALIDDATA) die("send", r);
                sent = 1;
            } else if (at >= size) {
                /* Drain the parser once, then the decoder. */
                av_parser_parse2(parser, dec, &out_data, &out_size, NULL, 0, AV_NOPTS_VALUE, AV_NOPTS_VALUE, 0);
                if (out_size) {
                    pkt->data = out_data;
                    pkt->size = out_size;
                    pkt->pts = pts++;
                    avcodec_send_packet(dec, pkt);
                }
                avcodec_send_packet(dec, NULL);
                flushing = 1;
            }
        }
        for (;;) {
            r = avcodec_receive_frame(dec, frame);
            if (r == AVERROR(EAGAIN)) break;
            if (r == AVERROR_EOF) { flushing = 2; break; }
            if (r < 0) die("receive", r);
            if (!graph) graph = graph_for(dec, frame, video, &src, &sink);
            if ((r = av_buffersrc_add_frame_flags(src, frame, AV_BUFFERSRC_FLAG_KEEP_REF)) < 0) die("buffersrc", r);
            av_frame_unref(frame);
            sink_frames(sink, o, video);
        }
        (void)sent;
    }
    if (graph) {
        if ((r = av_buffersrc_add_frame_flags(src, NULL, 0)) < 0) die("buffersrc eof", r);
        sink_frames(sink, o, video);
        avfilter_graph_free(&graph);
    }
    av_frame_free(&frame);
    av_packet_free(&pkt);
    avcodec_free_context(&dec);
    av_parser_close(parser);
}

/* Whether a lane reads a file format, and its demuxer (NULL: probe; Opus vectors come
 * in Ogg and Matroska). */
static int file_lane(const char *name, const char **demuxer) {
    *demuxer = NULL;
    if (!strcmp(name, "flac")) *demuxer = "flac";
    else if (!strcmp(name, "mp3")) *demuxer = "mp3";
    else if (!strcmp(name, "vorbis")) *demuxer = "ogg";
    else if (strcmp(name, "opus")) return 0;
    return 1;
}

/* A file-format lane: the demuxer reads the piped (non-seekable) file, the decoder for
 * its first audio stream decodes every packet, and the same filter chain converts. */
static void demux_pass(const char *demuxer, const uint8_t *data, size_t size, struct out *o) {
    struct memory_input m = {data, size, 0};
    unsigned char *buffer = av_malloc(65536);
    AVIOContext *io = avio_alloc_context(buffer, 65536, 0, &m, memory_read, NULL, NULL);
    AVFormatContext *fmt = avformat_alloc_context();
    fmt->pb = io;
    int r = avformat_open_input(&fmt, NULL, demuxer ? av_find_input_format(demuxer) : NULL, NULL);
    if (r < 0) die("open", r);
    /* The first audio stream, without probing: stream info would decode packets the
     * candidate does not, so the reference does the least work that yields the output. */
    int index = -1;
    for (unsigned i = 0; i < fmt->nb_streams && index < 0; i++)
        if (fmt->streams[i]->codecpar->codec_type == AVMEDIA_TYPE_AUDIO) index = (int)i;
    if (index < 0) die("stream", AVERROR_STREAM_NOT_FOUND);
    AVStream *st = fmt->streams[index];
    const AVCodec *codec = avcodec_find_decoder(st->codecpar->codec_id);
    /* Layer I and II: FFmpeg's float decoders, like Layer III's default, where the CLI
     * would pick the fixed-point decoders, whose 16-bit rounding alone reaches the float
     * decoder rule. So this lane's stereo output for Layer I and II is not the CLI's
     * (rule mpeg-audio-float-decoders, harness/codecs/numeric-rules.json). */
    if (st->codecpar->codec_id == AV_CODEC_ID_MP1) codec = avcodec_find_decoder_by_name("mp1float");
    if (st->codecpar->codec_id == AV_CODEC_ID_MP2) codec = avcodec_find_decoder_by_name("mp2float");
    if (!codec) die("decoder", AVERROR_DECODER_NOT_FOUND);
    AVCodecContext *dec = avcodec_alloc_context3(codec);
    if ((r = avcodec_parameters_to_context(dec, st->codecpar)) < 0) die("parameters", r);
    dec->pkt_timebase = st->time_base;
    dec->thread_count = 1;
    if ((r = avcodec_open2(dec, codec, NULL)) < 0) die("open", r);
    AVPacket *pkt = av_packet_alloc();
    AVFrame *frame = av_frame_alloc();
    AVFilterGraph *graph = NULL;
    AVFilterContext *src = NULL, *sink = NULL;
    for (int done = 0; !done;) {
        r = av_read_frame(fmt, pkt);
        if (r < 0) {
            avcodec_send_packet(dec, NULL);
            done = 1;
        } else {
            if (pkt->stream_index == index && (r = avcodec_send_packet(dec, pkt)) < 0 && r != AVERROR_INVALIDDATA)
                die("send", r);
            av_packet_unref(pkt);
        }
        for (;;) {
            r = avcodec_receive_frame(dec, frame);
            if (r == AVERROR(EAGAIN) || r == AVERROR_EOF) break;
            if (r < 0) die("receive", r);
            if (!graph) graph = graph_for(dec, frame, 0, &src, &sink);
            if ((r = av_buffersrc_add_frame_flags(src, frame, AV_BUFFERSRC_FLAG_KEEP_REF)) < 0) die("buffersrc", r);
            av_frame_unref(frame);
            sink_frames(sink, o, 0);
        }
    }
    if (graph) {
        if ((r = av_buffersrc_add_frame_flags(src, NULL, 0)) < 0) die("buffersrc eof", r);
        sink_frames(sink, o, 0);
        avfilter_graph_free(&graph);
    }
    av_frame_free(&frame);
    av_packet_free(&pkt);
    avcodec_free_context(&dec);
    avformat_close_input(&fmt);
    av_freep(&io->buffer);
    avio_context_free(&io);
}

static void pass(const char *name, const AVCodecParameters *par, const uint8_t *data, size_t size, int video,
                 struct out *o) {
    const char *demuxer;
    if (!video && !strcmp(name, "aac-access-units"))
        aac_access_units(data, size, o);
    else if (!video && file_lane(name, &demuxer))
        demux_pass(demuxer, data, size, o);
    else
        one_pass(name, par, data, size, video, o);
}

int main(int argc, char **argv) {
    /* KIND-CODEC-bench FILE ITERATIONS | KIND-CODEC-memory FILE | KIND-CODEC-raw FILE OUT */
    if (argc < 3) {
        fprintf(stderr, "usage: %s KIND-CODEC-bench FILE ITERATIONS | KIND-CODEC-memory FILE | KIND-CODEC-raw FILE OUT\n", argv[0]);
        return 2;
    }
    av_log_set_level(AV_LOG_ERROR);
    char op[64];
    snprintf(op, sizeof op, "%s", argv[1]);
    char *kind = strrchr(op, '-');
    if (!kind) return 2;
    *kind++ = 0;
    int video = strncmp(op, "video-", 6) == 0;
    const char *separator = strchr(op, '-');
    if (!separator || !separator[1]) return 2;
    const char *name = separator + 1;
    if (!strcmp(name, "dts")) name = "dca";
    const int configured_aac = !strcmp(op, "audio-aac-access-units");
    FILE *in = fopen(argv[2], "rb");
    if (!in) { perror("input"); return 1; }
    if (fseek(in, 0, SEEK_END)) die("input seek", AVERROR(errno));
    long size = ftell(in);
    if (size < 0) die("input size", AVERROR(errno));
    if ((uintmax_t)size > SIZE_MAX - AV_INPUT_BUFFER_PADDING_SIZE ||
        (configured_aac && (size < 16 || size > AAC_MAX_INPUT)))
        die("input file bounds", AVERROR_INVALIDDATA);
    if (fseek(in, 0, SEEK_SET)) die("input seek", AVERROR(errno));
    uint8_t *data = av_malloc((size_t)size + AV_INPUT_BUFFER_PADDING_SIZE);
    if (!data) die("input allocation", AVERROR(ENOMEM));
    if (fread(data, 1, (size_t)size, in) != (size_t)size) { perror("read"); return 1; }
    if (configured_aac && (fgetc(in) != EOF || ferror(in)))
        die("AAC input changed while reading", AVERROR(EIO));
    memset(data + size, 0, AV_INPUT_BUFFER_PADDING_SIZE);
    fclose(in);
    /* Video only: the probe decodes a few frames before the measured window, which warms
     * the reference's code and tables; audio output is identical without it, so audio
     * keeps the same in-window lifecycle as the candidate (MEASUREMENT-POLICY.md). */
    AVCodecParameters *par = video ? probe_parameters(name, data, (size_t)size) : NULL;
    struct out o = {0};
    if (!strcmp(kind, "bench")) {
        long iterations = argc > 3 ? strtol(argv[3], NULL, 10) : 1;
        uint64_t start = now_ns();
        for (long i = 0; i < iterations; i++) pass(name, par, data, (size_t)size, video, &o);
        uint64_t elapsed = now_ns() - start;
        printf("{\"schema\":1,\"iterations\":%ld,\"elapsed_ns\":%llu,\"packets\":%llu,\"bytes\":%llu,\"digest\":%llu}\n",
               iterations, (unsigned long long)elapsed, (unsigned long long)o.frames, (unsigned long long)o.bytes,
               (unsigned long long)fold_finish(o.digest));
    } else if (!strcmp(kind, "memory")) {
        /* write(2) directly: the observer interposes write, which glibc stdio bypasses. */
        if (write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18) return 2;
        pass(name, par, data, (size_t)size, video, &o);
        if (write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16) return 2;
        /* Equal memory work is equal units and bytes; the digest goes to stderr because
         * audio output may differ within the graded 1 LSB (see native_correctness.py). */
        printf("{\"packets\":%llu,\"bytes\":%llu}\n", (unsigned long long)o.frames, (unsigned long long)o.bytes);
        fprintf(stderr, "{\"digest\":%llu}\n", (unsigned long long)fold_finish(o.digest));
    } else if (!strcmp(kind, "raw") && argc > 3) {
        o.raw = fopen(argv[3], "wb");
        if (!o.raw) die("raw output", AVERROR(errno));
        pass(name, par, data, (size_t)size, video, &o);
        if (fclose(o.raw)) die("raw output", AVERROR(errno));
        printf("{\"packets\":%llu,\"bytes\":%llu,\"digest\":%llu", (unsigned long long)o.frames,
               (unsigned long long)o.bytes, (unsigned long long)fold_finish(o.digest));
        if (o.aac_channels) {
            printf(",\"speakers\":[");
            for (unsigned i = 0; i < o.aac_channels; i++)
                printf("%s%u", i ? "," : "", (unsigned)o.aac_speakers[i]);
            printf("]");
        }
        puts("}");
    } else {
        return 2;
    }
    av_free(data);
    return 0;
}
