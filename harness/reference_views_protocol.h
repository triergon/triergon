/* Prefix protocol for the framing and MOV views. Operations prepare their input
 * outside the observed window, and construct and tear down the reference inside it. */
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/adler32.h>
#include "reference_digest.h"

typedef struct {
    uint64_t packets, bytes, digest;
    int trace;
} ViewWork;

typedef struct ViewCase ViewCase;
struct ViewCase {
    const char *prefix, *name;
    const uint8_t *data;
    size_t size;
    int argc;
    char **argv;
    void *prepared;
    int (*run)(const ViewCase *, ViewWork *);
    void (*release)(void *);
};

typedef struct {
    const char *prefix;
    int (*prepare)(ViewCase *);
} ViewDispatch;

static void view_record(ViewWork *work, uint64_t kind, const uint64_t *words,
                        size_t count, const uint8_t *payload, size_t size)
{
    fold(&work->digest, kind);
    for (size_t i = 0; i < count; ++i)
        fold(&work->digest, words[i]);
    if (size)
        fold_bytes(&work->digest, payload, size);
    ++work->packets;
    work->bytes += size;
}

static int view_smoke(const ViewCase *input, ViewWork *work)
{
    const uint64_t checksum = av_adler32_update(1, input->data, input->size);
    const uint64_t words[] = {input->size, checksum};
    view_record(work, 3, words, 2, NULL, 0);
    if (work->trace)
        printf("3 %zu %" PRIu64 "\n", input->size, checksum);
    return 0;
}

static int view_smoke_prepare(ViewCase *input)
{
    if (input->argc)
        return 2;
    input->name = "views-smoke";
    input->run = view_smoke;
    return 0;
}

static uint64_t view_now(void)
{
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (uint64_t)now.tv_sec * UINT64_C(1000000000) + (uint64_t)now.tv_nsec;
}

static uint8_t *view_load(const char *path, size_t *size)
{
    FILE *file = fopen(path, "rb");
    if (!file)
        return NULL;
    long length = fseek(file, 0, SEEK_END) == 0 ? ftell(file) : -1;
    uint8_t *data = NULL;
    if (length >= 0 && length <= 256L * 1024 * 1024 && fseek(file, 0, SEEK_SET) == 0) {
        *size = (size_t)length;
        data = malloc(*size ? *size : 1);
        if (data && fread(data, 1, *size, file) != *size) {
            free(data);
            data = NULL;
        }
    }
    fclose(file);
    return data;
}

static int view_main(int argc, char **argv, const ViewDispatch *operations, size_t count)
{
    if (argc == 3 && !strcmp(argv[1], "identity") && !strcmp(argv[2], "-")) {
        printf("{\"schema\":1,\"avformat_version\":%u,\"avcodec_version\":%u,\"avutil_version\":%u}\n",
               avformat_version(), avcodec_version(), avutil_version());
        return 0;
    }
    const ViewDispatch *dispatch = NULL;
    const char *mode = NULL;
    for (size_t i = 0; argc >= 3 && i < count; ++i) {
        size_t length = strlen(operations[i].prefix);
        if (strncmp(argv[1], operations[i].prefix, length))
            continue;
        const char *suffix = argv[1] + length;
        if (!strcmp(suffix, "fold") || !strcmp(suffix, "trace") ||
            !strcmp(suffix, "bench") || !strcmp(suffix, "memory")) {
            dispatch = &operations[i];
            mode = suffix;
            break;
        }
    }
    if (!dispatch) {
        fputs("usage: PREFIX{fold|trace|memory} FILE [ARG...] | PREFIXbench FILE ITERATIONS [ARG...]\n", stderr);
        return 2;
    }
    int bench = !strcmp(mode, "bench"), memory = !strcmp(mode, "memory"), trace = !strcmp(mode, "trace");
    unsigned long iterations = 1;
    int first = 3;
    if (bench) {
        if (argc <= first)
            return 2;
        const char *number = argv[first++];
        if (!*number || strspn(number, "0123456789") != strlen(number))
            return 2;
        iterations = strtoul(number, NULL, 10);
        if (!iterations || iterations > 1000000)
            return 2;
    }
    ViewCase input = {.prefix = dispatch->prefix, .argc = argc - first, .argv = argv + first};
    uint8_t *data = view_load(argv[2], &input.size);
    if (!data) {
        fprintf(stderr, "cannot preload %s\n", argv[2]);
        return 2;
    }
    input.data = data;
    int result = dispatch->prepare(&input);
    ViewWork total = {0};
    if (!result && memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18)
        result = 2;
    const uint64_t start = bench && !result ? view_now() : 0;
    for (unsigned long i = 0; !result && i < iterations; ++i) {
        ViewWork once = {.trace = trace};
        result = input.run(&input, &once);
        total.packets += once.packets;
        total.bytes += once.bytes;
        fold(&total.digest, once.digest);
    }
    const uint64_t elapsed = bench && !result ? view_now() - start : 0;
    if (!result && memory && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16)
        result = 2;
    if (input.release)
        input.release(input.prepared);
    free(data);
    if (result)
        return result;
    printf("{\"schema\":1,\"operation\":\"%s\",\"iterations\":%lu,\"packets\":%" PRIu64
           ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true}\n",
           input.name, iterations, total.packets, total.bytes, total.digest, elapsed);
    return 0;
}
