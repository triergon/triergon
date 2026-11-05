/*
 * Permissive live-heap counter for the hardware-acceleration evaluation
 * (Linux/glibc, LD_PRELOAD; research tooling only).
 *
 * NOT the qualification observer: harness/allocations-linux.c deliberately
 * invalidates any window with thread creation, loader calls or mappings, which
 * every GPU driver performs. This counter tolerates them: it tracks live bytes
 * of the malloc family (via malloc_usable_size) from all threads with atomics
 * and reports the peak growth between the stdout markers SAFE_MEDIA_BEGIN_V1 /
 * SAFE_MEDIA_END_V1. Driver memory obtained with mmap or device allocations is
 * not heap and is reported separately (RSS, device memory).
 *
 *   cc -O2 -fPIC -shared -o heapcount.so heapcount.c -ldl -pthread
 *   LD_PRELOAD=heapcount.so SAFE_MEDIA_ALLOCATION_REPORT=out.json <worker> memory ...
 */
#define _GNU_SOURCE 1
#include <dlfcn.h>
#include <malloc.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void *(*real_malloc)(size_t);
static void *(*real_calloc)(size_t, size_t);
static void *(*real_realloc)(void *, size_t);
static void (*real_free)(void *);
static int (*real_posix_memalign)(void **, size_t, size_t);
static void *(*real_aligned_alloc)(size_t, size_t);
static void *(*real_memalign)(size_t, size_t);
static ssize_t (*real_write)(int, const void *, size_t);

static _Atomic int64_t live, peak, baseline;
static _Atomic int active, begins, ends;
static _Atomic uint64_t calls;
static int64_t result_peak = -1;
static _Thread_local int resolving;
static _Alignas(16) char boot[65536];
static _Atomic size_t boot_used;

static void resolve(void) {
    if (real_malloc) return;
    resolving = 1;
    real_malloc = dlsym(RTLD_NEXT, "malloc");
    real_calloc = dlsym(RTLD_NEXT, "calloc");
    real_realloc = dlsym(RTLD_NEXT, "realloc");
    real_free = dlsym(RTLD_NEXT, "free");
    real_posix_memalign = dlsym(RTLD_NEXT, "posix_memalign");
    real_aligned_alloc = dlsym(RTLD_NEXT, "aligned_alloc");
    real_memalign = dlsym(RTLD_NEXT, "memalign");
    real_write = dlsym(RTLD_NEXT, "write");
    resolving = 0;
}

static int is_boot(const void *p) { return (const char *)p >= boot && (const char *)p < boot + sizeof boot; }

static void *boot_alloc(size_t n) {
    size_t at = atomic_fetch_add(&boot_used, (n + 15) & ~(size_t)15);
    return at + n <= sizeof boot ? boot + at : NULL;
}

static void account(int64_t delta) {
    int64_t now = atomic_fetch_add(&live, delta) + delta;
    atomic_fetch_add(&calls, 1);
    if (atomic_load(&active)) {
        int64_t p = atomic_load(&peak);
        while (now > p && !atomic_compare_exchange_weak(&peak, &p, now)) {
        }
    }
}

void *malloc(size_t n) {
    if (resolving) return boot_alloc(n);
    resolve();
    void *p = real_malloc(n);
    if (p) account((int64_t)malloc_usable_size(p));
    return p;
}

void *calloc(size_t a, size_t b) {
    if (resolving) {
        void *p = boot_alloc(a * b);
        if (p) memset(p, 0, a * b);
        return p;
    }
    resolve();
    void *p = real_calloc(a, b);
    if (p) account((int64_t)malloc_usable_size(p));
    return p;
}

void free(void *p) {
    if (!p || is_boot(p)) return;
    resolve();
    account(-(int64_t)malloc_usable_size(p));
    real_free(p);
}

void *realloc(void *old, size_t n) {
    resolve();
    if (old && is_boot(old)) {
        void *p = malloc(n);
        if (p) memcpy(p, old, n);
        return p;
    }
    int64_t before = old ? (int64_t)malloc_usable_size(old) : 0;
    void *p = real_realloc(old, n);
    if (p) account((int64_t)malloc_usable_size(p) - before);
    else if (!n && old) account(-before);
    return p;
}

int posix_memalign(void **out, size_t align, size_t n) {
    resolve();
    int r = real_posix_memalign(out, align, n);
    if (!r && *out) account((int64_t)malloc_usable_size(*out));
    return r;
}

void *aligned_alloc(size_t align, size_t n) {
    resolve();
    void *p = real_aligned_alloc(align, n);
    if (p) account((int64_t)malloc_usable_size(p));
    return p;
}

void *memalign(size_t align, size_t n) {
    resolve();
    void *p = real_memalign(align, n);
    if (p) account((int64_t)malloc_usable_size(p));
    return p;
}

ssize_t write(int fd, const void *buf, size_t n) {
    resolve();
    if (fd == 1 && n == 20 && !memcmp(buf, "SAFE_MEDIA_BEGIN_V1\n", 20)) {
        atomic_store(&baseline, atomic_load(&live));
        atomic_store(&peak, atomic_load(&live));
        atomic_store(&active, 1);
        atomic_fetch_add(&begins, 1);
    } else if (fd == 1 && n == 18 && !memcmp(buf, "SAFE_MEDIA_END_V1\n", 18)) {
        atomic_store(&active, 0);
        atomic_fetch_add(&ends, 1);
        result_peak = atomic_load(&peak) - atomic_load(&baseline);
    }
    return real_write(fd, buf, n);
}

__attribute__((destructor)) static void report(void) {
    const char *path = getenv("SAFE_MEDIA_ALLOCATION_REPORT");
    if (!path) return;
    FILE *f = fopen(path, "w");
    if (!f) return;
    int ok = atomic_load(&begins) == 1 && atomic_load(&ends) == 1 && result_peak >= 0;
    fprintf(f, "{\"observer\":\"capture-hw-heapcount-v1\",\"mode\":\"heap\",\"status\":\"%s\",\"heap_peak_bytes\":%lld,"
               "\"calls\":%llu,\"scope\":\"malloc family, all threads; mmap and device memory excluded\"}\n",
            ok ? "measured" : "invalid", (long long)result_peak, (unsigned long long)atomic_load(&calls));
    fclose(f);
}
