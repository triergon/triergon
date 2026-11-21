/* Own public-API caller. The demuxer preserves PES packets; an explicit DTS
 * parser supplies AU boundaries. No stream discovery, decoder or FFmpeg source
 * is used. H.222.0 PES headers supply original clocks, and TS 102 114 Table 7-2
 * supplies the coded reference-clock span, independently of opaque asset data. */
#include <libavcodec/avcodec.h>

#define DTS_HD_PREFIX "ts-dts-hd-"
#define DTS_HD_OPERATION "mpegts-dts-hd-source-access-units"

typedef struct {
    uint8_t header[264];
    size_t header_size, carrier;
    uint64_t header_offset;
    int64_t pts, dts;
} DtsHdSource;

typedef struct { uint64_t numerator, denominator; } DtsHdSpan;

/* H.222.0 2.4.3.6: gather the original header, including split TS headers. */
static int dts_hd_source(const TsInput *input, const AVPacket *packet, int pid,
                         size_t carrier, DtsHdSource *source)
{
    if (packet->pos < 0 || (uint64_t)packet->pos >= input->size)
        return AVERROR_INVALIDDATA;
    source->carrier = (size_t)packet->pos / carrier * carrier;
    source->pts = source->dts = AV_NOPTS_VALUE;
    size_t used = 0, wanted = 9;
    int start = 1;
    for (size_t at = source->carrier; at <= input->size && carrier <= input->size - at;
         at += carrier) {
        const uint8_t *p = input->data + at + carrier - 188;
        if (p[0] != 0x47)
            return AVERROR_INVALIDDATA;
        if ((((p[1] & 31) << 8) | p[2]) != pid)
            continue;
        int control = (p[3] >> 4) & 3;
        if ((p[1] & 0x80) || (p[3] & 0xc0) || control == 0)
            return AVERROR_INVALIDDATA;
        if (!(control & 1))
            continue;
        if (!!(p[1] & 0x40) != start)
            return AVERROR_INVALIDDATA;
        size_t byte = control == 3 ? 5u + p[4] : 4u;
        if (byte >= 188)
            return AVERROR_INVALIDDATA;
        if (start)
            source->header_offset = at + carrier - 188 + byte;
        start = 0;
        for (; byte < 188 && used < wanted; ++byte) {
            source->header[used++] = p[byte];
            if (used == 9) {
                const uint8_t *h = source->header;
                if (h[0] || h[1] || h[2] != 1 || h[3] != 0xbd || (h[6] & 0xc4) != 0x84
                    || (h[7] & 0xc0) == 0x40)
                    return AVERROR_INVALIDDATA;
                wanted += h[8];
            }
        }
        if (used == wanted)
            break;
    }
    const uint8_t *h = source->header;
    if (used != wanted || packet->size <= 0 || (size_t)(h[4] << 8 | h[5]) != wanted - 6 + (size_t)packet->size)
        return AVERROR_INVALIDDATA;
    unsigned clocks = h[7] >> 6;
    if (wanted < 9u + (clocks == 3 ? 10u : clocks == 2 ? 5u : 0u))
        return AVERROR_INVALIDDATA;
    for (unsigned i = 0; i < (clocks == 3 ? 2u : clocks == 2 ? 1u : 0u); ++i) {
        const uint8_t *t = h + 9 + 5 * i;
        unsigned prefix = i ? 1 : clocks;
        if ((unsigned)(t[0] >> 4) != prefix || !(t[0] & t[2] & t[4] & 1))
            return AVERROR_INVALIDDATA;
        int64_t value = ((int64_t)(t[0] & 14) << 29) | ((int64_t)t[1] << 22)
            | ((int64_t)(t[2] & 254) << 14) | ((int64_t)t[3] << 7) | (t[4] >> 1);
        if (i) source->dts = value; else source->pts = value;
    }
    source->header_size = wanted;
    return 0;
}

static unsigned dts_hd_bits(const uint8_t *bytes, unsigned offset, unsigned count)
{
    unsigned value = 0;
    for (unsigned i = 0; i < count; ++i)
        value = (value << 1) | ((bytes[(offset + i) / 8] >> (7 - (offset + i) % 8)) & 1);
    return value;
}

/* Source timing, rather than the parser's duration inferred from asset bodies.
 * Each substream has its own static fields; the AU uses its lowest index. */
static int dts_hd_span(const uint8_t *bytes, size_t size, DtsHdSpan spans[4], DtsHdSpan *span)
{
    if (size < 12 || memcmp(bytes, "\x64\x58\x20\x25", 4))
        return AVERROR_INVALIDDATA;
    unsigned index = bytes[5] >> 6;
    unsigned offset = bytes[5] & 32 ? 75 : 67;
    if (dts_hd_bits(bytes, offset++, 1)) {
        unsigned clock = dts_hd_bits(bytes, offset, 2);
        uint64_t n = 512u * (dts_hd_bits(bytes, offset + 2, 3) + 1u);
        uint64_t d = clock == 0 ? 32000 : clock == 1 ? 44100 : clock == 2 ? 48000 : 0;
        if (!d)
            return AVERROR_INVALIDDATA;
        uint64_t a = n, b = d;
        while (b) { uint64_t next = a % b; a = b; b = next; }
        spans[index] = (DtsHdSpan){n / a, d / a};
    }
    *span = spans[index];
    return 0;
}

static int dts_hd_emit(const AVPacket *pes, const DtsHdSource *source,
                       const AVCodecParserContext *parser, const uint8_t *bytes, int size,
                       int pid, uint64_t base, size_t *emitted, DtsHdSpan spans[4], TsWork *work,
                       int trace, int verify_payload)
{
    if (size <= 0 || (size_t)size > (size_t)pes->size - *emitted
        || parser->frame_offset < 0 || (uint64_t)parser->frame_offset != *emitted
        || (verify_payload && memcmp(bytes, pes->data + *emitted, (size_t)size)))
        return AVERROR_INVALIDDATA;
    DtsHdSpan span;
    int rc = dts_hd_span(bytes, (size_t)size, spans, &span);
    if (rc < 0)
        return rc;
    const uint8_t *h = source->header;
    uint64_t fields[] = {
        (uint64_t)pes->stream_index, (uint64_t)pid, (uint64_t)parser->pts, (uint64_t)parser->dts,
        (uint64_t)source->pts, (uint64_t)source->dts, source->header_offset, source->carrier,
        (unsigned)(h[4] << 8 | h[5]), (uint64_t)pes->size, base, *emitted,
        *emitted + (size_t)size, base + (uint64_t)parser->frame_offset,
        base + (uint64_t)parser->frame_offset + (size_t)size, span.numerator, span.denominator,
        source->header_size, *emitted == 0,
    };
    /* PID is supplied by the selected demux stream, not a fixture constant. */
    for (size_t i = 0; i < sizeof(fields) / sizeof(*fields); ++i)
        fold(&work->digest, fields[i]);
    fold_bytes(&work->digest, h, source->header_size);
    fold_bytes(&work->digest, bytes, (size_t)size);
    work->packets++;
    work->bytes += (uint64_t)size;
    *emitted += (size_t)size;
    if (trace) {
        uint64_t digest = 0;
        fold_bytes(&digest, bytes, (size_t)size);
        printf("{\"fields\":[");
        for (size_t i = 0; i < sizeof(fields) / sizeof(*fields); ++i)
            printf("%s%" PRIu64, i ? ", " : "", fields[i]);
        printf("],\"payload_digest\":%" PRIu64 "}\n", digest);
    }
    return 0;
}

static int dts_hd_operation(const uint8_t *bytes, size_t size, int program,
                            size_t carrier, TsWork *work, int trace, int verify_payload)
{
    TsInput input = {bytes, size, 0};
    AVFormatContext *format = avformat_alloc_context();
    AVIOContext *io = NULL;
    AVPacket *packet = NULL;
    AVCodecParserContext *parser = NULL;
    AVCodecContext *codec = avcodec_alloc_context3(NULL);
    uint8_t *buffer = av_malloc(TS_AVIO_BYTES);
    int rc = AVERROR(ENOMEM);
    if (!format || !buffer || !codec) { av_free(buffer); goto done; }
    io = avio_alloc_context(buffer, TS_AVIO_BYTES, 0, &input, ts_read, NULL, ts_seek);
    if (!io) { av_free(buffer); goto done; }
    codec->codec_id = AV_CODEC_ID_DTS;
    format->pb = io;
    format->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN;
    rc = avformat_open_input(&format, NULL, av_find_input_format("mpegts"), NULL);
    if (rc < 0) goto done;
    packet = av_packet_alloc();
    if (!packet) { rc = AVERROR(ENOMEM); goto done; }
    uint64_t base = 0;
    DtsHdSpan spans[4] = {{0}};
    while ((rc = av_read_frame(format, packet)) >= 0) {
        int index = packet->stream_index;
        if (!ts_in_program(format, index, program)) { av_packet_unref(packet); continue; }
        /* The pinned demuxer exposes this DVB extension descriptor as bin_data.
         * This operation explicitly requests DTS framing of the selected PES. */
        if (format->nb_streams != 1) {
            rc = AVERROR_INVALIDDATA; goto done;
        }
        DtsHdSource source;
        rc = dts_hd_source(&input, packet, format->streams[index]->id, carrier, &source);
        if (rc < 0) goto done;
        /* Every aligned finite PES is independently complete (F.4.3).
         * Flush and close its parser before starting the next PES. */
        parser = av_parser_init(AV_CODEC_ID_DTS);
        if (!parser) { rc = AVERROR(ENOMEM); goto done; }
        size_t emitted = 0;
        int at = 0, flushing = 0;
        for (int calls = 0; ; ++calls) {
            if (calls > packet->size + 2) { rc = AVERROR_INVALIDDATA; goto done; }
            uint8_t *output = NULL;
            int output_size = 0;
            int used = av_parser_parse2(parser, codec, &output, &output_size,
                flushing ? NULL : packet->data + at, flushing ? 0 : packet->size - at,
                !flushing && !at ? source.pts : AV_NOPTS_VALUE,
                !flushing && !at ? source.dts : AV_NOPTS_VALUE, at);
            if (used < 0 || used > packet->size - at || (!flushing && !used && !output_size)) {
                rc = used < 0 ? used : AVERROR_INVALIDDATA; goto done;
            }
            if (output_size) {
                rc = dts_hd_emit(packet, &source, parser, output, output_size, format->streams[index]->id, base,
                    &emitted, spans, work, trace, verify_payload);
                if (rc < 0) goto done;
            }
            at += used;
            if (flushing && !output_size) break;
            flushing = at == packet->size;
        }
        if (emitted != (size_t)packet->size) { rc = AVERROR_INVALIDDATA; goto done; }
        base += emitted;
        av_parser_close(parser);
        parser = NULL;
        av_packet_unref(packet);
    }
    if (rc == AVERROR_EOF && work->packets) rc = 1;
done:
    if (parser) av_parser_close(parser);
    avcodec_free_context(&codec);
    av_packet_free(&packet);
    avformat_close_input(&format);
    if (io) { av_freep(&io->buffer); avio_context_free(&io); }
    return rc;
}

static int dts_hd_main(int argc, char **argv, const uint8_t *bytes, long size)
{
    const char *mode = argv[1] + sizeof(DTS_HD_PREFIX) - 1;
    int bench = !strcmp(mode, "bench"), memory = !strcmp(mode, "memory"), trace = !strcmp(mode, "trace");
    int at = bench ? 4 : 3;
    if (argc != at + 3 || (!bench && !memory && !trace && strcmp(mode, "fold"))
        || (strcmp(argv[at], "188") && strcmp(argv[at], "192")) || strcmp(argv[at+2], "--dvb"))
        return 2;
    char *end = NULL;
    unsigned long iterations = bench ? strtoul(argv[3], &end, 10) : 1;
    if (!iterations || iterations > 1000000 || (bench && *end)) return 2;
    long program = strtol(argv[at+1], &end, 10);
    if (*end || program < 1 || program > 65535) return 2;
    av_log_set_level(AV_LOG_ERROR);
    if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18) return 2;
    uint64_t start = bench ? now_ns() : 0;
    TsWork total = {0};
    for (unsigned long i = 0; i < iterations; ++i) {
        TsWork next = {0};
        int rc = dts_hd_operation(bytes, (size_t)size, (int)program, (size_t)atoi(argv[at]), &next,
                                  trace, !bench && !memory);
        if (rc != 1) { fprintf(stderr, "DTS-HD source comparison failed: %d\n", rc); return 2; }
        total.packets += next.packets; total.bytes += next.bytes; fold(&total.digest, next.digest);
    }
    uint64_t elapsed = bench ? now_ns() - start : 0;
    if (memory && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16) return 2;
    printf("{\"schema\":1,\"operation\":\"" DTS_HD_OPERATION "\",\"iterations\":%lu,\"packets\":%" PRIu64
        ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true}\n",
        iterations, total.packets, total.bytes, total.digest, elapsed);
    return 0;
}
