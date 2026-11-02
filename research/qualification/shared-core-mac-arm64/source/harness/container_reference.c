/* Isolated public-API FFmpeg MOV/TS observation adapter. Native research tooling;
 * never linked into the pure-safe production graph. No implementation copied
 * from FFmpeg. This trace alone is not a qualification/performance result. */
#include <libavformat/avformat.h>
#include <libavutil/dict.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef SAFE_MEDIA_REFERENCE_MOV
static const char *input_format = "mov";
static const char *operation = "mov-native-packets-observation-v1";
#else
static const char *input_format = "mpegts";
static const char *operation = "mpegts-native-pes-observation-v1";
#endif

typedef struct {
    const uint8_t *bytes;
    size_t length, position, maximum_read;
} Source;

static int read_source(void *opaque, uint8_t *bytes, int requested) {
    Source *source = opaque;
    size_t left = source->length - source->position;
    if (!left) return AVERROR_EOF;
    size_t count = (size_t)requested;
    if (count > left) count = left;
    if (count > source->maximum_read) count = source->maximum_read;
    memcpy(bytes, source->bytes + source->position, count);
    source->position += count;
    return (int)count;
}
static int64_t seek_source(void *opaque, int64_t offset, int origin) {
    Source *source = opaque;
    if (origin == AVSEEK_SIZE) return (int64_t)source->length;
    origin &= ~AVSEEK_FORCE;
    int64_t base = origin == SEEK_SET ? 0 : origin == SEEK_CUR ? (int64_t)source->position
        : origin == SEEK_END ? (int64_t)source->length : -1;
    if (base < 0 || offset < -base || offset > (int64_t)source->length - base)
        return AVERROR(EINVAL);
    source->position = (size_t)(base + offset);
    return (int64_t)source->position;
}
static void quote(const char *text) {
    putchar('"');
    if (text) for (; *text; ++text) {
        unsigned char byte = (unsigned char)*text;
        if (byte == '"' || byte == '\\') { putchar('\\'); putchar(byte); }
        else if (byte < 32) printf("\\u%04x", byte);
        else putchar(byte);
    }
    putchar('"');
}
static void hex(const uint8_t *bytes, size_t count) {
    putchar('"');
    for (size_t i = 0; i < count; ++i) printf("%02x", bytes[i]);
    putchar('"');
}
static void timestamp(int64_t value) {
    if (value == AV_NOPTS_VALUE) printf("null");
    else printf("%" PRId64, value);
}
static void dictionary(const AVDictionary *dictionary) {
    const AVDictionaryEntry *entry = NULL;
    unsigned count = 0;
    putchar('[');
    while ((entry = av_dict_iterate(dictionary, entry))) {
        if (count++) putchar(',');
        printf("{\"key_hex\":"); hex((const uint8_t *)entry->key, strlen(entry->key));
        printf(",\"value_hex\":"); hex((const uint8_t *)entry->value, strlen(entry->value));
        putchar('}');
    }
    putchar(']');
}
static int in_program(const AVFormatContext *format, int index, int program) {
    if (!program) return 1;
    for (unsigned i = 0; i < format->nb_programs; ++i) {
        const AVProgram *item = format->programs[i];
        if (item->program_num != program) continue;
        for (unsigned j = 0; j < item->nb_stream_indexes; ++j)
            if ((int)item->stream_index[j] == index) return 1;
    }
    return 0;
}

static int trace(const uint8_t *bytes, size_t length, size_t maximum_read, int program) {
    int result = AVERROR(ENOMEM), first = 1;
    Source source = {bytes, length, 0, maximum_read};
    AVFormatContext *format = avformat_alloc_context();
    AVIOContext *io = NULL;
    AVPacket *packet = NULL;
    uint8_t *buffer = av_malloc(32768);
    if (!format || !buffer) { av_free(buffer); goto done; }
    io = avio_alloc_context(buffer, 32768, 0, &source, read_source, NULL, seek_source);
    if (!io) { av_free(buffer); goto done; }
    format->pb = io;
    format->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN;
    result = avformat_open_input(&format, NULL, av_find_input_format(input_format), NULL);
    if (result < 0) goto done;
    packet = av_packet_alloc();
    if (!packet) { result = AVERROR(ENOMEM); goto done; }
    printf("{\"schema\":1,\"operation\":"); quote(operation);
    printf(",\"program\":%d,\"packets\":[", program);
    while ((result = av_read_frame(format, packet)) >= 0) {
        if (in_program(format, packet->stream_index, program)) {
            if (!first) putchar(',');
            first = 0;
            const AVStream *stream = format->streams[packet->stream_index];
            printf("{\"stream\":%d,\"pid\":%d,\"pts\":", packet->stream_index, stream->id);
            timestamp(packet->pts); printf(",\"dts\":"); timestamp(packet->dts);
            printf(",\"duration\":%" PRId64 ",\"position\":%" PRId64 ",\"flags\":%d,\"payload\":",
                packet->duration, packet->pos, packet->flags);
            hex(packet->data, (size_t)packet->size);
            printf(",\"side_data\":[");
            for (int i = 0; i < packet->side_data_elems; ++i) {
                if (i) putchar(',');
                printf("{\"type\":%d,\"bytes\":", packet->side_data[i].type);
                hex(packet->side_data[i].data, packet->side_data[i].size); putchar('}');
            }
            printf("]}");
        }
        av_packet_unref(packet);
    }
    printf("],\"streams\":["); first = 1;
    for (unsigned i = 0; i < format->nb_streams; ++i) {
        const AVStream *stream = format->streams[i];
        if (!in_program(format, (int)i, program)) continue;
        if (!first) putchar(','); first = 0;
        printf("{\"stream\":%u,\"pid\":%d,\"codec\":", i, stream->id);
        quote(avcodec_get_name(stream->codecpar->codec_id));
        printf(",\"type\":%d,\"time_base\":[%d,%d],\"extradata\":", stream->codecpar->codec_type,
            stream->time_base.num, stream->time_base.den);
        hex(stream->codecpar->extradata, (size_t)stream->codecpar->extradata_size);
        printf(",\"tags\":"); dictionary(stream->metadata); putchar('}');
    }
    printf("],\"programs\":["); first = 1;
    for (unsigned i = 0; i < format->nb_programs; ++i) {
        const AVProgram *item = format->programs[i];
        if (!first) putchar(','); first = 0;
        printf("{\"id\":%d,\"program\":%d,\"pmt_pid\":%d,\"pcr_pid\":%d,\"tags\":",
            item->id, item->program_num, item->pmt_pid, item->pcr_pid);
        dictionary(item->metadata); putchar('}');
    }
    printf("],\"read_result\":%d,\"confirmed_eof\":%s}\n", result, result == AVERROR_EOF ? "true" : "false");
    if (result == AVERROR_EOF) result = 0;
done:
    av_packet_free(&packet);
    avformat_close_input(&format);
    if (io) { av_freep(&io->buffer); avio_context_free(&io); }
    return result;
}

int main(int argc, char **argv) {
    av_log_set_level(AV_LOG_ERROR);
    if (argc == 2 && !strcmp(argv[1], "identity")) {
        printf("{\"version\":"); quote(av_version_info());
        printf(",\"configuration\":"); quote(avformat_configuration());
        printf(",\"input_format\":"); quote(input_format);
        printf(",\"format_flags\":%d,\"stream_info\":false,\"max_packet_size\":\"native default\"}\n",
            AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN);
        return 0;
    }
    char *end = NULL;
#ifdef SAFE_MEDIA_REFERENCE_MOV
    if (argc < 3 || argc > 4 || strcmp(argv[1], "trace")) return 2;
    long program = 0;
    const int chunk_index = 3;
#else
    if (argc < 4 || argc > 5 || strcmp(argv[1], "trace")) return 2;
    long program = strtol(argv[3], &end, 10);
    if (*end || program < 1 || program > 65535) return 2;
    const int chunk_index = 4;
#endif
    size_t maximum_read = 65536;
    if (argc > chunk_index) {
        unsigned long value = strtoul(argv[chunk_index], &end, 10);
        if (*end || !value || value > 65536) return 2;
        maximum_read = (size_t)value;
    }
    FILE *file = fopen(argv[2], "rb");
    if (!file) return 2;
    if (fseek(file, 0, SEEK_END) || ftell(file) < 0) { fclose(file); return 2; }
    long size = ftell(file); rewind(file);
    if (size > 64 * 1024 * 1024) { fclose(file); return 2; }
    uint8_t *data = malloc((size_t)size + 1);
    if (!data) { fclose(file); return 2; }
    if (fread(data, 1, (size_t)size, file) != (size_t)size) { free(data); fclose(file); return 2; }
    fclose(file);
    int result = trace(data, (size_t)size, maximum_read, (int)program);
    free(data);
    if (result < 0) { char error[256]; av_strerror(result, error, sizeof(error)); fprintf(stderr, "%s\n", error); }
    return result < 0 ? 1 : 0;
}
