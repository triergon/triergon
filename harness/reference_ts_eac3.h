/* ATSC A/52:2018 Annex E Table E1.2 and Annex G §3.3 adapter.
 * Public FFmpeg APIs only. One demux/parser pass, bounded retained PES owners.
 * The caller supplies the same preloaded entity as the ordinary TS operation. */
#include <libavcodec/avcodec.h>

typedef struct { const uint8_t *data; size_t size, bit; int bad; } EcBits;
static unsigned ec_get(EcBits *b, unsigned n)
{
    unsigned value = 0;
    if (n > 24 || b->bit > b->size * 8 || n > b->size * 8 - b->bit) {
        b->bad = 1;
        return 0;
    }
    while (n--) { value = value * 2 + ((b->data[b->bit / 8] >> (7 - b->bit % 8)) & 1); ++b->bit; }
    return value;
}
static void ec_skip(EcBits *b, unsigned n)
{
    while (n && !b->bad) { unsigned step = n > 24 ? 24 : n; ec_get(b, step); n -= step; }
}
typedef struct { unsigned type, id, blocks, rate, sync; size_t size; } EcFrame;
static const char *ec_frame(const uint8_t *data, size_t size, EcFrame *f)
{
    EcBits b = {data, size, 0, 0};
    if (ec_get(&b, 16) != 0x0b77) return "syncword";
    f->type = ec_get(&b, 2); f->id = ec_get(&b, 3); f->size = (ec_get(&b, 11) + 1) * 2;
    if (f->size < 7 || f->size > size) return "truncated-syncframe";
    b.size = f->size;
    unsigned rate = ec_get(&b, 2), code = ec_get(&b, 2);
    const unsigned rates[] = {48000, 44100, 32000}, blocks[] = {1, 2, 3, 6};
    if (rate == 3 && code == 3) return "reserved-rate";
    f->rate = rate == 3 ? rates[code] / 2 : rates[rate];
    if (rate == 3) code = 3;
    f->blocks = blocks[code];
    unsigned mode = ec_get(&b, 3); ec_get(&b, 1);
    if (ec_get(&b, 5) != 16 || f->type > 1 || (!f->type && f->id)) return "unsupported-substream";
    ec_get(&b, 5); if (ec_get(&b, 1)) ec_get(&b, 8);
    if (!mode) { ec_get(&b, 5); if (ec_get(&b, 1)) ec_get(&b, 8); }
    if (f->type == 1 && ec_get(&b, 1)) ec_get(&b, 16);
    if (ec_get(&b, 1)) return "mixing-metadata-unsupported";
    if (ec_get(&b, 1)) {
        ec_get(&b, 5);
        if (mode == 2) ec_get(&b, 4);
        if (mode >= 6) ec_get(&b, 2);
        if (ec_get(&b, 1)) ec_get(&b, 8);
        if (!mode && ec_get(&b, 1)) ec_get(&b, 8);
        if (rate < 3) ec_get(&b, 1);
    }
    f->sync = !f->type && code != 3 ? ec_get(&b, 1) : 1;
    if (ec_get(&b, 1)) { unsigned n = (ec_get(&b, 6) + 1) * 8; ec_skip(&b, n); }
    return b.bad ? "truncated-bsi" : NULL;
}

enum { EC_OWNERS = 32, EC_RETAINED = 1024 * 1024 };
typedef struct {
    AVPacket *packet;
    size_t verified, at, start;
    unsigned blocks, rate, last_blocks, seen, coverage[8];
} EcOwner;
typedef struct {
    EcOwner owners[EC_OWNERS]; unsigned count; size_t retained;
    TsWork work; uint64_t source_digest; const char *refusal; int trace;
} EcState;

/* Publish before inspecting the next primary's BSI. Side data belongs to the
 * containing PES on every AU; clocks and position are consumed only once. */
static const char *ec_publish(EcState *s, const AVFormatContext *fmt, EcOwner *o, int program)
{
    if (o->blocks != 6) return "incomplete-au";
    for (unsigned i = 0; i < 8; ++i)
        if (o->coverage[i] && o->coverage[i] != 6) return "dependent-coverage";
    if (!o->rate || 138240000 % o->rate) return "nonintegral-duration";
    AVPacket p = *o->packet;
    p.data += o->start; p.size = (int)(o->at - o->start);
    p.duration = 138240000 / o->rate; p.flags |= AV_PKT_FLAG_KEY;
    if (o->start) { p.pts = p.dts = AV_NOPTS_VALUE; p.pos = -1; }
    ts_fold_packet(fmt, &p, program, &s->work);
    if (s->trace) {
        printf("{\"stream\":%d,\"pid\":%d,\"pts\":%"PRId64",\"dts\":%"PRId64
               ",\"duration\":%"PRId64",\"pos\":%"PRId64",\"flags\":%d,\"hex\":\"",
               p.stream_index, fmt->streams[p.stream_index]->id, p.pts, p.dts, p.duration, p.pos, p.flags);
        for (int i = 0; i < p.size; ++i) printf("%02x", p.data[i]);
        printf("\",\"side_data\":[");
        for (int i = 0; i < p.side_data_elems; ++i) {
            const AVPacketSideData *side = &p.side_data[i];
            printf("%s{\"type\":%d,\"hex\":\"", i ? "," : "", side->type);
            for (size_t j = 0; j < side->size; ++j) printf("%02x", side->data[j]);
            printf("\"}");
        }
        puts("]}");
    }
    o->start = o->at; o->blocks = o->rate = o->last_blocks = o->seen = 0;
    memset(o->coverage, 0, sizeof(o->coverage));
    return NULL;
}
static const char *ec_advance(EcState *s, const AVFormatContext *fmt, int program)
{
    while (s->count) {
        EcOwner *o = &s->owners[0]; size_t size = (size_t)o->packet->size;
        while (o->at < size) {
            const uint8_t *p = o->packet->data + o->at;
            if (size - o->at < 4) return "truncated-header";
            if (p[0] != 0x0b || p[1] != 0x77) return "syncword";
            if (!(p[2] >> 6) && o->blocks == 6) {
                const char *error = ec_publish(s, fmt, o, program); if (error) return error;
            }
            size_t extent = ((((size_t)p[2] & 7) << 8) + p[3] + 1) * 2;
            if (extent > size - o->at) return "truncated-syncframe";
            if (extent > o->verified - o->at) return NULL;
            EcFrame f; const char *error = ec_frame(p, size - o->at, &f); if (error) return error;
            if (!f.type) {
                if (!o->blocks && !f.sync) return "missing-au-convsync";
                if (o->blocks + f.blocks > 6) return "primary-overflow";
                if (o->rate && o->rate != f.rate) return "primary-rate-change";
                o->rate = f.rate; o->blocks += f.blocks; o->last_blocks = f.blocks; o->seen = 0;
            } else {
                if (!o->last_blocks) return "orphan-dependent";
                if (o->seen & (1u << f.id)) return "duplicate-dependent";
                if (o->last_blocks != f.blocks || o->rate != f.rate) return "dependent-timing";
                o->seen |= 1u << f.id; o->coverage[f.id] += f.blocks;
            }
            o->at += extent;
        }
        const char *error = ec_publish(s, fmt, o, program); if (error) return error;
        s->retained -= size; av_packet_free(&o->packet);
        --s->count; memmove(s->owners, s->owners + 1, s->count * sizeof(*o));
    }
    return NULL;
}
static const char *ec_interval(EcState *s, const AVFormatContext *fmt, int program,
                               const uint8_t *data, int size)
{
    if (size <= 0) return NULL;
    for (unsigned i = 0; i < s->count; ++i) {
        EcOwner *o = &s->owners[i]; size_t left = (size_t)o->packet->size - o->verified;
        if (!left) continue;
        if ((size_t)size > left) return "parser-crosses-pes";
        if (memcmp(data, o->packet->data + o->verified, (size_t)size)) return "parser-byte-identity";
        o->verified += (size_t)size;
        return ec_advance(s, fmt, program);
    }
    return "parser-unowned-output";
}
/* Original PES clocks, without the demuxer's optional DTS synthesis. The
 * proof boundary is TS188 with the complete optional header in its first carrier.
 * No elementary payload is copied or rescanned to recover clocks. */
static const char *ec_source_clock(const uint8_t *bytes, size_t size, AVPacket *packet, int pid, uint64_t *digest)
{
    AVPacket original = *packet;
    packet = &original;
    if (packet->pos < 0 || (uint64_t)packet->pos > size || size - (size_t)packet->pos < 188
        || packet->pos % 188) return "source-position";
    const uint8_t *ts = bytes + (size_t)packet->pos;
    if (ts[0] != 0x47 || !(ts[1] & 0x40) || (ts[1] & 0x80) || (ts[3] & 0xc0)
        || (((ts[1] & 31) << 8) | ts[2]) != pid) return "source-carrier";
    unsigned control = (ts[3] >> 4) & 3;
    size_t at = control == 3 ? 5u + ts[4] : 4u;
    if (!(control & 1) || at > 188 - 9) return "source-header";
    const uint8_t *p = ts + at;
    if (p[0] || p[1] || p[2] != 1 || p[3] != 0xbd || (p[6] & 0xc0) != 0x80
        || (p[7] & 0xc0) == 0x40 || p[8] > 188 - at - 9) return "source-header";
    if ((((unsigned)p[4] << 8) | p[5]) != (unsigned)packet->size + 3u + p[8])
        return "finite-pes-extent";
    unsigned clocks = p[7] >> 6, needed = clocks == 3 ? 10 : clocks == 2 ? 5 : 0;
    if (p[8] < needed) return "source-clock-length";
    packet->pts = packet->dts = AV_NOPTS_VALUE;
    for (unsigned i = 0; i < needed / 5; ++i) {
        const uint8_t *t = p + 9 + i * 5;
        unsigned prefix = i ? 1 : clocks == 3 ? 3 : 2;
        if (t[0] >> 4 != prefix || !(t[0] & t[2] & t[4] & 1)) return "source-clock-marker";
        int64_t clock = ((int64_t)(t[0] & 14) << 29) | ((int64_t)t[1] << 22)
            | ((int64_t)(t[2] & 254) << 14) | ((int64_t)t[3] << 7) | (t[4] >> 1);
        if (i) packet->dts = clock; else packet->pts = clock;
    }
    fold(digest, (uint64_t)packet->pos);
    fold(digest, (uint64_t)packet->pts); fold(digest, (uint64_t)packet->dts);
    fold(digest, 9u + p[8]); fold_bytes(digest, p, 9u + p[8]);
    return NULL;
}
static int ec_proof(const uint8_t *bytes, size_t size, int program, int trace)
{
    TsInput input = {bytes, size, 0}; EcState state = {.trace = trace};
    AVFormatContext *fmt = avformat_alloc_context();
    AVIOContext *io = NULL; AVPacket *packet = av_packet_alloc();
    AVCodecContext *codec = avcodec_alloc_context3(NULL);
    AVCodecParserContext *parser = av_parser_init(AV_CODEC_ID_EAC3);
    uint8_t *buffer = av_malloc(TS_AVIO_BYTES); int rc = AVERROR(ENOMEM), selected = -1;
    if (!fmt || !packet || !codec || !parser || !buffer) goto done;
    io = avio_alloc_context(buffer, TS_AVIO_BYTES, 0, &input, ts_read, NULL, NULL);
    if (!io) goto done;
    buffer = NULL; fmt->pb = io;
    fmt->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN;
    codec->codec_id = AV_CODEC_ID_EAC3;
    rc = avformat_open_input(&fmt, NULL, av_find_input_format("mpegts"), NULL);
    if (rc < 0) goto done;
    while ((rc = av_read_frame(fmt, packet)) >= 0) {
        if (!ts_in_program(fmt, packet->stream_index, program)) { av_packet_unref(packet); continue; }
        const AVStream *stream = fmt->streams[packet->stream_index];
        if (stream->codecpar->codec_id != AV_CODEC_ID_EAC3 || stream->codecpar->codec_tag != 0x87 || (selected >= 0 && selected != packet->stream_index)
            || stream->time_base.num != 1 || stream->time_base.den != 90000 || packet->size <= 0) {
            state.refusal = "selection"; break;
        }
        selected = packet->stream_index;
        state.refusal = ec_source_clock(bytes, size, packet, stream->id, &state.source_digest);
        if (state.refusal) break;
        if (state.count == EC_OWNERS || (size_t)packet->size > EC_RETAINED - state.retained) {
            state.refusal = "retained-pes-limit"; break;
        }
        EcOwner *o = &state.owners[state.count]; memset(o, 0, sizeof(*o));
        o->packet = av_packet_clone(packet);
        if (!o->packet) { rc = AVERROR(ENOMEM); break; }
        ++state.count; state.retained += (size_t)packet->size;
        int at = 0, stalls = 0;
        while (at < packet->size) {
            uint8_t *out = NULL; int n = 0;
            int used = av_parser_parse2(parser, codec, &out, &n, packet->data + at, packet->size - at,
                at ? AV_NOPTS_VALUE : packet->pts, at ? AV_NOPTS_VALUE : packet->dts, packet->pos);
            if (used < 0 || used > packet->size - at || (!used && !n) || ++stalls > packet->size + 1024) {
                state.refusal = "parser-progress"; break;
            }
            state.refusal = ec_interval(&state, fmt, program, out, n);
            if (state.refusal) break;
            at += used;
        }
        av_packet_unref(packet); if (state.refusal) break;
    }
    if (!state.refusal && rc == AVERROR_EOF) {
        for (unsigned i = 0; i < 16; ++i) {
            uint8_t *out = NULL; int n = 0;
            int used = av_parser_parse2(parser, codec, &out, &n, NULL, 0, AV_NOPTS_VALUE, AV_NOPTS_VALUE, -1);
            if (used < 0) { state.refusal = "parser-drain"; break; }
            if (!n) break;
            state.refusal = ec_interval(&state, fmt, program, out, n);
            if (state.refusal) break;
            if (i == 15) state.refusal = "parser-drain-limit";
        }
        if (!state.refusal && (state.count || selected < 0)) state.refusal = "incomplete-parser-coverage";
    }
 done:
    for (unsigned i = 0; i < state.count; ++i) av_packet_free(&state.owners[i].packet);
    av_packet_free(&packet); if (parser) av_parser_close(parser); avcodec_free_context(&codec);
    avformat_close_input(&fmt); av_free(buffer);
    if (io) { av_freep(&io->buffer); avio_context_free(&io); }
    int eof = !state.refusal && rc == AVERROR_EOF;
    printf("{\"operation\":\"atsc-eac3-six-block-proof\",\"packets\":%"PRIu64
           ",\"bytes\":%"PRIu64",\"digest\":%"PRIu64",\"eof\":%s,\"refusal\":",
           state.work.packets, state.work.bytes, state.work.digest, eof ? "true" : "false");
    quote(state.refusal ? state.refusal : eof ? "" : "demux-or-allocation");
    printf(",\"source_digest\":%"PRIu64"}\n", state.source_digest);
    return eof ? 0 : 1;
}
