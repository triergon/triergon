#ifndef TRIERGON_REFERENCE_FRAMING_H
#define TRIERGON_REFERENCE_FRAMING_H
/* Public-API caller for AVC and HEVC packet-framing operations.
 * SMBSF001 scripts supply configuration, packet fields and session actions.
 * Immutable padded packet templates are prepared outside the operation on both
 * sides. Each operation opens a new BSF and retains all output packets until
 * after close. Native input packet refs share readonly caller payloads.
 */
#include <errno.h>
#include <inttypes.h>
#include <libavcodec/avcodec.h>
#include <libavcodec/bsf.h>
#include <libavutil/avutil.h>
#include <libavutil/buffer.h>
#include <libavutil/error.h>
#include <libavutil/log.h>
#include <libavutil/mem.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define CAP (16U * 1024U * 1024U)
#define EVENTS 64
#define OUTPUTS 256
typedef struct {
    const uint8_t *p;
    size_t n, at;
    int bad;
} Reader;
typedef struct {
    unsigned type;
    AVPacket *packet;
} Event;
typedef struct {
    unsigned codec, count;
    const uint8_t *config;
    unsigned config_n;
    Event events[EVENTS];
} Case;
typedef struct {
    uint64_t a, b;
} Digest;
typedef struct {
    Digest hash;
    uint64_t packets, bytes;
} Outcome;
static void group(Digest *h, const uint8_t *p) {
    /* Match the Rust worker's eight-byte polynomial grouping modulo 2^64. */
    static const uint64_t pow31[9] = {
        1, 31, 961, 29791, 923521, 28629151, 887503681, 27512614111, 852891037441};
    static const uint64_t pow131[9] = {
        1, 131, 17161, 2248091, 294499921, 38579489651, 5053913144281,
        662062621900811, 86730203469006241};
    uint64_t a = 0, b = 0;
    for (unsigned i = 0; i < 8; ++i) {
        a += (uint64_t)p[i] * pow31[7 - i];
        b += (uint64_t)p[i] * pow131[7 - i];
    }
    h->a = h->a * pow31[8] + a;
    h->b = h->b * pow131[8] + b;
}
static void raw(Digest *h, const uint8_t *p, size_t n) {
    while (n >= 8) {
        group(h, p);
        p += 8;
        n -= 8;
    }
    for (size_t i = 0; i < n; ++i) {
        h->a = h->a * 31 + p[i];
        h->b = h->b * 131 + p[i];
    }
}
static void word(Digest *h, uint64_t n) {
    uint8_t b[8];
    for (unsigned i = 0; i < 8; ++i)
        b[i] = (uint8_t)(n >> (i * 8));
    group(h, b);
}
static __attribute__((noinline)) void words(Digest *h, const uint64_t *values, size_t count) {
    for (size_t i = 0; i < count; ++i)
        word(h, values[i]);
}
static void blob(Digest *h, const uint8_t *p, size_t n) {
    word(h, n);
    raw(h, p, n);
}
static uint64_t get(Reader *r, unsigned n) {
    if (n > r->n - r->at) {
        r->bad = 1;
        return 0;
    }
    uint64_t v = 0;
    for (unsigned i = 0; i < n; ++i)
        v = (v << 8) | r->p[r->at++];
    return v;
}
static const uint8_t *span(Reader *r, size_t n) {
    if (n > r->n - r->at) {
        r->bad = 1;
        return NULL;
    }
    const uint8_t *p = r->p + r->at;
    r->at += n;
    return p;
}
static void release_buffer(void *opaque, uint8_t *data) {
    (void)opaque;
    av_free(data);
}
static void case_free(Case *c) {
    for (unsigned i = 0; i < c->count; ++i)
        av_packet_free(&c->events[i].packet);
}
static int case_load(Case *c, const uint8_t *data, size_t n) {
    Reader r = {data, n, 0, 0};
    const uint8_t *magic = span(&r, 8);
    if (!magic || memcmp(magic, "SMBSF001", 8))
        return -1;
    c->codec = (unsigned)get(&r, 1);
    c->config_n = (unsigned)get(&r, 4);
    c->config = span(&r, c->config_n);
    c->count = (unsigned)get(&r, 4);
    if (r.bad || c->codec < 1 || c->codec > 2 || c->count > EVENTS) {
        c->count = 0;
        return -1;
    }
    for (unsigned i = 0; i < c->count; ++i) {
        Event *e = &c->events[i];
        e->type = (unsigned)get(&r, 1);
        if (e->type == 1 || e->type == 2)
            continue;
        if (e->type != 0)
            return -1;
        AVPacket *p = e->packet = av_packet_alloc();
        if (!p)
            return -1;
        p->pts = (int64_t)get(&r, 8);
        p->dts = (int64_t)get(&r, 8);
        p->duration = (int64_t)get(&r, 8);
        p->pos = (int64_t)get(&r, 8);
        p->flags = (int)get(&r, 4);
        p->stream_index = (int)get(&r, 4);
        unsigned size = (unsigned)get(&r, 4), extra_n = (unsigned)get(&r, 4);
        if (size > CAP || (extra_n != UINT32_MAX && extra_n > CAP))
            return -1;
        const uint8_t *payload = span(&r, size);
        const uint8_t *extra = extra_n == UINT32_MAX ? NULL : span(&r, extra_n);
        if (r.bad)
            return -1;
        uint8_t *padded = av_mallocz((size_t)size + AV_INPUT_BUFFER_PADDING_SIZE);
        if (!padded)
            return -1;
        if (size)
            memcpy(padded, payload, size);
        p->buf = av_buffer_create(padded, (size_t)size + AV_INPUT_BUFFER_PADDING_SIZE,
                                  release_buffer, NULL, AV_BUFFER_FLAG_READONLY);
        if (!p->buf) {
            av_free(padded);
            return -1;
        }
        p->data = p->buf->data;
        p->size = (int)size;
        p->time_base = (AVRational){1, 90000};
        if (extra_n != UINT32_MAX) {
            uint8_t *side = av_packet_new_side_data(p, AV_PKT_DATA_NEW_EXTRADATA, extra_n);
            if (!side)
                return -1;
            if (extra_n)
                memcpy(side, extra, extra_n);
        }
    }
    return r.bad || r.at != n ? -1 : 0;
}
static void packet(Digest *h, unsigned kind, int input, unsigned output, const AVPacket *p) {
    const uint64_t fields[] = {
        kind, (uint64_t)(int64_t)input, output, (uint64_t)p->pts, (uint64_t)p->dts,
        (uint64_t)p->duration, (uint64_t)p->pos, (uint64_t)(int64_t)p->flags,
        (uint64_t)(int64_t)p->stream_index, (uint64_t)(int64_t)p->time_base.num,
        (uint64_t)(int64_t)p->time_base.den,
    };
    words(h, fields, sizeof(fields) / sizeof(fields[0]));
    blob(h, p->data, (size_t)p->size);
    word(h, (unsigned)p->side_data_elems);
    for (int i = 0; i < p->side_data_elems; ++i) {
        word(h, p->side_data[i].type);
        blob(h, p->side_data[i].data, p->side_data[i].size);
    }
}
static int receive_all(AVBSFContext *ctx, AVPacket **kept, unsigned *count, int input,
                       Outcome *out) {
    for (;;) {
        if (*count >= OUTPUTS)
            return -1;
        AVPacket *p = av_packet_alloc();
        if (!p)
            return -1;
        int rc = av_bsf_receive_packet(ctx, p);
        if (rc < 0) {
            av_packet_free(&p);
            if (rc == AVERROR(ENOMEM))
                return -1;
            if (rc != AVERROR(EAGAIN) && rc != AVERROR_EOF) {
                const uint64_t fields[] = {3, (uint64_t)(int64_t)input, (uint64_t)(int64_t)rc};
                words(&out->hash, fields, sizeof(fields) / sizeof(fields[0]));
            }
            return 0;
        }
        if (p->size < 0 || p->size > (int)CAP) {
            av_packet_free(&p);
            return -1;
        }
        packet(&out->hash, 2, input, *count, p);
        out->packets++;
        out->bytes += (unsigned)p->size;
        kept[(*count)++] = p;
    }
}
static int operation(const Case *c, Outcome *out) {
    const AVBitStreamFilter *filter =
        av_bsf_get_by_name(c->codec == 1 ? "h264_mp4toannexb" : "hevc_mp4toannexb");
    if (!filter)
        return -1;
    AVBSFContext *ctx = NULL;
    AVPacket *kept[OUTPUTS] = {0};
    unsigned count = 0;
    int failed = 0;
    if (av_bsf_alloc(filter, &ctx) < 0)
        return -1;
    ctx->par_in->codec_type = AVMEDIA_TYPE_VIDEO;
    ctx->par_in->codec_id = c->codec == 1 ? AV_CODEC_ID_H264 : AV_CODEC_ID_HEVC;
    ctx->par_in->extradata = av_mallocz((size_t)c->config_n + AV_INPUT_BUFFER_PADDING_SIZE);
    if (!ctx->par_in->extradata) {
        av_bsf_free(&ctx);
        return -1;
    }
    if (c->config_n)
        memcpy(ctx->par_in->extradata, c->config, c->config_n);
    ctx->par_in->extradata_size = (int)c->config_n;
    ctx->time_base_in = (AVRational){1, 90000};
    int rc = av_bsf_init(ctx);
    if (rc < 0) {
        word(&out->hash, 4);
        word(&out->hash, (uint64_t)(int64_t)rc);
    } else {
        word(&out->hash, 1);
        blob(&out->hash, ctx->par_out->extradata, (size_t)ctx->par_out->extradata_size);
        word(&out->hash, (uint64_t)(int64_t)ctx->time_base_out.num);
        word(&out->hash, (uint64_t)(int64_t)ctx->time_base_out.den);
        for (unsigned i = 0; i < c->count; ++i) {
            const Event *e = &c->events[i];
            if (e->type == 1) {
                av_bsf_flush(ctx);
                continue;
            }
            if (e->type == 2)
                rc = av_bsf_send_packet(ctx, NULL);
            else {
                AVPacket *p = av_packet_clone(e->packet);
                if (!p) {
                    failed = 1;
                    break;
                }
                rc = av_bsf_send_packet(ctx, p);
                av_packet_free(&p);
            }
            if (rc == AVERROR(ENOMEM) || (rc >= 0 && receive_all(ctx, kept, &count, (int)i, out))) {
                failed = 1;
                break;
            }
        }
    }
    av_bsf_free(&ctx);
    word(&out->hash, 6);
    word(&out->hash, count);
    for (unsigned i = 0; i < count; ++i) {
        packet(&out->hash, 5, -1, i, kept[i]);
        av_packet_free(&kept[i]);
    }
    return failed ? -1 : 0;
}
static uint64_t now_ns(void) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t))
        exit(74);
    return (uint64_t)t.tv_sec * UINT64_C(1000000000) + (uint64_t)t.tv_nsec;
}
#endif
