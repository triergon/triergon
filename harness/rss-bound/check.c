#define _GNU_SOURCE 1
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <unistd.h>
#include "rss_bound.h"
int main(int argc, char **argv) {
    (void)argv;
    if (prctl(PR_SET_THP_DISABLE, 1, 0, 0, 0)) {perror("prctl");return 2;}
    int fd = open("/dev/triergon_rss_bound", O_RDONLY|O_CLOEXEC);
    if (fd < 0) {perror("open");return 3;}
    size_t n=8u*1024u*1024u;
    volatile unsigned char *p=malloc(n);
    if(!p)return 4;
    for(size_t x=0;x<n;x+=4096)p[x]=1;
    if (argc == 2) {
        int control = open("/sys/kernel/debug/kprobes/enabled", O_WRONLY|O_CLOEXEC);
        if (control < 0 || write(control, "1\n", 2) != 2) {perror("global control");return 6;}
        close(control);
    }
    struct triergon_rss_bound value={0};
    if(ioctl(fd,TRIERGON_RSS_END,&value)){perror("ioctl");return 5;}
    printf("{\"abi\":%"PRIu64",\"initial\":%"PRIu64",\"positive_pages\":%"PRIu64",\"upper\":%"PRIu64",\"events\":%"PRIu64",\"missed\":%"PRIu64",\"errors\":%"PRIu64",\"page_size\":%"PRIu64"}\n",(uint64_t)value.abi,(uint64_t)value.initial_bytes,(uint64_t)value.positive_pages,(uint64_t)value.upper_bytes,(uint64_t)value.events,(uint64_t)value.missed,(uint64_t)value.errors,(uint64_t)value.page_size);
    free((void *)p);close(fd);
    return value.abi!=1||value.errors||value.missed||value.positive_pages<2048;
}
