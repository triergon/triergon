/*
 * Rasterising reference: the libass build of harness/reference.c (tooling only;
 * never linked into the library). It includes libass's ass_render.c from the
 * pinned source tree to reach the static glyph, quantisation, composition and
 * clipping functions, and links the built libass.a for everything else. It is
 * compiled with the flags libass compiled itself with (libass-internal.pc), so
 * floating-point evaluation matches the library.
 *
 * Usage:
 *   reference OPERATION-fold   FILE MODE...
 *   reference OPERATION-bench  FILE ITERATIONS MODE...
 *   reference OPERATION-memory FILE MODE...
 *   reference OPERATION-dump   FILE MODE...
 *
 * OPERATION selects libass's bitmap engine:
 *   raster       the plain C engine (ass_bitmap_engine_init(ASS_CPU_FLAG_NONE)): C
 *                functions, 16x16 tiles and 16-byte rows, on every host. It is what
 *                ass_renderer_init gives a 32-bit ARMv7 build, which has no assembly,
 *                and does not depend on the host, so it is the gated reference.
 *   raster-simd  the engine ass_renderer_init chooses for the host (NEON on AArch64,
 *                AVX2, SSSE3 or SSE2 on x86), measured on demand where it departs
 *                from the C engine.
 *
 * MODE is one of:
 *   glyphs HEIGHT COUNT CASE...  FILE is a font: COUNT glyph ids spread over face 0
 *                                (the k-th is k * glyphs / COUNT, or every glyph
 *                                when there are fewer) are loaded as libass loads
 *                                them with hinting off: GDI face metrics, a
 *                                REAL_DIM size request of the 26.6 pixel HEIGHT and
 *                                FT_LOAD_NO_BITMAP | FT_LOAD_IGNORE_GLOBAL_ADVANCE_WIDTH
 *                                | FT_LOAD_IGNORE_TRANSFORM | FT_LOAD_NO_HINTING;
 *                                each CASE argument is one case, `g K` naming the
 *                                k-th glyph.
 *   lines                        FILE holds one case per line.
 *
 * The case grammar is crates/raster-worker/src/case.rs's. Each case is performed
 * with libass's own stages: ass_outline_convert, ass_outline_rotate_90,
 * ass_outline_stroke, ass_outline_transform_2d and _3d, ass_outline_to_bitmap,
 * get_bitmap_glyph and ass_composite_construct for a run, ass_synth_blur, the
 * engine's add, mul and imul functions, ass_fix_outline, ass_shift_bitmap and
 * render_glyph's clipping. Each record starts with a tag and folds as
 * crates/raster-worker/src/record.rs folds it; `packets` counts cases and `bytes`
 * outline points and bitmap pixels recorded. The file is read, the glyphs loaded
 * and the cases parsed before the operation; the renderer is created once,
 * outside it. `-dump` prints the same records as lines, identical to
 * raster-worker's.
 */
#include "ass_render.c"

#include FT_TRUETYPE_TABLES_H
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define RASTER_ABSENT UINT64_MAX
#define RASTER_FNV_INIT 0xcbf29ce484222325ULL
#define RASTER_MAX_INPUT (64L << 20)
#define RASTER_MAX_SIDE 4096
#define RASTER_MAX_POINTS 65535
#define RASTER_MAX_CLIP (1LL << 24)
#define RASTER_MAX_OFFSET (1LL << 30)
#define RASTER_MAX_RUN 8
#define RASTER_MAX_GLYPHS 4096

enum {
    RASTER_CONVERT = 1,
    RASTER_ROTATE,
    RASTER_STROKE,
    RASTER_TRANSFORM,
    RASTER_RASTER,
    RASTER_RUN,
    RASTER_BLUR,
    RASTER_ADD,
    RASTER_MUL,
    RASTER_IMUL,
    RASTER_FIX,
    RASTER_SHIFT,
    RASTER_CLIP,
    RASTER_UNPARSED,
    RASTER_GLYPHS = 20,
    RASTER_GLYPH,
};

/* Status values: invalid source or parameters, done, failed. */
enum { RASTER_INVALID, RASTER_OK, RASTER_FAILED };

static const char *raster_name(int tag) {
    static const char *const names[] = {"",    "convert", "rotate", "stroke", "transform",
                                        "raster", "run",  "blur",   "add",    "mul",
                                        "imul",   "fix",  "shift",  "clip",   "unparsed"};
    if (tag == RASTER_GLYPHS) return "glyphs";
    if (tag == RASTER_GLYPH) return "glyph";
    return names[tag];
}

/* ---------------------------------------------------------------- records */

typedef struct {
    uint64_t packets, bytes, digest;
    FILE *dump; /* -dump: print lines instead of folding */
} RasterWork;

static void raster_fold(RasterWork *w, uint64_t v) { w->digest = w->digest * 31 + v; }

static void raster_begin(RasterWork *w, int tag) {
    if (w->dump) {
        fputs(raster_name(tag), w->dump);
        return;
    }
    if (tag < RASTER_GLYPHS) w->packets++;
    raster_fold(w, (uint64_t)tag);
}

static void raster_value(RasterWork *w, int64_t v) {
    if (w->dump)
        fprintf(w->dump, " %" PRId64, v);
    else
        raster_fold(w, (uint64_t)v);
}

static void raster_absent(RasterWork *w) {
    if (w->dump)
        fputs(" -", w->dump);
    else
        raster_fold(w, RASTER_ABSENT);
}

static void raster_end(RasterWork *w) {
    if (w->dump) fputc('\n', w->dump);
}

static uint64_t raster_fnv(uint64_t h, const void *data, size_t n) {
    const unsigned char *p = data;
    for (size_t i = 0; i < n; i++) {
        h ^= p[i];
        h *= 0x100000001b3ULL;
    }
    return h;
}

/* Points and segments and their hash: each point's x and y as little-endian
 * 32-bit values, then the segment bytes as libass stores them. */
static void raster_outline(RasterWork *w, const ASS_Outline *o) {
    uint64_t h = RASTER_FNV_INIT;
    for (size_t i = 0; i < o->n_points; i++) {
        uint32_t x = (uint32_t)o->points[i].x, y = (uint32_t)o->points[i].y;
        unsigned char bytes[8] = {x, x >> 8, x >> 16, x >> 24, y, y >> 8, y >> 16, y >> 24};
        h = raster_fnv(h, bytes, sizeof bytes);
    }
    h = raster_fnv(h, o->segments, o->n_segments);
    w->bytes += o->n_points;
    raster_value(w, (int64_t)o->n_points);
    raster_value(w, (int64_t)o->n_segments);
    raster_value(w, (int64_t)h);
}

/* Position, size, stride and the hash of every row over the whole stride, the
 * padding included, or absent. */
static void raster_bitmap(RasterWork *w, const Bitmap *bm) {
    if (!bm || !bm->buffer) {
        raster_absent(w);
        return;
    }
    uint64_t h = raster_fnv(RASTER_FNV_INIT, bm->buffer, (size_t)bm->stride * (size_t)bm->h);
    w->bytes += (uint64_t)bm->w * (uint64_t)bm->h;
    raster_value(w, bm->left);
    raster_value(w, bm->top);
    raster_value(w, bm->w);
    raster_value(w, bm->h);
    raster_value(w, bm->stride);
    raster_value(w, (int64_t)h);
}

/* ---------------------------------------------------------------- grammar */

/* Tokens separated by single spaces; an empty token is a token. */
typedef struct {
    const char *at, *end;
    int done, error;
} RasterTokens;

static int raster_token(RasterTokens *t, const char **token, size_t *n) {
    if (t->done) {
        t->error = 1;
        return 0;
    }
    const char *space = memchr(t->at, ' ', (size_t)(t->end - t->at));
    *token = t->at;
    if (space) {
        *n = (size_t)(space - t->at);
        t->at = space + 1;
    } else {
        *n = (size_t)(t->end - t->at);
        t->at = t->end;
        t->done = 1;
    }
    return 1;
}

/* A decimal integer, optionally negative, of one to 19 digits, within [min, max]. */
static int raster_int(RasterTokens *t, long long min, long long max, long long *out) {
    const char *token;
    size_t n;
    if (!raster_token(t, &token, &n)) return 0;
    int negative = n > 0 && token[0] == '-';
    size_t digits = n - (size_t)negative;
    if (digits == 0 || digits > 19) return t->error = 1, 0;
    unsigned long long value = 0;
    for (size_t i = (size_t)negative; i < n; i++) {
        if (token[i] < '0' || token[i] > '9') return t->error = 1, 0;
        unsigned d = (unsigned)(token[i] - '0');
        if (value > (ULLONG_MAX - d) / 10) return t->error = 1, 0;
        value = value * 10 + d;
    }
    long long v;
    if (negative) {
        if (value > (unsigned long long)LLONG_MAX + 1) return t->error = 1, 0;
        v = value == (unsigned long long)LLONG_MAX + 1 ? LLONG_MIN : -(long long)value;
    } else {
        if (value > (unsigned long long)LLONG_MAX) return t->error = 1, 0;
        v = (long long)value;
    }
    if (v < min || v > max) return t->error = 1, 0;
    *out = v;
    return 1;
}

static int raster_i32(RasterTokens *t, int32_t *out) {
    long long v;
    if (!raster_int(t, INT32_MIN, INT32_MAX, &v)) return 0;
    *out = (int32_t)v;
    return 1;
}

/* A double as the 16 lower-case hex digits of its bits. */
static int raster_real(RasterTokens *t, double *out) {
    const char *token;
    size_t n;
    if (!raster_token(t, &token, &n)) return 0;
    if (n != 16) return t->error = 1, 0;
    uint64_t bits = 0;
    for (size_t i = 0; i < 16; i++) {
        char c = token[i];
        int d = c >= '0' && c <= '9' ? c - '0' : c >= 'a' && c <= 'f' ? c - 'a' + 10 : -1;
        if (d < 0) return t->error = 1, 0;
        bits = bits << 4 | (uint64_t)d;
    }
    memcpy(out, &bits, sizeof *out);
    return 1;
}

typedef struct {
    int glyph; /* the selection index, or -1 inline */
    FT_Outline ft;
} RasterSource;

typedef struct {
    double reals[14];
    int32_t ints[8];
    RasterSource source;
} RasterRunGlyph;

typedef struct {
    int kind;
    RasterSource sources[2];
    int32_t ints[19];
    long long seed;
    int perspective;
    double m[3][3];
    double reals[8];
    int n_glyphs;
    RasterRunGlyph glyphs[RASTER_MAX_RUN];
} RasterCase;

static void raster_source_free(RasterSource *s) {
    free(s->ft.points);
    free(s->ft.tags);
    free(s->ft.contours);
    memset(s, 0, sizeof *s);
}

static int raster_source(RasterTokens *t, RasterSource *s) {
    long long v;
    memset(s, 0, sizeof *s);
    s->glyph = -1;
    const char *token;
    size_t n;
    if (!raster_token(t, &token, &n)) return 0;
    if (n == 1 && token[0] == 'g') {
        if (!raster_int(t, 0, RASTER_MAX_POINTS, &v)) return 0;
        s->glyph = (int)v;
        return 1;
    }
    if (!(n == 1 && token[0] == 'O')) return t->error = 1, 0;
    long long contours, points;
    if (!raster_int(t, 0, RASTER_MAX_POINTS, &contours)) return 0;
    s->ft.contours = malloc((size_t)(contours ? contours : 1) * sizeof *s->ft.contours);
    if (!s->ft.contours) return t->error = 1, 0;
    s->ft.n_contours = (unsigned short)contours;
    for (long long i = 0; i < contours; i++) {
        if (!raster_int(t, 0, RASTER_MAX_POINTS, &v)) return 0;
        s->ft.contours[i] = (unsigned short)(v - 1); /* exclusive end; 0 wraps to 65535 */
    }
    if (!raster_int(t, 0, RASTER_MAX_POINTS, &points)) return 0;
    s->ft.points = malloc((size_t)(points ? points : 1) * sizeof *s->ft.points);
    s->ft.tags = malloc((size_t)(points ? points : 1));
    if (!s->ft.points || !s->ft.tags) return t->error = 1, 0;
    s->ft.n_points = (unsigned short)points;
    for (long long i = 0; i < points; i++) {
        int32_t x, y;
        long long kind;
        if (!raster_i32(t, &x) || !raster_i32(t, &y) || !raster_int(t, 0, 2, &kind)) return 0;
        s->ft.points[i].x = x;
        s->ft.points[i].y = y;
        /* 0 quadratic (FT_CURVE_TAG_CONIC), 1 on, 2 cubic: FreeType's own values */
        s->ft.tags[i] = (unsigned char)kind;
    }
    return 1;
}

static int raster_matrix(RasterTokens *t, double m[3][3]) {
    for (int i = 0; i < 3; i++)
        for (int j = 0; j < 3; j++)
            if (!raster_real(t, &m[i][j])) return 0;
    return 1;
}

static int raster_projection(RasterTokens *t, int *perspective) {
    const char *token;
    size_t n;
    if (!raster_token(t, &token, &n) || n != 1 || (token[0] != 'a' && token[0] != 'p'))
        return t->error = 1, 0;
    *perspective = token[0] == 'p';
    return 1;
}

static int raster_ints(RasterTokens *t, int32_t *out, int count, long long min, long long max) {
    for (int i = 0; i < count; i++) {
        long long v;
        if (!raster_int(t, min, max, &v)) return 0;
        out[i] = (int32_t)v;
    }
    return 1;
}

/* One case; 0 outside the grammar. */
static int raster_parse(const char *line, size_t n, RasterCase *c) {
    static const char *const kinds[] = {"",     "convert", "rotate", "stroke", "transform",
                                        "raster", "run",   "blur",   "add",    "mul",
                                        "imul",   "fix",   "shift",  "clip"};
    RasterTokens t = {line, line + n, 0, 0};
    memset(c, 0, sizeof *c);
    c->sources[0].glyph = c->sources[1].glyph = -1;
    const char *token;
    size_t len;
    if (!raster_token(&t, &token, &len)) return 0;
    for (c->kind = RASTER_CONVERT; c->kind <= RASTER_CLIP; c->kind++)
        if (len == strlen(kinds[c->kind]) && !memcmp(token, kinds[c->kind], len)) break;
    long long v;
    const long long side = RASTER_MAX_SIDE;
    int32_t *ints = c->ints;
    switch (c->kind) {
    case RASTER_CONVERT:
        raster_source(&t, &c->sources[0]);
        break;
    case RASTER_ROTATE:
        if (raster_ints(&t, ints, 2, -RASTER_MAX_OFFSET, RASTER_MAX_OFFSET))
            raster_source(&t, &c->sources[0]);
        break;
    case RASTER_STROKE:
        if (raster_ints(&t, ints, 3, INT32_MIN, INT32_MAX)) raster_source(&t, &c->sources[0]);
        break;
    case RASTER_TRANSFORM:
        if (raster_projection(&t, &c->perspective) && raster_matrix(&t, c->m))
            raster_source(&t, &c->sources[0]);
        break;
    case RASTER_RASTER:
        if (raster_projection(&t, &c->perspective) && raster_matrix(&t, c->m) &&
            raster_source(&t, &c->sources[0]))
            raster_source(&t, &c->sources[1]);
        break;
    case RASTER_RUN:
        if (!raster_int(&t, 0, 63, &v)) break;
        ints[0] = (int32_t)v;
        if (!raster_int(&t, 0, 255, &v)) break;
        ints[1] = (int32_t)v;
        for (int i = 0; i < 8; i++)
            if (!raster_real(&t, &c->reals[i])) break;
        if (t.error || !raster_int(&t, 1, RASTER_MAX_RUN, &v)) break;
        c->n_glyphs = (int)v;
        for (int g = 0; g < c->n_glyphs && !t.error; g++) {
            RasterRunGlyph *glyph = &c->glyphs[g];
            glyph->source.glyph = -1;
            for (int i = 0; i < 14 && !t.error; i++) raster_real(&t, &glyph->reals[i]);
            if (!t.error && raster_ints(&t, glyph->ints, 8, INT32_MIN, INT32_MAX))
                raster_source(&t, &glyph->source);
        }
        break;
    case RASTER_BLUR:
        if (raster_ints(&t, ints, 2, 1, side) && raster_int(&t, LLONG_MIN, LLONG_MAX, &c->seed) &&
            raster_real(&t, &c->reals[0]) && raster_real(&t, &c->reals[1]))
            raster_ints(&t, ints + 2, 1, 0, 255);
        break;
    case RASTER_ADD:
        if (raster_int(&t, LLONG_MIN, LLONG_MAX, &c->seed) && raster_ints(&t, ints, 4, 1, side)) {
            if (ints[2] > ints[0] || ints[3] > ints[1]) {
                t.error = 1;
                break;
            }
            if (raster_ints(&t, ints + 4, 1, 0, ints[0] - ints[2]))
                raster_ints(&t, ints + 5, 1, 0, ints[1] - ints[3]);
        }
        break;
    case RASTER_MUL:
    case RASTER_IMUL:
        if (raster_int(&t, LLONG_MIN, LLONG_MAX, &c->seed)) raster_ints(&t, ints, 2, 1, side);
        break;
    case RASTER_FIX:
        if (raster_int(&t, LLONG_MIN, LLONG_MAX, &c->seed) && raster_ints(&t, ints, 2, 1, side) &&
            raster_ints(&t, ints + 2, 2, INT32_MIN, INT32_MAX) && raster_ints(&t, ints + 4, 2, 1, side))
            raster_ints(&t, ints + 6, 2, INT32_MIN, INT32_MAX);
        break;
    case RASTER_SHIFT:
        if (raster_int(&t, LLONG_MIN, LLONG_MAX, &c->seed) && raster_ints(&t, ints, 2, 1, side))
            raster_ints(&t, ints + 2, 2, 0, 63);
        break;
    case RASTER_CLIP:
        if (raster_int(&t, LLONG_MIN, LLONG_MAX, &c->seed) && raster_ints(&t, ints, 2, 1, side))
            for (int i = 0; i < 16 && !t.error; i++)
                raster_ints(&t, ints + 2 + i, 1, i == 9 ? 0 : -RASTER_MAX_CLIP,
                            i == 9 ? 1 : RASTER_MAX_CLIP);
        break;
    default:
        return 0;
    }
    return !t.error && t.done;
}

static void raster_case_free(RasterCase *c) {
    raster_source_free(&c->sources[0]);
    raster_source_free(&c->sources[1]);
    for (int g = 0; g < c->n_glyphs; g++) raster_source_free(&c->glyphs[g].source);
}

/* ------------------------------------------------------------ the renderer */

static ASS_Library *raster_library;
static ASS_Renderer *raster_renderer;
static ASS_Track *raster_track;

/* The selected glyphs of a font case: each id and its loaded FreeType outline. */
typedef struct {
    long id;
    int loaded;
    FT_Outline ft;
} RasterGlyph;

static RasterGlyph *raster_glyphs;
static long raster_glyph_count = -1; /* -1: no font case, or the face or size failed */

static void raster_quiet(int level, const char *fmt, va_list args, void *data) {
    (void)level, (void)fmt, (void)args, (void)data;
}

/* Glyph outlines in an outline cache of their own, so they carry the reference
 * counts libass's bitmap and border keys take. */
typedef struct {
    uint64_t id;
} RasterSourceKey;

static const ASS_Outline *raster_staging;

static ass_hashcode raster_source_hash(void *key, ass_hashcode hval) {
    return raster_fnv(hval, key, sizeof(RasterSourceKey));
}

static bool raster_source_compare(void *a, void *b) {
    return ((RasterSourceKey *)a)->id == ((RasterSourceKey *)b)->id;
}

static bool raster_source_key_move(void *dst, void *src) {
    if (dst) memcpy(dst, src, sizeof(RasterSourceKey));
    return true;
}

static size_t raster_source_construct(void *key, void *value, void *priv) {
    (void)key, (void)priv;
    OutlineHashValue *v = value;
    memset(v, 0, sizeof *v);
    if (raster_staging->n_points) {
        if (!ass_outline_alloc(&v->outline[0], raster_staging->n_points, raster_staging->n_segments))
            return 1;
        memcpy(v->outline[0].points, raster_staging->points,
               raster_staging->n_points * sizeof(ASS_Vector));
        memcpy(v->outline[0].segments, raster_staging->segments, raster_staging->n_segments);
        v->outline[0].n_points = raster_staging->n_points;
        v->outline[0].n_segments = raster_staging->n_segments;
    }
    rectangle_reset(&v->cbox);
    ass_outline_update_cbox(&v->outline[0], &v->cbox);
    if (v->cbox.x_min > v->cbox.x_max || v->cbox.y_min > v->cbox.y_max)
        v->cbox.x_min = v->cbox.y_min = v->cbox.x_max = v->cbox.y_max = 0;
    v->valid = true;
    return 1;
}

static void raster_source_destruct(void *key, void *value) {
    (void)key;
    OutlineHashValue *v = value;
    ass_outline_free(&v->outline[0]);
    ass_outline_free(&v->outline[1]);
}

static const CacheDesc raster_source_desc = {
    .hash_func = raster_source_hash,
    .compare_func = raster_source_compare,
    .key_move_func = raster_source_key_move,
    .construct_func = raster_source_construct,
    .destruct_func = raster_source_destruct,
    .key_size = sizeof(RasterSourceKey),
    .value_size = sizeof(OutlineHashValue),
};

static Cache *raster_sources;
static uint64_t raster_source_ids;

/* The renderer, with the C engine unless `host_engine`. ass_renderer_init picks the
 * host's SIMD engine (AVX2's 32-byte rows change \be at a bitmap's right edge), so
 * the C engine replaces it, and the rasteriser state is rebuilt for it. The C
 * engine must have 16x16 tiles and 16-byte rows, as an ARMv7 build has. */
static int raster_setup(int host_engine) {
    raster_library = ass_library_init();
    if (!raster_library) return 0;
    ass_set_message_cb(raster_library, raster_quiet, NULL);
    raster_renderer = ass_renderer_init(raster_library);
    raster_sources = ass_cache_create(&raster_source_desc);
    raster_track = ass_new_track(raster_library);
    if (!raster_renderer || !raster_sources || !raster_track) return 0;
    if (!host_engine) {
        raster_renderer->engine = ass_bitmap_engine_init(ASS_CPU_FLAG_NONE);
        if (raster_renderer->engine.tile_order != 4 || raster_renderer->engine.align_order != 4) {
            fprintf(stderr, "the C engine is not 16x16 tiles with 16-byte rows\n");
            return 0;
        }
        RasterizerData *rasterizer = &raster_renderer->state.rasterizer;
        ass_rasterizer_done(rasterizer);
        if (!ass_rasterizer_init(&raster_renderer->engine, rasterizer, RASTERIZER_PRECISION))
            return 0;
    }
    raster_track->PlayResX = raster_track->PlayResY = 1;
    raster_renderer->track = raster_track;
    return 1;
}

/* The FreeType outline of a source, or NULL for a glyph that is not loaded. */
static const FT_Outline *raster_ft(const RasterSource *s) {
    if (s->glyph < 0) return &s->ft;
    if (s->glyph >= raster_glyph_count || !raster_glyphs[s->glyph].loaded) return NULL;
    return &raster_glyphs[s->glyph].ft;
}

/* Converts a source as ass_get_glyph_outline does. */
static bool raster_convert(const RasterSource *s, ASS_Outline *out) {
    ass_outline_clear(out);
    const FT_Outline *ft = raster_ft(s);
    if (!ft) return false;
    if (!ft->n_points) return true;
    if (!ass_outline_alloc(out, 2 * ft->n_points, ft->n_points)) return false;
    if (!ass_outline_convert(out, ft)) {
        ass_outline_free(out);
        return false;
    }
    return true;
}

/* ------------------------------------------------------------- the cases */

typedef struct {
    uint64_t state;
} RasterRng;

static RasterRng raster_seeded(long long seed) {
    RasterRng r = {((uint64_t)seed * 0x9E3779B97F4A7C15ULL) | 1};
    return r;
}

static uint64_t raster_next(RasterRng *r) {
    r->state ^= r->state << 13;
    r->state ^= r->state >> 7;
    r->state ^= r->state << 17;
    return r->state;
}

/* Runs of empty, full and arbitrary coverage. */
static bool raster_random_bitmap(RasterRng *r, Bitmap *bm, int32_t w, int32_t h) {
    if (!ass_alloc_bitmap(&raster_renderer->engine, bm, w, h, true)) return false;
    bm->left = bm->top = 0;
    uint8_t value = 0;
    for (int32_t y = 0; y < h; y++)
        for (int32_t x = 0; x < w; x++) {
            uint64_t v = raster_next(r);
            if (v % 5 == 0) {
                unsigned mode = (v >> 8) % 3;
                value = mode == 0 ? 0 : mode == 1 ? 255 : (uint8_t)(v >> 16);
            }
            bm->buffer[y * bm->stride + x] = value;
        }
    return true;
}

static bool raster_transform(const RasterCase *c, const ASS_Outline *in, ASS_Outline *out) {
    if (c->perspective) return ass_outline_transform_3d(out, in, c->m);
    return ass_outline_transform_2d(out, in, (const double(*)[3])c->m);
}

static void raster_outline_case(const RasterCase *c, RasterWork *w) {
    ASS_Outline o, r0, r1;
    if (!raster_convert(&c->sources[0], &o)) {
        raster_value(w, RASTER_INVALID);
        return;
    }
    switch (c->kind) {
    case RASTER_CONVERT:
        raster_value(w, RASTER_OK);
        raster_outline(w, &o);
        break;
    case RASTER_ROTATE: {
        ASS_Vector offset = {c->ints[0], c->ints[1]};
        if (ass_outline_rotate_90(&o, offset)) {
            raster_value(w, RASTER_OK);
            raster_outline(w, &o);
        } else {
            raster_value(w, RASTER_FAILED);
        }
        break;
    }
    case RASTER_STROKE: {
        int bx = c->ints[0], by = c->ints[1], eps = c->ints[2];
        int radius = FFMAX(bx, by);
        if (eps < 1 || radius < eps || radius > OUTLINE_MAX) {
            raster_value(w, RASTER_INVALID);
            break;
        }
        /* libass strokes only outlines with points. */
        ass_outline_clear(&r0);
        ass_outline_clear(&r1);
        if (!o.n_points || ass_outline_stroke(&r0, &r1, &o, bx, by, eps)) {
            raster_value(w, RASTER_OK);
            raster_outline(w, &r0);
            raster_outline(w, &r1);
        } else {
            raster_value(w, RASTER_FAILED);
        }
        ass_outline_free(&r0);
        ass_outline_free(&r1);
        break;
    }
    case RASTER_TRANSFORM:
        if (raster_transform(c, &o, &r0)) {
            raster_value(w, RASTER_OK);
            raster_outline(w, &r0);
            ass_outline_free(&r0);
        } else {
            raster_value(w, RASTER_FAILED);
        }
        break;
    }
    ass_outline_free(&o);
}

static void raster_raster(const RasterCase *c, RasterWork *w) {
    ASS_Outline o[2], r[2];
    for (int i = 0; i < 2; i++) {
        if (!raster_convert(&c->sources[i], &o[i])) {
            raster_value(w, RASTER_INVALID);
            if (i) {
                ass_outline_free(&o[0]);
                ass_outline_free(&r[0]);
            }
            return;
        }
        raster_transform(c, &o[i], &r[i]); /* a failure leaves the outline empty */
    }
    Bitmap bm;
    if (!ass_outline_to_bitmap(&raster_renderer->state, &bm, &r[0], &r[1])) memset(&bm, 0, sizeof bm);
    raster_value(w, RASTER_OK);
    raster_bitmap(w, &bm);
    ass_free_bitmap(&bm);
    for (int i = 0; i < 2; i++) {
        ass_outline_free(&o[i]);
        ass_outline_free(&r[i]);
    }
}

static void raster_bitmap_case(const RasterCase *c, RasterWork *w) {
    RasterRng r = raster_seeded(c->seed);
    const BitmapEngine *e = &raster_renderer->engine;
    const int32_t *v = c->ints;
    Bitmap a = {0}, b = {0};
    /* The sizes are within the grammar's bounds, so only memory can fail. */
    int ok = 1;
    switch (c->kind) {
    case RASTER_BLUR:
        if ((ok = raster_random_bitmap(&r, &a, v[0], v[1])))
            ass_synth_blur(e, &a, v[2], c->reals[0], c->reals[1]);
        break;
    case RASTER_ADD:
        ok = raster_random_bitmap(&r, &a, v[0], v[1]) && raster_random_bitmap(&r, &b, v[2], v[3]);
        if (ok)
            e->add_bitmaps(a.buffer + v[5] * a.stride + v[4], a.stride, b.buffer, b.stride, b.w,
                           b.h);
        break;
    case RASTER_MUL: {
        Bitmap d = {0};
        ok = raster_random_bitmap(&r, &a, v[0], v[1]) && raster_random_bitmap(&r, &b, v[0], v[1]) &&
             ass_alloc_bitmap(e, &d, a.w, a.h, true);
        if (!ok) break;
        d.left = d.top = 0;
        e->mul_bitmaps(d.buffer, d.stride, a.buffer, a.stride, b.buffer, b.stride, a.w, a.h);
        ass_free_bitmap(&a);
        a = d;
        break;
    }
    case RASTER_IMUL:
        ok = raster_random_bitmap(&r, &a, v[0], v[1]) && raster_random_bitmap(&r, &b, v[0], v[1]);
        if (ok) e->imul_bitmaps(a.buffer, a.stride, b.buffer, b.stride, a.w, a.h);
        break;
    case RASTER_FIX:
        ok = raster_random_bitmap(&r, &b, v[0], v[1]) && raster_random_bitmap(&r, &a, v[4], v[5]);
        if (!ok) break;
        b.left = v[2];
        b.top = v[3];
        a.left = v[6];
        a.top = v[7];
        ass_fix_outline(&b, &a);
        break;
    default: /* RASTER_SHIFT */
        if ((ok = raster_random_bitmap(&r, &a, v[0], v[1]))) ass_shift_bitmap(&a, v[2], v[3]);
        break;
    }
    if (ok) {
        raster_value(w, RASTER_OK);
        raster_bitmap(w, &a);
    } else {
        raster_value(w, RASTER_FAILED);
    }
    ass_free_bitmap(&a);
    ass_free_bitmap(&b);
}

/* The clip state of one case, reset afterwards so no case sees another's. */
static void raster_set_clip(const int32_t *v) {
    RenderContext *state = &raster_renderer->state;
    state->clip_x0 = v ? v[7] : 0;
    state->clip_y0 = v ? v[8] : 0;
    state->clip_x1 = v ? v[9] : 0;
    state->clip_y1 = v ? v[10] : 0;
    state->clip_mode = v ? v[11] : 0;
    raster_renderer->width = v ? v[12] : 0;
    raster_renderer->height = v ? v[13] : 0;
    raster_renderer->settings.left_margin = v ? v[14] : 0;
    raster_renderer->settings.top_margin = v ? v[15] : 0;
    raster_renderer->frame_content_width = v ? v[16] - v[14] : 0;
    raster_renderer->frame_content_height = v ? v[17] - v[15] : 0;
}

/* render_glyph over a random bitmap held, as libass holds every bitmap it clips, by
 * a composite of the composite cache; the images are released with libass's own
 * routine, which drops their references to it. */
static void raster_clip(const RasterCase *c, RasterWork *w) {
    const int32_t *v = c->ints;
    RasterRng r = raster_seeded(c->seed);
    CompositeHashKey key = {{0}, 0, NULL};
    CompositeHashValue *source = ass_cache_get(raster_renderer->cache.composite_cache, &key,
                                               raster_renderer);
    if (!source || !raster_random_bitmap(&r, &source->bm, v[0], v[1])) {
        raster_value(w, RASTER_FAILED);
        ass_cache_empty(raster_renderer->cache.composite_cache);
        return;
    }
    Bitmap *bm = &source->bm;
    bm->left = v[2];
    bm->top = v[3];
    raster_set_clip(v);
    ASS_Image *head = NULL;
    /* render_glyph leaves the last image's next unset and returns where it is;
     * libass terminates the list (ass_render_frame's *tail = NULL). */
    ASS_Image **tail = render_glyph(&raster_renderer->state, bm, v[4], v[5], 1, 2, v[6], &head,
                                    IMAGE_TYPE_CHARACTER, source);
    *tail = NULL;
    raster_set_clip(NULL);
    int64_t count = 0;
    for (ASS_Image *img = head; img; img = img->next) count++;
    raster_value(w, RASTER_OK);
    raster_value(w, count);
    for (ASS_Image *img = head; img; img = img->next) {
        ptrdiff_t offset = img->bitmap - bm->buffer;
        int64_t values[7] = {img->dst_x, img->dst_y, offset % bm->stride, offset / bm->stride,
                             img->w,     img->h,     img->color};
        for (int i = 0; i < 7; i++) raster_value(w, values[i]);
    }
    ass_frame_ref(head);
    ass_frame_unref(head);
    ass_cache_empty(raster_renderer->cache.composite_cache);
}

static void raster_run(const RasterCase *c, RasterWork *w) {
    RenderContext *state = &raster_renderer->state;
    int flags = c->ints[0] & 31;
    int karaoke = c->ints[0] & 32;
    state->blur_scale_x = c->reals[3];
    state->blur_scale_y = c->reals[4];
    state->border_scale_x = c->reals[5];
    state->border_scale_y = c->reals[6];
    raster_renderer->par_scale_x = c->reals[7];

    FilterDesc filter = {.flags = flags, .be = c->ints[1]};
    int32_t mask_x, mask_y;
    double blur_radius_scale = 2 / sqrt(log(256));
    double blur_scale_x = state->blur_scale_x * blur_radius_scale;
    double blur_scale_y = state->blur_scale_y * blur_radius_scale;
    filter.blur_x = quantize_blur(c->reals[0] * blur_scale_x, &mask_x);
    filter.blur_y = quantize_blur(c->reals[0] * blur_scale_y, &mask_y);
    if (flags & FILTER_NONZERO_SHADOW) {
        int32_t x = double_to_d6(c->reals[1] * state->border_scale_x);
        int32_t y = double_to_d6(c->reals[2] * state->border_scale_y);
        filter.shadow.x = (x + (mask_x >> 1)) & ~mask_x;
        filter.shadow.y = (y + (mask_y >> 1)) & ~mask_y;
    }

    BitmapRef refs[RASTER_MAX_RUN];
    size_t count = 0;
    int32_t min_x = INT_MAX, min_y = INT_MAX;
    int32_t leftmost = OUTLINE_MAX;
    ASS_DVector offset = {0, 0};
    for (int g = 0; g < c->n_glyphs; g++) {
        const double *gr = c->glyphs[g].reals;
        const int32_t *gi = c->glyphs[g].ints;
        ASS_Outline outline;
        if (!raster_convert(&c->glyphs[g].source, &outline)) {
            raster_value(w, RASTER_INVALID);
            continue;
        }
        raster_staging = &outline;
        RasterSourceKey key = {++raster_source_ids};
        OutlineHashValue *value = ass_cache_get(raster_sources, &key, NULL);
        ass_outline_free(&outline);

        GlyphInfo info;
        memset(&info, 0, sizeof info);
        info.outline = value;
        info.symbol = 'a';
        info.frx = gr[0];
        info.fry = gr[1];
        info.frz = gr[2];
        info.fax = gr[3];
        info.fay = gr[4];
        info.scale_x = gr[5];
        info.scale_y = gr[6];
        info.transform.scale.x = gr[7];
        info.transform.scale.y = gr[8];
        info.transform.offset.x = gr[9];
        info.transform.offset.y = gr[10];
        info.border_x = gr[11];
        info.border_y = gr[12];
        info.scale_fix = gr[13];
        info.shift.x = gi[0];
        info.shift.y = gi[1];
        info.asc = gi[2];
        info.desc = gi[3];
        info.pos.x = gi[4];
        info.pos.y = gi[5];
        info.hspacing_scaled = gi[6];
        info.advance.x = gi[7];
        info.effect_type = karaoke ? EF_KARAOKE_KF : EF_NONE;
        int gflags = flags & (FILTER_BORDER_STYLE_3 | FILTER_NONZERO_BORDER | FILTER_NONZERO_SHADOW);

        ASS_Vector pos = {0, 0}, pos_o = {0, 0};
        get_bitmap_glyph(state, &info, &leftmost, &pos, &pos_o, &offset, !count, gflags);
        raster_value(w, RASTER_OK);
        raster_bitmap(w, info.bm);
        raster_value(w, info.bm ? pos.x : 0);
        raster_value(w, info.bm ? pos.y : 0);
        raster_bitmap(w, info.bm_o);
        raster_value(w, info.bm_o ? pos_o.x : 0);
        raster_value(w, info.bm_o ? pos_o.y : 0);
        if (!info.bm && !info.bm_o) continue;
        refs[count] = (BitmapRef){info.bm, info.bm_o, pos, pos_o};
        count++;
        min_x = FFMIN(min_x, pos.x);
        min_y = FFMIN(min_y, pos.y);
    }
    raster_value(w, leftmost);
    if (count) {
        for (size_t i = 0; i < count; i++) {
            refs[i].pos.x -= min_x;
            refs[i].pos.y -= min_y;
            refs[i].pos_o.x -= min_x;
            refs[i].pos_o.y -= min_y;
        }
        CompositeHashKey key = {filter, count, refs};
        CompositeHashValue value;
        ass_composite_construct(&key, &value, raster_renderer);
        raster_bitmap(w, &value.bm);
        raster_bitmap(w, &value.bm_o);
        raster_bitmap(w, &value.bm_s);
        ass_free_bitmap(&value.bm);
        ass_free_bitmap(&value.bm_o);
        ass_free_bitmap(&value.bm_s);
    } else {
        for (int i = 0; i < 3; i++) raster_absent(w);
    }
    /* ass_cache_get returns values the caller borrows: an item's references are
     * the cache queue's and those other items' keys hold. Emptying the bitmap
     * cache first destroys the bitmaps, whose keys release the border and source
     * outlines, so the later caches free those too and nothing outlives the run. */
    ass_cache_empty(raster_renderer->cache.bitmap_cache);
    ass_cache_empty(raster_renderer->cache.outline_cache);
    ass_cache_empty(raster_sources);
}

static void raster_perform(const RasterCase *c, int parsed, RasterWork *w) {
    if (!parsed) {
        raster_begin(w, RASTER_UNPARSED);
        raster_end(w);
        return;
    }
    raster_begin(w, c->kind);
    switch (c->kind) {
    case RASTER_CONVERT:
    case RASTER_ROTATE:
    case RASTER_STROKE:
    case RASTER_TRANSFORM:
        raster_outline_case(c, w);
        break;
    case RASTER_RASTER:
        raster_raster(c, w);
        break;
    case RASTER_RUN:
        raster_run(c, w);
        break;
    case RASTER_CLIP:
        raster_clip(c, w);
        break;
    default:
        raster_bitmap_case(c, w);
        break;
    }
    raster_end(w);
}

/* ------------------------------------------------------------- the glyphs */

/* libass's set_font_metrics: GDI ascent and descent where the face has them. */
static void raster_gdi_metrics(FT_Face face) {
    TT_OS2 *os2 = FT_Get_Sfnt_Table(face, FT_SFNT_OS2);
    if (os2 && ((short)os2->usWinAscent + (short)os2->usWinDescent != 0)) {
        face->ascender = (short)os2->usWinAscent;
        face->descender = -(short)os2->usWinDescent;
        face->height = face->ascender - face->descender;
    }
    if (face->ascender - face->descender == 0 || face->height == 0) {
        if (os2 && (os2->sTypoAscender - os2->sTypoDescender) != 0) {
            face->ascender = os2->sTypoAscender;
            face->descender = os2->sTypoDescender;
        } else {
            face->ascender = face->bbox.yMax;
            face->descender = face->bbox.yMin;
        }
        face->height = face->ascender - face->descender;
    }
}

static int raster_copy_outline(const FT_Outline *from, FT_Outline *to) {
    memset(to, 0, sizeof *to);
    to->points = malloc((from->n_points ? from->n_points : 1) * sizeof *to->points);
    to->tags = malloc(from->n_points ? from->n_points : 1);
    to->contours = malloc((from->n_contours ? from->n_contours : 1) * sizeof *to->contours);
    if (!to->points || !to->tags || !to->contours) return 0;
    memcpy(to->points, from->points, from->n_points * sizeof *to->points);
    memcpy(to->tags, from->tags, from->n_points);
    memcpy(to->contours, from->contours, from->n_contours * sizeof *to->contours);
    to->n_points = from->n_points;
    to->n_contours = from->n_contours;
    return 1;
}

/* Loads the selection of a font case; 0 when out of memory. */
static int raster_load(FT_Library ft, const unsigned char *data, long size, long height, long count) {
    FT_Face face;
    raster_glyph_count = -1;
    if (FT_New_Memory_Face(ft, data, size, 0, &face)) return 1;
    raster_gdi_metrics(face);
    FT_Size_RequestRec rq;
    memset(&rq, 0, sizeof rq);
    rq.type = FT_SIZE_REQUEST_TYPE_REAL_DIM;
    rq.height = height;
    if (FT_Request_Size(face, &rq) || !face->size->metrics.x_ppem || !face->size->metrics.y_ppem) {
        FT_Done_Face(face);
        return 1;
    }
    long total = face->num_glyphs;
    long n = total <= count ? total : count;
    raster_glyphs = calloc((size_t)(n ? n : 1), sizeof *raster_glyphs);
    if (!raster_glyphs) return 0;
    for (long k = 0; k < n; k++) {
        RasterGlyph *g = &raster_glyphs[k];
        g->id = total <= count ? k : (long)(((uint64_t)k * (uint64_t)total) / (uint64_t)count);
        const FT_Int32 flags = FT_LOAD_NO_BITMAP | FT_LOAD_IGNORE_GLOBAL_ADVANCE_WIDTH |
                               FT_LOAD_IGNORE_TRANSFORM | FT_LOAD_NO_HINTING;
        if (FT_Load_Glyph(face, (FT_UInt)g->id, flags) ||
            face->glyph->format != FT_GLYPH_FORMAT_OUTLINE)
            continue;
        if (!raster_copy_outline(&face->glyph->outline, &g->ft)) return 0;
        g->loaded = 1;
    }
    raster_glyph_count = n;
    FT_Done_Face(face);
    return 1;
}

static void raster_selection(RasterWork *w) {
    raster_begin(w, RASTER_GLYPHS);
    if (raster_glyph_count < 0) {
        raster_absent(w);
        raster_end(w);
        return;
    }
    raster_value(w, raster_glyph_count);
    raster_end(w);
    for (long k = 0; k < raster_glyph_count; k++) {
        raster_begin(w, RASTER_GLYPH);
        raster_value(w, raster_glyphs[k].id);
        if (raster_glyphs[k].loaded)
            raster_value(w, raster_glyphs[k].ft.n_points);
        else
            raster_absent(w);
        raster_end(w);
    }
}

/* ------------------------------------------------------------------- main */

static uint64_t raster_now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * UINT64_C(1000000000) + (uint64_t)t.tv_nsec;
}

static void raster_operation(int glyphs, RasterCase *cases, const int *parsed, size_t n, RasterWork *w) {
    if (glyphs) raster_selection(w);
    for (size_t i = 0; i < n; i++) raster_perform(&cases[i], parsed[i], w);
}

int main(int argc, char **argv) {
    const char *usage = "usage: reference OPERATION-fold FILE MODE... | OPERATION-bench FILE N MODE...\n"
                        "       | OPERATION-memory FILE MODE... | OPERATION-dump FILE MODE...\n"
                        "  OPERATION: raster (the C engine) or raster-simd (the host's engine)\n"
                        "  MODE: glyphs HEIGHT COUNT CASE... or lines\n";
    const char *mode = argc > 1 ? argv[1] : "";
    int host_engine = !strncmp(mode, "raster-simd-", 12);
    const char *action = mode + (host_engine ? 12 : 7);
    if (strncmp(mode, "raster-", 7)) action = "";
    int bench = !strcmp(action, "bench"), memory = !strcmp(action, "memory");
    int dump = !strcmp(action, "dump"), fold = !strcmp(action, "fold");
    if (!(bench || memory || dump || fold) || argc < 4 + bench) {
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
    int glyphs = !strcmp(args[0], "glyphs");
    long height = 0, selected = 0;
    if (glyphs) {
        char *e1 = NULL, *e2 = NULL;
        if (count < 3) {
            fputs(usage, stderr);
            return 2;
        }
        height = strtol(args[1], &e1, 10);
        selected = strtol(args[2], &e2, 10);
        if (*e1 || *e2 || height < 1 || height > INT32_MAX || selected < 1 ||
            selected > RASTER_MAX_GLYPHS) {
            fputs(usage, stderr);
            return 2;
        }
    } else if (count != 1 || strcmp(args[0], "lines")) {
        fputs(usage, stderr);
        return 2;
    }
    FILE *file = fopen(argv[2], "rb");
    if (!file) {
        perror(argv[2]);
        return 2;
    }
    fseek(file, 0, SEEK_END);
    long size = ftell(file);
    rewind(file);
    unsigned char *data = size >= 0 && size <= RASTER_MAX_INPUT ? malloc((size_t)size + 1) : NULL;
    int read_ok = data && fread(data, 1, (size_t)size, file) == (size_t)size;
    fclose(file);
    if (!read_ok || !raster_setup(host_engine)) {
        fprintf(stderr, "%s: unreadable, too large, or no renderer\n", argv[2]);
        return 2;
    }
    FT_Library ft = NULL;
    size_t n = 0, capacity = 0;
    RasterCase *cases = NULL;
    int *parsed = NULL;
    int ok = 1;
    if (glyphs) {
        ok = !FT_Init_FreeType(&ft) && raster_load(ft, data, size, height, selected);
        n = (size_t)(count - 3);
        cases = calloc(n ? n : 1, sizeof *cases);
        parsed = calloc(n ? n : 1, sizeof *parsed);
        ok = ok && cases && parsed;
        for (size_t i = 0; ok && i < n; i++)
            parsed[i] = raster_parse(args[3 + i], strlen(args[3 + i]), &cases[i]);
    } else {
        for (long at = 0; ok && at < size;) {
            const unsigned char *nl = memchr(data + at, '\n', (size_t)(size - at));
            long len = nl ? (long)(nl - (data + at)) : size - at;
            if (n == capacity) {
                capacity = capacity ? 2 * capacity : 64;
                RasterCase *more = realloc(cases, capacity * sizeof *cases);
                int *more_parsed = realloc(parsed, capacity * sizeof *parsed);
                if (more) cases = more;
                if (more_parsed) parsed = more_parsed;
                ok = more && more_parsed;
                if (!ok) break;
            }
            parsed[n] = raster_parse((const char *)data + at, (size_t)len, &cases[n]);
            n++;
            at += len + 1;
        }
    }
    if (!ok) {
        fprintf(stderr, "%s: out of memory\n", argv[2]);
        return 2;
    }
    int status = 0;
    if (dump) {
        RasterWork work = {0, 0, 0, stdout};
        raster_operation(glyphs, cases, parsed, n, &work);
    } else {
        if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18) status = 2;
        uint64_t start = raster_now_ns(), packets = 0, bytes = 0, digest = 0;
        for (unsigned long i = 0; i < iterations && !status; i++) {
            RasterWork work = {0, 0, 0, NULL};
            raster_operation(glyphs, cases, parsed, n, &work);
            packets += work.packets;
            bytes += work.bytes;
            digest = digest * 31 + work.digest;
        }
        uint64_t elapsed = bench ? raster_now_ns() - start : 0;
        if (memory && !status && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16) status = 2;
        if (!status)
            printf("{\"schema\":1,\"operation\":\"raster\",\"iterations\":%lu,\"packets\":%" PRIu64
                   ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true}\n",
                   iterations, packets, bytes, digest, elapsed);
    }
    fflush(stdout);
    for (size_t i = 0; i < n; i++) raster_case_free(&cases[i]);
    free(cases);
    free(parsed);
    for (long k = 0; k < raster_glyph_count; k++) {
        free(raster_glyphs[k].ft.points);
        free(raster_glyphs[k].ft.tags);
        free(raster_glyphs[k].ft.contours);
    }
    free(raster_glyphs);
    if (ft) FT_Done_FreeType(ft);
    ass_cache_done(raster_sources);
    ass_free_track(raster_track);
    ass_renderer_done(raster_renderer);
    ass_library_done(raster_library);
    free(data);
    return status;
}
