/* Own reference tooling: PGS display sets through FFmpeg's pgssub decoder and the
 * public avcodec_decode_subtitle2 API, the same operation as the pgs- modes of
 * subtitle-capsule-worker.
 *
 * The input is a case capsule (SMBMO001): the packet time base, the forced-only
 * option and up to 64 records, each a complete PGS packet with raw PTS and DTS or a
 * hard reset that frees and reopens the decoder. Each packet is decoded until its
 * bytes are consumed and every published subtitle is folded with its packet,
 * offset, consumed bytes, PTS, display window and canvas, and per rectangle the
 * geometry, flags, stride, index plane, palette and the palette value of every
 * pixel. A rejected packet ends the case with a refusal marker. The fold is the
 * ordered digest of the byte transcript the recorded PGS cases store. */
#include <libavcodec/avcodec.h>
#include <libavutil/opt.h>

#define PGS_OPERATION "pgs-receiver-display"
enum { PGS_MAX_RECORDS = 64, PGS_MAX_INPUT = 16 * 1024 * 1024, PGS_MAX_PIXELS = 8 * 1024 * 1024,
       PGS_MAX_DRAIN = 64 };

typedef struct {
    int reset;
    uint8_t wire[28];
    AVPacket *packet;
} PgsRecord;
typedef struct {
    uint8_t header[29];
    uint32_t flags, private_size, count;
    AVRational timebase;
    PgsRecord records[PGS_MAX_RECORDS];
} PgsCase;
typedef struct { uint64_t digest, events, pixels; } PgsWork;

static uint32_t pgs_be32(const uint8_t *b) { return (uint32_t)b[0] << 24 | (uint32_t)b[1] << 16 | (uint32_t)b[2] << 8 | b[3]; }
static void pgs_le32(uint8_t *out, uint32_t v) { for (int i = 0; i < 4; ++i) out[i] = (uint8_t)(v >> (8 * i)); }
static void pgs_data(PgsWork *w, const void *bytes, size_t n) { fold_bytes(&w->digest, bytes, n); }
static void pgs_u32(PgsWork *w, uint32_t v) { uint8_t b[4]; pgs_le32(b, v); pgs_data(w, b, 4); }
static void pgs_i64(PgsWork *w, int64_t v) {
    uint8_t b[8]; uint64_t u = (uint64_t)v;
    for (int i = 0; i < 8; ++i) b[i] = (uint8_t)(u >> (8 * i));
    pgs_data(w, b, 8);
}

static void pgs_free(PgsCase *c)
{
    for (uint32_t i = 0; i < c->count; ++i)
        av_packet_free(&c->records[i].packet);
}

/* Parse the capsule, build every packet and the transcript header outside the
 * measured operation. */
static int pgs_parse(const uint8_t *in, size_t size, PgsCase *c)
{
    size_t at = 0, total = 0;
    memset(c, 0, sizeof(*c));
#define PGS_TAKE(n) do { if (size - at < (size_t)(n)) return -1; } while (0)
    PGS_TAKE(29);
    if (memcmp(in, "SMBMO001", 8) || in[8] != 1)
        return -1;
    c->flags = pgs_be32(in + 9);
    uint32_t numerator = pgs_be32(in + 13), denominator = pgs_be32(in + 17);
    c->private_size = pgs_be32(in + 21);
    if ((c->flags & ~1U) || !numerator || !denominator || numerator > INT32_MAX || denominator > INT32_MAX ||
        c->private_size > 65536)
        return -1;
    at = 25;
    if (c->private_size)  /* The receiver takes no private configuration. */
        return -1;
    PGS_TAKE(4);
    uint32_t count = pgs_be32(in + at);
    at += 4;
    if (!count || count > PGS_MAX_RECORDS)
        return -1;
    c->timebase = (AVRational){(int)numerator, (int)denominator};
    memcpy(c->header, "SMBOUT02", 8);
    c->header[8] = 1;
    pgs_le32(c->header + 9, c->flags);
    pgs_le32(c->header + 13, numerator);
    pgs_le32(c->header + 17, denominator);
    pgs_le32(c->header + 21, 0);
    pgs_le32(c->header + 25, count);
    for (uint32_t i = 0; i < count; ++i) {
        PGS_TAKE(23);
        const uint8_t *r = in + at;
        uint8_t reset = r[0], pts_known = r[1], dts_known = r[10];
        uint32_t length = pgs_be32(r + 19);
        at += 23;
        if ((reset && reset != 2) || pts_known > 1 || dts_known > 1 || (!reset) != (length != 0) ||
            length > PGS_MAX_INPUT || total > PGS_MAX_INPUT - length)
            return -1;
        PGS_TAKE(length);
        total += length;
        PgsRecord *record = &c->records[i];
        c->count = i + 1;
        record->reset = reset != 0;
        record->wire[0] = 'P';
        pgs_le32(record->wire + 1, i);
        record->wire[5] = reset;
        record->wire[6] = pts_known;
        for (int k = 0; k < 8; ++k) {
            record->wire[7 + k] = r[9 - k];
            record->wire[16 + k] = r[18 - k];
        }
        record->wire[15] = dts_known;
        pgs_le32(record->wire + 24, length);
        if (!length)
            continue;
        record->packet = av_packet_alloc();
        if (!record->packet || av_new_packet(record->packet, (int)length) < 0)
            return -1;
        memcpy(record->packet->data, in + at, length);
        at += length;
        int64_t pts = 0, dts = 0;
        for (int k = 0; k < 8; ++k) {
            pts = (int64_t)((uint64_t)pts << 8 | r[2 + k]);
            dts = (int64_t)((uint64_t)dts << 8 | r[11 + k]);
        }
        record->packet->pts = pts_known ? pts : AV_NOPTS_VALUE;
        record->packet->dts = dts_known ? dts : AV_NOPTS_VALUE;
    }
#undef PGS_TAKE
    return at == size ? 0 : -1;
}

static AVCodecContext *pgs_open(const PgsCase *c)
{
    const AVCodec *codec = avcodec_find_decoder(AV_CODEC_ID_HDMV_PGS_SUBTITLE);
    AVCodecContext *ctx = codec ? avcodec_alloc_context3(codec) : NULL;
    if (!ctx)
        return NULL;
    ctx->pkt_timebase = c->timebase;
    ctx->thread_count = 1;
    if ((c->flags && av_opt_set_int(ctx->priv_data, "forced_subs_only", 1, 0) < 0) ||
        avcodec_open2(ctx, codec, NULL) < 0)
        avcodec_free_context(&ctx);
    return ctx;
}

/* Fold one published subtitle; a rectangle is validated completely before any of
 * it is folded. */
static int pgs_emit(PgsWork *w, const AVSubtitle *sub, const AVCodecContext *ctx, uint32_t packet,
                    uint32_t offset, int used, int flush)
{
    pgs_data(w, "E", 1);
    pgs_u32(w, packet);
    pgs_u32(w, offset);
    pgs_u32(w, (uint32_t)used);
    uint8_t marks[4] = {(uint8_t)flush, 0, 0, sub->pts != AV_NOPTS_VALUE};
    pgs_data(w, marks, 4);
    pgs_i64(w, sub->pts != AV_NOPTS_VALUE ? sub->pts : 0);
    pgs_u32(w, sub->start_display_time);
    pgs_u32(w, sub->end_display_time);
    pgs_u32(w, (uint32_t)ctx->width);
    pgs_u32(w, (uint32_t)ctx->height);
    pgs_u32(w, sub->num_rects);
    ++w->events;
    for (unsigned i = 0; i < sub->num_rects; ++i) {
        const AVSubtitleRect *r = sub->rects[i];
        if (!r || r->type != SUBTITLE_BITMAP || r->w < 0 || r->h < 0 || r->linesize[0] < r->w ||
            r->nb_colors < 0 || r->nb_colors > 256)
            return -1;
        uint64_t pixels = (uint64_t)r->w * (uint64_t)r->h;
        if (pixels > PGS_MAX_PIXELS || w->pixels + pixels > PGS_MAX_PIXELS ||
            (pixels && !r->data[0]) || (r->nb_colors && !r->data[1]))
            return -1;
        uint64_t words[256];
        for (int y = 0; y < r->h; ++y)
            for (int x = 0; x < r->w; ++x)
                if (r->data[0][(size_t)y * (size_t)r->linesize[0] + (size_t)x] >= r->nb_colors)
                    return -1;
        pgs_u32(w, i);
        const int32_t geometry[6] = {r->x, r->y, r->w, r->h, r->flags, r->linesize[0]};
        for (int k = 0; k < 6; ++k)
            pgs_u32(w, (uint32_t)geometry[k]);
        pgs_u32(w, (uint32_t)r->nb_colors);
        pgs_i64(w, (int64_t)pixels);
        for (int y = 0; y < r->h; ++y)
            pgs_data(w, r->data[0] + (size_t)y * (size_t)r->linesize[0], (size_t)r->w);
        for (int j = 0; j < r->nb_colors; ++j) {
            uint32_t colour;
            memcpy(&colour, r->data[1] + (size_t)j * 4, 4);
            uint8_t b[4];
            pgs_le32(b, colour);
            pgs_data(w, b, 4);
            words[j] = (uint64_t)b[0] * 29791 + (uint64_t)b[1] * 961 + (uint64_t)b[2] * 31 + b[3];
        }
        /* The palette value of every pixel, four bytes each: h * 31^4 + word fold. */
        for (int y = 0; y < r->h; ++y) {
            const uint8_t *row = r->data[0] + (size_t)y * (size_t)r->linesize[0];
            for (int x = 0; x < r->w; ++x)
                w->digest = w->digest * UINT64_C(923521) + words[row[x]];
        }
        w->pixels += pixels;
    }
    return 0;
}

/* Decode every record; a negative return is a refusal. */
static int pgs_decode(const PgsCase *c, PgsWork *w)
{
    AVCodecContext *ctx = pgs_open(c);
    int rc = ctx ? 0 : -1;
    for (uint32_t i = 0; rc == 0 && i < c->count; ++i) {
        const PgsRecord *record = &c->records[i];
        if (record->reset) {
            avcodec_free_context(&ctx);
            ctx = pgs_open(c);
            rc = ctx ? 0 : -1;
            continue;
        }
        int offset = 0;
        while (rc == 0 && offset < record->packet->size) {
            AVPacket view = *record->packet;
            view.data += offset;
            view.size -= offset;
            AVSubtitle sub = {0};
            int got = 0;
            int used = avcodec_decode_subtitle2(ctx, &sub, &got, &view);
            if (got) {
                rc = pgs_emit(w, &sub, ctx, i, (uint32_t)offset, used, 0);
                avsubtitle_free(&sub);
            }
            if (used <= 0 || used > view.size)
                rc = -1;
            else
                offset += used;
        }
    }
    for (int call = 0; rc == 0 && call < PGS_MAX_DRAIN; ++call) {
        AVPacket drain = {0};
        drain.pts = drain.dts = AV_NOPTS_VALUE;
        AVSubtitle sub = {0};
        int got = 0;
        int used = avcodec_decode_subtitle2(ctx, &sub, &got, &drain);
        if (got) {
            rc = pgs_emit(w, &sub, ctx, c->count, (uint32_t)call, used, 1);
            avsubtitle_free(&sub);
        }
        if (used < 0 || (got && call + 1 == PGS_MAX_DRAIN))
            rc = -1;
        if (!got)
            break;
    }
    avcodec_free_context(&ctx);
    return rc;
}

static void pgs_operation(const PgsCase *c, PgsWork *w)
{
    pgs_data(w, c->header, sizeof(c->header));
    for (uint32_t i = 0; i < c->count; ++i) {
        const PgsRecord *record = &c->records[i];
        pgs_data(w, record->wire, sizeof(record->wire));
        if (record->packet)
            pgs_data(w, record->packet->data, (size_t)record->packet->size);
    }
    int refused = pgs_decode(c, w) < 0;
    pgs_data(w, refused ? "F" : "Z", 1);
}

/* pgs-fold FILE | pgs-bench FILE ITERATIONS | pgs-memory FILE */
static int pgs_main(int argc, char **argv, const uint8_t *bytes, long size)
{
    const int bench = argc == 4 && !strcmp(argv[1], "pgs-bench");
    const int memory = argc == 3 && !strcmp(argv[1], "pgs-memory");
    if (!bench && !memory && !(argc == 3 && !strcmp(argv[1], "pgs-fold")))
        return 2;
    unsigned long iterations = 1;
    if (bench) {
        char *end = NULL;
        iterations = strtoul(argv[3], &end, 10);
        if (*end || !iterations || iterations > 1000000)
            return 2;
    }
    /* Decoder diagnostics are not part of the compared work; writing them would
     * add stdio inside the measured window. */
    av_log_set_level(AV_LOG_QUIET);
    static PgsCase c;
    if (size < 0 || pgs_parse(bytes, (size_t)size, &c) < 0) {
        pgs_free(&c);
        fprintf(stderr, "reference: not a PGS case capsule\n");
        return 2;
    }
    if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18)
        return 2;
    PgsWork total = {0};
    const uint64_t start = bench ? now_ns() : 0;
    for (unsigned long i = 0; i < iterations; ++i) {
        PgsWork once = {0};
        pgs_operation(&c, &once);
        total.digest = total.digest * 31 + once.digest;
        total.events += once.events;
        total.pixels += once.pixels;
    }
    const uint64_t elapsed = bench ? now_ns() - start : 0;
    if (memory && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16)
        return 2;
    pgs_free(&c);
    printf("{\"schema\":1,\"operation\":\"" PGS_OPERATION "\",\"iterations\":%lu"
           ",\"packets\":%" PRIu64 ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64
           ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true}\n",
           iterations, total.events, total.pixels, total.digest, elapsed);
    return 0;
}
