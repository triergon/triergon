/* The subtitles reference's C library calls whose result can differ between
 * its targets (tooling only), included by reference_subtitles.h.
 *
 * The reference runs on 32-bit ARM hard-float and on
 * aarch64-unknown-linux-gnu with the same glibc release. C gives `long`,
 * `size_t` and pointers 32 bits on the first and 64 on the second, and leaves
 * a rounding function's result unspecified when it does not fit its type. The
 * worker is linked with `--wrap` for each C library call (in FFmpeg's and
 * libzvbi's archives and its own code) whose result can differ between the two
 * for the same arguments, and each wrapper computes both targets' results from
 * the arguments; where they differ, `sub-fold` adds "host_dependent":true:
 *   - strtol and strtoul (and glibc's C23 entry points of both, which also
 *     read a 0b prefix): the result saturates at, or is negated modulo, a
 *     32-bit or a 64-bit `long`.
 *   - sscanf's integer conversions other than ll, j and L: glibc converts the
 *     field as strtol or strtoul do, with `long`'s width, and stores the
 *     result in the object's low bits, so an int-sized object gets a
 *     saturated value on ARMv7 and the low 32 bits of the number on AArch64
 *     (probed on both targets). Conversions to long, size_t, ptrdiff_t and
 *     pointers are `long`-sized. A format's l is read as `long`; on AArch64
 *     the SCN*64 formats of int64_t spell it so too, and such a field beyond
 *     32 bits is flagged there although ARMv7 reads it with ll.
 *   - lrint, lrintf and lround: a rounded value outside a 32-bit `long`
 *     (or NaN or an infinity), which C leaves unspecified there.
 *   - llrint and llrintf: a rounded value outside `long long` (or NaN),
 *     unspecified in C; glibc for ARMv7 returns LLONG_MAX for NaN where the
 *     AArch64 instruction gives 0 (probed).
 * The rounding functions are calls on ARMv7 only; GCC compiles them to
 * instructions for AArch64, where they are not seen. Not flagged: conversions
 * of the same width on both targets (strtoll, strtoull, strtod, the other
 * sscanf conversions), exact functions (floor, frexp and the like), the libm
 * functions whose last bit differs between the targets for some arguments
 * (exp, log10, pow, sin and others), which a wrapper cannot tell from its
 * arguments, the magnitude of memcmp's and strcmp's results, of which C defines
 * only the sign, what callers keep of a flagged result (a palette entry keeps
 * 24 bits of strtoul's), and anything compiled into the libraries rather than
 * called: conversions from floating point out of range, `long double`
 * arithmetic, which has 64 bits on ARMv7 and 128 on AArch64, and
 * floating-point contraction. A run that fails prints no fold, so no flag; a
 * decoder that refuses to open is not such a failure, and its fold carries the
 * flag. What the flag cannot see, the libm functions among it, the harness
 * still catches: the subtitle features require 32-bit ARMv7 and pin each case
 * once from AArch64, so every ARMv7 run compares that target's reference with
 * those pins, and a case whose result differs between the targets fails there
 * unless a feature pinned per target holds it (docs/quality.md). */
#include <math.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>

/* Whether the run reached a call whose result differs between the targets. */
static atomic_bool sub_host_dependent;

static void sub_differs(bool differs) {
    if (differs) atomic_store_explicit(&sub_host_dependent, true, memory_order_relaxed);
}

static bool sub_space(char c) { return c == ' ' || (c >= '\t' && c <= '\r'); }

static int sub_digit(char c, int base) {
    int value = c >= '0' && c <= '9' ? c - '0'
              : c >= 'a' && c <= 'z' ? c - 'a' + 10
              : c >= 'A' && c <= 'Z' ? c - 'A' + 10
                                     : 36;
    return value < base ? value : -1;
}

/* A number as the C library reads it: its sign and magnitude, which `big`
 * says exceeds 64 bits. */
typedef struct {
    bool negative, big;
    uint64_t magnitude;
} SubNumber;

/* The integer subject at `p`, at most `width` bytes after any white space, in
 * `base` (0 for C's prefixes); `binary` admits C23's 0b. Its end, or NULL
 * when it has no digits. */
static const char *sub_number(const char *p, size_t width, int base, bool binary, SubNumber *n) {
    while (sub_space(*p)) p++;
    size_t at = 0;
    *n = (SubNumber){*p == '-', false, 0};
    if (at < width && (p[at] == '-' || p[at] == '+')) at++;
    bool zero = at < width && p[at] == '0';
    char marker = zero && at + 1 < width ? (char)(p[at + 1] | 0x20) : 0;
    bool prefixed = at + 2 < width;
    if ((base == 0 || base == 16) && marker == 'x' && prefixed && sub_digit(p[at + 2], 16) >= 0) {
        base = 16;
        at += 2;
    } else if (binary && (base == 0 || base == 2) && marker == 'b' && prefixed && sub_digit(p[at + 2], 2) >= 0) {
        base = 2;
        at += 2;
    } else if (base == 0) {
        base = zero ? 8 : 10;
    }
    size_t digits = at;
    for (int d; at < width && (d = sub_digit(p[at], base)) >= 0; at++) {
        if (n->magnitude > (UINT64_MAX - (uint64_t)d) / (uint64_t)base) n->big = true;
        n->magnitude = n->magnitude * (uint64_t)base + (uint64_t)d;
    }
    return at == digits ? NULL : p + at;
}

/* strtol (signed) or strtoul with a `bits`-bit `long`, as 64-bit two's
 * complement. */
static uint64_t sub_long(const SubNumber *n, bool is_signed, int bits) {
    uint64_t max = bits == 64 ? UINT64_MAX : UINT32_MAX;
    if (is_signed) {
        uint64_t high = max >> 1;
        if (n->big || n->magnitude > high + n->negative) return n->negative ? ~high : high;
    } else if (n->big || n->magnitude > max) {
        return max;
    }
    uint64_t value = n->negative ? 0 - n->magnitude : n->magnitude;
    return is_signed ? value : value & max;
}

/* `value` stored in an object of `bits` bits, read back. */
static uint64_t sub_narrow(uint64_t value, int bits, bool is_signed) {
    if (bits == 64) return value;
    uint64_t mask = (UINT64_C(1) << bits) - 1;
    value &= mask;
    return is_signed && value >> (bits - 1) ? value | ~mask : value;
}

/* Whether converting `n` with `long`'s width and storing it in an object of
 * `bits` bits on ARMv7 (0: `long`-sized) gives another value than on AArch64. */
static bool sub_long_differs(const SubNumber *n, bool is_signed, int bits) {
    uint64_t narrow = sub_narrow(sub_long(n, is_signed, 32), bits ? bits : 32, is_signed);
    uint64_t wide = sub_narrow(sub_long(n, is_signed, 64), bits ? bits : 64, is_signed);
    return narrow != wide;
}

static void sub_check_strto(const char *text, const char *end, int base, bool is_signed, bool c23) {
    SubNumber n;
    if (end != text && sub_number(text, SIZE_MAX, base, c23, &n))
        sub_differs(sub_long_differs(&n, is_signed, 0));
}

#define SUB_STRTO(type, name, is_signed, c23)                              \
    type __real_##name(const char *, char **, int);                        \
    type __wrap_##name(const char *, char **, int);                        \
    type __wrap_##name(const char *text, char **end, int base) {           \
        char *stop;                                                        \
        type value = __real_##name(text, &stop, base);                     \
        sub_check_strto(text, stop, base, is_signed, c23);                 \
        if (end) *end = stop;                                              \
        return value;                                                      \
    }
SUB_STRTO(long, strtol, true, false)
SUB_STRTO(unsigned long, strtoul, false, false)
SUB_STRTO(long, __isoc23_strtol, true, true)
SUB_STRTO(unsigned long, __isoc23_strtoul, false, true)

/* The size of the object an integer conversion stores, from its length
 * modifier, which it skips: 0 for `long`-sized, 64 for long long and intmax_t. */
static int sub_object_bits(const char **f) {
    switch (**f) {
    case 'h':
        if (*++*f != 'h') return 16;
        ++*f;
        return 8;
    case 'l':
        if (*++*f != 'l') return 0;
        ++*f;
        return 64;
    case 'j': case 'q': case 'L':
        ++*f;
        return 64;
    case 'z': case 't':
        ++*f;
        return 0;
    default:
        return 32;
    }
}

/* Walks a scanf format over its input as the conversions read it, checking
 * each stored integer conversion; stops where the input stops matching. */
static void sub_check_scan(const char *in, const char *format) {
    for (const char *f = format; *f;) {
        if (sub_space(*f)) {
            while (sub_space(*in)) in++;
            f++;
            continue;
        }
        if (*f != '%' || f[1] == '%') {
            if (*f == '%') {
                f++;
                while (sub_space(*in)) in++;
            }
            if (*in++ != *f++) return;
            continue;
        }
        f++;
        bool store = *f != '*';
        if (!store) f++;
        size_t width = 0;
        while (*f >= '0' && *f <= '9') width = width * 10 + (size_t)(*f++ - '0');
        if (*f == '$') return; /* positional arguments: not followed */
        int bits = sub_object_bits(&f);
        char conversion = *f++;
        const char *end;
        SubNumber n;
        switch (conversion) {
        case 'd': case 'i': case 'u': case 'o': case 'x': case 'X': case 'p': {
            int base = conversion == 'd' || conversion == 'u' ? 10
                     : conversion == 'i' ? 0
                     : conversion == 'o' ? 8 : 16;
            if (!(end = sub_number(in, width ? width : SIZE_MAX, base, false, &n))) return;
            if (conversion == 'p') bits = 0;
            if (store && bits != 64)
                sub_differs(sub_long_differs(&n, conversion == 'd' || conversion == 'i', bits));
            in = end;
            break;
        }
        case 'n':
            break;
        case 'c':
            for (size_t k = width ? width : 1; k; k--)
                if (!*in++) return;
            break;
        case 's': {
            while (sub_space(*in)) in++;
            const char *start = in;
            while (*in && !sub_space(*in) && (!width || (size_t)(in - start) < width)) in++;
            if (in == start) return;
            break;
        }
        case '[': {
            bool invert = *f == '^';
            if (invert) f++;
            const char *set = f;
            if (*f == ']') f++;
            while (*f && *f != ']') f++;
            if (!*f) return;
            size_t length = (size_t)(f++ - set);
            const char *start = in;
            while (*in && (!width || (size_t)(in - start) < width)) {
                bool member = false;
                for (size_t k = 0; k < length && !member; k++) {
                    if (k + 2 < length && set[k + 1] == '-') {
                        member = (unsigned char)*in >= (unsigned char)set[k] &&
                                 (unsigned char)*in <= (unsigned char)set[k + 2];
                        k += 2;
                    } else {
                        member = *in == set[k];
                    }
                }
                if (member == invert) break;
                in++;
            }
            if (in == start) return;
            break;
        }
        default:
            /* Floating-point conversions read as strtod does, the same on both
             * targets; a format that goes on after one is not followed. */
            return;
        }
    }
}

int __isoc99_vsscanf(const char *, const char *, va_list);
int __wrap___isoc99_sscanf(const char *, const char *, ...);
int __wrap___isoc99_sscanf(const char *text, const char *format, ...) {
    va_list arguments;
    va_start(arguments, format);
    int stored = __isoc99_vsscanf(text, format, arguments);
    va_end(arguments);
    if (stored > 0) sub_check_scan(text, format);
    return stored;
}

/* A rounded value outside [low, high], NaN included. */
#define SUB_ROUND(type, name, argument, rounded, low, high)                \
    type __real_##name(argument);                                          \
    type __wrap_##name(argument);                                          \
    type __wrap_##name(argument x) {                                       \
        double r = (double)rounded;                                        \
        sub_differs(!(r >= (low) && r <= (high)));                         \
        return __real_##name(x);                                           \
    }
SUB_ROUND(long, lrint, double, rint(x), -0x1p31, 0x1p31 - 1)
SUB_ROUND(long, lrintf, float, rintf(x), -0x1p31, 0x1p31 - 1)
SUB_ROUND(long, lround, double, round(x), -0x1p31, 0x1p31 - 1)
SUB_ROUND(long long, llrint, double, x, -0x1p63, 0x1p63 - 1024)
SUB_ROUND(long long, llrintf, float, x, -0x1p63, 0x1p63 - 0x1p39)
