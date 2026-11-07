/* Original known-allocation/release calibration; native TOOLING only.
 * Build with -O0 -fno-builtin -Wall -Wextra -Werror. No native execution by
 * the author. Default mode preserves the original schema keys on Darwin/Linux.
 *
 * Parent calibration matrix (each invocation is a fresh process/receipt):
 *   default, writev, failures, zero, transient, retained, missing-begin,
 *   missing-end, duplicate-begin, end-before-begin, table-limit.
 * v3 adds guard-mmap, guard-mmap64, guard-munmap, their three -failure modes,
 * guard-pthread-create, guard-dlopen, guard-dlmopen, guard-dlclose,
 * guard-dlopen-failure, guard-dlmopen-failure, and scope-startup. The twelve
 * guard-* modes MUST invalidate both heap and RSS receipts, including failed
 * API attempts and a thread created AND joined before END. They validate API
 * success/error controls and independently specify required guard arrays for
 * the authored top-level requests. Additional internal interposable calls are
 * retained; require observed counts >= these minima, never erase extra calls.
 * Mapping/loader cleanup occurs after END except the explicit munmap/dlclose
 * cases, whose resources were prepared before BEGIN. scope-startup exercises
 * all seven guards before BEGIN, closes/joins everything, then runs the same
 * positive known-allocation window with zero window guard calls. Heap peak
 * assertions apply only to admitted positive modes, not rejected private-TLS
 * guard windows. All original eleven mode definitions are preserved.
 * Positive heap cases: default/writev/failures/zero/transient. Check exact
 * expected_peak_bytes, window_allocation_calls, nonzero hits for all six
 * selected symbols in default, and live_end == live_start. Allocation sizes
 * use the native diagnostic API, not allocator rounding guesses. This probe
 * computes the caller-visible simultaneous live peak independently of the
 * observer's table. It never writes diagnostic usable-size slack.
 * RSS: run default/writev/transient SEPARATELY, require resident growth for the
 * touched temporary allocation. Retain the observer's HWM provenance/status;
 * positive calibration alone cannot establish an undocumented API error bound.
 * retained is a release-negative control (valid window, nonzero retained heap);
 * missing/duplicate/reordered markers MUST produce a non-measured receipt;
 * table-limit is HEAP ONLY and MUST invalidate capacity, never silently drop.
 * On Darwin use default for existing admission; Linux-specific zero semantics
 * and fragmented writev are separate controls for the new Linux observer.
 * Preserve every negative receipt; do not submit it as an admission calibration.
 *
 * Public diagnostic/API references (consulted 2026-10-31):
 * https://man7.org/linux/man-pages/man3/malloc_usable_size.3.html
 * https://man7.org/linux/man-pages/man3/malloc.3.html
 * https://man7.org/linux/man-pages/man3/posix_memalign.3.html
 * malloc_usable_size is diagnostic: writes remain within REQUESTED sizes.
 */
#define _GNU_SOURCE 1
#if defined(__arm__)
/* 32-bit ARM: keep the 32-bit off_t default so guard-mmap calls mmap, not mmap64,
 * when a toolchain predefines 64-bit file offsets. */
#undef _TIME_BITS
#undef _FILE_OFFSET_BITS
#endif
#include <errno.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/uio.h>
#include <unistd.h>
#if defined(__linux__)
#include <dlfcn.h>
#include <link.h>
#include <pthread.h>
#include <fcntl.h>
#include <sched.h>
#include <time.h>
#include <sys/mman.h>
#endif

#if defined(__APPLE__)
#include <malloc/malloc.h>
static size_t usable(void *pointer) { return pointer ? malloc_size(pointer) : 0; }
#elif defined(__linux__)
#include <malloc.h>
static size_t usable(void *pointer) { return pointer ? malloc_usable_size(pointer) : 0; }
#else
#error "memory-check needs a reviewed native usable-size diagnostic API"
#endif

static uint64_t peak, live, allocation_calls;
static unsigned char *limited[65537];

enum {
    MODE_MMAP = 11, MODE_MMAP64, MODE_MUNMAP, MODE_MMAP_FAILURE,
    MODE_MMAP64_FAILURE, MODE_MUNMAP_FAILURE, MODE_PTHREAD_CREATE,
    MODE_DLOPEN, MODE_DLMOPEN, MODE_DLCLOSE, MODE_DLOPEN_FAILURE,
    MODE_DLMOPEN_FAILURE, MODE_SCOPE_STARTUP
};

typedef struct {
    void *mapping;
    void *handle;
    size_t page;
    unsigned calls[7], successes[7], failures[7];
} GuardControl;

#if defined(__linux__)
static void *empty_thread(void *argument) { return argument; }

static int guard_prepare(unsigned mode, GuardControl *control) {
    long page = sysconf(_SC_PAGESIZE);
    if (page <= 0) return 0;
    control->page = (size_t)page;
    if (mode == MODE_MUNMAP || mode == MODE_MUNMAP_FAILURE) {
        control->mapping = mmap(NULL, control->page, PROT_READ | PROT_WRITE,
                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (control->mapping == MAP_FAILED) { control->mapping = NULL; return 0; }
        *(volatile unsigned char *)control->mapping = 19;
    }
    if (mode == MODE_DLCLOSE) {
        control->handle = dlopen(NULL, RTLD_NOW | RTLD_LOCAL);
        if (!control->handle) return 0;
    }
    return 1;
}

static void expected_guard(GuardControl *control, unsigned which, int successful) {
    ++control->calls[which];
    if (successful) ++control->successes[which]; else ++control->failures[which];
}

static int guard_operation(unsigned mode, GuardControl *control) {
    if (mode == MODE_MMAP || mode == MODE_MMAP64 ||
        mode == MODE_MMAP_FAILURE || mode == MODE_MMAP64_FAILURE) {
        int wide = mode == MODE_MMAP64 || mode == MODE_MMAP64_FAILURE;
        int failing = mode == MODE_MMAP_FAILURE || mode == MODE_MMAP64_FAILURE;
        size_t length = failing ? 0 : control->page;
        errno = EDOM;
        void *out = wide ? mmap64(NULL, length, PROT_READ | PROT_WRITE,
                                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) :
                           mmap(NULL, length, PROT_READ | PROT_WRITE,
                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        int observed_errno = errno;
        expected_guard(control, wide ? 1 : 0, !failing);
        if (failing) return out == MAP_FAILED && observed_errno == EINVAL;
        if (out == MAP_FAILED) return 0;
        control->mapping = out;
        *(volatile unsigned char *)out = 23;
        ((volatile unsigned char *)out)[control->page - 1] = 29;
        return 1;
    }
    if (mode == MODE_MUNMAP || mode == MODE_MUNMAP_FAILURE) {
        int failing = mode == MODE_MUNMAP_FAILURE;
        void *address = failing ? (unsigned char *)control->mapping + 1 : control->mapping;
        errno = EDOM;
        int result = munmap(address, control->page), observed_errno = errno;
        expected_guard(control, 2, !failing);
        if (failing)
            return result == -1 && observed_errno == EINVAL &&
                   *(volatile unsigned char *)control->mapping == 19;
        if (result) return 0;
        control->mapping = NULL;
        return 1;
    }
    if (mode == MODE_PTHREAD_CREATE) {
        pthread_t thread;
        void *out = NULL;
        int result = pthread_create(&thread, NULL, empty_thread, control);
        expected_guard(control, 3, 1);
        if (result) return 0;
        return pthread_join(thread, &out) == 0 && out == control;
    }
    if (mode == MODE_DLOPEN || mode == MODE_DLMOPEN) {
        if (mode == MODE_DLOPEN) control->handle = dlopen(NULL, RTLD_NOW | RTLD_LOCAL);
        else control->handle = dlmopen(LM_ID_NEWLM, "libc.so.6", RTLD_NOW | RTLD_LOCAL);
        expected_guard(control, mode == MODE_DLOPEN ? 4 : 5, 1);
        return control->handle != NULL;
    }
    if (mode == MODE_DLCLOSE) {
        int result = dlclose(control->handle);
        expected_guard(control, 6, 1);
        if (result) return 0;
        control->handle = NULL;
        return 1;
    }
    if (mode == MODE_DLOPEN_FAILURE || mode == MODE_DLMOPEN_FAILURE) {
        (void)dlerror();
        void *out = mode == MODE_DLOPEN_FAILURE ?
            dlopen("/__triergon_missing_scope_control__", RTLD_NOW | RTLD_LOCAL) :
            dlmopen(LM_ID_NEWLM, NULL, RTLD_NOW | RTLD_LOCAL);
        const char *error = dlerror();
        expected_guard(control, mode == MODE_DLOPEN_FAILURE ? 4 : 5, 0);
        if (out) { control->handle = out; return 0; }
        return error != NULL;
    }
    return 1;
}

static int guard_cleanup(GuardControl *control) {
    if (control->mapping && munmap(control->mapping, control->page)) return 0;
    if (control->handle && dlclose(control->handle)) return 0;
    control->mapping = control->handle = NULL;
    return 1;
}

/* Joining a thread may precede the kernel task-count observation becoming
 * one. Establish the observer's existing single-thread precondition BEFORE
 * BEGIN; timeout remains a failed calibration, never an admitted sample. */
static int await_single_thread(void) {
    struct timespec start, now;
    if (clock_gettime(CLOCK_MONOTONIC, &start)) return 0;
    for (unsigned attempt = 0; attempt < 10000; ++attempt) {
        char status[8192];
        int descriptor = open("/proc/self/status", O_RDONLY | O_CLOEXEC);
        if (descriptor < 0) return 0;
        ssize_t length = read(descriptor, status, sizeof(status) - 1);
        int closed = close(descriptor);
        if (length <= 0 || length == (ssize_t)sizeof(status) - 1 || closed) return 0;
        status[length] = 0;
        char *field = strstr(status, "\nThreads:");
        if (!field) return 0;
        char *end;
        errno = 0;
        unsigned long threads = strtoul(field + 9, &end, 10);
        if (errno || end == field + 9 || (*end != '\n' && *end != ' ' && *end != '\t')) return 0;
        if (threads == 1) return 1;
        if (clock_gettime(CLOCK_MONOTONIC, &now) ||
            now.tv_sec - start.tv_sec > 1 ||
            (now.tv_sec - start.tv_sec == 1 && now.tv_nsec >= start.tv_nsec)) return 0;
        sched_yield();
    }
    return 0;
}

static int startup_controls(void) {
    static const unsigned modes[] = { MODE_MMAP, MODE_MMAP64, MODE_MUNMAP,
        MODE_PTHREAD_CREATE, MODE_DLOPEN, MODE_DLMOPEN, MODE_DLCLOSE };
    for (size_t n = 0; n < sizeof(modes) / sizeof(modes[0]); ++n) {
        GuardControl control = {0};
        if (!guard_prepare(modes[n], &control) || !guard_operation(modes[n], &control) ||
            !guard_cleanup(&control)) return 0;
    }
    return await_single_thread();
}
#endif

static void add(void *pointer, size_t requested) {
    if (!pointer) exit(3);
    size_t size = usable(pointer);
    if (size < requested || size > UINT64_MAX - live) exit(3);
    live += size;
    ++allocation_calls;
    if (live > peak) peak = live;
    /* Volatile byte stores ensure every requested byte is touched even if
     * the compiler can prove the buffer is subsequently released. */
    volatile unsigned char *bytes = pointer;
    for (size_t n = 0; n < requested; ++n) bytes[n] = 1;
}

static void drop(void *pointer) {
    size_t size = usable(pointer);
    if (size > live) exit(3);
    live -= size;
#if defined(__linux__)
    int saved = errno;
#endif
    free(pointer);
#if defined(__linux__)
    if (errno != saved) exit(5);
#endif
}

static int raw_write(const void *buffer, size_t count) {
    const unsigned char *bytes = buffer;
    while (count) {
        ssize_t written = write(STDOUT_FILENO, bytes, count);
        if (written < 0 && errno == EINTR) continue;
        if (written <= 0) return 0;
        bytes += (size_t)written;
        count -= (size_t)written;
    }
    return 1;
}

static int marker(int begin, int fragmented) {
    const char *text = begin ? "TRIERGON_BEGIN_V1\n" : "TRIERGON_END_V1\n";
    size_t length = begin ? 18 : 16;
    if (!fragmented) return raw_write(text, length);
    /* One marker split across syscalls AND multiple iovecs, including an empty
     * vector. It must count once, without treating an arbitrary write as work. */
    if (!raw_write(text, 3)) return 0;
    struct iovec parts[4] = {
        { (void *)(text + 3), 4 }, { (void *)(text + 7), 0 },
        { (void *)(text + 7), 5 }, { (void *)(text + 12), length - 12 }
    };
    size_t wanted = length - 3;
    ssize_t written;
    do { written = writev(STDOUT_FILENO, parts, 4); } while (written < 0 && errno == EINTR);
    /* Finish a legal short write without resending its successful prefix. */
    if (written < 0 || (size_t)written > wanted) return 0;
    return raw_write(text + 3 + (size_t)written, wanted - (size_t)written);
}

static int failure_controls(unsigned char *kept) {
    size_t before = usable(kept);
    volatile size_t huge_source = SIZE_MAX;
    size_t huge = huge_source;
    errno = 0;
    void *resized = realloc(kept, huge);
    if (resized || errno != ENOMEM || usable(kept) != before || kept[0] != 1) return 0;
    errno = 0;
    void *out = malloc(huge);
    if (out || errno != ENOMEM) return 0;
    errno = 0;
    out = calloc(huge, 2);
    if (out || errno != ENOMEM) return 0;
    void *sentinel = kept;
    errno = ERANGE;
    int result = posix_memalign(&sentinel, 3, 64);
    if (result != EINVAL || sentinel != kept || errno != ERANGE) return 0;
    errno = ERANGE;
    result = posix_memalign(&sentinel, 4096, huge);
    if (result != ENOMEM || sentinel != kept || errno != ERANGE) return 0;
    errno = 0;
    out = aligned_alloc(4096, huge - huge % 4096);
    if (out || errno != ENOMEM) return 0;
    errno = EDOM;
    free(NULL);
    return errno == EDOM;
}

int main(int argc, char **argv) {
    const char *which = argc == 1 ? "default" : argv[1];
    if (argc > 2) return 2;
    const char *modes[] = {
        "default", "writev", "failures", "zero", "transient", "retained",
        "missing-begin", "missing-end", "duplicate-begin", "end-before-begin", "table-limit",
        "guard-mmap", "guard-mmap64", "guard-munmap", "guard-mmap-failure",
        "guard-mmap64-failure", "guard-munmap-failure", "guard-pthread-create",
        "guard-dlopen", "guard-dlmopen", "guard-dlclose", "guard-dlopen-failure",
        "guard-dlmopen-failure", "scope-startup"
    };
    unsigned selected = (unsigned)(sizeof(modes) / sizeof(modes[0]));
    for (unsigned n = 0; n < sizeof(modes) / sizeof(modes[0]); ++n)
        if (!strcmp(which, modes[n])) selected = n;
    if (selected == sizeof(modes) / sizeof(modes[0])) return 2;
#if !defined(__linux__)
    if (selected == 1 || selected == 3 || selected >= MODE_MMAP) return 2;
#endif
    GuardControl scope_control = {0};
#if defined(__linux__)
    if (selected == MODE_SCOPE_STARTUP && !startup_controls()) return 5;
    if (selected >= MODE_MMAP && !guard_prepare(selected, &scope_control)) return 5;
#endif
    int fragmented = selected == 1;
    /* A deliberately held pre-window allocation proves return to an initial
     * control baseline rather than assuming a literal zero. */
    void *initial = malloc(8192);
    add(initial, 8192);
    uint64_t initial_live = live;
    peak = live;
    allocation_calls = 0;
    if (selected == 9 && !marker(0, 0)) return 2;
    if (selected != 6 && !marker(1, fragmented)) return 2;
    if (selected == 8 && !marker(1, 0)) return 2;

    void *a = malloc(12345);
    add(a, 12345);
    void *b = calloc(31, 64);
    if (!b) return 3;
    for (size_t n = 0; n < 31 * 64; ++n) if (((unsigned char *)b)[n]) return 3;
    add(b, 31 * 64);
    if (selected == 2 && !failure_controls(a)) return 5;
    size_t old = usable(a);
    void *resized = realloc(a, 123456);
    if (!resized) return 3;
    a = resized;
    live -= old;
    for (size_t n = 0; n < 12345; ++n) if (((unsigned char *)a)[n] != 1) return 5;
    add(a, 123456);
    void *c = NULL;
    if (posix_memalign(&c, 4096, 65536) || (uintptr_t)c % 4096) return 3;
    add(c, 65536);
    void *d = aligned_alloc(256, 131072);
    if ((uintptr_t)d % 256) return 3;
    add(d, 131072);

    size_t temporary_size = selected == 4 ? 32u * 1024u * 1024u : 4u * 1024u * 1024u;
    void *temporary = malloc(temporary_size);
    add(temporary, temporary_size);
    drop(temporary); /* Peak is gone before END; an end-only observer fails. */
#if defined(__linux__)
    if (selected == 3) {
        void *zero = malloc(0);
        add(zero, 0);
        drop(zero);
        zero = calloc(0, 19);
        add(zero, 0);
        drop(zero);
        zero = malloc(17);
        add(zero, 17);
        size_t zero_size = usable(zero);
        void *nothing = realloc(zero, 0);
        if (nothing) return 5; /* Explicit glibc semantics, not portable C. */
        live -= zero_size;
        /* Successful releasing realloc returns NULL: its attempt succeeds,
         * but it does not add a successful allocation to window_calls. */
    }
#endif
    if (selected == 10) {
        for (size_t n = 0; n < sizeof(limited) / sizeof(limited[0]); ++n) {
            limited[n] = malloc(1);
            add(limited[n], 1);
        }
        for (size_t n = 0; n < sizeof(limited) / sizeof(limited[0]); ++n) drop(limited[n]);
    }
#if defined(__linux__)
    if (selected >= MODE_MMAP && selected < MODE_SCOPE_STARTUP &&
        !guard_operation(selected, &scope_control)) return 5;
#endif
    drop(d);
    drop(c);
    drop(b);
    if (selected != 5) drop(a);
    uint64_t at_end = live;
    if (selected != 7 && !marker(0, fragmented)) return 2;
    /* Release pre-window storage and the deliberate retained-negative block
     * after END. Neither can change the operation's end observation. */
    if (selected == 5) drop(a);
    drop(initial);
#if defined(__linux__)
    if (!guard_cleanup(&scope_control)) return 5;
#endif
    printf("{\"expected_peak_bytes\":%" PRIu64 ",\"expected_live_end\":%" PRIu64 ","
           "\"expected_owned_live_start\":%" PRIu64 ",\"expected_owned_live_end\":%" PRIu64 ","
           "\"expected_window_allocation_calls\":%" PRIu64 ","
           "\"expected_release_equal\":%s,\"temporary_requested_bytes\":%zu,"
           "\"calibration\":\"%s\",\"expect_guard_invalid\":%s,"
           "\"guard_oracle_scope\":\"authored-top-level-request-minima\","
           "\"required_guard_window_calls\":[%u,%u,%u,%u,%u,%u,%u],"
           "\"required_guard_window_successes\":[%u,%u,%u,%u,%u,%u,%u],"
           "\"required_guard_window_failures\":[%u,%u,%u,%u,%u,%u,%u]}\n",
           peak - initial_live, at_end - initial_live, initial_live, at_end,
           allocation_calls, selected == 5 ? "false" : "true", temporary_size, which,
           selected >= MODE_MMAP && selected < MODE_SCOPE_STARTUP ? "true" : "false",
           scope_control.calls[0], scope_control.calls[1], scope_control.calls[2],
           scope_control.calls[3], scope_control.calls[4], scope_control.calls[5], scope_control.calls[6],
           scope_control.successes[0], scope_control.successes[1], scope_control.successes[2],
           scope_control.successes[3], scope_control.successes[4], scope_control.successes[5],
           scope_control.successes[6],
           scope_control.failures[0], scope_control.failures[1], scope_control.failures[2],
           scope_control.failures[3], scope_control.failures[4], scope_control.failures[5],
           scope_control.failures[6]);
    return live ? 4 : 0;
}
