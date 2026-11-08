/* Own reference tooling: text subtitles through public FFmpeg APIs, the same
 * operations as playback-worker's subtitle-text modes.
 *
 * - packets: a session capsule (TRSTXT01) of decoder opens, packets and flushes
 *   through the mov_text or ass decoder and avcodec_decode_subtitle2, with the
 *   capsule's packet time base, frame size, extradata and charset label
 *   (sub_charenc). mov_text calls fold their return value and the complete
 *   subtitle; ass calls fold the published text and its clock.
 * - srt and vtt: one document through the srt or webvtt demuxer from a preloaded
 *   custom source, folding every packet's PTS and duration in milliseconds, its
 *   text and, for WebVTT, its identifier and settings side data.
 *
 * The digest is the ordered fold of a little-endian byte transcript; a document
 * the demuxer refuses ends with a refusal marker. */
#include <errno.h>
#include <libavcodec/avcodec.h>
#include <libavutil/opt.h>

#define TEXT_OPERATION "subtitle-text-cues"
enum { TEXT_MAX_OPS = 4096 };

typedef struct {
    char kind;
    int num, den, width, height, has_extradata;
    uint8_t *extradata;
    int extradata_size;
    AVPacket *packet;
} TextOp;
typedef struct {
    int codec; /* 1 mov_text, 2 ass */
    char *label;
    uint32_t count;
    TextOp ops[TEXT_MAX_OPS];
} TextSession;
typedef struct { uint64_t digest, packets, bytes; } TextWork;

static uint32_t text_be32(const uint8_t *b) { return (uint32_t)b[0] << 24 | (uint32_t)b[1] << 16 | (uint32_t)b[2] << 8 | b[3]; }
static int64_t text_be64(const uint8_t *b)
{
    uint64_t v = 0;
    for (int i = 0; i < 8; ++i)
        v = v << 8 | b[i];
    return (int64_t)v;
}
static void text_data(TextWork *w, const void *bytes, size_t n) { fold_bytes(&w->digest, bytes, n); }
static void text_byte(TextWork *w, uint8_t v) { text_data(w, &v, 1); }
static void text_u32(TextWork *w, uint32_t v)
{
    uint8_t b[4];
    for (int i = 0; i < 4; ++i)
        b[i] = (uint8_t)(v >> (8 * i));
    text_data(w, b, 4);
}
static void text_i64(TextWork *w, int64_t v)
{
    uint8_t b[8];
    for (int i = 0; i < 8; ++i)
        b[i] = (uint8_t)((uint64_t)v >> (8 * i));
    text_data(w, b, 8);
}
static void text_framed(TextWork *w, const void *bytes, size_t n)
{
    text_u32(w, (uint32_t)n);
    text_data(w, bytes, n);
}
static void text_text(TextWork *w, const void *bytes, size_t n)
{
    text_framed(w, bytes, n);
    ++w->packets;
    w->bytes += n;
}

static void text_free(TextSession *s)
{
    for (uint32_t i = 0; i < s->count; ++i) {
        av_free(s->ops[i].extradata);
        av_packet_free(&s->ops[i].packet);
    }
    av_free(s->label);
    s->label = NULL;
    s->count = 0;
}

/* Parse the capsule and build every packet outside the measured operation. */
static int text_parse(const uint8_t *in, size_t size, TextSession *s)
{
    size_t at = 0;
    memset(s, 0, sizeof(*s));
#define TEXT_TAKE(n) do { if (size - at < (size_t)(n)) return -1; } while (0)
    TEXT_TAKE(11);
    if (memcmp(in, "TRSTXT01", 8) || (in[8] != 1 && in[8] != 2))
        return -1;
    s->codec = in[8];
    size_t label = (size_t)in[9] << 8 | in[10];
    at = 11;
    TEXT_TAKE(label);
    if (label) {
        if (s->codec != 2 || !(s->label = av_malloc(label + 1)))
            return -1;
        memcpy(s->label, in + at, label);
        s->label[label] = 0;
        at += label;
    }
    TEXT_TAKE(4);
    uint32_t count = text_be32(in + at);
    at += 4;
    if (!count || count > TEXT_MAX_OPS)
        return -1;
    for (uint32_t i = 0; i < count; ++i) {
        TextOp *op = &s->ops[i];
        s->count = i + 1;
        TEXT_TAKE(1);
        op->kind = (char)in[at++];
        if (op->kind == 'O') {
            TEXT_TAKE(21);
            const uint8_t *r = in + at;
            uint32_t num = text_be32(r), den = text_be32(r + 4), width = text_be32(r + 8),
                     height = text_be32(r + 12), length = text_be32(r + 17);
            op->has_extradata = r[16];
            at += 21;
            if (num > INT32_MAX || den > INT32_MAX || width > INT32_MAX || height > INT32_MAX ||
                op->has_extradata > 1 || (!op->has_extradata && length) || length > INT32_MAX / 2)
                return -1;
            TEXT_TAKE(length);
            op->num = (int)num;
            op->den = (int)den;
            op->width = (int)width;
            op->height = (int)height;
            op->extradata_size = (int)length;
            if (op->has_extradata) {
                if (!(op->extradata = av_mallocz(length + AV_INPUT_BUFFER_PADDING_SIZE)))
                    return -1;
                memcpy(op->extradata, in + at, length);
            }
            at += length;
        } else if (op->kind == 'P') {
            TEXT_TAKE(22);
            const uint8_t *r = in + at;
            uint32_t length = text_be32(r + 18);
            at += 22;
            if (r[0] > 1 || r[9] > 1 || length > INT32_MAX / 2)
                return -1;
            TEXT_TAKE(length);
            if (!(op->packet = av_packet_alloc()) || av_new_packet(op->packet, (int)length) < 0)
                return -1;
            memcpy(op->packet->data, in + at, length);
            at += length;
            op->packet->pts = r[0] ? text_be64(r + 1) : AV_NOPTS_VALUE;
            op->packet->dts = AV_NOPTS_VALUE;
            op->packet->duration = r[9] ? text_be64(r + 10) : 0;
        } else if (op->kind != 'F') {
            return -1;
        }
    }
#undef TEXT_TAKE
    return at == size && s->ops[0].kind == 'O' ? 0 : -1;
}

static AVCodecContext *text_open(const TextSession *s, const TextOp *op)
{
    const AVCodec *codec = avcodec_find_decoder(s->codec == 1 ? AV_CODEC_ID_MOV_TEXT : AV_CODEC_ID_ASS);
    AVCodecContext *ctx = codec ? avcodec_alloc_context3(codec) : NULL;
    if (!ctx)
        return NULL;
    ctx->pkt_timebase = (AVRational){op->num, op->den};
    if (s->codec == 1 && (av_opt_set_int(ctx, "width", op->width, AV_OPT_SEARCH_CHILDREN) < 0 ||
                          av_opt_set_int(ctx, "height", op->height, AV_OPT_SEARCH_CHILDREN) < 0))
        goto fail;
    if (op->has_extradata) {
        if (!(ctx->extradata = av_mallocz((size_t)op->extradata_size + AV_INPUT_BUFFER_PADDING_SIZE)))
            goto fail;
        memcpy(ctx->extradata, op->extradata, (size_t)op->extradata_size);
        ctx->extradata_size = op->extradata_size;
    }
    if (s->label && !(ctx->sub_charenc = av_strdup(s->label)))
        goto fail;
    if (avcodec_open2(ctx, codec, NULL) < 0)
        goto fail;
    return ctx;
fail:
    avcodec_free_context(&ctx);
    return NULL;
}

static void text_packet(const TextSession *s, AVCodecContext *ctx, const AVPacket *packet, TextWork *w)
{
    AVSubtitle sub = {0};
    int got = 0;
    int rc = avcodec_decode_subtitle2(ctx, &sub, &got, packet);
    if (s->codec == 1)
        text_u32(w, (uint32_t)rc);
    text_byte(w, (uint8_t)!!got);
    if (!got)
        return;
    if (s->codec == 1)
        text_u32(w, sub.format);
    text_byte(w, sub.pts != AV_NOPTS_VALUE);
    text_i64(w, sub.pts != AV_NOPTS_VALUE ? sub.pts : 0);
    text_u32(w, sub.start_display_time);
    text_u32(w, sub.end_display_time);
    text_u32(w, sub.num_rects);
    for (unsigned i = 0; i < sub.num_rects; ++i) {
        const AVSubtitleRect *r = sub.rects[i];
        if (s->codec == 1) {
            text_u32(w, (uint32_t)r->type);
            const int32_t geometry[4] = {r->x, r->y, r->w, r->h};
            for (int k = 0; k < 4; ++k)
                text_u32(w, (uint32_t)geometry[k]);
            text_u32(w, (uint32_t)r->flags);
            text_u32(w, (uint32_t)r->nb_colors);
        }
        const char *ass = r->ass ? r->ass : "";
        text_text(w, ass, strlen(ass));
    }
    avsubtitle_free(&sub);
}

static void text_session(const TextSession *s, TextWork *w)
{
    AVCodecContext *ctx = NULL;
    text_data(w, "TXT1", 4);
    text_byte(w, (uint8_t)s->codec);
    for (uint32_t i = 0; i < s->count; ++i) {
        const TextOp *op = &s->ops[i];
        if (op->kind == 'O') {
            avcodec_free_context(&ctx);
            ctx = text_open(s, op);
            text_byte(w, 'O');
            text_byte(w, ctx != NULL);
            if (ctx) {
                /* The ass decoder allocates a header even without extradata; only
                 * its bytes are compared. */
                if (s->codec == 1)
                    text_byte(w, ctx->subtitle_header != NULL);
                text_framed(w, ctx->subtitle_header, ctx->subtitle_header ? (size_t)ctx->subtitle_header_size : 0);
            }
        } else if (op->kind == 'F') {
            text_byte(w, 'F');
            if (ctx)
                avcodec_flush_buffers(ctx);
        } else if (ctx) {
            text_byte(w, 'P');
            text_packet(s, ctx, op->packet, w);
        }
    }
    avcodec_free_context(&ctx);
    text_byte(w, 'Z');
}

static const uint8_t *text_side(const AVPacket *p, enum AVPacketSideDataType type, size_t *size)
{
    *size = 0;
    const uint8_t *data = av_packet_get_side_data(p, type, size);
    if (!data)
        *size = 0;
    return data;
}

static void text_document(const uint8_t *bytes, size_t size, int vtt, TextWork *w)
{
    Source source = {bytes, size, 0};
    AVFormatContext *f = avformat_alloc_context();
    uint8_t *buffer = av_malloc(32768);
    AVIOContext *io = NULL;
    AVPacket *p = av_packet_alloc();
    int rc = AVERROR(ENOMEM);
    if (!f || !buffer || !p) {
        av_free(buffer);
        goto done;
    }
    io = avio_alloc_context(buffer, 32768, 0, &source, read_bytes, NULL, seek_bytes);
    if (!io) {
        av_free(buffer);
        goto done;
    }
    f->pb = io;
    f->flags |= AVFMT_FLAG_CUSTOM_IO;
    rc = avformat_open_input(&f, NULL, av_find_input_format(vtt ? "webvtt" : "srt"), NULL);
    if (rc < 0)
        goto done;
    while ((rc = av_read_frame(f, p)) >= 0) {
        AVRational tb = f->streams[p->stream_index]->time_base, ms = {1, 1000};
        text_byte(w, 'C');
        text_i64(w, p->pts == AV_NOPTS_VALUE ? INT64_MIN : av_rescale_q(p->pts, tb, ms));
        text_i64(w, av_rescale_q(p->duration, tb, ms));
        text_text(w, p->data, (size_t)p->size);
        if (vtt) {
            size_t n;
            const uint8_t *id = text_side(p, AV_PKT_DATA_WEBVTT_IDENTIFIER, &n);
            text_framed(w, id, n);
            const uint8_t *settings = text_side(p, AV_PKT_DATA_WEBVTT_SETTINGS, &n);
            text_framed(w, settings, n);
        }
        av_packet_unref(p);
    }
done:
    text_byte(w, rc == AVERROR_EOF ? 'Z' : 'F');
    av_packet_free(&p);
    avformat_close_input(&f);
    if (io) {
        av_freep(&io->buffer);
        avio_context_free(&io);
    }
}

/* subtitle-text-fold FILE KIND [ENCODING] | subtitle-text-bench FILE N KIND [ENCODING] |
 * subtitle-text-memory FILE KIND [ENCODING]. The SubRip and WebVTT demuxers detect a
 * UTF-16 byte-order mark themselves; the stated encoding is the candidate's input. */
static int text_main(int argc, char **argv, const uint8_t *bytes, long size)
{
    const int bench = !strcmp(argv[1], "subtitle-text-bench");
    const int memory = !strcmp(argv[1], "subtitle-text-memory");
    if (!bench && !memory && strcmp(argv[1], "subtitle-text-fold"))
        return 2;
    const int first = bench ? 4 : 3;
    if (argc < first + 1 || argc > first + 2 || size < 0)
        return 2;
    unsigned long iterations = 1;
    if (bench) {
        char *end = NULL;
        iterations = strtoul(argv[3], &end, 10);
        if (*end || !iterations || iterations > 1000000)
            return 2;
    }
    const char *kind = argv[first];
    const int packets = !strcmp(kind, "packets"), vtt = !strcmp(kind, "vtt");
    if (!packets && !vtt && strcmp(kind, "srt"))
        return 2;
    /* Decoder diagnostics are not part of the compared work; writing them would
     * add stdio inside the measured window. */
    av_log_set_level(AV_LOG_QUIET);
    static TextSession session;
    if (packets && (argc != first + 1 || text_parse(bytes, (size_t)size, &session) < 0)) {
        text_free(&session);
        fprintf(stderr, "reference: not a subtitle text capsule\n");
        return 2;
    }
    if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18)
        return 2;
    TextWork total = {0};
    const uint64_t start = bench ? now_ns() : 0;
    for (unsigned long i = 0; i < iterations; ++i) {
        TextWork once = {0};
        if (packets)
            text_session(&session, &once);
        else
            text_document(bytes, (size_t)size, vtt, &once);
        total.digest = total.digest * 31 + once.digest;
        total.packets += once.packets;
        total.bytes += once.bytes;
    }
    const uint64_t elapsed = bench ? now_ns() - start : 0;
    if (memory && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16)
        return 2;
    text_free(&session);
    printf("{\"schema\":1,\"operation\":\"" TEXT_OPERATION "\",\"iterations\":%lu"
           ",\"packets\":%" PRIu64 ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64
           ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true}\n",
           iterations, total.packets, total.bytes, total.digest, elapsed);
    return 0;
}
