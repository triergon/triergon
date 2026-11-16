/* Public-API reference for the complete packet-framing operation. */
#define _POSIX_C_SOURCE 200809L
#include "reference_framing.h"
#include <fcntl.h>
#include <unistd.h>

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

static int read_exact(int fd, uint8_t *data, size_t length) {
    while (length) {
        ssize_t n = read(fd, data, length);
        if (n < 0 && errno == EINTR)
            continue;
        if (n <= 0)
            return -1;
        data += (size_t)n;
        length -= (size_t)n;
    }
    return 0;
}

static int read_case(Case *c, const char *path) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0)
        return 66;
    off_t length = lseek(fd, 0, SEEK_END);
    if (length < 0 || length > (off_t)CAP || lseek(fd, 0, SEEK_SET) < 0) {
        close(fd);
        return 65;
    }
    size_t n = (size_t)length;
    uint8_t small[SCRIPT_STACK_BYTES] = {0};
    uint8_t *data = n <= sizeof(small) ? small : calloc(n, 1);
    int read_failed = !data || read_exact(fd, data, n);
    close(fd);
    int result = read_failed ? 74 : case_load(c, data, n) ? 65 : 0;
    if (data != small)
        free(data);
    return result;
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
    Case c = {0};
    int load = read_case(&c, argv[2]);
    if (load) {
        case_free(&c);
        return load;
    }
    if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18) {
        case_free(&c);
        return 74;
    }
    uint64_t start = timing ? now_ns() : 0;
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
    if (memory && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16) {
        case_free(&c);
        return 74;
    }
    case_free(&c);
    if (failed)
        return 70;
    printf("{\"schema\":1,\"operation\":\"NalPacketWholeFoldV1\",\"iterations\":%llu,\"packets\":"
           "%" PRIu64 ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 ",\"digest_b\":%" PRIu64
           ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true}\n",
           iterations, total.packets, total.bytes, total.hash.a, total.hash.b, elapsed);
    return ferror(stdout) ? 74 : 0;
}
