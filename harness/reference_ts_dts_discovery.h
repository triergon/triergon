/* Independent public-API caller for opt-in canonical DTS core admission.
 * PSI syntax: ISO/IEC 13818-1. AMODE: TS 102 114 core header.
 * The pinned public parser's framing observations are reproduced by the seven
 * mpegts-dts-discovery inputs; no decoder or reference implementation source.
 */
#include <libavcodec/avcodec.h>

#define TS_DTS_FRAME_MAX 16384
#define TS_DTS_PROBE_MAX (2 * TS_DTS_FRAME_MAX)

/* Complete current single-section PMTs in TS188. Scan all PMTs so a changed
 * route is refused rather than silently treated as the initial route forever.
 * Bind the result to AVProgram below; a caller never supplies the ES PID. */
static int ts_dts_route(const uint8_t *bytes, size_t size, int program, int *pmt)
{
    int found = -1;
    if (size % 188) return -1;
    for (size_t offset = 0; offset < size; offset += 188) {
        const uint8_t *ts = bytes + offset;
        if (ts[0] != 0x47) return -1;
        if (!(ts[1] & 0x40) || !(ts[3] & 0x10)) continue;
        size_t at = ts[3] & 0x20 ? 5u + ts[4] : 4u;
        if (at >= 188) continue;
        at += 1u + ts[at];
        if (at + 12 > 188 || ts[at] != 2) continue;
        const uint8_t *section = ts + at;
        if (((section[3] << 8) | section[4]) != program) continue;
        size_t length = 3u + (((section[1] & 15) << 8) | section[2]);
        if (length < 16 || at + length > 188 || !(section[5] & 1)
            || section[6] || section[7] || (ts[1] & 0x80) || (ts[3] & 0xc0)) return -1;
        uint32_t crc = UINT32_MAX;
        for (size_t i = 0; i < length; ++i) {
            crc ^= (uint32_t)section[i] << 24;
            for (unsigned bit = 0; bit < 8; ++bit)
                crc = (crc << 1) ^ (crc & 0x80000000 ? 0x04c11db7 : 0);
        }
        if (crc || (section[10] & 15) || section[11]) return -1;
        int count = 0, pid = -1;
        for (size_t i = 12; i < length - 4;) {
            if (i + 5 > length - 4) return -1;
            size_t descriptors = ((section[i + 3] & 15) << 8) | section[i + 4];
            if (i + 5 + descriptors > length - 4) return -1;
            if (section[i] == 6 && !descriptors) {
                ++count;
                pid = ((section[i + 1] & 31) << 8) | section[i + 2];
            }
            i += 5 + descriptors;
        }
        int map_pid = ((ts[1] & 31) << 8) | ts[2];
        if (count != 1 || (found >= 0 && (found != pid || *pmt != map_pid))) return -1;
        *pmt = map_pid;
        found = pid;
    }
    return found;
}

/* A fresh streaming parser sees a whole envelope and its copied lookahead.
 * Its first boundary must cover exactly the original bytes. No COMPLETE_FRAMES
 * assertion, flush of a truncated tail, or handwritten DTS extent parser. */
static int ts_dts_envelope(const uint8_t *bytes, int size, int *rate, int *duration)
{
    if (size < 15 || size > TS_DTS_FRAME_MAX) return 0;
    AVCodecParserContext *parser = av_parser_init(AV_CODEC_ID_DTS);
    AVCodecContext *codec = avcodec_alloc_context3(NULL);
    uint8_t *input = av_mallocz(2u * size + AV_INPUT_BUFFER_PADDING_SIZE);
    int accepted = 0;
    if (!parser || !codec || !input) goto done;
    memcpy(input, bytes, size);
    memcpy(input + size, bytes, size);
    for (int at = 0, calls = 0; at < 2 * size && calls <= 2 * size; ++calls) {
        uint8_t *output = NULL;
        int output_size = 0;
        int used = av_parser_parse2(parser, codec, &output, &output_size,
            input + at, 2 * size - at, AV_NOPTS_VALUE, AV_NOPTS_VALUE, 0);
        if (used < 0 || used > 2 * size - at || (!used && !output_size)) break;
        at += used;
        if (output_size) {
            accepted = output_size == size && !memcmp(output, bytes, size);
            *rate = codec->sample_rate;
            *duration = parser->duration;
            break;
        }
    }
done:
    av_free(input);
    avcodec_free_context(&codec);
    if (parser) av_parser_close(parser);
    return accepted;
}

/* Delimit two original envelopes, starting at offset zero of the first PES.
 * Copied lookahead may establish a boundary but cannot supply admitted bytes. */
static int ts_dts_admit(const AVPacket *packet)
{
    if (packet->size < 30 || packet->size > TS_DTS_PROBE_MAX) return 0;
    AVCodecParserContext *parser = av_parser_init(AV_CODEC_ID_DTS);
    AVCodecContext *codec = avcodec_alloc_context3(NULL);
    uint8_t *input = av_mallocz(2u * packet->size + AV_INPUT_BUFFER_PADDING_SIZE);
    int accepted = 0;
    if (!parser || !codec || !input) goto done;
    memcpy(input, packet->data, packet->size);
    memcpy(input + packet->size, packet->data, packet->size);
    int count = 0, extent = 0, rate[2] = {0}, duration[2] = {0};
    unsigned mode[2] = {0};
    for (int at = 0, calls = 0; at < 2 * packet->size && count < 2
         && calls <= 2 * packet->size; ++calls) {
        uint8_t *output = NULL;
        int size = 0;
        int used = av_parser_parse2(parser, codec, &output, &size, input + at,
            2 * packet->size - at, AV_NOPTS_VALUE, AV_NOPTS_VALUE, packet->pos);
        if (used < 0 || used > 2 * packet->size - at || (!used && !size)) break;
        at += used;
        if (!size) continue;
        if (size < 15 || size > TS_DTS_FRAME_MAX || size > packet->size - extent
            || memcmp(output, packet->data + extent, size)
            || memcmp(output, "\x7f\xfe\x80\x01", 4)
            || !ts_dts_envelope(output, size, &rate[count], &duration[count])) break;
        mode[count] = ((output[7] & 15) << 2) | (output[8] >> 6);
        extent += size;
        ++count;
    }
    accepted = count == 2 && rate[0] > 0 && duration[0] > 0
        && rate[0] == rate[1] && duration[0] == duration[1] && mode[0] == mode[1];
done:
    av_free(input);
    avcodec_free_context(&codec);
    if (parser) av_parser_close(parser);
    return accepted;
}

static int ts_dts_discover(const uint8_t *bytes, size_t size, int program)
{
    int pmt = -1, result = -1;
    int pid = ts_dts_route(bytes, size, program, &pmt);
    if (pid < 0) return -1;
    TsInput input = {bytes, size, 0};
    AVFormatContext *format = avformat_alloc_context();
    uint8_t *buffer = av_malloc(TS_AVIO_BYTES);
    AVIOContext *io = buffer ? avio_alloc_context(buffer, TS_AVIO_BYTES, 0,
        &input, ts_read, NULL, ts_seek) : NULL;
    if (!io) av_free(buffer);
    AVPacket *packet = av_packet_alloc();
    if (!format || !io || !packet) goto done;
    format->pb = io;
    format->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_NOFILLIN | AVFMT_FLAG_NOPARSE;
    if (avformat_open_input(&format, NULL, av_find_input_format("mpegts"), NULL) < 0) goto done;
    unsigned bound = 0;
    for (unsigned i = 0; i < format->nb_programs; ++i)
        if (format->programs[i]->program_num == program && format->programs[i]->pmt_pid == pmt) ++bound;
    if (bound != 1) goto done;
    while (av_read_frame(format, packet) >= 0) {
        AVStream *stream = format->streams[packet->stream_index];
        if (stream->id != pid || !ts_in_program(format, packet->stream_index, program)) {
            av_packet_unref(packet);
            continue;
        }
        if (packet->pos < 0 || size < 188 || (uint64_t)packet->pos > size - 188) break;
        const uint8_t *ts = bytes + (size_t)packet->pos;
        size_t at = ts[3] & 0x20 ? 5u + ts[4] : 4u;
        if (ts[0] != 0x47 || !(ts[1] & 0x40) || at + 4 > 188
            || memcmp(ts + at, "\0\0\1\xbd", 4)) break;
        if (stream->codecpar->codec_id == AV_CODEC_ID_BIN_DATA && ts_dts_admit(packet)) result = pid;
        break;
    }
done:
    av_packet_free(&packet);
    avformat_close_input(&format);
    if (io) { av_freep(&io->buffer); avio_context_free(&io); }
    return result;
}

/* Admission and full replay belong to each operation. Public demuxing consumed
 * the admission PES with NOPARSE; reopen unchanged bytes to let parser mode see
 * the entire prefix. There is no redundant explicit-selection comparison pass. */
static int ts_dts_automatic_operation(const uint8_t *bytes, size_t size, int program, TsWork *work)
{
    int pid = ts_dts_discover(bytes, size, program);
    return pid < 0 ? AVERROR_INVALIDDATA : ts_operation(bytes, size, program, 1, pid, work);
}
