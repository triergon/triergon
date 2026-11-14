/*
 * The fixtures of the ASS references (tooling only; never linked into the
 * library): a script or stream fixture's directives (;@force, ;@extract-fonts,
 * ;@time) and stream commands (@private, @data and @styles blocks, @chunk,
 * @dump, @flush, @prune, @check-readorder, @frames), read as
 * crates/ass-worker/src/fixture.rs reads them, and the helpers both libass
 * builds share. Included by reference_ass.h, the parity reference, and by
 * reference_ass_player.h, the measurement reference, after libass's headers.
 */
#include <stdarg.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "ass.h"
#include "ass_priv.h"

#define ASSR_MAX_INPUT (64L << 20)
#define ASSR_MAX_OVERRIDES 255
#define ASSR_MAX_TIMES 64

typedef struct {
    const char *start;
    size_t len;
} AssLine;

enum {
    ASSC_PRIVATE,
    ASSC_DATA,
    ASSC_STYLES,
    ASSC_CHUNK,
    ASSC_DUMP,
    ASSC_FLUSH,
    ASSC_PRUNE,
    ASSC_CHECK,
    ASSC_FRAMES
};

typedef struct {
    int kind;
    char *text; /* a block's lines, each with its LF; NUL-terminated */
    size_t len;
    const char *packet;
    size_t packet_len;
    long long start, duration; /* a chunk's times, the prune deadline, the check */
    long long end;             /* the last frame of @frames, whose step is duration */
} AssCommand;

typedef struct {
    char *overrides[ASSR_MAX_OVERRIDES + 1];
    size_t n_overrides;
    int extract_fonts;
    long long times[ASSR_MAX_TIMES];
    size_t n_times;
    AssCommand *commands;
    size_t n_commands;
} AssFixture;

static int assr_starts(AssLine l, const char *prefix) {
    size_t n = strlen(prefix);
    return l.len >= n && !memcmp(l.start, prefix, n);
}

/* An optional '-' and 1 to 18 digits after any spaces, within [at, end). */
static int assr_number(const char **at, const char *end, long long *out) {
    const char *p = *at;
    while (p < end && *p == ' ') p++;
    int negative = p < end && *p == '-';
    if (negative) p++;
    long long v = 0;
    int digits = 0;
    while (p < end && *p >= '0' && *p <= '9') {
        if (++digits > 18) return 0;
        v = v * 10 + (*p++ - '0');
    }
    if (!digits) return 0;
    *out = negative ? -v : v;
    *at = p;
    return 1;
}

static int assr_line_number(AssLine l, size_t skip, long long *out) {
    const char *at = l.start + skip;
    return assr_number(&at, l.start + l.len, out);
}

static char *assr_join(const AssLine *lines, size_t from, size_t to, size_t *size) {
    size_t total = 0;
    for (size_t i = from; i < to; i++) total += lines[i].len + 1;
    char *buf = malloc(total + 1);
    if (!buf) return NULL;
    size_t at = 0;
    for (size_t i = from; i < to; i++) {
        memcpy(buf + at, lines[i].start, lines[i].len);
        at += lines[i].len;
        buf[at++] = '\n';
    }
    buf[at] = 0;
    *size = at;
    return buf;
}

static int assr_read_fixture(const char *data, size_t size, int stream, AssFixture *f) {
    memset(f, 0, sizeof *f);
    size_t count = 0, cap = 64;
    AssLine *lines = malloc(cap * sizeof *lines);
    if (!lines) return 0;
    for (const char *p = data, *end = data + size; p < end;) {
        const char *q = memchr(p, '\n', (size_t)(end - p));
        if (!q) q = end;
        if (count == cap) {
            AssLine *more = realloc(lines, (cap *= 2) * sizeof *lines);
            if (!more) {
                free(lines);
                return 0;
            }
            lines = more;
        }
        lines[count++] = (AssLine){p, (size_t)(q - p)};
        p = q + 1;
    }
    for (size_t i = 0; i < count; i++) {
        AssLine l = lines[i];
        if (assr_starts(l, ";@force ")) {
            if (f->n_overrides < ASSR_MAX_OVERRIDES)
                f->overrides[f->n_overrides++] = strndup(l.start + 8, l.len - 8);
        } else if (assr_starts(l, ";@extract-fonts")) {
            f->extract_fonts = 1;
        } else if (assr_starts(l, ";@time ")) {
            const char *at = l.start + 7, *end = l.start + l.len;
            while (f->n_times < ASSR_MAX_TIMES && assr_number(&at, end, &f->times[f->n_times]))
                f->n_times++;
        }
    }
    f->commands = calloc(count ? count : 1, sizeof *f->commands);
    if (!f->commands) {
        free(lines);
        return 0;
    }
    for (size_t i = 0; stream && i < count; i++) {
        AssLine l = lines[i];
        AssCommand *c = f->commands + f->n_commands;
        int block = assr_starts(l, "@private")  ? ASSC_PRIVATE
                    : assr_starts(l, "@data")   ? ASSC_DATA
                    : assr_starts(l, "@styles") ? ASSC_STYLES
                                                : -1;
        if (block >= 0) {
            size_t end = i + 1;
            while (end < count && !assr_starts(lines[end], "@end")) end++;
            c->kind = block;
            c->text = assr_join(lines, i + 1, end, &c->len);
            if (!c->text) {
                free(lines);
                return 0;
            }
            f->n_commands++;
            i = end;
        } else if (assr_starts(l, "@chunk ")) {
            const char *at = l.start + 7, *end = l.start + l.len;
            if (assr_number(&at, end, &c->start) && assr_number(&at, end, &c->duration) &&
                i + 1 < count) {
                i++;
                c->kind = ASSC_CHUNK;
                c->packet = lines[i].start;
                c->packet_len = lines[i].len;
                f->n_commands++;
            }
        } else if (assr_starts(l, "@frames ")) {
            const char *at = l.start + 8, *end = l.start + l.len;
            if (assr_number(&at, end, &c->start) && assr_number(&at, end, &c->duration) &&
                assr_number(&at, end, &c->end) && c->duration > 0) {
                c->kind = ASSC_FRAMES;
                f->n_commands++;
            }
        } else if (assr_starts(l, "@dump")) {
            c->kind = ASSC_DUMP;
            f->n_commands++;
        } else if (assr_starts(l, "@flush")) {
            c->kind = ASSC_FLUSH;
            f->n_commands++;
        } else if (assr_starts(l, "@prune ")) {
            c->kind = ASSC_PRUNE;
            if (!assr_line_number(l, 7, &c->start)) c->start = 0;
            f->n_commands++;
        } else if (assr_starts(l, "@check-readorder ")) {
            c->kind = ASSC_CHECK;
            long long n;
            c->start = assr_line_number(l, 17, &n) && n == 1;
            f->n_commands++;
        }
    }
    free(lines);
    return 1;
}

static void assr_free_fixture(AssFixture *f) {
    for (size_t i = 0; i < f->n_overrides; i++) free(f->overrides[i]);
    for (size_t i = 0; i < f->n_commands; i++) free(f->commands[i].text);
    free(f->commands);
}

/* ass_read_styles (ass.c) on memory: the lines are read in the styles section,
 * which is restored afterwards. */
static void assr_read_styles(ASS_Track *track, const char *text, size_t len) {
    ParserState old_state = track->parser_priv->state;
    track->parser_priv->state = PST_STYLES;
    ass_process_data(track, text, (int)len);
    track->parser_priv->state = old_state;
}

static void assr_quiet(int level, const char *fmt, va_list args, void *data) {
    (void)level, (void)fmt, (void)args, (void)data;
}

static uint64_t assr_now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * UINT64_C(1000000000) + (uint64_t)t.tv_nsec;
}
