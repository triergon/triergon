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
 * v4 adds file-mapping, which maps one page of this executable read-only in
 * the window, reads it and unmaps it, and must stay measured with the page in
 * file_mapped_bytes; and guard-file-writable, guard-anonymous-readonly and
 * guard-file-munmap (a mapping made before BEGIN), which must invalidate.
 * Every mode reports the operator new and delete forms it ran in cxx_forms.
 * Positive heap cases: default/writev/failures/zero/transient. Check exact
 * expected_peak_bytes, window_allocation_calls, nonzero hits for all eight
 * selected symbols in default, and live_end == live_start. On Linux every mode
 * also allocates through all eight C++ operator new forms and releases through
 * all twelve operator delete forms, held live together, so each one's bytes
 * must be in the exact peak: the calibration that admits them unhooked. Allocation sizes
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

#if defined(__linux__)
/* libstdc++'s replaceable allocation functions by their mangled names (Itanium
 * ABI: a reference is a pointer, std::align_val_t a size_t, and size_t is 'm'
 * on LP64 and 'j' on ILP32). The probe links libstdc++ for them. */
#if SIZE_MAX == UINT64_MAX
#define CXX_SIZE "m"
#else
#define CXX_SIZE "j"
#endif
#define FORM_NEW "_Znw" CXX_SIZE
#define FORM_NEW_ARRAY "_Zna" CXX_SIZE
#define FORM_NEW_NOTHROW FORM_NEW "RKSt9nothrow_t"
#define FORM_NEW_ARRAY_NOTHROW FORM_NEW_ARRAY "RKSt9nothrow_t"
#define FORM_NEW_ALIGNED FORM_NEW "St11align_val_t"
#define FORM_NEW_ARRAY_ALIGNED FORM_NEW_ARRAY "St11align_val_t"
#define FORM_NEW_ALIGNED_NOTHROW FORM_NEW_ALIGNED "RKSt9nothrow_t"
#define FORM_NEW_ARRAY_ALIGNED_NOTHROW FORM_NEW_ARRAY_ALIGNED "RKSt9nothrow_t"
#define FORM_DELETE "_ZdlPv"
#define FORM_DELETE_ARRAY "_ZdaPv"
#define FORM_DELETE_SIZED FORM_DELETE CXX_SIZE
#define FORM_DELETE_ARRAY_SIZED FORM_DELETE_ARRAY CXX_SIZE
#define FORM_DELETE_NOTHROW FORM_DELETE "RKSt9nothrow_t"
#define FORM_DELETE_ARRAY_NOTHROW FORM_DELETE_ARRAY "RKSt9nothrow_t"
#define FORM_DELETE_ALIGNED FORM_DELETE "St11align_val_t"
#define FORM_DELETE_ARRAY_ALIGNED FORM_DELETE_ARRAY "St11align_val_t"
#define FORM_DELETE_SIZED_ALIGNED FORM_DELETE_SIZED "St11align_val_t"
#define FORM_DELETE_ARRAY_SIZED_ALIGNED FORM_DELETE_ARRAY_SIZED "St11align_val_t"
#define FORM_DELETE_ALIGNED_NOTHROW FORM_DELETE_ALIGNED "RKSt9nothrow_t"
#define FORM_DELETE_ARRAY_ALIGNED_NOTHROW FORM_DELETE_ARRAY_ALIGNED "RKSt9nothrow_t"
typedef struct { char unused; } NoThrow;
extern const NoThrow cxx_nothrow __asm__("_ZSt7nothrow");
void *cxx_new(size_t) __asm__(FORM_NEW);
void *cxx_new_array(size_t) __asm__(FORM_NEW_ARRAY);
void *cxx_new_nothrow(size_t, const NoThrow *) __asm__(FORM_NEW_NOTHROW);
void *cxx_new_array_nothrow(size_t, const NoThrow *) __asm__(FORM_NEW_ARRAY_NOTHROW);
void *cxx_new_aligned(size_t, size_t) __asm__(FORM_NEW_ALIGNED);
void *cxx_new_array_aligned(size_t, size_t) __asm__(FORM_NEW_ARRAY_ALIGNED);
void *cxx_new_aligned_nothrow(size_t, size_t, const NoThrow *) __asm__(FORM_NEW_ALIGNED_NOTHROW);
void *cxx_new_array_aligned_nothrow(size_t, size_t, const NoThrow *)
    __asm__(FORM_NEW_ARRAY_ALIGNED_NOTHROW);
void cxx_delete(void *) __asm__(FORM_DELETE);
void cxx_delete_array(void *) __asm__(FORM_DELETE_ARRAY);
void cxx_delete_sized(void *, size_t) __asm__(FORM_DELETE_SIZED);
void cxx_delete_array_sized(void *, size_t) __asm__(FORM_DELETE_ARRAY_SIZED);
void cxx_delete_nothrow(void *, const NoThrow *) __asm__(FORM_DELETE_NOTHROW);
void cxx_delete_array_nothrow(void *, const NoThrow *) __asm__(FORM_DELETE_ARRAY_NOTHROW);
void cxx_delete_aligned(void *, size_t) __asm__(FORM_DELETE_ALIGNED);
void cxx_delete_array_aligned(void *, size_t) __asm__(FORM_DELETE_ARRAY_ALIGNED);
void cxx_delete_sized_aligned(void *, size_t, size_t) __asm__(FORM_DELETE_SIZED_ALIGNED);
void cxx_delete_array_sized_aligned(void *, size_t, size_t) __asm__(FORM_DELETE_ARRAY_SIZED_ALIGNED);
void cxx_delete_aligned_nothrow(void *, size_t, const NoThrow *) __asm__(FORM_DELETE_ALIGNED_NOTHROW);
void cxx_delete_array_aligned_nothrow(void *, size_t, const NoThrow *)
    __asm__(FORM_DELETE_ARRAY_ALIGNED_NOTHROW);
#endif

static uint64_t peak, live, allocation_calls;
static unsigned char *limited[65537];

enum {
    MODE_MMAP = 11, MODE_MMAP64, MODE_MUNMAP, MODE_MMAP_FAILURE,
    MODE_MMAP64_FAILURE, MODE_MUNMAP_FAILURE, MODE_PTHREAD_CREATE,
    MODE_DLOPEN, MODE_DLMOPEN, MODE_DLCLOSE, MODE_DLOPEN_FAILURE,
    MODE_DLMOPEN_FAILURE, MODE_SCOPE_STARTUP, MODE_FILE_MAPPING,
    MODE_FILE_WRITABLE, MODE_ANONYMOUS_READONLY, MODE_FILE_MUNMAP
};

/* Modes whose window makes a guarded call, which must invalidate the receipt. */
static int guarded(unsigned mode) {
    return (mode >= MODE_MMAP && mode < MODE_SCOPE_STARTUP) || mode > MODE_FILE_MAPPING;
}

typedef struct {
    void *mapping;
    void *handle;
    size_t page, file_mapped;
    int file, has_file;
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
    if (mode == MODE_FILE_MAPPING || mode == MODE_FILE_WRITABLE || mode == MODE_FILE_MUNMAP) {
        control->file = open("/proc/self/exe", O_RDONLY | O_CLOEXEC);
        if (control->file < 0) return 0;
        control->has_file = 1;
    }
    if (mode == MODE_FILE_MUNMAP) {
        control->mapping = mmap(NULL, control->page, PROT_READ, MAP_PRIVATE, control->file, 0);
        if (control->mapping == MAP_FAILED) { control->mapping = NULL; return 0; }
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
    if (mode == MODE_FILE_MAPPING) {
        /* Admitted: a read-only file page, read, then unmapped in the window. */
        void *out = mmap(NULL, control->page, PROT_READ, MAP_PRIVATE, control->file, 0);
        if (out == MAP_FAILED || ((volatile unsigned char *)out)[1] != 'E') return 0;
        control->file_mapped = control->page;
        return munmap(out, control->page) == 0;
    }
    if (mode == MODE_FILE_WRITABLE || mode == MODE_ANONYMOUS_READONLY) {
        int file = mode == MODE_FILE_WRITABLE;
        void *out = mmap(NULL, control->page, file ? PROT_READ | PROT_WRITE : PROT_READ,
                         file ? MAP_PRIVATE : MAP_PRIVATE | MAP_ANONYMOUS,
                         file ? control->file : -1, 0);
        expected_guard(control, 0, out != MAP_FAILED);
        if (out == MAP_FAILED) return 0;
        control->mapping = out;
        return 1;
    }
    if (mode == MODE_FILE_MUNMAP) {
        int result = munmap(control->mapping, control->page);
        expected_guard(control, 2, result == 0);
        if (result) return 0;
        control->mapping = NULL;
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
    if (control->has_file && close(control->file)) return 0;
    control->mapping = control->handle = NULL;
    control->has_file = 0;
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

static void forget(void *pointer) {
    size_t size = usable(pointer);
    if (size > live) exit(3);
    live -= size;
}

static void drop(void *pointer) {
    forget(pointer);
#if defined(__linux__)
    int saved = errno;
#endif
    free(pointer);
#if defined(__linux__)
    if (errno != saved) exit(5);
#endif
}

/* strdup and strndup, hooked allocations with their own counters, and on
 * Linux every operator new form. All are held live through the window's peak
 * and released before END, the C++ ones through every operator delete form:
 * an uncounted allocation lowers the observed peak and its release is an
 * unknown pointer, so each must be counted for the receipt to match. */
enum { BYTES = 4000, ALIGNMENT = 64 };
static char *copy, *prefix;
#if defined(__linux__)
static void *cxx[16];
/* The forms run, each once, in the order first run: the work report. */
static const char *ran[20];
static unsigned ran_count;

static void run(const char *form) {
    for (unsigned n = 0; n < ran_count; ++n) if (!strcmp(ran[n], form)) return;
    if (ran_count == 20) exit(5);
    ran[ran_count++] = form;
}
#endif

static void hold_other_forms(void) {
    copy = strdup("triergon");
    prefix = strndup("triergon", 3);
    if (!copy || !prefix || strcmp(copy, "triergon") || strcmp(prefix, "tri")) exit(5);
    add(copy, 9);
    add(prefix, 4);
#if defined(__linux__)
    for (int n = 0; n < 3; ++n) {
        cxx[n] = cxx_new(BYTES);
        cxx[4 + n] = cxx_new_array(BYTES);
        cxx[8 + n] = cxx_new_aligned(BYTES, ALIGNMENT);
        cxx[12 + n] = cxx_new_array_aligned(BYTES, ALIGNMENT);
    }
    run(FORM_NEW);
    run(FORM_NEW_ARRAY);
    run(FORM_NEW_ALIGNED);
    run(FORM_NEW_ARRAY_ALIGNED);
    cxx[3] = cxx_new_nothrow(BYTES, &cxx_nothrow);
    run(FORM_NEW_NOTHROW);
    cxx[7] = cxx_new_array_nothrow(BYTES, &cxx_nothrow);
    run(FORM_NEW_ARRAY_NOTHROW);
    cxx[11] = cxx_new_aligned_nothrow(BYTES, ALIGNMENT, &cxx_nothrow);
    run(FORM_NEW_ALIGNED_NOTHROW);
    cxx[15] = cxx_new_array_aligned_nothrow(BYTES, ALIGNMENT, &cxx_nothrow);
    run(FORM_NEW_ARRAY_ALIGNED_NOTHROW);
    for (int n = 0; n < 16; ++n) {
        if (n >= 8 && (uintptr_t)cxx[n] % ALIGNMENT) exit(5);
        add(cxx[n], BYTES);
    }
#endif
}

static void release_other_forms(void) {
#if defined(__linux__)
    for (int n = 0; n < 16; ++n) forget(cxx[n]);
    cxx_delete(cxx[0]);
    run(FORM_DELETE);
    cxx_delete_sized(cxx[1], BYTES);
    run(FORM_DELETE_SIZED);
    cxx_delete_nothrow(cxx[2], &cxx_nothrow);
    run(FORM_DELETE_NOTHROW);
    cxx_delete(cxx[3]);
    cxx_delete_array(cxx[4]);
    run(FORM_DELETE_ARRAY);
    cxx_delete_array_sized(cxx[5], BYTES);
    run(FORM_DELETE_ARRAY_SIZED);
    cxx_delete_array_nothrow(cxx[6], &cxx_nothrow);
    run(FORM_DELETE_ARRAY_NOTHROW);
    cxx_delete_array(cxx[7]);
    cxx_delete_aligned(cxx[8], ALIGNMENT);
    run(FORM_DELETE_ALIGNED);
    cxx_delete_sized_aligned(cxx[9], BYTES, ALIGNMENT);
    run(FORM_DELETE_SIZED_ALIGNED);
    cxx_delete_aligned_nothrow(cxx[10], ALIGNMENT, &cxx_nothrow);
    run(FORM_DELETE_ALIGNED_NOTHROW);
    cxx_delete_aligned(cxx[11], ALIGNMENT);
    cxx_delete_array_aligned(cxx[12], ALIGNMENT);
    run(FORM_DELETE_ARRAY_ALIGNED);
    cxx_delete_array_sized_aligned(cxx[13], BYTES, ALIGNMENT);
    run(FORM_DELETE_ARRAY_SIZED_ALIGNED);
    cxx_delete_array_aligned_nothrow(cxx[14], ALIGNMENT, &cxx_nothrow);
    run(FORM_DELETE_ARRAY_ALIGNED_NOTHROW);
    cxx_delete_array_aligned(cxx[15], ALIGNMENT);
#endif
    drop(prefix);
    drop(copy);
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
        "guard-dlmopen-failure", "scope-startup", "file-mapping", "guard-file-writable",
        "guard-anonymous-readonly", "guard-file-munmap"
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
    hold_other_forms();

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
    if (selected >= MODE_MMAP && selected != MODE_SCOPE_STARTUP &&
        !guard_operation(selected, &scope_control)) return 5;
#endif
    release_other_forms();
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
           "\"required_guard_window_failures\":[%u,%u,%u,%u,%u,%u,%u],"
           "\"expected_file_mapped_bytes\":%zu,\"cxx_forms\":[",
           peak - initial_live, at_end - initial_live, initial_live, at_end,
           allocation_calls, selected == 5 ? "false" : "true", temporary_size, which,
           guarded(selected) ? "true" : "false",
           scope_control.calls[0], scope_control.calls[1], scope_control.calls[2],
           scope_control.calls[3], scope_control.calls[4], scope_control.calls[5], scope_control.calls[6],
           scope_control.successes[0], scope_control.successes[1], scope_control.successes[2],
           scope_control.successes[3], scope_control.successes[4], scope_control.successes[5],
           scope_control.successes[6],
           scope_control.failures[0], scope_control.failures[1], scope_control.failures[2],
           scope_control.failures[3], scope_control.failures[4], scope_control.failures[5],
           scope_control.failures[6], scope_control.file_mapped);
#if defined(__linux__)
    for (unsigned n = 0; n < ran_count; ++n) printf("%s\"%s\"", n ? "," : "", ran[n]);
#endif
    printf("]}\n");
    return live ? 4 : 0;
}
