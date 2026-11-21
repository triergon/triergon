/* The transport comparison linked with the selected demuxer and parser. */
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

int main(int argc, char **argv)
{
    if (argc < 3 || strncmp(argv[1], "ts-au-", 6))
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
    int status = read == (size_t)size ? ts_main(argc, argv, bytes, size) : 2;
    free(bytes);
    return status;
}
