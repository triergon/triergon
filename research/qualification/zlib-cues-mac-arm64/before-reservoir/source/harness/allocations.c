/* External macOS memory observer; never linked into production Rust or timed.
 * Heap and RSS have separate runs and explicit operation boundaries. */
#include <malloc/malloc.h>
#include <mach/mach.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/uio.h>
#include <unistd.h>
typedef struct {uintptr_t key;size_t size;} Entry;
static Entry entries[65536];
static atomic_flag mutex=ATOMIC_FLAG_INIT;
static _Thread_local int nested;
static int enabled,mode,active,invalid,begins,ends;
static uint64_t live,peak,baseline,window_end_live,calls,window_calls;
static uint64_t rss_start,rss_peak,rss_end,hwm_start,hwm_end;
static const char *destination;
static void lock(void){while(atomic_flag_test_and_set_explicit(&mutex,memory_order_acquire)){} }
static void unlock(void){atomic_flag_clear_explicit(&mutex,memory_order_release);}
static uint64_t resident(void){
    mach_task_basic_info_data_t value;mach_msg_type_number_t count=MACH_TASK_BASIC_INFO_COUNT;
    if(task_info(mach_task_self(),MACH_TASK_BASIC_INFO,(task_info_t)&value,&count)!=KERN_SUCCESS){invalid=1;return 0;}
    return value.resident_size;
}
static uint64_t highwater(void){struct rusage value;if(getrusage(RUSAGE_SELF,&value)){invalid=1;return 0;}return(uint64_t)value.ru_maxrss;}
static void sample(void){if(active&&mode==2){uint64_t value=resident();if(value>rss_peak)rss_peak=value;}}
static size_t slot(uintptr_t key,int insert){
    size_t at=((key>>4)*UINT64_C(11400714819323198485))>>48,tomb=SIZE_MAX;
    for(size_t n=0;n<65536;n++,at=(at+1)&65535){
        if(entries[at].key==key)return at;
        if(entries[at].key==1&&tomb==SIZE_MAX)tomb=at;
        if(entries[at].key==0)return insert?(tomb==SIZE_MAX?at:tomb):SIZE_MAX;
    }
    if(insert&&tomb!=SIZE_MAX)return tomb;
    if(insert)invalid=1;return SIZE_MAX;
}
static void add(void *p){
    if(!p||mode!=1)return;size_t at=slot((uintptr_t)p,1);if(at==SIZE_MAX)return;
    if(entries[at].key==(uintptr_t)p){invalid=1;return;}
    size_t size=malloc_size(p);entries[at]=(Entry){(uintptr_t)p,size};live+=size;calls++;
    if(active){window_calls++;if(live>peak)peak=live;}
}
static void remove_pointer(void *p){
    if(!p||mode!=1)return;size_t at=slot((uintptr_t)p,0);if(at==SIZE_MAX)return;
    if(live<entries[at].size){invalid=1;return;}live-=entries[at].size;entries[at].key=1;
}
static void *observed_malloc(size_t n){
    if(nested||!enabled)return malloc(n);nested=1;void *p=malloc(n);lock();add(p);sample();unlock();nested=0;return p;
}
static void *observed_calloc(size_t n,size_t w){
    if(nested||!enabled)return calloc(n,w);nested=1;void *p=calloc(n,w);lock();add(p);sample();unlock();nested=0;return p;
}
static void observed_free(void *p){
    if(nested||!enabled){free(p);return;}nested=1;lock();sample();remove_pointer(p);unlock();free(p);nested=0;
}
static void *observed_realloc(void *p,size_t n){
    if(nested||!enabled)return realloc(p,n);nested=1;lock();sample();unlock();
    uintptr_t old=(uintptr_t)p;void *out=realloc(p,n);
    lock();if(out){remove_pointer((void *)old);add(out);}else if(n==0){invalid=1;}
    sample();unlock();nested=0;return out;
}
static int observed_posix_memalign(void **p,size_t a,size_t n){
    if(nested||!enabled)return posix_memalign(p,a,n);nested=1;int result=posix_memalign(p,a,n);
    lock();if(!result)add(*p);sample();unlock();nested=0;return result;
}
static void *observed_aligned_alloc(size_t a,size_t n){
    if(nested||!enabled)return aligned_alloc(a,n);nested=1;void *p=aligned_alloc(a,n);
    lock();add(p);sample();unlock();nested=0;return p;
}
static void control(const void *buffer,size_t size){
    if(!enabled||nested)return;
    int begin=size==20&&!memcmp(buffer,"SAFE_MEDIA_BEGIN_V1\n",20);
    int end=size==18&&!memcmp(buffer,"SAFE_MEDIA_END_V1\n",18);if(!begin&&!end)return;
    nested=1;lock();
    if(begin){
        if(active||begins++)invalid=1;
        for(int n=0;n<4;n++)(void)resident();
        baseline=live;peak=live;window_calls=0;hwm_start=highwater();rss_start=resident();rss_peak=rss_start;active=1;
    }else{
        if(!active||ends++)invalid=1;sample();rss_end=resident();hwm_end=highwater();window_end_live=live;active=0;
    }
    unlock();nested=0;
}
static ssize_t observed_write(int fd,const void *buffer,size_t n){if(fd==STDOUT_FILENO)control(buffer,n);return write(fd,buffer,n);}
static ssize_t observed_writev(int fd,const struct iovec *vectors,int n){if(fd==STDOUT_FILENO&&n==1)control(vectors[0].iov_base,vectors[0].iov_len);return writev(fd,vectors,n);}
#define INTERPOSE(replacement,original) \
    __attribute__((used)) static struct {const void *a;const void *b;} interpose_##original \
    __attribute__((section("__DATA,__interpose")))={(const void *)(uintptr_t)&replacement,(const void *)(uintptr_t)&original}
INTERPOSE(observed_malloc,malloc);
INTERPOSE(observed_calloc,calloc);
INTERPOSE(observed_free,free);
INTERPOSE(observed_realloc,realloc);
INTERPOSE(observed_posix_memalign,posix_memalign);
INTERPOSE(observed_aligned_alloc,aligned_alloc);
INTERPOSE(observed_write,write);
INTERPOSE(observed_writev,writev);
__attribute__((constructor)) static void initialize(void){
    destination=getenv("SAFE_MEDIA_ALLOCATION_REPORT");const char *which=getenv("SAFE_MEDIA_MEMORY_MODE");
    mode=which&&!strcmp(which,"rss")?2:1;enabled=destination!=NULL;
}
__attribute__((destructor)) static void report(void){
    enabled=0;if(!destination)return;if(begins!=1||ends!=1||active||peak<baseline)invalid=1;
    FILE *out=fopen(destination,"w");if(!out)return;
    fprintf(out,"{\"schema\":2,\"mode\":\"%s\",\"status\":\"%s\",\"heap_peak_bytes\":%llu,\"heap_live_start\":%llu,\"heap_live_end\":%llu,\"window_allocation_calls\":%llu,\"calls\":%llu,\"rss_start\":%llu,\"rss_peak_observed\":%llu,\"rss_end\":%llu,\"hwm_start\":%llu,\"hwm_end\":%llu,\"page_size\":%ld}\n",
        mode==1?"heap":"rss",invalid?"invalid":"measured",(unsigned long long)(peak>=baseline?peak-baseline:0),
        (unsigned long long)baseline,(unsigned long long)window_end_live,(unsigned long long)window_calls,(unsigned long long)calls,
        (unsigned long long)rss_start,(unsigned long long)rss_peak,(unsigned long long)rss_end,(unsigned long long)hwm_start,(unsigned long long)hwm_end,sysconf(_SC_PAGESIZE));fclose(out);
}
