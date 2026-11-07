/* Public-API reference for the complete packet-framing operation. */
#define _POSIX_C_SOURCE 200809L
#include "reference_framing.h"

static void quote(const char *text) {
    putchar('"');
    for (const unsigned char *p = (const unsigned char *)text; *p; ++p) {
        if (*p == '"' || *p == '\\') {
            putchar('\\');
            putchar(*p);
        } else if (*p < 32)
            printf("\\u%04x", (unsigned)*p);
        else
            putchar(*p);
    }
    putchar('"');
}

int main(int argc, char **argv) {
    if (avcodec_version() != LIBAVCODEC_VERSION_INT || avutil_version() != LIBAVUTIL_VERSION_INT)
        return 78;
    av_log_set_level(AV_LOG_QUIET);
    if ((argc == 2 || argc == 3) && !strcmp(argv[1], "identity")) {
        printf("{\"operation\":\"NalPacketWholeFoldV1\",\"adapter\":\"ffmpeg-packet-framing\","
               "\"version\":");
        quote(av_version_info());
        printf(",\"configuration\":");
        quote(avcodec_configuration());
        printf(",\"codec_version\":%u,\"util_version\":%u,\"header_codec_version\":%u,\"header_"
               "util_version\":%u}\n",
               avcodec_version(), avutil_version(), LIBAVCODEC_VERSION_INT, LIBAVUTIL_VERSION_INT);
        return ferror(stdout) ? 74 : 0;
    }
    if (argc < 3)
        return 64;
    int timing = !strcmp(argv[1], "nal-bench"), memory = !strcmp(argv[1], "nal-memory");
    if ((!timing && !memory && strcmp(argv[1], "nal-fold") && strcmp(argv[1], "nal-check")) ||
        argc != (timing ? 4 : 3))
        return 64;
    char *end = NULL;
    unsigned long long iterations = timing ? strtoull(argv[3], &end, 10) : 1;
    if (!iterations || iterations > 1000000 || (timing && (!end || *end)))
        return 64;
    FILE *file = fopen(argv[2], "rb");
    if (!file)
        return 66;
    if (fseek(file, 0, SEEK_END) || ftell(file) < 0 || ftell(file) > (long)CAP) {
        fclose(file);
        return 65;
    }
    size_t n = (size_t)ftell(file);
    rewind(file);
    uint8_t *data = malloc(n ? n : 1);
    if (!data || fread(data, 1, n, file) != n) {
        free(data);
        fclose(file);
        return 74;
    }
    fclose(file);
    Case c = {0};
    if (case_load(&c, data, n)) {
        case_free(&c);
        free(data);
        return 65;
    }
    if (memory) {
        puts("TRIERGON_BEGIN_V1");
        if (fflush(stdout))
            return 74;
    }
    uint64_t start = now_ns();
    Outcome total = {0};
    int failed = 0;
    for (unsigned long long i = 0; i < iterations; ++i) {
        Outcome next = {0};
        if (operation(&c, &next)) {
            failed = 1;
            break;
        }
        word(&total.hash, next.hash.a);
        word(&total.hash, next.hash.b);
        total.packets += next.packets;
        total.bytes += next.bytes;
    }
    uint64_t elapsed = timing ? now_ns() - start : 0;
    if (memory) {
        puts("TRIERGON_END_V1");
        if (fflush(stdout))
            return 74;
    }
    case_free(&c);
    free(data);
    if (failed)
        return 70;
    printf("{\"schema\":1,\"operation\":\"NalPacketWholeFoldV1\",\"iterations\":%llu,\"packets\":"
           "%" PRIu64 ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 ",\"digest_b\":%" PRIu64
           ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true}\n",
           iterations, total.packets, total.bytes, total.hash.a, total.hash.b, elapsed);
    return ferror(stdout) ? 74 : 0;
}
