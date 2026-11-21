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

int main(int argc, char **argv)
{
    /* Reuse the ordinary input and packet fold without exposing its CLI. */
    (void)ts_main;
    if (argc != 3 || (strcmp(argv[1], "ts-eac3-proof") && strcmp(argv[1], "ts-eac3-proof-trace")))
        return 2;
    FILE *file = fopen(argv[2], "rb");
    if (!file)
        return 2;
    long size = fseek(file, 0, SEEK_END) ? -1 : ftell(file);
    if (size < 1 || size > 64 * 1024 * 1024 || fseek(file, 0, SEEK_SET)) {
        fclose(file);
        return 2;
    }
    uint8_t *bytes = malloc((size_t)size);
    size_t read = bytes ? fread(bytes, 1, (size_t)size, file) : 0;
    fclose(file);
    int status = read != (size_t)size ? 2
        : ec_proof(bytes, (size_t)size, 7, !strcmp(argv[1], "ts-eac3-proof-trace"));
    free(bytes);
    return status;
}
