/* Comparison-only ATSC E-AC-3 adapter, linked with the formats profile. */
#include <libavformat/avformat.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "reference_digest.h"
/* JSON strings and the monotonic clock shared by reference workers. */
static void quote(const char *s) {
    putchar('"');if(s)for(;*s;s++){unsigned char c=(unsigned char)*s;
        if(c=='"'||c=='\\'){putchar('\\');putchar(c);}else if(c<32)printf("\\u%04x",c);else putchar(c);
    }putchar('"');
}
static uint64_t now_ns(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return(uint64_t)t.tv_sec*1000000000+(uint64_t)t.tv_nsec;}
#include "reference_ts.h"
#include "reference_ts_eac3.h"

/* The proof commands retain their original trace protocol. The measured modes
 * retain parser coverage and source clock validation without proof-only passes. */
static int ec_main(int argc, char **argv, const uint8_t *bytes, size_t size)
{
    if (argc == 3 && (!strcmp(argv[1], "ts-eac3-proof")
                      || !strcmp(argv[1], "ts-eac3-proof-trace")))
        return ec_proof(bytes, size, 7, !strcmp(argv[1], "ts-eac3-proof-trace"));
    const char *prefix = "ts-eac3-six-block-";
    if (strncmp(argv[1], prefix, strlen(prefix))) return 2;
    const char *mode = argv[1] + strlen(prefix);
    int bench = !strcmp(mode, "bench"), memory = !strcmp(mode, "memory");
    if (!bench && !memory && strcmp(mode, "fold")) return 2;
    int at = bench ? 4 : 3;
    if (argc != at + 3 || strcmp(argv[at], "188") || strcmp(argv[at + 2], "--atsc")) return 2;
    char *end = NULL;
    unsigned long iterations = bench ? strtoul(argv[3], &end, 10) : 1;
    if (!iterations || iterations > 1000000 || (bench && (*end || end == argv[3]))) return 2;
    long program = strtol(argv[at + 1], &end, 10);
    if (*end || end == argv[at + 1] || program < 1 || program > 65535) return 2;
    av_log_set_level(AV_LOG_QUIET);
    if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18) return 2;
    uint64_t start = bench ? now_ns() : 0;
    TsWork total = {0};
    int eof = 1;
    for (unsigned long i = 0; i < iterations; ++i) {
        EcState state = {.correctness = !bench && !memory};
        if (!ec_operation(bytes, size, (int)program, &state)) { eof = 0; break; }
        total.packets += state.work.packets;
        total.bytes += state.work.bytes;
        fold(&total.digest, state.work.digest);
    }
    uint64_t elapsed = bench ? now_ns() - start : 0;
    if (memory && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16) return 2;
    if (!eof) return 1;
    printf("{\"schema\":1,\"operation\":\"mpegts-atsc-eac3-six-block\",\"iterations\":%lu,"
           "\"packets\":%"PRIu64",\"bytes\":%"PRIu64",\"digest\":%"PRIu64
           ",\"elapsed_ns\":%"PRIu64",\"eof\":true}\n",
           iterations, total.packets, total.bytes, total.digest, elapsed);
    return 0;
}

int main(int argc, char **argv)
{
    (void)ts_main;
    if (argc < 3) return 2;
    FILE *file = fopen(argv[2], "rb");
    if (!file) return 2;
    long size = fseek(file, 0, SEEK_END) ? -1 : ftell(file);
    if (size < 1 || size > 64 * 1024 * 1024 || fseek(file, 0, SEEK_SET)) {
        fclose(file);
        return 2;
    }
    uint8_t *bytes = malloc((size_t)size);
    size_t read = bytes ? fread(bytes, 1, (size_t)size, file) : 0;
    fclose(file);
    int status = read != (size_t)size ? 2 : ec_main(argc, argv, bytes, (size_t)size);
    free(bytes);
    return status;
}
