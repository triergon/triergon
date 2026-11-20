/* AC-3 encoding through FFmpeg's public frame and packet API. */
#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <libavcodec/avcodec.h>
#include <libavutil/buffer.h>
#include <libavutil/error.h>

enum { SAMPLES = 1536, FRAMES = 6, REPEATS = 49 };

typedef struct {
    unsigned rate, channels, bitrate;
    const char *layout;
    uint64_t header;
    AVBufferRef *pcm;
} Input;

typedef struct {
    uint64_t packets, bytes, digest;
} Work;

static _Noreturn void fail(const char *reason) {
    fprintf(stderr, "reference_ac3_encode: %s\n", reason);
    exit(2);
}

static unsigned number(const char *text) {
    char *end = NULL;
    errno = 0;
    uintmax_t value = strtoumax(text, &end, 10);
    if (text == end || *end || errno || !value || value > UINT32_MAX) fail("number");
    return (unsigned)value;
}

static Input read_input(const char *path, unsigned rate, unsigned channels) {
    Input input = {.rate = rate, .channels = channels};
    unsigned fscod, acmod, lfe = 0, size_code;
    switch (rate) {
    case 32000: fscod = 2; break;
    case 44100: fscod = 1; break;
    case 48000: fscod = 0; break;
    default: fail("sample rate");
    }
    switch (channels) {
    case 1:
        input.layout = "mono";
        input.bitrate = 192000;
        acmod = 1;
        size_code = 10;
        break;
    case 2:
        input.layout = "stereo";
        input.bitrate = 256000;
        acmod = 2;
        size_code = 12;
        break;
    case 6:
        input.layout = "5.1(side)";
        input.bitrate = 640000;
        acmod = 7;
        lfe = 1;
        size_code = 18;
        break;
    default: fail("channel layout");
    }
    input.header = (fscod << 16) | (8 << 12) | (acmod << 8) | (lfe << 6) | size_code;
    const uint16_t one = 1;
    if (sizeof(float) != 4 || *(const unsigned char *)&one != 1) fail("PCM host representation");
    FILE *file = fopen(path, "rb");
    if (!file) fail("input open");
    size_t bytes = (size_t)FRAMES * SAMPLES * channels * sizeof(float);
    input.pcm = av_buffer_alloc(bytes);
    if (!input.pcm) fail("input allocation");
    float *pcm = (float *)input.pcm->data;
    for (unsigned frame = 0; frame < FRAMES; ++frame) {
        for (unsigned i = 0; i < SAMPLES; ++i) {
            float sample[6];
            if (fread(sample, sizeof(float), channels, file) != channels) fail("incomplete PCM");
            for (unsigned ch = 0; ch < channels; ++ch) {
                if (!(sample[ch] >= -1.0f && sample[ch] <= 1.0f)) fail("PCM outside the normalized range");
                pcm[(frame * channels + ch) * SAMPLES + i] = sample[ch];
            }
        }
    }
    if (fgetc(file) != EOF || ferror(file) || fclose(file)) fail("oversized PCM or input read error");
    return input;
}

static void record(const AVPacket *packet, uint64_t expected, Work *work, FILE *sink) {
    const uint8_t *data = packet->data;
    if (packet->size < 7 || data[0] != 0x0b || data[1] != 0x77) fail("AC-3 syncframe");
    unsigned acmod = data[6] >> 5;
    unsigned lfe;
    switch (acmod) {
    case 1: lfe = (data[6] >> 4) & 1; break;
    case 2: lfe = (data[6] >> 2) & 1; break;
    case 7: lfe = data[6] & 1; break;
    default: fail("encoded layout");
    }
    uint64_t header = ((uint64_t)(data[4] >> 6) << 16)
        | ((uint64_t)(data[5] >> 3) << 12) | ((uint64_t)acmod << 8)
        | ((uint64_t)lfe << 6) | ((data[4] & 63) >> 1);
    if (header != expected) fail("encoded rate, layout or bitrate");
    ++work->packets;
    work->bytes += (uint64_t)packet->size;
    work->digest = (work->digest ^ header) * UINT64_C(1099511628211);
    if (sink && fwrite(data, 1, (size_t)packet->size, sink) != (size_t)packet->size)
        fail("write packet");
}

static void drain(AVCodecContext *ctx, AVPacket *packet, uint64_t header, Work *work, FILE *sink,
                  int flushing) {
    int status;
    while ((status = avcodec_receive_packet(ctx, packet)) == 0) {
        record(packet, header, work, sink);
        av_packet_unref(packet);
    }
    if (status != (flushing ? AVERROR_EOF : AVERROR(EAGAIN))) fail("receive packet");
}

static Work encode(const Input *input, unsigned repeats, FILE *sink) {
    const AVCodec *codec = avcodec_find_encoder_by_name("ac3");
    if (!codec) fail("AC-3 encoder absent");
    AVCodecContext *ctx = avcodec_alloc_context3(codec);
    AVPacket *packet = av_packet_alloc();
    AVFrame *frame = av_frame_alloc();
    if (!ctx || !packet || !frame) fail("encoder allocation");
    ctx->sample_rate = (int)input->rate;
    ctx->sample_fmt = AV_SAMPLE_FMT_FLTP;
    ctx->bit_rate = input->bitrate;
    ctx->thread_count = 1;
    ctx->time_base = (AVRational){1, (int)input->rate};
    if (av_channel_layout_from_string(&ctx->ch_layout, input->layout) < 0
        || avcodec_open2(ctx, codec, NULL) < 0 || ctx->frame_size != SAMPLES)
        fail("open AC-3 encoder");

    frame->format = ctx->sample_fmt;
    frame->sample_rate = ctx->sample_rate;
    frame->nb_samples = SAMPLES;
    frame->buf[0] = av_buffer_ref(input->pcm);
    if (!frame->buf[0] || av_channel_layout_copy(&frame->ch_layout, &ctx->ch_layout) < 0)
        fail("frame layout");
    frame->linesize[0] = SAMPLES * sizeof(float);
    frame->pts = 0;
    Work work = {.digest = UINT64_C(14695981039346656037)};
    for (unsigned r = 0; r < repeats; ++r) {
        for (unsigned i = 0; i < FRAMES; ++i) {
            for (unsigned ch = 0; ch < input->channels; ++ch) {
                size_t offset = (i * input->channels + ch) * SAMPLES * sizeof(float);
                frame->data[ch] = input->pcm->data + offset;
            }
            if (avcodec_send_frame(ctx, frame) < 0) fail("send PCM");
            drain(ctx, packet, input->header, &work, sink, 0);
            frame->pts += SAMPLES;
        }
    }
    if (avcodec_send_frame(ctx, NULL) < 0) fail("flush encoder");
    drain(ctx, packet, input->header, &work, sink, 1);
    av_frame_free(&frame);
    av_packet_free(&packet);
    avcodec_free_context(&ctx);
    if (work.packets != (uint64_t)repeats * FRAMES) fail("packet count");
    return work;
}

static uint64_t nanoseconds(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts)) fail("monotonic clock");
    return (uint64_t)ts.tv_sec * UINT64_C(1000000000) + (uint64_t)ts.tv_nsec;
}

int main(int argc, char **argv) {
    if (argc < 3) fail("operation and input");
    int bench = !strcmp(argv[1], "ac3-encode-bench");
    int memory = !strcmp(argv[1], "ac3-encode-memory");
    int emit = !strcmp(argv[1], "ac3-encode-emit");
    int fold = !strcmp(argv[1], "ac3-encode-fold");
    if (!(bench || memory || fold || emit) || argc != 5 + bench + emit) fail("arguments");
    unsigned iterations = bench ? number(argv[3]) : 1;
    Input input = read_input(argv[2], number(argv[3 + bench]), number(argv[4 + bench]));
    if (emit) {
        FILE *file = fopen(argv[5], "wbx");
        if (!file) fail("output open");
        encode(&input, REPEATS, file);
        if (fclose(file)) fail("output close");
    } else if (bench) {
        Work first = {0};
        uint64_t start = nanoseconds();
        for (unsigned i = 0; i < iterations; ++i) {
            Work work = encode(&input, REPEATS, NULL);
            if (!i) first = work;
            if (work.packets != first.packets || work.bytes != first.bytes
                || work.digest != first.digest)
                fail("encoded work changed between iterations");
        }
        printf("{\"iterations\":%u,\"elapsed_ns\":%" PRIu64 ",\"packets\":%" PRIu64
               ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 "}\n",
               iterations, nanoseconds() - start, first.packets * iterations,
               first.bytes * iterations, first.digest);
    } else {
        if (memory && (puts("TRIERGON_BEGIN_V1") == EOF || fflush(stdout))) fail("marker");
        Work work = encode(&input, REPEATS, NULL);
        if (memory && (puts("TRIERGON_END_V1") == EOF || fflush(stdout))) fail("marker");
        printf("{\"eof\":true,\"packets\":%" PRIu64 ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 "}\n",
               work.packets, work.bytes, work.digest);
    }
    av_buffer_unref(&input.pcm);
    return ferror(stdout) ? 2 : 0;
}
