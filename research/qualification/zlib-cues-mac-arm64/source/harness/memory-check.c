/* Independent known-allocation probe, built without builtin allocation elision. */
#include <malloc/malloc.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
static size_t peak,live;
static void add(void *p){if(!p)exit(3);live+=malloc_size(p);if(live>peak)peak=live;memset(p,1,malloc_size(p));}
static void drop(void *p){live-=malloc_size(p);free(p);}
int main(void){
    if(write(1,"SAFE_MEDIA_BEGIN_V1\n",20)!=20)return 2;
    void *a=malloc(12345);add(a);void *b=calloc(31,64);add(b);
    size_t old=malloc_size(a);a=realloc(a,123456);live-=old;add(a);
    void *c=NULL;if(posix_memalign(&c,4096,65536))return 3;add(c);
    void *d=aligned_alloc(256,131072);add(d);
    drop(d);drop(c);drop(b);drop(a);
    if(write(1,"SAFE_MEDIA_END_V1\n",18)!=18)return 2;
    printf("{\"expected_peak_bytes\":%zu,\"expected_live_end\":%zu}\n",peak,live);
    return live?4:0;
}
