/*
 * ASS parsing reference: the libass build of harness/reference.c (tooling only;
 * never linked into the library). It includes libass's ass_render.c from the
 * pinned source tree, as reference_raster.h does, to reach the render context
 * and its static init_render_context and free_render_context, ass_drawing.c
 * and a copy of ass_parse.c, to see the conversions they make (below), and links
 * the built libass.a for everything else. It is compiled with the flags libass compiled itself with
 * (libass-internal.pc).
 *
 * Usage:
 *   reference ass-fold   FILE MODE
 *   reference ass-bench  FILE ITERATIONS MODE
 *   reference ass-memory FILE MODE
 *   reference ass-dump   FILE MODE
 *
 * MODE is one of:
 *   script          FILE is a whole script, read with ass_read_memory; the
 *                   track is recorded (no track when libass returns none).
 *   chunks          FILE is a stream fixture: its commands run on a track from
 *                   ass_new_track through ass_process_codec_private,
 *                   ass_process_data, ass_process_chunk (ReadOrder checked as
 *                   ass_set_check_readorder sets it), ass_flush_events and
 *                   ass_prune_events; the track is recorded at every @dump and
 *                   at the end.
 *   script events   every event of the script, or of the stream after its
 *   chunks events   commands, walked at the fixture's times after
 *                   ass_lazy_track_init: init_render_context, then the text
 *                   loop of parse_events with ass_parse_tags and
 *                   ass_get_next_char, every drawing and the vector clip read
 *                   with ass_drawing_parse, and ass_process_karaoke_effects over
 *                   synthetic positions.
 *
 * The fixture directives (;@force, ;@extract-fonts, ;@time) and stream commands
 * are crates/ass-worker/src/fixture.rs's; the directives configure the library
 * (ass_set_style_overrides, ass_set_extract_fonts) before the operation. Each
 * record starts with a tag and folds as crates/ass-worker/src/record.rs folds it;
 * `packets` counts records and `bytes` the bytes of the strings recorded.
 *
 * Restated from libass 0.17.5, as tooling, because libass keeps them static:
 *   - the text loop of parse_events (ass_render.c), without its font and glyph
 *     metrics, which belong to layout;
 *   - ass_read_styles (ass.c) reading from memory: the parser state is switched
 *     to the styles section around ass_process_data instead of loading a file.
 * Values libass reads but treats alike are recorded as one: Underline and
 * StrikeOut as 0 or 1, Alignment by its four low bits, WrapStyle and Justify by
 * the values libass names (others as 0, which libass treats them as).
 *
 * `ass-render-*` modes render frames instead (reference_ass_render.h); a
 * stream's `@frames` command belongs to them and does nothing here.
 *
 * The file is read and its directives and commands parsed before each timed and
 * memory operation; the library and, for a walk, the track and the renderer are
 * created once, outside it. The renderer has no font provider, so the font
 * lookups libass's tags make find no font. `ass-dump` prints the same records
 * as lines, identical to ass-worker's.
 *
 * A case whose result C leaves to the target is flagged, by explicit checks
 * at each step where it can arise, on every host:
 *   - a drawing coordinate: double_to_d6 is lrint(x * 64) narrowed from long
 *     to int, and a 26.6 value outside int's range or NaN converts as the
 *     target's lrint and long have it (saturating at long's range on ARM,
 *     LONG_MIN on x86-64; 32 or 64 bits). ass_drawing.c is compiled into this
 *     file with its calls of double_to_d6 going through assr_double_to_d6.
 *   - a coordinate landing on INT_MIN, whose abs libass's bound check in
 *     ass_outline_add_point takes (Clang reads it as out of bounds, GCC as in
 *     bounds): those calls go through assr_add_point.
 *   - every other conversion from double to int that C leaves undefined out of
 *     range or for NaN: an animated \clip or \iclip, \fad and \fade's alpha,
 *     the delay of a Banner or Scroll effect (in a copy of ass_parse.c that
 *     xtask writes with each of them wrapped in ASSR_TO_INT, compiled here in
 *     place of the library's), and \pbo copied into a drawing glyph (in the
 *     text loop below). assr_to_int notes such a value and converts it as the
 *     compiler does.
 * The floating-point invalid exception is not used: C defines results that
 * raise it too (0 x inf in strtod, inf - inf in \t, fmod(inf), and dtoi32,
 * which Clang compiles to a conversion before its range check).
 * A flagged fold adds "host_dependent":true and ass-dump says so on stderr;
 * ass-worker cannot know, so the flag comes from the reference alone.
 */
#include "ass_render.c"

/* Whether the case reached a step whose result C leaves to the target. */
static bool assr_host_dependent;

static int assr_double_to_d6(double x) {
    double d6 = rint(x * 64);
    if (!(d6 >= INT_MIN && d6 <= INT_MAX)) assr_host_dependent = true;
    return double_to_d6(x);
}

static bool assr_add_point(ASS_Outline *outline, ASS_Vector pt, char segment);
#define ass_outline_add_point assr_add_point
#define double_to_d6 assr_double_to_d6
#include "ass_drawing.c"
#undef double_to_d6
#undef ass_outline_add_point

/* A double converted to int: out of int's range or NaN, C leaves the result to
 * the target, so the case is noted; the conversion is the compiler's. */
static int assr_to_int(double x) {
    if (!(x > INT_MIN - 1.0 && x < INT_MAX + 1.0)) assr_host_dependent = true;
    return (int)x;
}
#define ASSR_TO_INT(x) assr_to_int(x)
#include "ass_parse_observed.c"

#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define ASSR_ABSENT UINT64_MAX
#define ASSR_FNV_INIT 0xcbf29ce484222325ULL
#define ASSR_MAX_INPUT (64L << 20)
#define ASSR_MAX_OVERRIDES 255
#define ASSR_MAX_TIMES 64

enum {
    ASSR_TRACK = 1,
    ASSR_FORMATS,
    ASSR_STYLE,
    ASSR_EVENT,
    ASSR_FONT,
    ASSR_NO_TRACK,
    ASSR_PLAY_RES,
    ASSR_WALK,
    ASSR_GLYPH,
    ASSR_OUTLINE,
    ASSR_STATE,
    ASSR_CLIP,
    ASSR_KARAOKE,
};

static const char *assr_name(int tag) {
    static const char *const names[] = {"",      "track",   "formats", "style", "event",
                                        "font",  "no-track", "playres", "walk",  "glyph",
                                        "outline", "state",  "clip",    "kara"};
    return names[tag];
}

/* ---------------------------------------------------------------- records */

typedef struct {
    uint64_t packets, bytes, digest;
    FILE *dump; /* -dump: print lines instead of folding */
} AssWork;

static void assr_fold(AssWork *w, uint64_t v) { w->digest = w->digest * 31 + v; }


static bool assr_add_point(ASS_Outline *outline, ASS_Vector pt, char segment) {
    if (pt.x == INT32_MIN || pt.y == INT32_MIN) assr_host_dependent = true;
    return ass_outline_add_point(outline, pt, segment);
}

static void assr_begin(AssWork *w, int tag) {
    if (w->dump) {
        fputs(assr_name(tag), w->dump);
        return;
    }
    w->packets++;
    assr_fold(w, (uint64_t)tag);
}

static void assr_int(AssWork *w, int64_t v) {
    if (w->dump)
        fprintf(w->dump, " %" PRId64, v);
    else
        assr_fold(w, (uint64_t)v);
}

static void assr_flag(AssWork *w, int v) { assr_int(w, v != 0); }

static void assr_real(AssWork *w, double v) {
    uint64_t bits;
    memcpy(&bits, &v, sizeof bits);
    if (w->dump)
        fprintf(w->dump, " %016" PRIx64, bits);
    else
        assr_fold(w, bits);
}

static void assr_text(AssWork *w, const char *s, size_t n) {
    if (w->dump) {
        fputs(" \"", w->dump);
        for (size_t i = 0; i < n; i++) {
            unsigned char c = (unsigned char)s[i];
            if (c >= 0x20 && c < 0x7f && c != '"' && c != '\\')
                fputc(c, w->dump);
            else
                fprintf(w->dump, "\\x%02x", c);
        }
        fputc('"', w->dump);
        return;
    }
    w->bytes += n;
    assr_fold(w, n);
    for (size_t i = 0; i < n; i++) assr_fold(w, (unsigned char)s[i]);
}

static void assr_absent(AssWork *w) {
    if (w->dump)
        fputs(" -", w->dump);
    else
        assr_fold(w, ASSR_ABSENT);
}

static void assr_optional(AssWork *w, const char *s) {
    if (s)
        assr_text(w, s, strlen(s));
    else
        assr_absent(w);
}

static void assr_end(AssWork *w) {
    if (w->dump) fputc('\n', w->dump);
}

static uint64_t assr_fnv(uint64_t h, const void *data, size_t n) {
    const unsigned char *p = data;
    for (size_t i = 0; i < n; i++) {
        h ^= p[i];
        h *= 0x100000001b3ULL;
    }
    return h;
}

/* WrapStyle and Justify by the values libass names. */
static int assr_wrap(int v) { return v == 1 || v == 2 || v == 3 ? v : 0; }
static int assr_justify(int v) {
    return v == ASS_JUSTIFY_LEFT || v == ASS_JUSTIFY_CENTER || v == ASS_JUSTIFY_RIGHT ? v : 0;
}

/* A drawing read with ass_drawing_parse: its control box, point and segment
 * counts and the hash of each point's x and y as little-endian 32-bit values,
 * then the segment bytes; absent when libass rejects it. */
static void assr_drawing(AssWork *w, int tag, const char *text, size_t len) {
    char *copy = strndup(text, len);
    ASS_Outline o;
    ASS_Rect cbox;
    assr_begin(w, tag);
    if (copy && ass_drawing_parse(&o, &cbox, copy, NULL)) {
        uint64_t h = ASSR_FNV_INIT;
        for (size_t i = 0; i < o.n_points; i++) {
            uint32_t x = (uint32_t)o.points[i].x, y = (uint32_t)o.points[i].y;
            unsigned char bytes[8] = {x, x >> 8, x >> 16, x >> 24, y, y >> 8, y >> 16, y >> 24};
            h = assr_fnv(h, bytes, sizeof bytes);
        }
        h = assr_fnv(h, o.segments, o.n_segments);
        assr_int(w, cbox.x_min);
        assr_int(w, cbox.y_min);
        assr_int(w, cbox.x_max);
        assr_int(w, cbox.y_max);
        assr_int(w, (int64_t)o.n_points);
        assr_int(w, (int64_t)o.n_segments);
        assr_int(w, (int64_t)h);
        ass_outline_free(&o);
    } else {
        assr_absent(w);
    }
    assr_end(w);
    free(copy);
}

/* The track: header values, formats, styles, events and embedded fonts. */
static void assr_track(AssWork *w, ASS_Library *lib, ASS_Track *t) {
    assr_begin(w, ASSR_TRACK);
    assr_int(w, (int)t->track_type);
    assr_int(w, t->PlayResX);
    assr_int(w, t->PlayResY);
    assr_int(w, t->LayoutResX);
    assr_int(w, t->LayoutResY);
    assr_real(w, t->Timer);
    assr_int(w, assr_wrap(t->WrapStyle));
    assr_flag(w, t->ScaledBorderAndShadow);
    assr_flag(w, t->Kerning);
    assr_int(w, (int)t->YCbCrMatrix);
    assr_optional(w, t->Language);
    assr_int(w, t->default_style);
    assr_int(w, t->n_styles);
    assr_int(w, t->n_events);
    assr_end(w);
    assr_begin(w, ASSR_FORMATS);
    assr_optional(w, t->style_format);
    assr_optional(w, t->event_format);
    assr_end(w);
    for (int i = 0; i < t->n_styles; i++) {
        ASS_Style *s = t->styles + i;
        assr_begin(w, ASSR_STYLE);
        assr_int(w, i);
        assr_optional(w, s->Name);
        assr_optional(w, s->FontName);
        assr_real(w, s->FontSize);
        assr_int(w, s->PrimaryColour);
        assr_int(w, s->SecondaryColour);
        assr_int(w, s->OutlineColour);
        assr_int(w, s->BackColour);
        assr_int(w, s->Bold);
        assr_int(w, s->Italic);
        assr_flag(w, s->Underline);
        assr_flag(w, s->StrikeOut);
        assr_real(w, s->ScaleX);
        assr_real(w, s->ScaleY);
        assr_real(w, s->Spacing);
        assr_real(w, s->Angle);
        assr_int(w, s->BorderStyle);
        assr_real(w, s->Outline);
        assr_real(w, s->Shadow);
        assr_int(w, s->Alignment & 15);
        assr_int(w, s->MarginL);
        assr_int(w, s->MarginR);
        assr_int(w, s->MarginV);
        assr_int(w, s->Encoding);
        assr_real(w, s->Blur);
        assr_int(w, assr_justify(s->Justify));
        assr_end(w);
    }
    for (int i = 0; i < t->n_events; i++) {
        ASS_Event *e = t->events + i;
        assr_begin(w, ASSR_EVENT);
        assr_int(w, i);
        assr_int(w, e->Start);
        assr_int(w, e->Duration);
        assr_int(w, e->ReadOrder);
        assr_int(w, e->Layer);
        assr_int(w, e->Style);
        assr_optional(w, e->Name);
        assr_int(w, e->MarginL);
        assr_int(w, e->MarginR);
        assr_int(w, e->MarginV);
        assr_optional(w, e->Effect);
        assr_text(w, e->Text ? e->Text : "", e->Text ? strlen(e->Text) : 0);
        assr_end(w);
    }
    for (size_t i = 0; i < lib->num_fontdata; i++) {
        ASS_Fontdata *f = lib->fontdata + i;
        assr_begin(w, ASSR_FONT);
        assr_int(w, (int64_t)i);
        assr_optional(w, f->name);
        assr_int(w, f->size);
        assr_int(w, (int64_t)assr_fnv(ASSR_FNV_INIT, f->data, (size_t)f->size));
        assr_end(w);
    }
}

/* -------------------------------------------------------------- fixtures */

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

/* ------------------------------------------------------------ operations */

/* ass_read_styles (ass.c) on memory: the lines are read in the styles section,
 * which is restored afterwards. */
static void assr_read_styles(ASS_Track *track, const char *text, size_t len) {
    ParserState old_state = track->parser_priv->state;
    track->parser_priv->state = PST_STYLES;
    ass_process_data(track, text, (int)len);
    track->parser_priv->state = old_state;
}

/* A stream fixture's commands on a new track, recorded at every @dump and at
 * the end. */
static ASS_Track *assr_stream(AssWork *w, ASS_Library *lib, const AssFixture *f) {
    ASS_Track *track = ass_new_track(lib);
    if (!track) return NULL;
    for (size_t i = 0; i < f->n_commands; i++) {
        const AssCommand *c = f->commands + i;
        switch (c->kind) {
        case ASSC_PRIVATE: ass_process_codec_private(track, c->text, (int)c->len); break;
        case ASSC_DATA: ass_process_data(track, c->text, (int)c->len); break;
        case ASSC_STYLES: assr_read_styles(track, c->text, c->len); break;
        case ASSC_CHUNK:
            ass_process_chunk(track, c->packet, (int)c->packet_len, c->start, c->duration);
            break;
        case ASSC_DUMP: assr_track(w, lib, track); break;
        case ASSC_FLUSH: ass_flush_events(track); break;
        case ASSC_PRUNE: ass_prune_events(track, c->start); break;
        case ASSC_CHECK: ass_set_check_readorder(track, (int)c->start); break;
        case ASSC_FRAMES: break;
        }
    }
    assr_track(w, lib, track);
    return track;
}

/* Glyphs reused across walks. */
static GlyphInfo *assr_glyphs;
static size_t assr_capacity;

static void assr_glyph(AssWork *w, RenderContext *s, GlyphInfo *g, size_t offset) {
    assr_begin(w, ASSR_GLYPH);
    assr_int(w, (int64_t)offset);
    if (g->drawing_text.str) {
        assr_int(w, 1);
        assr_text(w, g->drawing_text.str, g->drawing_text.len);
        assr_int(w, g->drawing_scale);
        assr_int(w, g->drawing_pbo);
    } else {
        assr_int(w, 0);
        assr_int(w, g->symbol);
    }
    assr_text(w, s->family.str, s->family.len);
    assr_int(w, (uint32_t)g->bold);
    assr_int(w, (uint32_t)g->italic);
    assr_real(w, s->font_size);
    for (int i = 0; i < 4; i++) assr_int(w, g->c[i]);
    assr_int(w, g->flags);
    assr_int(w, g->border_style);
    assr_real(w, g->border_x);
    assr_real(w, g->border_y);
    assr_real(w, g->scale_x);
    assr_real(w, g->scale_y);
    assr_real(w, g->hspacing);
    assr_int(w, g->be);
    assr_real(w, g->blur);
    assr_real(w, g->shadow_x);
    assr_real(w, g->shadow_y);
    assr_real(w, g->frx);
    assr_real(w, g->fry);
    assr_real(w, g->frz);
    assr_real(w, g->fax);
    assr_real(w, g->fay);
    assr_int(w, s->font_encoding);
    assr_int(w, g->fade);
    assr_int(w, (int)g->effect_type);
    assr_int(w, g->effect_timing);
    assr_int(w, g->effect_skip_timing);
    assr_flag(w, g->reset_effect);
    assr_end(w);
}

static void assr_state(AssWork *w, RenderContext *s) {
    assr_begin(w, ASSR_STATE);
    assr_int(w, s->alignment & 15);
    assr_int(w, assr_justify(s->justify));
    assr_int(w, assr_wrap(s->wrap_style));
    assr_flag(w, s->evt_type & EVENT_POSITIONED);
    assr_real(w, s->pos_x);
    assr_real(w, s->pos_y);
    assr_flag(w, s->have_origin);
    assr_real(w, s->org_x);
    assr_real(w, s->org_y);
    assr_int(w, s->clip_x0);
    assr_int(w, s->clip_y0);
    assr_int(w, s->clip_x1);
    assr_int(w, s->clip_y1);
    assr_flag(w, s->clip_mode);
    if (s->clip_drawing_text.str) {
        assr_text(w, s->clip_drawing_text.str, s->clip_drawing_text.len);
        assr_int(w, s->clip_drawing_scale);
        assr_flag(w, s->clip_drawing_mode);
    } else {
        assr_absent(w);
    }
    assr_flag(w, s->detect_collisions);
    assr_flag(w, s->explicit);
    assr_int(w, s->fade);
    assr_int(w, s->drawing_scale);
    assr_real(w, s->pbo);
    if (s->evt_type & (EVENT_HSCROLL | EVENT_VSCROLL)) {
        assr_int(w, (int)s->scroll_direction);
        assr_real(w, s->scroll_shift);
        if (s->evt_type & EVENT_VSCROLL) {
            assr_int(w, s->scroll_y0);
            assr_int(w, s->scroll_y1);
        } else {
            assr_absent(w);
        }
    } else {
        assr_absent(w);
    }
    assr_optional(w, s->style->Name);
    assr_end(w);
}

/* One event at one time: init_render_context, the text loop of parse_events,
 * the event state, the vector clip and karaoke over synthetic positions. */
static int assr_walk(AssWork *w, ASS_Renderer *r, ASS_Track *t, int index, long long now) {
    RenderContext *s = &r->state;
    ASS_Event *event = t->events + index;
    r->time = now;
    assr_begin(w, ASSR_WALK);
    assr_int(w, index);
    assr_int(w, now);
    assr_end(w);
    free_render_context(s);
    init_render_context(s, event);
    size_t n = 0;
    char *p = event->Text, *q;
    /* The text loop of parse_events, without fonts. */
    while (1) {
        ASS_StringView drawing = {NULL, 0};
        unsigned code = 0;
        char *start = p;
        while (*p) {
            if (*p == '{' && (q = strchr(p, '}'))) {
                p = ass_parse_tags(s, p, q, 1., false);
                p++;
            } else if (s->drawing_scale) {
                q = p;
                if (*p == '{') q++;
                while (*q != '{' && *q) q++;
                drawing.str = p;
                drawing.len = (size_t)(q - p);
                code = 0xfffc;
                start = p;
                p = q;
                break;
            } else {
                start = p;
                code = ass_get_next_char(s, &p);
                break;
            }
        }
        if (!code) break;
        if (n == assr_capacity) {
            size_t capacity = assr_capacity ? 2 * assr_capacity : 64;
            GlyphInfo *more = realloc(assr_glyphs, capacity * sizeof *more);
            if (!more) return 0;
            assr_glyphs = more;
            assr_capacity = capacity;
        }
        GlyphInfo *g = assr_glyphs + n++;
        memset(g, 0, sizeof *g);
        if (drawing.str) {
            g->drawing_text = drawing;
            g->drawing_scale = s->drawing_scale;
            g->drawing_pbo = assr_to_int(s->pbo);
        }
        g->symbol = code;
        for (int i = 0; i < 4; i++) g->c[i] = s->c[i];
        g->effect_type = s->effect_type;
        g->effect_timing = s->effect_timing;
        g->effect_skip_timing = s->effect_skip_timing;
        g->reset_effect = s->reset_effect;
        g->be = s->be;
        g->blur = s->blur;
        g->shadow_x = s->shadow_x;
        g->shadow_y = s->shadow_y;
        g->scale_x = s->scale_x;
        g->scale_y = s->scale_y;
        g->border_style = s->border_style;
        g->border_x = s->border_x;
        g->border_y = s->border_y;
        g->hspacing = s->hspacing;
        g->bold = s->bold;
        g->italic = s->italic;
        g->flags = s->flags;
        g->frx = s->frx;
        g->fry = s->fry;
        g->frz = s->frz;
        g->fax = s->fax;
        g->fay = s->fay;
        g->fade = s->fade;
        assr_glyph(w, s, g, (size_t)(start - event->Text));
        if (drawing.str) assr_drawing(w, ASSR_OUTLINE, drawing.str, drawing.len);
        s->effect_type = EF_NONE;
        s->effect_timing = 0;
        s->effect_skip_timing = 0;
        s->reset_effect = false;
    }
    assr_state(w, s);
    if (s->clip_drawing_text.str)
        assr_drawing(w, ASSR_CLIP, s->clip_drawing_text.str, s->clip_drawing_text.len);

    /* Karaoke over synthetic positions: run starts where libass's style-run
     * split puts them for karaoke alone, every glyph 10 pixels plus its index
     * mod 3 wide, spaces invisible. */
    GlyphInfo *glyphs = assr_glyphs;
    Effect last_type = n ? glyphs[0].effect_type : EF_NONE;
    int x = 0;
    for (size_t i = 0; i < n; i++) {
        GlyphInfo *g = glyphs + i;
        Effect type = g->effect_type;
        g->starts_new_run = !i || g->effect_timing || (type != EF_NONE && type != last_type) ||
                            g->drawing_text.str || glyphs[i ? i - 1 : 0].drawing_text.str;
        if (type != EF_NONE) last_type = type;
        g->skip = g->symbol == ' ';
        g->pos.x = x;
        g->advance.x = 64 * (10 + (int)(i % 3));
        x += g->advance.x;
    }
    TextInfo saved = s->text_info;
    s->text_info.glyphs = glyphs;
    s->text_info.length = (int)n;
    ass_process_karaoke_effects(s);
    s->text_info = saved;
    for (size_t i = 0; i < n; i++) {
        assr_begin(w, ASSR_KARAOKE);
        assr_int(w, (int64_t)i);
        assr_int(w, (int)glyphs[i].effect_type);
        assr_int(w, glyphs[i].effect_timing);
        assr_int(w, glyphs[i].c[0]);
        assr_int(w, glyphs[i].c[1]);
        assr_end(w);
    }
    free_render_context(s);
    return 1;
}

static int assr_walk_all(AssWork *w, ASS_Renderer *r, ASS_Track *t, const AssFixture *f) {
    assr_begin(w, ASSR_PLAY_RES);
    assr_int(w, t->PlayResX);
    assr_int(w, t->PlayResY);
    assr_end(w);
    for (int e = 0; e < t->n_events; e++)
        for (size_t k = 0; k < f->n_times; k++)
            if (!assr_walk(w, r, t, e, f->times[k])) return 0;
    return 1;
}

/* ------------------------------------------------------------------- main */

static void assr_quiet(int level, const char *fmt, va_list args, void *data) {
    (void)level, (void)fmt, (void)args, (void)data;
}

static uint64_t assr_now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * UINT64_C(1000000000) + (uint64_t)t.tv_nsec;
}

enum { ASSM_SCRIPT, ASSM_CHUNKS, ASSM_EVENTS };

/* One operation; 0 when out of memory. */
static int assr_operation(AssWork *w, int mode, ASS_Library *lib, char *data, size_t size,
                          const AssFixture *f, ASS_Renderer *r, ASS_Track *walked) {
    if (mode == ASSM_EVENTS) {
        if (!walked) {
            assr_begin(w, ASSR_NO_TRACK);
            assr_end(w);
            return 1;
        }
        return assr_walk_all(w, r, walked, f);
    }
    ASS_Track *track = mode == ASSM_SCRIPT ? ass_read_memory(lib, data, size, NULL)
                                           : assr_stream(w, lib, f);
    if (mode == ASSM_SCRIPT) {
        if (track) {
            assr_track(w, lib, track);
        } else {
            assr_begin(w, ASSR_NO_TRACK);
            assr_end(w);
        }
    }
    if (track) ass_free_track(track);
    ass_clear_fonts(lib);
    return mode == ASSM_SCRIPT || track != NULL;
}

#include "reference_ass_render.h"

int main(int argc, char **argv) {
    if (argc > 1 && !strncmp(argv[1], "ass-render-", 11)) return assr_render_main(argc, argv);
    const char *usage = "usage: reference ass-fold FILE MODE | ass-bench FILE N MODE\n"
                        "       | ass-memory FILE MODE | ass-dump FILE MODE\n"
                        "  MODE: script | chunks, either followed by events\n";
    const char *action = argc > 1 ? argv[1] : "";
    int bench = !strcmp(action, "ass-bench"), memory = !strcmp(action, "ass-memory");
    int dump = !strcmp(action, "ass-dump"), fold = !strcmp(action, "ass-fold");
    if (!(bench || memory || dump || fold) || argc < 4 + bench || argc > 5 + bench) {
        fputs(usage, stderr);
        return 2;
    }
    char *end = NULL;
    unsigned long iterations = bench ? strtoul(argv[3], &end, 10) : 1;
    if ((end && *end) || iterations < 1 || iterations > 1000000) {
        fputs(usage, stderr);
        return 2;
    }
    char **args = argv + 3 + bench;
    int count = argc - 3 - bench;
    int stream = !strcmp(args[0], "chunks");
    if ((!stream && strcmp(args[0], "script")) || (count == 2 && strcmp(args[1], "events"))) {
        fputs(usage, stderr);
        return 2;
    }
    int mode = count == 2 ? ASSM_EVENTS : stream ? ASSM_CHUNKS : ASSM_SCRIPT;
    FILE *file = fopen(argv[2], "rb");
    if (!file) {
        perror(argv[2]);
        return 2;
    }
    fseek(file, 0, SEEK_END);
    long size = ftell(file);
    rewind(file);
    char *data = size >= 0 && size <= ASSR_MAX_INPUT ? malloc((size_t)size + 1) : NULL;
    int read_ok = data && fread(data, 1, (size_t)size, file) == (size_t)size;
    fclose(file);
    AssFixture fixture;
    ASS_Library *lib = read_ok ? ass_library_init() : NULL;
    if (!lib || !assr_read_fixture(data, (size_t)size, stream, &fixture)) {
        fprintf(stderr, "%s: unreadable, too large, or out of memory\n", argv[2]);
        return 2;
    }
    data[size] = 0;
    ass_set_message_cb(lib, assr_quiet, NULL);
    fixture.overrides[fixture.n_overrides] = NULL;
    ass_set_style_overrides(lib, fixture.overrides);
    ass_set_extract_fonts(lib, fixture.extract_fonts);

    /* An event walk runs over a track read, and a renderer made, before it. */
    ASS_Track *walked = NULL;
    ASS_Renderer *renderer = NULL;
    if (mode == ASSM_EVENTS) {
        AssWork discard = {0, 0, 0, NULL};
        walked = stream ? assr_stream(&discard, lib, &fixture)
                        : ass_read_memory(lib, data, (size_t)size, NULL);
        renderer = ass_renderer_init(lib);
        if (!renderer || (stream && !walked)) {
            fprintf(stderr, "%s: out of memory\n", argv[2]);
            return 2;
        }
        ass_set_fonts(renderer, NULL, NULL, ASS_FONTPROVIDER_NONE, NULL, 0);
        if (walked) {
            ass_lazy_track_init(lib, walked);
            renderer->track = walked;
        }
    }
    int status = 0;
    if (dump) {
        AssWork work = {0, 0, 0, stdout};
        if (!assr_operation(&work, mode, lib, data, (size_t)size, &fixture, renderer, walked))
            status = 2;
        if (assr_host_dependent) fprintf(stderr, "%s: host-dependent\n", argv[2]);
    } else {
        if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18) status = 2;
        uint64_t start = assr_now_ns(), packets = 0, bytes = 0, digest = 0;
        for (unsigned long i = 0; i < iterations && !status; i++) {
            AssWork work = {0, 0, 0, NULL};
            if (!assr_operation(&work, mode, lib, data, (size_t)size, &fixture, renderer, walked))
                status = 2;
            packets += work.packets;
            bytes += work.bytes;
            digest = digest * 31 + work.digest;
        }
        uint64_t elapsed = bench ? assr_now_ns() - start : 0;
        if (memory && !status && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16) status = 2;
        if (!status)
            printf("{\"schema\":1,\"operation\":\"ass-parse\",\"iterations\":%lu,\"packets\":%" PRIu64
                   ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 ",\"elapsed_ns\":%" PRIu64
                   "%s,\"eof\":true}\n",
                   iterations, packets, bytes, digest, elapsed,
                   assr_host_dependent ? ",\"host_dependent\":true" : "");
    }
    fflush(stdout);
    if (status) fprintf(stderr, "%s: out of memory\n", argv[2]);
    free(assr_glyphs);
    if (renderer) ass_renderer_done(renderer);
    if (walked) ass_free_track(walked);
    assr_free_fixture(&fixture);
    ass_library_done(lib);
    free(data);
    return status;
}
