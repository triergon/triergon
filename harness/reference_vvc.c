/* Public FFmpeg parser API reference for VVC Annex B access units. */
#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <libavcodec/avcodec.h>
#include <libavutil/log.h>
#include <libavutil/mem.h>
#include <libavutil/avutil.h>

#include "reference_digest.h"

#define INPUT_LIMIT (16L * 1024L * 1024L)
#define OPERATION "vvc-annex-b-access-units"

typedef struct { uint64_t packets, bytes, digest; } Result;

static void quote(const char *s)
{
    putchar('"');
    for (const unsigned char *p = (const unsigned char *)s; *p; ++p) {
        if (*p == '"' || *p == '\\') { putchar('\\'); putchar(*p); }
        else if (*p < 32) printf("\\u%04x", (unsigned)*p);
        else putchar(*p);
    }
    putchar('"');
}

static uint64_t now_ns(void)
{
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) return 0;
    return (uint64_t)t.tv_sec * UINT64_C(1000000000) + (uint64_t)t.tv_nsec;
}

static int operation(const uint8_t *input, size_t size, Result *result)
{
    AVCodecParserContext *parser = av_parser_init(AV_CODEC_ID_VVC);
    AVCodecContext *codec = avcodec_alloc_context3(NULL);
    size_t at = 0;
    int ok = 0;
    if (!parser || !codec) goto done;
    for (;;) {
        uint8_t *packet = NULL;
        int length = 0;
        int used = av_parser_parse2(parser, codec, &packet, &length,
                                    input + at, (int)(size - at),
                                    AV_NOPTS_VALUE, AV_NOPTS_VALUE, (int64_t)at);
        if (used < 0 || (size_t)used > size - at || length < 0 ||
            (length && !packet) || (uint64_t)length > size - result->bytes)
            goto done;
        if (length) {
            fold(&result->digest, result->bytes);
            fold(&result->digest, (uint64_t)length);
            fold_bytes(&result->digest, packet, (size_t)length);
            result->bytes += (uint64_t)length;
            ++result->packets;
        }
        at += (size_t)used;
        if (!used && !length) {
            if (at == size && result->bytes == size) ok = 1;
            break;
        }
    }
done:
    if (parser) av_parser_close(parser);
    avcodec_free_context(&codec);
    return ok ? 0 : -1;
}

int main(int argc, char **argv)
{
    if (avcodec_version() != LIBAVCODEC_VERSION_INT || avutil_version() != LIBAVUTIL_VERSION_INT)
        return 78;
    av_log_set_level(AV_LOG_QUIET);
    if ((argc == 2 || argc == 3) && !strcmp(argv[1], "identity")) {
        printf("{\"operation\":\"" OPERATION "\",\"adapter\":\"ffmpeg-vvc-parser\",\"version\":");
        quote(av_version_info());
        printf(",\"configuration\":"); quote(avcodec_configuration());
        printf(",\"codec_version\":%u,\"util_version\":%u,\"header_codec_version\":%u,\"header_util_version\":%u}\n",
               avcodec_version(), avutil_version(), LIBAVCODEC_VERSION_INT, LIBAVUTIL_VERSION_INT);
        return ferror(stdout) ? 74 : 0;
    }
    if (argc < 3) return 64;
    int bench = !strcmp(argv[1], "vvc-au-bench");
    int memory = !strcmp(argv[1], "vvc-au-memory");
    if ((!bench && !memory && strcmp(argv[1], "vvc-au-fold")) || argc != (bench ? 4 : 3)) return 64;
    char *end = NULL;
    errno = 0;
    unsigned long iterations = bench ? strtoul(argv[3], &end, 10) : 1;
    if (!iterations || iterations > 1000000 || (bench && (errno || end == argv[3] || *end || argv[3][0] == '-'))) return 64;
    FILE *file = fopen(argv[2], "rb");
    if (!file) return 66;
    long length = fseek(file, 0, SEEK_END) ? -1 : ftell(file);
    if (length < 0 || length > INPUT_LIMIT || fseek(file, 0, SEEK_SET)) { fclose(file); return 65; }
    uint8_t *input = av_mallocz((size_t)length + AV_INPUT_BUFFER_PADDING_SIZE);
    if (!input) { fclose(file); return 70; }
    int failed = fread(input, 1, (size_t)length, file) != (size_t)length;
    if (fclose(file)) failed = 1;
    if (failed) { av_free(input); return 74; }
    if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18) { av_free(input); return 74; }
    uint64_t start = bench ? now_ns() : 0;
    Result total = {0};
    for (unsigned long i = 0; i < iterations; ++i) {
        Result next = {0};
        if (operation(input, (size_t)length, &next)) { failed = 1; break; }
        total.packets += next.packets;
        total.bytes += next.bytes;
        fold(&total.digest, next.digest);
    }
    uint64_t elapsed = bench ? now_ns() - start : 0;
    if (memory && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16) failed = 1;
    av_free(input);
    if (failed) return 70;
    printf("{\"schema\":1,\"operation\":\"" OPERATION "\",\"iterations\":%lu,\"packets\":%" PRIu64
           ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true}\n",
           iterations, total.packets, total.bytes, total.digest, elapsed);
    return ferror(stdout) ? 74 : 0;
}
