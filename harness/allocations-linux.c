/*
 * Original Linux/glibc external memory observer, schema 2. Tooling only.
 * Never link this file into production Rust or load it in a timing run.
 *
 * Parent build (NOT executed by this lane):
 *   cc -std=c11 -O2 -Wall -Wextra -Werror -fPIC -shared -fno-builtin
 *      -Wl,-z,now harness/allocations-linux.c -ldl -pthread -o allocations.so
 *   cc -std=c11 -O0 -Wall -Wextra -Werror -fno-builtin
 *      harness/portable-memory-check.c -ldl -pthread -o memory-check
 *   cc -std=c11 -O0 -Wall -Wextra -Werror -fno-builtin
 *      -DTRIERGON_LINUX_OBSERVER_SELFTEST harness/allocations-linux.c -ldl
 *      -o allocations-linux-selftest
 * Run the selftest, then the probe separately in heap and rss mode, setting
 * LD_PRELOAD to the absolute .so path, TRIERGON_ALLOCATION_REPORT to a new
 * receipt path and TRIERGON_MEMORY_MODE to exactly heap or rss. Archive each
 * command, exit, stdout, stderr, receipt, compiler/libc/kernel/page-size identity,
 * source/binary hashes and the selected workers' ELF allocator-import audit.
 * Admission requires calibration AND release, not just compilation or status.
 *
 * RSS contract, deliberately conservative:
 * /proc/self/smaps_rollup Rss is a page-table snapshot. It is sampled at both
 * markers, before release/reallocation and after acquisition; NOT a continuous
 * peak. No polling interval is claimed to bound a missed transient. Linux
 * status VmHWM/statm are documented inaccurate, with no two-page error bound.
 * getrusage(RUSAGE_SELF).ru_maxrss is retained in hwm_* in BYTES (Linux API
 * unit KiB), NOT silently assumed an exact RSS upper bound.
 *
 * Owner/auditor-approved v2 bound (2026-10-31): for ordinary same-process
 * mappings, resident(t) <= mapped virtual size(t) <= lifetime VmPeak at END.
 * rss_peak_upper_bound_bytes is therefore VmPeak_end, with explicit basis
 * process-lifetime-VmPeak. It is a continuous upper BOUND, not an exact resident
 * peak. hwm_* remain the raw reported resident HWM; VmPeak NEVER enters them.
 * rss_exact_peak_status stays unresolved, and the RSS-counter error remains
 * unverified. No two-page precision is claimed for an asynchronous RSS counter.
 *
 * Parent evaluation MUST select the explicit Linux bound formula:
 * candidate upper = rss_peak_upper_bound_bytes - rss_start + 2 * page_size;
 * reference lower = rss_peak_observed - rss_start - 2 * page_size.
 * Both must be positive; upper paired95 ratio must be strictly below 1.0.
 * VmPeak is NEVER a denominator, never a VM ratio, and is never discounted by
 * subtracting its starting peak. A large pre-window virtual peak only makes
 * the gate harder. Exact RSS snapshots must all be <= their virtual ceiling,
 * and the END ceiling must cover the maximum observed snapshot. Failed scope,
 * provenance or ceiling checks invalidate the observation, never clamp it.
 * Heap calibration, RSS growth calibration and release are still required
 * before observer admission; status=measured alone does not qualify anything.
 * The initial unresolved v1 source is archived separately and unchanged.
 *
 * v3 closed-window guards: every observed mmap/mmap64/munmap, pthread_create,
 * dlopen/dlmopen/dlclose attempt during an active window sets a sticky scope
 * error BEFORE forwarding the original call. Failed attempts invalidate too.
 * No mapping bytes enter the heap ledger and nothing is counted twice. The
 * real API return, resulting errno and loader error state are preserved.
 * dlclose is guarded as well so a pre-window handle cannot be unloaded during
 * the operation. Thread/loader calls run without the observer's ledger lock;
 * holding it across a constructor or a new thread could deadlock its malloc.
 *
 * The guarded private-TLS-import decision belongs to the parent certificate:
 * these are public entry-point guards, NOT guessed GLIBC_PRIVATE ABI wrappers.
 * Only the pinned libc importer and demonstrated startup-only thread/loader
 * scope may cover its three private TLS imports. Require actual export/binding
 * checks, all guard negative controls, one thread, zero window guard attempts,
 * zero unknown WINDOW releases, and the unchanged loaded-library closure.
 * Direct worker/private-ABI imports, clone/thrd_create/custom loader/alternate
 * TLS or raw-syscall bypasses are outside this selected profile, not waived.
 * All lifetime guard attempts and unknown releases are retained separately.
 * Relative caller-sensitive dlopen search is not certified by this wrapper;
 * admitted selected windows perform NO loader call. Controls use NULL, a
 * pinned absolute path, or the reviewed libc soname in a rejected window.
 *
 * Selected scope: glibc dynamic ELF, one process/thread during the window,
 * six allocator entry points below plus malloc_usable_size for diagnostics.
 * No other preload/allocator, static binary, fork/exec, signal-handler allocation,
 * unreviewed dlopen, custom arenas or unobserved allocation imports are admitted.
 * Inspect imports of BOTH workers and their selected loaded dependency closure;
 * an import audit of only the executable is not universal heap coverage.
 * Unknown active-window frees/reallocs, nested allocation, table/bootstrap/
 * lock/parser limits, malformed markers and API failures invalidate the receipt.
 * A hard bootstrap/lock/active-recursion failure exits 126 rather than deadlocking or silently
 * dropping allocations. Bootstrap storage is bounded and never handed to libc.
 * No observer metadata is dynamically allocated; heap and RSS are separate runs.
 *
 * Primary API documentation consulted 2026-10-31 (no implementation copied):
 * https://sourceware.org/glibc/manual/latest/html_node/Replacing-malloc.html
 * https://man7.org/linux/man-pages/man3/dlsym.3.html
 * https://man7.org/linux/man-pages/man3/malloc_usable_size.3.html
 * https://man7.org/linux/man-pages/man3/malloc.3.html
 * https://man7.org/linux/man-pages/man3/posix_memalign.3.html
 * https://man7.org/linux/man-pages/man5/proc_pid_statm.5.html
 * https://man7.org/linux/man-pages/man5/proc_pid_status.5.html
 * https://man7.org/linux/man-pages/man2/getrusage.2.html
 * https://man7.org/linux/man-pages/man2/mmap.2.html
 * https://man7.org/linux/man-pages/man3/pthread_create.3.html
 * https://man7.org/linux/man-pages/man3/dlopen.3.html
 * https://sourceware.org/glibc/manual/latest/html_node/Memory_002dmapped-I_002fO.html
 * https://www.kernel.org/doc/html/latest/filesystems/proc.html
 * glibc requires initial-exec TLS in replacement allocators; usable-size is a
 * diagnostic and does NOT authorize a caller to write beyond its requested size.
 */
#define _GNU_SOURCE 1
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <gnu/libc-version.h>
#include <inttypes.h>
#include <limits.h>
#include <link.h>
#include <malloc.h>
#include <pthread.h>
#include <stddef.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include "rss-bound-api.h"
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <unistd.h>

#if !defined(__linux__) || !defined(__GLIBC__)
#error "allocations-linux.c requires Linux and glibc; Darwin observer is separate"
#endif
_Static_assert(ATOMIC_INT_LOCK_FREE == 2 && ATOMIC_LONG_LOCK_FREE == 2 &&
               ATOMIC_LLONG_LOCK_FREE == 2, "observer needs lock-free integer atomics");
_Static_assert(sizeof(void *) == 8 && sizeof(off_t) == 8 && sizeof(off64_t) == 8,
               "mapping bootstrap selects the 64-bit Linux syscall ABI");

#define ENTRY_COUNT 65536u
#define BOOT_BYTES 131072u
#define BOOT_ENTRIES 512u
#define SPIN_LIMIT 1000000u
#define PROC_BYTES 16384u
#define OUTPUT_LIMIT UINT64_C(16777216)
#define TOMBSTONE ((uintptr_t)1)

enum {
    ERR_RECURSION = 1u << 0, ERR_TABLE = 1u << 1,
    ERR_ACCOUNTING = 1u << 2, ERR_UNKNOWN = 1u << 3,
    ERR_PROC = 1u << 4, ERR_HWM = 1u << 5,
    ERR_MARKER = 1u << 6, ERR_THREAD = 1u << 7,
    ERR_BOOT = 1u << 8, ERR_CONFIG = 1u << 9,
    ERR_LIMIT = 1u << 10, ERR_FORK = 1u << 11,
    ERR_SCOPE = 1u << 12
};
enum { SYM_MALLOC, SYM_CALLOC, SYM_REALLOC, SYM_FREE,
       SYM_POSIX, SYM_ALIGNED, SYM_COUNT };
enum { GUARD_MMAP, GUARD_MMAP64, GUARD_MUNMAP, GUARD_PTHREAD_CREATE,
       GUARD_DLOPEN, GUARD_DLMOPEN, GUARD_DLCLOSE, GUARD_COUNT };
typedef struct { uintptr_t key; size_t size; } Entry;
typedef struct { uintptr_t key; size_t size; int released; } BootEntry;
typedef struct { uint64_t rss, vm_peak, threads; } Snapshot;

static void *(*real_malloc)(size_t);
static void *(*real_calloc)(size_t, size_t);
static void *(*real_realloc)(void *, size_t);
static void (*real_free)(void *);
static int (*real_posix_memalign)(void **, size_t, size_t);
static void *(*real_aligned_alloc)(size_t, size_t);
static size_t (*real_usable_size)(void *);
static void *(*real_mmap)(void *, size_t, int, int, int, off_t);
static void *(*real_mmap64)(void *, size_t, int, int, int, off64_t);
static int (*real_munmap)(void *, size_t);
static int (*real_pthread_create)(pthread_t *, const pthread_attr_t *,
                                  void *(*)(void *), void *);
static void *(*real_dlopen)(const char *, int);
static void *(*real_dlmopen)(Lmid_t, const char *, int);
static int (*real_dlclose)(void *);
static unsigned guard_resolved_mask;
static atomic_uint_fast64_t guard_calls[GUARD_COUNT];
static atomic_uint_fast64_t guard_window_calls[GUARD_COUNT];
static atomic_uint_fast64_t guard_window_successes[GUARD_COUNT];
static atomic_uint_fast64_t guard_window_failures[GUARD_COUNT];
static atomic_int resolution;
static atomic_uint errors;
static atomic_flag guard = ATOMIC_FLAG_INIT;
static atomic_flag boot_guard = ATOMIC_FLAG_INIT;
static _Thread_local unsigned depth __attribute__((tls_model("initial-exec")));
static _Thread_local unsigned resolving __attribute__((tls_model("initial-exec")));
static _Alignas(max_align_t) unsigned char boot_arena[BOOT_BYTES];
static BootEntry boot_entries[BOOT_ENTRIES];
static size_t boot_used, boot_count;
static Entry entries[ENTRY_COUNT];
static char proc_buffer[PROC_BYTES], report_buffer[8192], destination[4096];
static char marker_line[18];
static size_t marker_length;
static uint64_t stdout_bytes;
static int configured, mode = 1, begins, ends;
static atomic_int enabled, active;
static pid_t owner_pid, window_tid;
static long page_size;
static uint64_t live, peak, baseline, end_live, calls, window_calls;
static uint64_t attempts[SYM_COUNT], successes[SYM_COUNT], failures[SYM_COUNT];
static uint64_t unknown_releases, unknown_window_releases, rss_samples;
static atomic_uint_fast64_t recursive_calls;
static uint64_t rss_start, rss_peak, rss_end, hwm_start, hwm_end;
static uint64_t vm_peak_start, vm_peak_end, bootstrap_at_begin;

static void fail(unsigned reason) {
    atomic_fetch_or_explicit(&errors, reason, memory_order_relaxed);
}

static _Noreturn void fatal(void) {
    static const char message[] = "triergon Linux memory observer fatal limit\n";
    (void)syscall(SYS_write, STDERR_FILENO, message, sizeof(message) - 1);
    (void)syscall(SYS_exit_group, 126);
    _Exit(126);
}

static void acquire(atomic_flag *flag) {
    for (unsigned n = 0; n < SPIN_LIMIT; ++n) {
        if (!atomic_flag_test_and_set_explicit(flag, memory_order_acquire)) return;
        if ((n & 255u) == 255u) (void)syscall(SYS_sched_yield);
    }
    fail(ERR_LIMIT);
    fatal();
}

static void release(atomic_flag *flag) {
    atomic_flag_clear_explicit(flag, memory_order_release);
}

static size_t boot_index(const void *pointer) {
    uintptr_t key = (uintptr_t)pointer;
    for (size_t n = 0; n < boot_count; ++n)
        if (boot_entries[n].key == key) return n;
    return SIZE_MAX;
}

static int is_boot(const void *pointer) {
    uintptr_t key = (uintptr_t)pointer, first = (uintptr_t)boot_arena;
    return key >= first && key - first < BOOT_BYTES;
}

static void *boot_allocate(size_t size, size_t alignment) {
    if (!alignment || (alignment & (alignment - 1))) {
        errno = EINVAL;
        return NULL;
    }
    acquire(&boot_guard);
    uintptr_t first = (uintptr_t)boot_arena;
    size_t padding = (size_t)(-(first + boot_used) & (alignment - 1));
    size_t occupied = size ? size : 1;
    if (boot_count == BOOT_ENTRIES || padding > BOOT_BYTES - boot_used ||
        occupied > BOOT_BYTES - boot_used - padding) {
        release(&boot_guard);
        fail(ERR_BOOT);
        errno = ENOMEM;
        return NULL;
    }
    void *out = boot_arena + boot_used + padding;
    boot_used += padding + occupied;
    boot_entries[boot_count++] = (BootEntry){ (uintptr_t)out, size, 0 };
    release(&boot_guard);
    return out;
}

static void boot_drop(void *pointer) {
    if (!pointer) return;
    acquire(&boot_guard);
    size_t at = boot_index(pointer);
    if (at == SIZE_MAX || boot_entries[at].released) fail(ERR_BOOT);
    else boot_entries[at].released = 1;
    release(&boot_guard);
}

static void symbol(void *target, size_t target_size, const char *name) {
    (void)dlerror();
    void *value = dlsym(RTLD_NEXT, name);
    const char *error = dlerror();
    /* POSIX dlsym function addresses are usable as function pointers. memcpy
     * avoids C aliasing violations; reject any ABI with a different size. */
    if (error || !value || target_size != sizeof(value)) fatal();
    memcpy(target, &value, sizeof(value));
}

static void resolve(void) {
    if (atomic_load_explicit(&resolution, memory_order_acquire) == 2) return;
    int empty = 0;
    if (atomic_compare_exchange_strong_explicit(&resolution, &empty, 1,
                                               memory_order_acq_rel, memory_order_acquire)) {
        resolving = 1;
        symbol(&real_malloc, sizeof(real_malloc), "malloc");
        symbol(&real_calloc, sizeof(real_calloc), "calloc");
        symbol(&real_realloc, sizeof(real_realloc), "realloc");
        symbol(&real_free, sizeof(real_free), "free");
        symbol(&real_posix_memalign, sizeof(real_posix_memalign), "posix_memalign");
        symbol(&real_aligned_alloc, sizeof(real_aligned_alloc), "aligned_alloc");
        symbol(&real_usable_size, sizeof(real_usable_size), "malloc_usable_size");
        symbol(&real_mmap, sizeof(real_mmap), "mmap");
        symbol(&real_mmap64, sizeof(real_mmap64), "mmap64");
        symbol(&real_munmap, sizeof(real_munmap), "munmap");
        symbol(&real_pthread_create, sizeof(real_pthread_create), "pthread_create");
        symbol(&real_dlopen, sizeof(real_dlopen), "dlopen");
        symbol(&real_dlmopen, sizeof(real_dlmopen), "dlmopen");
        symbol(&real_dlclose, sizeof(real_dlclose), "dlclose");
        guard_resolved_mask = (1u << GUARD_COUNT) - 1u;
        resolving = 0;
        atomic_store_explicit(&resolution, 2, memory_order_release);
        return;
    }
    for (unsigned n = 0; n < SPIN_LIMIT; ++n) {
        if (atomic_load_explicit(&resolution, memory_order_acquire) == 2) return;
        if ((n & 255u) == 255u) (void)syscall(SYS_sched_yield);
    }
    fail(ERR_BOOT);
    fatal();
}

static void increment_guard(atomic_uint_fast64_t *counter) {
    if (atomic_fetch_add_explicit(counter, 1, memory_order_relaxed) == UINT64_MAX) {
        fail(ERR_ACCOUNTING);
        fatal();
    }
}

static uint64_t guard_value(const atomic_uint_fast64_t *counter) {
    return (uint64_t)atomic_load_explicit(counter, memory_order_relaxed);
}

static int guard_attempt(unsigned which) {
    increment_guard(&guard_calls[which]);
    if (!atomic_load_explicit(&enabled, memory_order_acquire) ||
        !atomic_load_explicit(&active, memory_order_acquire)) return 0;
    if (getpid() != owner_pid) { fail(ERR_FORK); fatal(); }
    increment_guard(&guard_window_calls[which]);
    fail(ERR_SCOPE);
    return 1;
}

static void guard_result(unsigned which, int in_window, int successful) {
    if (in_window)
        increment_guard(successful ? &guard_window_successes[which] :
                                     &guard_window_failures[which]);
}

void *mmap(void *address, size_t length, int protection, int flags, int fd, off_t offset) {
    int incoming = errno, in_window = guard_attempt(GUARD_MMAP);
    void *out;
    if (resolving) {
        errno = incoming;
        out = (void *)syscall(SYS_mmap, address, length, protection, flags, fd, offset);
    } else {
        resolve();
        errno = incoming;
        out = real_mmap(address, length, protection, flags, fd, offset);
    }
    int saved = errno;
    guard_result(GUARD_MMAP, in_window, out != MAP_FAILED);
    errno = saved;
    return out;
}

void *mmap64(void *address, size_t length, int protection, int flags, int fd, off64_t offset) {
    int incoming = errno, in_window = guard_attempt(GUARD_MMAP64);
    void *out;
    if (resolving) {
        errno = incoming;
        out = (void *)syscall(SYS_mmap, address, length, protection, flags, fd, offset);
    } else {
        resolve();
        errno = incoming;
        out = real_mmap64(address, length, protection, flags, fd, offset);
    }
    int saved = errno;
    guard_result(GUARD_MMAP64, in_window, out != MAP_FAILED);
    errno = saved;
    return out;
}

int munmap(void *address, size_t length) {
    int incoming = errno, in_window = guard_attempt(GUARD_MUNMAP);
    int result;
    if (resolving) {
        errno = incoming;
        result = (int)syscall(SYS_munmap, address, length);
    } else {
        resolve();
        errno = incoming;
        result = real_munmap(address, length);
    }
    int saved = errno;
    guard_result(GUARD_MUNMAP, in_window, result == 0);
    errno = saved;
    return result;
}

int pthread_create(pthread_t *thread, const pthread_attr_t *attributes,
                   void *(*start)(void *), void *argument) {
    int incoming = errno, in_window = guard_attempt(GUARD_PTHREAD_CREATE);
    if (resolving) { fail(ERR_BOOT); fatal(); }
    resolve();
    errno = incoming;
    int result = real_pthread_create(thread, attributes, start, argument);
    int saved = errno;
    guard_result(GUARD_PTHREAD_CREATE, in_window, result == 0);
    errno = saved;
    return result;
}

void *dlopen(const char *path, int flags) {
    int incoming = errno, in_window = guard_attempt(GUARD_DLOPEN);
    if (resolving) { fail(ERR_BOOT); fatal(); }
    resolve();
    errno = incoming;
    void *out = real_dlopen(path, flags);
    int saved = errno;
    guard_result(GUARD_DLOPEN, in_window, out != NULL);
    errno = saved;
    return out;
}

void *dlmopen(Lmid_t namespace_id, const char *path, int flags) {
    int incoming = errno, in_window = guard_attempt(GUARD_DLMOPEN);
    if (resolving) { fail(ERR_BOOT); fatal(); }
    resolve();
    errno = incoming;
    void *out = real_dlmopen(namespace_id, path, flags);
    int saved = errno;
    guard_result(GUARD_DLMOPEN, in_window, out != NULL);
    errno = saved;
    return out;
}

int dlclose(void *handle) {
    int incoming = errno, in_window = guard_attempt(GUARD_DLCLOSE);
    if (resolving) { fail(ERR_BOOT); fatal(); }
    resolve();
    errno = incoming;
    int result = real_dlclose(handle);
    int saved = errno;
    guard_result(GUARD_DLCLOSE, in_window, result == 0);
    errno = saved;
    return result;
}

/* Whole-file bounded read: missing, truncated, duplicate and overflow fields
 * are errors, never zero-valued observations. No stdio/heap inside sampling. */
static int read_proc(const char *path, size_t *length) {
    int fd = (int)syscall(SYS_openat, AT_FDCWD, path, O_RDONLY | O_CLOEXEC, 0);
    if (fd < 0) return 0;
    size_t used = 0;
    int okay = 1;
    unsigned reads = 0;
    while (used < sizeof(proc_buffer)) {
        if (++reads > 128) { okay = 0; break; }
        ssize_t got = (ssize_t)syscall(SYS_read, fd, proc_buffer + used,
                                     sizeof(proc_buffer) - used);
        if (got < 0 && errno == EINTR) continue;
        if (got < 0) { okay = 0; break; }
        if (!got) break;
        used += (size_t)got;
    }
    if (used == sizeof(proc_buffer)) okay = 0;
    if (syscall(SYS_close, fd)) okay = 0;
    *length = used;
    return okay;
}

static int field(const char *buffer, size_t length, const char *name,
                 int kib, uint64_t *value) {
    size_t name_size = strlen(name), at = 0;
    unsigned found = 0;
    while (at < length) {
        size_t end = at;
        while (end < length && buffer[end] != '\n') ++end;
        if (end == length) return 0;
        if (end - at >= name_size && !memcmp(buffer + at, name, name_size)) {
            size_t p = at + name_size;
            uint64_t number = 0;
            while (p < end && (buffer[p] == ' ' || buffer[p] == '\t')) ++p;
            size_t start = p;
            while (p < end && buffer[p] >= '0' && buffer[p] <= '9') {
                unsigned digit = (unsigned)(buffer[p++] - '0');
                if (number > (UINT64_MAX - digit) / 10) return 0;
                number = number * 10 + digit;
            }
            if (p == start) return 0;
            while (p < end && (buffer[p] == ' ' || buffer[p] == '\t')) ++p;
            if (kib) {
                if (end - p != 2 || buffer[p] != 'k' || buffer[p + 1] != 'B' ||
                    number > UINT64_MAX / 1024) return 0;
                number *= 1024;
            } else if (p != end) return 0;
            *value = number;
            ++found;
        }
        at = end + 1;
    }
    return found == 1;
}

static int snapshot(Snapshot *out) {
    size_t length;
    uint64_t shared_huge, private_huge, hugetlb;
    if (!read_proc("/proc/self/smaps_rollup", &length) ||
        !field(proc_buffer, length, "Rss:", 1, &out->rss) ||
        !field(proc_buffer, length, "Shared_Hugetlb:", 1, &shared_huge) ||
        !field(proc_buffer, length, "Private_Hugetlb:", 1, &private_huge) ||
        shared_huge || private_huge ||
        !read_proc("/proc/self/status", &length) ||
        !field(proc_buffer, length, "VmPeak:", 1, &out->vm_peak) ||
        !field(proc_buffer, length, "Threads:", 0, &out->threads) ||
        !field(proc_buffer, length, "HugetlbPages:", 1, &hugetlb) || hugetlb ||
        out->rss > out->vm_peak || !out->rss || !out->vm_peak) {
        fail(ERR_PROC);
        return 0;
    }
    if (out->threads != 1) fail(ERR_THREAD);
    return 1;
}

static uint64_t reported_highwater(void) {
    struct rusage usage;
    if (getrusage(RUSAGE_SELF, &usage) || usage.ru_maxrss < 0 ||
        (uint64_t)usage.ru_maxrss > UINT64_MAX / 1024) {
        fail(ERR_HWM);
        return 0;
    }
    return (uint64_t)usage.ru_maxrss * 1024;
}

static void sample(void) {
    if (!active || mode != 2) return;
    Snapshot value;
    if (snapshot(&value)) {
        if (value.rss > rss_peak) rss_peak = value.rss;
        ++rss_samples;
    }
}

static size_t locate(uintptr_t key, int inserting) {
    size_t at = (size_t)(((key >> 4) * UINT64_C(11400714819323198485)) >> 48);
    size_t tomb = SIZE_MAX;
    for (size_t n = 0; n < ENTRY_COUNT; ++n, at = (at + 1) & (ENTRY_COUNT - 1)) {
        if (entries[at].key == key) return at;
        if (entries[at].key == TOMBSTONE && tomb == SIZE_MAX) tomb = at;
        if (!entries[at].key) return inserting ? (tomb == SIZE_MAX ? at : tomb) : SIZE_MAX;
    }
    if (inserting && tomb != SIZE_MAX) return tomb;
    if (inserting) fail(ERR_TABLE);
    return SIZE_MAX;
}

static void add(void *pointer) {
    if (!pointer) return;
    ++calls;
    if (active) ++window_calls;
    if (mode != 1) return;
    size_t at = locate((uintptr_t)pointer, 1);
    if (at == SIZE_MAX) return;
    size_t size = real_usable_size(pointer);
    if (entries[at].key == (uintptr_t)pointer || size > UINT64_MAX - live) {
        fail(ERR_ACCOUNTING);
        return;
    }
    entries[at] = (Entry){ (uintptr_t)pointer, size };
    live += size;
    if (active && live > peak) peak = live;
}

static void drop(uintptr_t key) {
    if (!key || mode != 1) return;
    size_t at = locate(key, 0);
    if (at == SIZE_MAX) {
        ++unknown_releases;
        if (active) { ++unknown_window_releases; fail(ERR_UNKNOWN); }
        return;
    }
    if (entries[at].size > live) { fail(ERR_ACCOUNTING); return; }
    live -= entries[at].size;
    entries[at].key = TOMBSTONE;
}

/* The lock spans each real allocator call to prevent cross-thread pointer reuse
 * racing a ledger update. Nested allocator calls never re-enter this lock. */
static int enter(unsigned which) {
    ++depth;
    if (depth != 1) {
        if (enabled) {
            atomic_fetch_add_explicit(&recursive_calls, 1, memory_order_relaxed);
            fail(ERR_RECURSION);
            fatal();
        }
        return 0;
    }
    if (!enabled) return 0;
    if (getpid() != owner_pid) { fail(ERR_FORK); fatal(); }
    acquire(&guard);
    if (active && (pid_t)syscall(SYS_gettid) != window_tid) fail(ERR_THREAD);
    ++attempts[which];
    return 1;
}

static void leave(unsigned which, int observing, int okay) {
    if (observing) {
        if (okay) ++successes[which]; else ++failures[which];
        sample();
        release(&guard);
    }
    --depth;
}

void *malloc(size_t size) {
    if (resolving) return boot_allocate(size, _Alignof(max_align_t));
    resolve();
    int observing = enter(SYM_MALLOC);
    void *out = real_malloc(size);
    int saved = errno;
    if (observing) add(out);
    leave(SYM_MALLOC, observing, out != NULL);
    errno = saved;
    return out;
}

void *calloc(size_t count, size_t width) {
    if (resolving) {
        if (width && count > SIZE_MAX / width) { errno = ENOMEM; return NULL; }
        size_t size = count * width;
        void *out = boot_allocate(size, _Alignof(max_align_t));
        if (out) memset(out, 0, size);
        return out;
    }
    resolve();
    int observing = enter(SYM_CALLOC);
    void *out = real_calloc(count, width);
    int saved = errno;
    if (observing) add(out);
    leave(SYM_CALLOC, observing, out != NULL);
    errno = saved;
    return out;
}

void free(void *pointer) {
    int saved = errno;
    if (is_boot(pointer)) {
        if (enabled && active) fail(ERR_BOOT);
        boot_drop(pointer);
        errno = saved;
        return;
    }
    if (resolving) { if (pointer) { fail(ERR_BOOT); fatal(); } return; }
    resolve();
    int observing = enter(SYM_FREE);
    if (observing) { sample(); drop((uintptr_t)pointer); }
    real_free(pointer);
    leave(SYM_FREE, observing, 1);
    errno = saved;
}

void *realloc(void *pointer, size_t size) {
    if (is_boot(pointer)) {
        acquire(&boot_guard);
        size_t at = boot_index(pointer);
        if (at == SIZE_MAX || boot_entries[at].released) { fail(ERR_BOOT); fatal(); }
        size_t old = boot_entries[at].size;
        release(&boot_guard);
        if (enabled) fail(ERR_BOOT);
        if (!size) { boot_drop(pointer); return NULL; }
        void *out;
        if (resolving) out = boot_allocate(size, _Alignof(max_align_t));
        else { resolve(); out = malloc(size); }
        if (out) { memcpy(out, pointer, old < size ? old : size); boot_drop(pointer); }
        return out;
    }
    if (resolving) {
        if (pointer) { fail(ERR_BOOT); fatal(); }
        return boot_allocate(size, _Alignof(max_align_t));
    }
    resolve();
    int observing = enter(SYM_REALLOC);
    uintptr_t old = (uintptr_t)pointer;
    if (observing) sample();
    void *out = real_realloc(pointer, size);
    int saved = errno;
    if (observing && (out || (old && size == 0))) {
        /* This file explicitly selects glibc: realloc(p, 0) frees p even when
         * returning NULL. A failed nonzero resize MUST keep the old entry. */
        drop(old);
        add(out);
    }
    leave(SYM_REALLOC, observing, out != NULL || (old && size == 0));
    errno = saved;
    return out;
}

int posix_memalign(void **pointer, size_t alignment, size_t size) {
    if (resolving) {
        int saved = errno;
        if (alignment < sizeof(void *) || (alignment & (alignment - 1))) return EINVAL;
        void *out = boot_allocate(size, alignment);
        int result = out ? 0 : ENOMEM;
        if (out) *pointer = out;
        errno = saved;
        return result;
    }
    resolve();
    int saved = errno, observing = enter(SYM_POSIX);
    int result = real_posix_memalign(pointer, alignment, size);
    if (observing && result == 0) add(*pointer);
    leave(SYM_POSIX, observing, result == 0);
    errno = saved;
    return result;
}

void *aligned_alloc(size_t alignment, size_t size) {
    if (resolving) {
        if (!alignment || size % alignment) { errno = EINVAL; return NULL; }
        return boot_allocate(size, alignment);
    }
    resolve();
    int observing = enter(SYM_ALIGNED);
    void *out = real_aligned_alloc(alignment, size);
    int saved = errno;
    if (observing) add(out);
    leave(SYM_ALIGNED, observing, out != NULL);
    errno = saved;
    return out;
}

size_t malloc_usable_size(void *pointer) {
    if (!pointer) return 0;
    if (is_boot(pointer)) {
        if (enabled && active) fail(ERR_BOOT);
        acquire(&boot_guard);
        size_t at = boot_index(pointer);
        size_t size = at == SIZE_MAX || boot_entries[at].released ? 0 : boot_entries[at].size;
        if (at == SIZE_MAX || boot_entries[at].released) fail(ERR_BOOT);
        release(&boot_guard);
        return size;
    }
    if (resolving) { fail(ERR_BOOT); fatal(); }
    resolve();
    return real_usable_size(pointer);
}

static int kernel_rss_fd = -1, kernel_rss_requested;
static struct triergon_rss_bound kernel_rss;

static void boundary(int begin) {
    if (begin) {
        if (active || begins || ends) { fail(ERR_MARKER); ++begins; return; }
        ++begins;
        window_tid = (pid_t)syscall(SYS_gettid);
        Snapshot value;
        /* Warm the same bounded sampling path before fixing the baseline. */
        for (unsigned n = 0; n < 4; ++n) (void)snapshot(&value);
        baseline = live;
        peak = live;
        window_calls = 0;
        hwm_start = reported_highwater();
        if (snapshot(&value)) {
            rss_start = value.rss;
            rss_peak = value.rss;
            vm_peak_start = value.vm_peak;
        }
        bootstrap_at_begin = boot_used;
        active = 1;
    } else {
        ++ends;
        if (!active || ends != 1 || begins != 1) { fail(ERR_MARKER); return; }
        if ((pid_t)syscall(SYS_gettid) != window_tid) fail(ERR_THREAD);
        sample();
        Snapshot value;
        if (snapshot(&value)) {
            rss_end = value.rss;
            if (rss_end > rss_peak) rss_peak = rss_end;
            vm_peak_end = value.vm_peak;
        }
        hwm_end = reported_highwater();
        if (kernel_rss_requested) {
            if (kernel_rss_fd < 0 || syscall(SYS_ioctl, kernel_rss_fd, TRIERGON_RSS_END, &kernel_rss) ||
                kernel_rss.abi != 1 || kernel_rss.errors || kernel_rss.missed ||
                kernel_rss.page_size != (uint64_t)page_size || kernel_rss.upper_bytes < rss_peak)
                fail(ERR_CONFIG);
        }
        end_live = live;
        active = 0;
        if (boot_used != bootstrap_at_begin) fail(ERR_BOOT);
    }
}

static void stream(const char *bytes, size_t count) {
    if (!enabled || !count) return;
    if (depth) {
        atomic_fetch_add_explicit(&recursive_calls, 1, memory_order_relaxed);
        fail(ERR_RECURSION);
        return;
    }
    ++depth;
    acquire(&guard);
    if (getpid() != owner_pid) { fail(ERR_FORK); fatal(); }
    if (stdout_bytes > OUTPUT_LIMIT || count > OUTPUT_LIMIT - stdout_bytes) {
        fail(ERR_LIMIT);
        release(&guard);
        --depth;
        return;
    }
    stdout_bytes += count;
    for (size_t n = 0; n < count; ++n) {
        char c = bytes[n];
        if (marker_length < sizeof(marker_line)) marker_line[marker_length] = c;
        if (marker_length <= sizeof(marker_line)) ++marker_length;
        if (c == '\n') {
            if (marker_length == 18 && !memcmp(marker_line, "TRIERGON_BEGIN_V1\n", 18)) boundary(1);
            if (marker_length == 16 && !memcmp(marker_line, "TRIERGON_END_V1\n", 16)) boundary(0);
            marker_length = 0;
        }
    }
    release(&guard);
    --depth;
}

ssize_t write(int fd, const void *buffer, size_t count) {
    ssize_t result = (ssize_t)syscall(SYS_write, fd, buffer, count);
    int saved = errno;
    if (fd == STDOUT_FILENO && result > 0) stream(buffer, (size_t)result);
    errno = saved;
    return result;
}

ssize_t writev(int fd, const struct iovec *vectors, int count) {
    ssize_t result = (ssize_t)syscall(SYS_writev, fd, vectors, count);
    int saved = errno;
    if (fd == STDOUT_FILENO && result > 0) {
        size_t remaining = (size_t)result;
        if (count < 0 || count > 1024) fail(ERR_LIMIT);
        else for (int n = 0; n < count && remaining; ++n) {
            size_t take = vectors[n].iov_len < remaining ? vectors[n].iov_len : remaining;
            stream(vectors[n].iov_base, take);
            remaining -= take;
        }
        if (remaining) fail(ERR_MARKER);
    }
    errno = saved;
    return result;
}

__attribute__((constructor)) static void initialize(void) {
    resolve();
    const char *path = getenv("TRIERGON_ALLOCATION_REPORT");
    const char *which = getenv("TRIERGON_MEMORY_MODE");
    if (!path) return;
    configured = 1;
    size_t length = strnlen(path, sizeof(destination));
    if (!length || length == sizeof(destination)) fatal();
    memcpy(destination, path, length + 1);
    if (which && !strcmp(which, "rss")) mode = 2;
    else if (!which || strcmp(which, "heap")) fail(ERR_CONFIG);
    owner_pid = getpid();
    page_size = sysconf(_SC_PAGESIZE);
    if (page_size <= 0 || ((uint64_t)page_size & ((uint64_t)page_size - 1))) fail(ERR_CONFIG);
    /* Fault observer-only storage before the operation. RSS mode never walks
     * or modifies the heap table, so ledger pages cannot inflate its peak. */
    memset(proc_buffer, 0, sizeof(proc_buffer));
    memset(report_buffer, 0, sizeof(report_buffer));
    if (mode == 1) memset(entries, 0, sizeof(entries));
    const char *kernel = getenv("TRIERGON_RSS_KERNEL_BOUND");
    if (mode == 2 && kernel && !strcmp(kernel, "1")) {
        kernel_rss_requested = 1;
        if (syscall(SYS_prctl, PR_SET_THP_DISABLE, 1, 0, 0, 0)) fail(ERR_CONFIG);
        else {
            kernel_rss_fd = (int)syscall(SYS_openat, AT_FDCWD, "/dev/triergon_rss_bound", O_RDONLY | O_CLOEXEC, 0);
            if (kernel_rss_fd < 0) fail(ERR_CONFIG);
        }
    }
    enabled = 1;
}

static int write_all(int fd, const char *bytes, size_t length) {
    size_t sent = 0;
    unsigned writes = 0;
    while (sent < length) {
        if (++writes > sizeof(report_buffer) + 64u) return 0;
        ssize_t count = (ssize_t)syscall(SYS_write, fd, bytes + sent, length - sent);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) return 0;
        sent += (size_t)count;
    }
    return 1;
}

__attribute__((destructor)) static void report(void) {
    if (!configured) return;
    enabled = 0;
    if (getpid() != owner_pid) { fail(ERR_FORK); fatal(); }
    if (begins != 1 || ends != 1 || active || peak < baseline ||
        !rss_start || !rss_end || hwm_end < hwm_start ||
        vm_peak_end < vm_peak_start || vm_peak_end < rss_peak) fail(ERR_MARKER);
    unsigned problems = atomic_load_explicit(&errors, memory_order_relaxed);
    uint64_t rss_upper = vm_peak_end;
    if (kernel_rss_requested && kernel_rss.upper_bytes < rss_upper) rss_upper = kernel_rss.upper_bytes;
    int length = snprintf(report_buffer, sizeof(report_buffer),
        "{\"schema\":2,\"observer\":\"linux-glibc-v3\",\"mode\":\"%s\",\"status\":\"%s\","
        "\"heap_peak_bytes\":%" PRIu64 ",\"heap_live_start\":%" PRIu64 ",\"heap_live_end\":%" PRIu64 ","
        "\"window_allocation_calls\":%" PRIu64 ",\"calls\":%" PRIu64 ","
        "\"rss_start\":%" PRIu64 ",\"rss_peak_observed\":%" PRIu64 ",\"rss_end\":%" PRIu64 ","
        "\"hwm_start\":%" PRIu64 ",\"hwm_end\":%" PRIu64 ",\"page_size\":%ld,"
        "\"rss_source\":\"proc-self-smaps_rollup-Rss\","
        "\"hwm_source\":\"getrusage-RUSAGE_SELF-ru_maxrss-KiB-times-1024\","
        "\"hwm_scope\":\"process-lifetime-reported\",\"rss_exact_peak_status\":\"unresolved\","
        "\"hwm_upper_bound_status\":\"unproven\","
        "\"virtual_peak_source\":\"proc-self-status-VmPeak\","
        "\"virtual_peak_start\":%" PRIu64 ",\"virtual_peak_end\":%" PRIu64 ","
        "\"rss_peak_upper_bound_bytes\":%" PRIu64 ","
        "\"rss_peak_upper_bound_basis\":\"%s\","
        "\"rss_bound_scope\":\"same-process-ordinary-mappings\","
        "\"rss_bound_formula\":\"candidate-ceiling-minus-start-plus-two-pages-v1\","
        "\"kernel_rss_requested\":%d,\"kernel_rss_abi\":%" PRIu64 ","
        "\"kernel_rss_initial_bytes\":%" PRIu64 ",\"kernel_rss_positive_pages\":%" PRIu64 ","
        "\"kernel_rss_upper_bytes\":%" PRIu64 ",\"kernel_rss_events\":%" PRIu64 ","
        "\"kernel_rss_missed\":%" PRIu64 ",\"kernel_rss_errors\":%" PRIu64 ","
        "\"kernel_rss_page_size\":%" PRIu64 ","
        "\"rss_samples\":%" PRIu64 ",\"error_bits\":%u,\"begins\":%d,\"ends\":%d,"
        "\"unknown_releases\":%" PRIu64 ",\"recursive_calls\":%" PRIu64 ","
        "\"unknown_window_releases\":%" PRIu64 ","
        "\"guard_policy\":\"invalidate-any-active-attempt-v1\","
        "\"guard_scope\":\"observed-interposable-entrypoints\","
        "\"guard_resolved_mask\":%u,"
        "\"private_tls_basis\":\"no-active-thread-or-loader-entrypoints\","
        "\"private_tls_direct_hooks\":false,"
        "\"guard_order\":[\"mmap\",\"mmap64\",\"munmap\",\"pthread_create\",\"dlopen\",\"dlmopen\",\"dlclose\"],"
        "\"guard_calls\":[%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 "],"
        "\"guard_window_calls\":[%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 "],"
        "\"guard_window_successes\":[%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 "],"
        "\"guard_window_failures\":[%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 "],"
        "\"bootstrap_bytes\":%zu,\"bootstrap_allocations\":%zu,"
        "\"entry_capacity\":%u,\"selected_thread_scope\":1,\"glibc\":\"%s\","
        "\"symbol_order\":[\"malloc\",\"calloc\",\"realloc\",\"free\",\"posix_memalign\",\"aligned_alloc\"],"
        "\"symbol_attempts\":[%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 "],"
        "\"symbol_successes\":[%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 "],"
        "\"symbol_failures\":[%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 "]}\n",
        mode == 1 ? "heap" : "rss", problems ? "invalid" : "measured",
        peak >= baseline ? peak - baseline : 0, baseline, end_live, window_calls, calls,
        rss_start, rss_peak, rss_end, hwm_start, hwm_end, page_size,
        vm_peak_start, vm_peak_end, rss_upper,
        kernel_rss_requested ? "min-kernel-positive-deltas-lifetime-VmPeak" : "process-lifetime-VmPeak",
        kernel_rss_requested, (uint64_t)kernel_rss.abi, (uint64_t)kernel_rss.initial_bytes,
        (uint64_t)kernel_rss.positive_pages, (uint64_t)kernel_rss.upper_bytes, (uint64_t)kernel_rss.events,
        (uint64_t)kernel_rss.missed, (uint64_t)kernel_rss.errors, (uint64_t)kernel_rss.page_size,
        rss_samples, problems, begins, ends,
        unknown_releases, (uint64_t)atomic_load_explicit(&recursive_calls, memory_order_relaxed),
        unknown_window_releases, guard_resolved_mask,
        guard_value(&guard_calls[0]), guard_value(&guard_calls[1]), guard_value(&guard_calls[2]),
        guard_value(&guard_calls[3]), guard_value(&guard_calls[4]), guard_value(&guard_calls[5]),
        guard_value(&guard_calls[6]),
        guard_value(&guard_window_calls[0]), guard_value(&guard_window_calls[1]),
        guard_value(&guard_window_calls[2]), guard_value(&guard_window_calls[3]),
        guard_value(&guard_window_calls[4]), guard_value(&guard_window_calls[5]),
        guard_value(&guard_window_calls[6]),
        guard_value(&guard_window_successes[0]), guard_value(&guard_window_successes[1]),
        guard_value(&guard_window_successes[2]), guard_value(&guard_window_successes[3]),
        guard_value(&guard_window_successes[4]), guard_value(&guard_window_successes[5]),
        guard_value(&guard_window_successes[6]),
        guard_value(&guard_window_failures[0]), guard_value(&guard_window_failures[1]),
        guard_value(&guard_window_failures[2]), guard_value(&guard_window_failures[3]),
        guard_value(&guard_window_failures[4]), guard_value(&guard_window_failures[5]),
        guard_value(&guard_window_failures[6]),
        boot_used, boot_count, ENTRY_COUNT, gnu_get_libc_version(),
        attempts[0], attempts[1], attempts[2], attempts[3], attempts[4], attempts[5],
        successes[0], successes[1], successes[2], successes[3], successes[4], successes[5],
        failures[0], failures[1], failures[2], failures[3], failures[4], failures[5]);
    if (length < 0 || (size_t)length >= sizeof(report_buffer)) fatal();
    int fd = (int)syscall(SYS_openat, AT_FDCWD, destination,
                         O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (fd < 0) fatal();
    int okay = write_all(fd, report_buffer, (size_t)length);
    if (syscall(SYS_close, fd)) okay = 0;
    if (!okay) fatal();
    if (kernel_rss_fd >= 0) (void)syscall(SYS_close, kernel_rss_fd);
}

#ifdef TRIERGON_LINUX_OBSERVER_SELFTEST
/* Parent-only native parser/ledger regressions, not executed by the author.
 * No proc fixture values are used as measurements or admission certificates. */
int main(void) {
    uint64_t value = 0;
    static const char valid[] = "Rss:\t 42 kB\nThreads: 1\n";
    static const char duplicate[] = "Rss: 1 kB\nRss: 2 kB\n";
    static const char overflow[] = "Rss: 18446744073709551615 kB\n";
    static const char malformed[] = "Rss: -1 kB\n";
    static const char truncated[] = "Rss: 1 kB";
    static const char trailing[] = "Rss: 1 kB junk\n";
    int okay = field(valid, sizeof(valid) - 1, "Rss:", 1, &value) && value == 43008;
    okay &= field(valid, sizeof(valid) - 1, "Threads:", 0, &value) && value == 1;
    okay &= !field(valid, sizeof(valid) - 1, "Missing:", 1, &value);
    okay &= !field(duplicate, sizeof(duplicate) - 1, "Rss:", 1, &value);
    okay &= !field(overflow, sizeof(overflow) - 1, "Rss:", 1, &value);
    okay &= !field(malformed, sizeof(malformed) - 1, "Rss:", 1, &value);
    okay &= !field(truncated, sizeof(truncated) - 1, "Rss:", 1, &value);
    okay &= !field(trailing, sizeof(trailing) - 1, "Rss:", 1, &value);
    configured = enabled = 0;
    memset(entries, 0, sizeof(entries));
    size_t first = locate(16, 1);
    entries[first] = (Entry){16, 7};
    okay &= locate(16, 0) == first;
    entries[first].key = TOMBSTONE;
    okay &= locate(16, 0) == SIZE_MAX && locate(16, 1) == first;
    unsigned saved_errors = atomic_load_explicit(&errors, memory_order_relaxed);
    for (size_t n = 0; n < ENTRY_COUNT; ++n)
        entries[n] = (Entry){ (uintptr_t)(n + 1) * 16u, 0 };
    okay &= locate((uintptr_t)(ENTRY_COUNT + 1u) * 16u, 1) == SIZE_MAX &&
            (atomic_load_explicit(&errors, memory_order_relaxed) & ERR_TABLE);
    entries[42].key = TOMBSTONE;
    okay &= locate((uintptr_t)(ENTRY_COUNT + 1u) * 16u, 1) == 42;
    atomic_store_explicit(&errors, saved_errors, memory_order_relaxed);
    void *aligned = boot_allocate(17, 4096);
    okay &= aligned != NULL && (uintptr_t)aligned % 4096 == 0;
    if (aligned) {
        okay &= malloc_usable_size(aligned) == 17;
        boot_drop(aligned);
    } else okay = 0;
    errno = 0;
    okay &= boot_allocate(SIZE_MAX, _Alignof(max_align_t)) == NULL && errno == ENOMEM &&
            (atomic_load_explicit(&errors, memory_order_relaxed) & ERR_BOOT);
    atomic_store_explicit(&errors, saved_errors, memory_order_relaxed);
    owner_pid = getpid();
    for (unsigned n = 0; n < GUARD_COUNT; ++n) {
        uint64_t lifetime = guard_value(&guard_calls[n]);
        uint64_t window = guard_value(&guard_window_calls[n]);
        uint64_t succeeded = guard_value(&guard_window_successes[n]);
        uint64_t failed = guard_value(&guard_window_failures[n]);
        enabled = 1;
        active = 0;
        int outside = guard_attempt(n);
        guard_result(n, outside, 1);
        okay &= !outside && guard_value(&guard_calls[n]) == lifetime + 1 &&
                guard_value(&guard_window_calls[n]) == window &&
                atomic_load_explicit(&errors, memory_order_relaxed) == saved_errors;
        active = 1;
        int inside = guard_attempt(n);
        guard_result(n, inside, 1);
        okay &= inside && guard_value(&guard_window_calls[n]) == window + 1 &&
                guard_value(&guard_window_successes[n]) == succeeded + 1 &&
                (atomic_load_explicit(&errors, memory_order_relaxed) & ERR_SCOPE);
        inside = guard_attempt(n);
        guard_result(n, inside, 0);
        active = 0;
        okay &= inside && guard_value(&guard_window_calls[n]) == window + 2 &&
                guard_value(&guard_window_failures[n]) == failed + 1 &&
                (atomic_load_explicit(&errors, memory_order_relaxed) & ERR_SCOPE);
        enabled = 0;
        atomic_store_explicit(&errors, saved_errors, memory_order_relaxed);
    }
    static const char passed[] = "{\"source_selftest\":\"pass\",\"checks\":36}\n";
    static const char failed[] = "{\"source_selftest\":\"fail\"}\n";
    (void)write_all(STDOUT_FILENO, okay ? passed : failed,
                    okay ? sizeof(passed) - 1 : sizeof(failed) - 1);
    return okay ? 0 : 1;
}
#endif
