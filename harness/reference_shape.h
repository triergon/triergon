/*
 * Shaping reference: the HarfBuzz, FriBidi and FreeType build of harness/reference.c
 * (tooling only; never linked into the library). Public HarfBuzz, FriBidi and
 * FreeType API only; the libass operation restates libass 0.17.5's font functions
 * over them, each piece naming the libass function it reproduces.
 *
 * Usage:
 *   reference OPERATION-fold   FILE ARGUMENT...
 *   reference OPERATION-bench  FILE ITERATIONS ARGUMENT...
 *   reference OPERATION-memory FILE ARGUMENT...
 *   reference OPERATION-dump   FILE ARGUMENT...
 *
 * OPERATION is one of:
 *   shape    FILE is a font; ARGUMENTs are DIRECTION (ltr or rtl), SCRIPT (ISO 15924,
 *            as hb_script_from_iso15924_tag takes it), LANGUAGE (BCP 47, as
 *            hb_language_from_string takes it), FEATURES (`-` or comma-separated
 *            TAG=VALUE, each global, in that order) and then TEXTs, each upper-case
 *            hexadecimal code points separated by single spaces, optionally after
 *            `START,LENGTH:` naming the item (the rest is context, as
 *            hb_buffer_add_utf32 takes it). Every face of the file gets a font with
 *            HarfBuzz's OpenType font functions at the default scale, and every
 *            text is one hb_shape call: glyph ids, clusters, advances and offsets.
 *   shape-libass  FILE is a font; ARGUMENTs are SIZE (a 26.6 pixel height) and then
 *            those of shape. Every face of the file is opened with FreeType and set up
 *            as libass 0.17.5 sets up its faces and HarfBuzz fonts with hinting off:
 *            its charmap (ass_charmap_magic), GDI metrics (set_font_metrics), the
 *            size (ass_face_set_size) and a HarfBuzz font over the face's tables
 *            (ass_create_hb_font) with libass's font functions at its scale and
 *            ppem (get_hb_font, update_hb_size, init_font_funcs); then as shape.
 *            The font is not an @ font, so glyph advances are not rotated.
 *   bidi     FILE holds one text per line, each optionally followed by `|` and
 *            comma-separated line starts; ARGUMENTs are BASE (ltr, rtl or on) and
 *            BRACKETS (0 or 1). As libass's ass_shaper_shape and ass_shaper_reorder
 *            with whole-text layout: bidi types (and bracket types), levels per
 *            paragraph (ending after each BS character), each line reordered with its
 *            paragraph's direction (lines end before each line start, after each BS
 *            character and at the end): paragraph directions, levels and the map.
 *   simple   FILE holds one text per line; ARGUMENT is BASE. libass's
 *            ASS_SHAPING_SIMPLE over one paragraph: levels, joining types,
 *            fribidi_join_arabic, fribidi_shape with FRIBIDI_FLAGS_DEFAULT |
 *            FRIBIDI_FLAGS_ARABIC: the shaped code points.
 *   unicode  FILE holds code point ranges, FIRST and LAST in hexadecimal per line:
 *            for every code point HarfBuzz's default Unicode functions (general
 *            category, combining class, script, mirroring, decomposition) and
 *            FriBidi's bidi type, bracket, mirror and joining type.
 *
 * Each group of values starts with a tag and folds as crates/shape-worker/src/
 * record.rs folds it; `packets` counts glyphs (shape), lines (bidi, simple) or code
 * points (unicode) and `bytes` the code points shaped or resolved. The file is read
 * before the operation; faces, fonts and buffers are created inside it. `-dump`
 * prints the same values as lines, identical to shape-worker's.
 */
#define _POSIX_C_SOURCE 200809L
#include <fribidi.h>
#include <hb-ot.h>
#include <hb.h>

#include <ft2build.h>
#include FT_FREETYPE_H
#include FT_TRUETYPE_TABLES_H
#include FT_TRUETYPE_IDS_H

#include <inttypes.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define SHAPE_ABSENT UINT64_MAX
/* Bounds on one input, so no file or argument can demand unbounded work. */
#define SHAPE_MAX_TEXT 65536u
#define SHAPE_MAX_FEATURES 64u
#define SHAPE_MAX_INPUT (64L << 20)

enum {
    SHAPE_FACE = 1,
    SHAPE_FACE_ERROR,
    SHAPE_TEXT,
    SHAPE_GLYPH,
    SHAPE_TEXT_ERROR,
    BIDI_LINE = 10,
    BIDI_PARAGRAPH,
    BIDI_LEVEL,
    BIDI_MAP,
    BIDI_LINE_ERROR,
    SIMPLE_LINE = 20,
    SIMPLE_CHAR,
    SIMPLE_LINE_ERROR,
    UNICODE_RANGE = 30,
    UNICODE_POINT,
    UNICODE_RANGE_ERROR,
};

typedef enum { SHAPE_SHAPE, SHAPE_BIDI, SHAPE_SIMPLE, SHAPE_UNICODE, SHAPE_LIBASS } ShapeOperation;

static const char *const shape_operation_names[] = {"shape-text", "shape-bidi", "shape-simple",
                                                    "shape-unicode", "shape-libass"};

typedef struct {
    uint64_t packets, bytes, digest;
    FILE *dump; /* -dump: print lines instead of folding */
} ShapeWork;

static void shape_fold(ShapeWork *w, uint64_t v) { w->digest = w->digest * 31 + v; }

static uint64_t shape_signed(int64_t v) { return (uint64_t)v; }

static uint64_t shape_now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * UINT64_C(1000000000) + (uint64_t)t.tv_nsec;
}

/* A tag and values: folded, or printed as `key v v ...`. */
static void shape_record(ShapeWork *w, int tag, const char *key, const int64_t *values, size_t n) {
    if (w->dump) {
        fputs(key, w->dump);
        for (size_t i = 0; i < n; i++) fprintf(w->dump, " %" PRId64, values[i]);
        fputc('\n', w->dump);
        return;
    }
    shape_fold(w, (uint64_t)tag);
    for (size_t i = 0; i < n; i++) shape_fold(w, shape_signed(values[i]));
}

static void shape_record1(ShapeWork *w, int tag, const char *key, int64_t value) {
    shape_record(w, tag, key, &value, 1);
}

/* ---------------------------------------------------------------- parsing */

static int shape_hex_digit(char c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}

/* One to six upper-case hex digits naming a Unicode scalar value. */
static int shape_parse_scalar(const char *s, size_t n, uint32_t *out) {
    if (n == 0 || n > 6) return 0;
    uint32_t value = 0;
    for (size_t i = 0; i < n; i++) {
        int d = shape_hex_digit(s[i]);
        if (d < 0) return 0;
        value = value * 16 + (uint32_t)d;
    }
    if (value > 0x10FFFF || (value >= 0xD800 && value <= 0xDFFF)) return 0;
    *out = value;
    return 1;
}

/* A decimal number of one to seven digits. */
static int shape_parse_decimal(const char *s, size_t n, unsigned long *out) {
    if (n == 0 || n > 7) return 0;
    unsigned long value = 0;
    for (size_t i = 0; i < n; i++) {
        if (s[i] < '0' || s[i] > '9') return 0;
        value = value * 10 + (unsigned long)(s[i] - '0');
    }
    *out = value;
    return 1;
}

/* Scalar values separated by single spaces, at least one and at most
 * SHAPE_MAX_TEXT. `text` holds SHAPE_MAX_TEXT values. */
static int shape_parse_text(const char *s, size_t n, uint32_t *text, size_t *count) {
    size_t k = 0, start = 0;
    for (size_t i = 0; i <= n; i++) {
        if (i < n && s[i] != ' ') continue;
        if (k == SHAPE_MAX_TEXT || !shape_parse_scalar(s + start, i - start, &text[k])) return 0;
        k++;
        start = i + 1;
    }
    *count = k;
    return 1;
}

/* Four printable ASCII bytes. */
static int shape_parse_tag(const char *s, size_t n, hb_tag_t *out) {
    if (n != 4) return 0;
    for (size_t i = 0; i < 4; i++)
        if (s[i] < 0x20 || s[i] > 0x7E) return 0;
    *out = HB_TAG(s[0], s[1], s[2], s[3]);
    return 1;
}

/* `-`, or comma-separated TAG=VALUE, each for the whole buffer. */
static int shape_parse_features(const char *field, hb_feature_t *features, unsigned *count) {
    *count = 0;
    if (strcmp(field, "-") == 0) return 1;
    const char *p = field;
    for (;;) {
        const char *end = strchr(p, ',');
        size_t n = end ? (size_t)(end - p) : strlen(p);
        const char *eq = memchr(p, '=', n);
        if (!eq || *count == SHAPE_MAX_FEATURES) return 0;
        hb_feature_t *f = &features[*count];
        unsigned long value;
        if (!shape_parse_tag(p, (size_t)(eq - p), &f->tag) ||
            !shape_parse_decimal(eq + 1, n - (size_t)(eq - p) - 1, &value))
            return 0;
        f->value = (uint32_t)value;
        f->start = HB_FEATURE_GLOBAL_START;
        f->end = HB_FEATURE_GLOBAL_END;
        (*count)++;
        if (!end) return 1;
        p = end + 1;
    }
}

static int shape_parse_base(const char *field, FriBidiParType *out) {
    if (strcmp(field, "ltr") == 0)
        *out = FRIBIDI_PAR_LTR;
    else if (strcmp(field, "rtl") == 0)
        *out = FRIBIDI_PAR_RTL;
    else if (strcmp(field, "on") == 0)
        *out = FRIBIDI_PAR_ON;
    else
        return 0;
    return 1;
}

/* The next line of `data` from `*at`, without its newline. */
static int shape_next_line(const char *data, size_t size, size_t *at, const char **line, size_t *n) {
    if (*at >= size) return 0;
    const char *start = data + *at;
    const char *end = memchr(start, '\n', size - *at);
    *line = start;
    *n = end ? (size_t)(end - start) : size - *at;
    *at += *n + (end ? 1 : 0);
    return 1;
}

/* ------------------------------------------------------------------ shape */

typedef struct {
    hb_direction_t direction;
    hb_script_t script;
    hb_language_t language;
    hb_feature_t features[SHAPE_MAX_FEATURES];
    unsigned feature_count;
    char *const *texts;
    int text_count;
} ShapeSettings;

static int shape_settings(char *const *args, int count, ShapeSettings *s) {
    if (count < 4) return 0;
    if (strcmp(args[0], "ltr") == 0)
        s->direction = HB_DIRECTION_LTR;
    else if (strcmp(args[0], "rtl") == 0)
        s->direction = HB_DIRECTION_RTL;
    else
        return 0;
    hb_tag_t script;
    if (!shape_parse_tag(args[1], strlen(args[1]), &script) || args[2][0] == '\0' ||
        !shape_parse_features(args[3], s->features, &s->feature_count))
        return 0;
    s->script = hb_script_from_iso15924_tag(script);
    s->language = hb_language_from_string(args[2], -1);
    s->texts = args + 4;
    s->text_count = count - 4;
    return 1;
}

/* One text argument: its code points and item. */
static int shape_parse_item(const char *arg, uint32_t *text, size_t *len, unsigned long *start,
                            unsigned long *length) {
    const char *colon = strchr(arg, ':');
    const char *cps = colon ? colon + 1 : arg;
    if (!shape_parse_text(cps, strlen(cps), text, len)) return 0;
    *start = 0;
    *length = *len;
    if (colon) {
        const char *comma = memchr(arg, ',', (size_t)(colon - arg));
        if (!comma || !shape_parse_decimal(arg, (size_t)(comma - arg), start) ||
            !shape_parse_decimal(comma + 1, (size_t)(colon - comma - 1), length) ||
            *start > *len || *length > *len - *start)
            return 0;
    }
    return 1;
}

/* Every text of `s` shaped with `font`, one hb_shape call each. */
static int shape_texts(hb_font_t *font, hb_buffer_t *buffer, const ShapeSettings *s, uint32_t *text,
                       ShapeWork *w) {
    for (int t = 0; t < s->text_count; t++) {
        size_t len;
        unsigned long start, length;
        if (!shape_parse_item(s->texts[t], text, &len, &start, &length)) {
            shape_record1(w, SHAPE_TEXT_ERROR, "text-error", t);
            continue;
        }
        hb_buffer_reset(buffer);
        hb_buffer_add_utf32(buffer, text, (int)len, (unsigned)start, (int)length);
        hb_segment_properties_t props = HB_SEGMENT_PROPERTIES_DEFAULT;
        props.direction = s->direction;
        props.script = s->script;
        props.language = s->language;
        hb_buffer_set_segment_properties(buffer, &props);
        hb_shape(font, buffer, s->features, s->feature_count);
        if (!hb_buffer_allocation_successful(buffer)) return 2;
        unsigned glyphs;
        const hb_glyph_info_t *info = hb_buffer_get_glyph_infos(buffer, &glyphs);
        const hb_glyph_position_t *pos = hb_buffer_get_glyph_positions(buffer, NULL);
        shape_record1(w, SHAPE_TEXT, "text", glyphs);
        for (unsigned g = 0; g < glyphs; g++) {
            int64_t v[6] = {info[g].codepoint, info[g].cluster, pos[g].x_advance,
                            pos[g].y_advance,  pos[g].x_offset, pos[g].y_offset};
            shape_record(w, SHAPE_GLYPH, "glyph", v, 6);
        }
        w->packets += glyphs;
        w->bytes += length;
    }
    return 0;
}

static int shape_text(hb_blob_t *blob, const ShapeSettings *s, uint32_t *text, ShapeWork *w) {
    unsigned faces = hb_face_count(blob);
    hb_buffer_t *buffer = hb_buffer_create();
    if (!hb_buffer_allocation_successful(buffer)) return 2;
    int status = 0;
    for (unsigned i = 0; i < faces && !status; i++) {
        hb_face_t *face = hb_face_create(blob, i);
        hb_font_t *font = hb_font_create(face);
        shape_record1(w, SHAPE_FACE, "face", i);
        status = shape_texts(font, buffer, s, text, w);
        hb_font_destroy(font);
        hb_face_destroy(face);
    }
    hb_buffer_destroy(buffer);
    return status;
}

/* ----------------------------------------------------------- shape-libass */

/* libass's face, size and glyph metrics cache for one face: the values
 * ass_glyph_metrics_construct caches, per glyph of the face. */
typedef struct {
    FT_Glyph_Metrics metrics;
    unsigned char state; /* 0 not loaded, 1 loaded, 2 FT_Load_Glyph failed */
} ShapeCachedMetrics;

typedef struct {
    FT_Face face;
    long height;
    ShapeCachedMetrics *cache;
} ShapeLibassFont;

/* libass ass_face_set_size: a REAL_DIM request without width or resolution. */
static void shape_libass_set_size(FT_Face face, long height) {
    FT_Size_RequestRec rq;
    memset(&rq, 0, sizeof rq);
    rq.type = FT_SIZE_REQUEST_TYPE_REAL_DIM;
    rq.height = height;
    FT_Request_Size(face, &rq);
}

/* libass ass_glyph_metrics_construct (without @ font rotation): the default load
 * at the size, cached; get_cached_metrics: NULL when the load failed. */
static const FT_Glyph_Metrics *shape_libass_metrics(ShapeLibassFont *f, hb_codepoint_t glyph,
                                                    FT_Glyph_Metrics *scratch) {
    int cached = glyph < (hb_codepoint_t)f->face->num_glyphs;
    ShapeCachedMetrics *entry = cached ? &f->cache[glyph] : NULL;
    if (entry && entry->state) return entry->state == 1 ? &entry->metrics : NULL;
    shape_libass_set_size(f->face, f->height);
    int failed = FT_Load_Glyph(f->face, glyph,
                               FT_LOAD_DEFAULT | FT_LOAD_IGNORE_GLOBAL_ADVANCE_WIDTH |
                                   FT_LOAD_IGNORE_TRANSFORM) != 0;
    FT_Glyph_Metrics *out = entry ? &entry->metrics : scratch;
    if (!failed) *out = f->face->glyph->metrics;
    if (entry) entry->state = failed ? 2 : 1;
    return failed ? NULL : out;
}

/* libass ass_font_index_magic for the charmaps it maps without conversion:
 * Unicode as is, Windows Symbol at 0xF000 | c. */
static uint32_t shape_libass_index_magic(FT_Face face, uint32_t symbol) {
    if (face->charmap && face->charmap->platform_id == TT_PLATFORM_MICROSOFT &&
        face->charmap->encoding == FT_ENCODING_MS_SYMBOL)
        return 0xF000 | symbol;
    return symbol;
}

/* libass get_glyph_nominal. */
static hb_bool_t shape_libass_nominal(hb_font_t *font, void *data, hb_codepoint_t unicode,
                                      hb_codepoint_t *glyph, void *user) {
    (void)font, (void)user;
    ShapeLibassFont *f = data;
    FT_Glyph_Metrics scratch;
    *glyph = shape_libass_index_magic(f->face, unicode);
    if (*glyph) *glyph = FT_Get_Char_Index(f->face, *glyph);
    if (!*glyph) return 0;
    shape_libass_metrics(f, *glyph, &scratch);
    return 1;
}

/* libass get_glyph_variation. */
static hb_bool_t shape_libass_variation(hb_font_t *font, void *data, hb_codepoint_t unicode,
                                        hb_codepoint_t variation, hb_codepoint_t *glyph, void *user) {
    (void)font, (void)user;
    ShapeLibassFont *f = data;
    FT_Glyph_Metrics scratch;
    *glyph = shape_libass_index_magic(f->face, unicode);
    if (*glyph) *glyph = FT_Face_GetCharVariantIndex(f->face, *glyph, variation);
    if (!*glyph) return 0;
    shape_libass_metrics(f, *glyph, &scratch);
    return 1;
}

/* libass cached_h_advance. */
static hb_position_t shape_libass_h_advance(hb_font_t *font, void *data, hb_codepoint_t glyph,
                                            void *user) {
    (void)font, (void)user;
    FT_Glyph_Metrics scratch;
    const FT_Glyph_Metrics *m = shape_libass_metrics(data, glyph, &scratch);
    return m ? (hb_position_t)m->horiAdvance : 0;
}

/* libass cached_h_origin. */
static hb_bool_t shape_libass_h_origin(hb_font_t *font, void *data, hb_codepoint_t glyph,
                                       hb_position_t *x, hb_position_t *y, void *user) {
    (void)font, (void)data, (void)glyph, (void)x, (void)y, (void)user;
    return 1;
}

/* libass get_h_kerning. */
static hb_position_t shape_libass_h_kerning(hb_font_t *font, void *data, hb_codepoint_t first,
                                            hb_codepoint_t second, void *user) {
    (void)font, (void)user;
    ShapeLibassFont *f = data;
    FT_Vector kern;
    if (FT_Get_Kerning(f->face, first, second, FT_KERNING_DEFAULT, &kern)) return 0;
    return (hb_position_t)kern.x;
}

/* libass cached_extents. */
static hb_bool_t shape_libass_extents(hb_font_t *font, void *data, hb_codepoint_t glyph,
                                      hb_glyph_extents_t *extents, void *user) {
    (void)font, (void)user;
    FT_Glyph_Metrics scratch;
    const FT_Glyph_Metrics *m = shape_libass_metrics(data, glyph, &scratch);
    if (!m) return 0;
    extents->x_bearing = (hb_position_t)m->horiBearingX;
    extents->y_bearing = (hb_position_t)m->horiBearingY;
    extents->width = (hb_position_t)m->width;
    extents->height = (hb_position_t)-m->height;
    return 1;
}

/* libass get_contour_point. */
static hb_bool_t shape_libass_contour_point(hb_font_t *font, void *data, hb_codepoint_t glyph,
                                            unsigned point, hb_position_t *x, hb_position_t *y,
                                            void *user) {
    (void)font, (void)user;
    ShapeLibassFont *f = data;
    if (FT_Load_Glyph(f->face, glyph,
                      FT_LOAD_DEFAULT | FT_LOAD_IGNORE_GLOBAL_ADVANCE_WIDTH |
                          FT_LOAD_IGNORE_TRANSFORM))
        return 0;
    if (point >= (unsigned)f->face->glyph->outline.n_points) return 0;
    *x = (hb_position_t)f->face->glyph->outline.points[point].x;
    *y = (hb_position_t)f->face->glyph->outline.points[point].y;
    return 1;
}

/* libass init_font_funcs, the horizontal callbacks. */
static hb_font_funcs_t *shape_libass_funcs(void) {
    hb_font_funcs_t *funcs = hb_font_funcs_create();
    hb_font_funcs_set_nominal_glyph_func(funcs, shape_libass_nominal, NULL, NULL);
    hb_font_funcs_set_variation_glyph_func(funcs, shape_libass_variation, NULL, NULL);
    hb_font_funcs_set_glyph_h_advance_func(funcs, shape_libass_h_advance, NULL, NULL);
    hb_font_funcs_set_glyph_h_origin_func(funcs, shape_libass_h_origin, NULL, NULL);
    hb_font_funcs_set_glyph_h_kerning_func(funcs, shape_libass_h_kerning, NULL, NULL);
    hb_font_funcs_set_glyph_extents_func(funcs, shape_libass_extents, NULL, NULL);
    hb_font_funcs_set_glyph_contour_point_func(funcs, shape_libass_contour_point, NULL, NULL);
    hb_font_funcs_make_immutable(funcs);
    return funcs;
}

/* libass ass_charmap_magic: a full Microsoft Unicode charmap anywhere, else the
 * first BMP one, else the first Microsoft one, else FreeType's choice or the
 * first. */
static void shape_libass_charmap(FT_Face face) {
    int ms_cmap = -1, ms_unicode_cmap = -1;
    for (int i = 0; i < face->num_charmaps; i++) {
        FT_CharMap cmap = face->charmaps[i];
        if (cmap->platform_id != TT_PLATFORM_MICROSOFT) continue;
        switch (cmap->encoding_id) {
        case TT_MS_ID_UCS_4:
            FT_Set_Charmap(face, cmap);
            return;
        case TT_MS_ID_UNICODE_CS:
            if (ms_unicode_cmap < 0) ms_unicode_cmap = ms_cmap = i;
            break;
        default:
            if (ms_cmap < 0) ms_cmap = i;
        }
    }
    if (ms_cmap >= 0) {
        FT_Set_Charmap(face, face->charmaps[ms_cmap]);
        return;
    }
    if (!face->charmap && face->num_charmaps > 0) FT_Set_Charmap(face, face->charmaps[0]);
}

/* libass set_font_metrics: GDI's ascender, descender and height. */
static void shape_libass_gdi_metrics(FT_Face face) {
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

/* Whether ass_font_index_magic would convert codes for the face's charmap, which
 * this restatement does not: the Windows multibyte encodings (iconv) and Symbol
 * charmaps of the legacy Arabic character sets. */
static int shape_libass_converts(FT_Face face) {
    FT_CharMap c = face->charmap;
    if (!c || c->platform_id != TT_PLATFORM_MICROSOFT) return 0;
    if (c->encoding == FT_ENCODING_MS_SYMBOL) {
        TT_OS2 *os2 = FT_Get_Sfnt_Table(face, FT_SFNT_OS2);
        unsigned charset = os2 ? (os2->fsSelection >> 8) & 0xFF : 0;
        return charset == 178 || charset == 179;
    }
    return c->encoding == FT_ENCODING_SJIS || c->encoding == FT_ENCODING_PRC ||
           c->encoding == FT_ENCODING_BIG5 || c->encoding == FT_ENCODING_WANSUNG ||
           c->encoding == FT_ENCODING_JOHAB;
}

/* libass get_reference_table: the face's tables as FreeType reads them. */
static hb_blob_t *shape_libass_table(hb_face_t *face, hb_tag_t tag, void *data) {
    (void)face;
    FT_ULong len = 0;
    if (FT_Load_Sfnt_Table(data, tag, 0, NULL, &len)) return NULL;
    char *buf = malloc(len ? len : 1);
    if (!buf) return NULL;
    if (FT_Load_Sfnt_Table(data, tag, 0, (FT_Byte *)buf, &len)) {
        free(buf);
        return NULL;
    }
    return hb_blob_create(buf, (unsigned)len, HB_MEMORY_MODE_WRITABLE, buf, free);
}

static int shape_libass(FT_Library library, const char *data, size_t size, long height,
                        const ShapeSettings *s, uint32_t *text, ShapeWork *w) {
    FT_Face probe;
    long faces = 0;
    if (!FT_New_Memory_Face(library, (const FT_Byte *)data, (FT_Long)size, -1, &probe)) {
        faces = probe->num_faces;
        FT_Done_Face(probe);
    }
    hb_buffer_t *buffer = hb_buffer_create();
    hb_font_funcs_t *funcs = shape_libass_funcs();
    if (!hb_buffer_allocation_successful(buffer) || funcs == hb_font_funcs_get_empty()) {
        hb_buffer_destroy(buffer);
        hb_font_funcs_destroy(funcs);
        return 2;
    }
    int status = 0;
    for (long i = 0; i < faces && !status; i++) {
        FT_Face face;
        if (FT_New_Memory_Face(library, (const FT_Byte *)data, (FT_Long)size, i, &face)) {
            shape_record1(w, SHAPE_FACE_ERROR, "face-error", i);
            continue;
        }
        /* ass_face_open, add_face, ass_face_size_metrics_construct */
        shape_libass_charmap(face);
        shape_libass_gdi_metrics(face);
        shape_libass_set_size(face, height);
        if (shape_libass_converts(face)) {
            fprintf(stderr, "face %ld: libass converts codes for this charmap\n", i);
            status = 2;
        } else if (!face->size->metrics.x_ppem || !face->size->metrics.y_ppem) {
            shape_record1(w, SHAPE_FACE_ERROR, "face-error", i);
        } else {
            ShapeLibassFont f = {face, height, calloc((size_t)face->num_glyphs + 1, sizeof *f.cache)};
            /* ass_create_hb_font */
            hb_face_t *hb_face = hb_face_create_for_tables(shape_libass_table, face, NULL);
            hb_face_set_index(hb_face, (unsigned)face->face_index);
            hb_face_set_upem(hb_face, face->units_per_EM);
            hb_font_t *parent = hb_font_create(hb_face);
            hb_face_destroy(hb_face);
            /* get_hb_font, update_hb_size */
            hb_font_t *font = hb_font_create_sub_font(parent);
            hb_font_set_funcs(font, funcs, &f, NULL);
            FT_Size_Metrics *m = &face->size->metrics;
            hb_font_set_scale(font, (int)(((uint64_t)m->x_scale * (uint64_t)face->units_per_EM) >> 16),
                              (int)(((uint64_t)m->y_scale * (uint64_t)face->units_per_EM) >> 16));
            hb_font_set_ppem(font, m->x_ppem, m->y_ppem);
            if (!f.cache || hb_font_is_immutable(font)) {
                status = 2;
            } else {
                shape_record1(w, SHAPE_FACE, "face", i);
                status = shape_texts(font, buffer, s, text, w);
            }
            hb_font_destroy(font);
            hb_font_destroy(parent);
            free(f.cache);
        }
        FT_Done_Face(face);
    }
    hb_font_funcs_destroy(funcs);
    hb_buffer_destroy(buffer);
    return status;
}

/* ------------------------------------------------------------- bidi, simple */

typedef struct {
    FriBidiCharType types[SHAPE_MAX_TEXT];
    FriBidiBracketType brackets[SHAPE_MAX_TEXT];
    FriBidiLevel levels[SHAPE_MAX_TEXT];
    FriBidiStrIndex map[SHAPE_MAX_TEXT];
    FriBidiParType pars[SHAPE_MAX_TEXT];
    FriBidiJoiningType joins[SHAPE_MAX_TEXT];
    unsigned char line_starts[SHAPE_MAX_TEXT];
} ShapeBidi;

/* A text line, optionally followed by `|` and comma-separated line starts in
 * 1..len. */
static int shape_parse_line(const char *line, size_t n, int with_starts, uint32_t *text, size_t *len,
                            ShapeBidi *b) {
    const char *bar = with_starts ? memchr(line, '|', n) : NULL;
    size_t text_n = bar ? (size_t)(bar - line) : n;
    if (!shape_parse_text(line, text_n, text, len)) return 0;
    memset(b->line_starts, 0, *len);
    if (!bar) return 1;
    const char *p = bar + 1, *end = line + n;
    for (;;) {
        const char *comma = memchr(p, ',', (size_t)(end - p));
        size_t m = comma ? (size_t)(comma - p) : (size_t)(end - p);
        unsigned long at;
        if (!shape_parse_decimal(p, m, &at) || at == 0 || at >= *len) return 0;
        b->line_starts[at] = 1;
        if (!comma) return 1;
        p = comma + 1;
    }
}

static int shape_bidi(const char *data, size_t size, FriBidiParType base, int with_brackets,
                      uint32_t *text, ShapeBidi *b, ShapeWork *w) {
    size_t at = 0, n, len;
    const char *line;
    int64_t index = 0;
    while (shape_next_line(data, size, &at, &line, &n)) {
        if (!shape_parse_line(line, n, 1, text, &len, b)) {
            shape_record1(w, BIDI_LINE_ERROR, "line-error", index++);
            continue;
        }
        index++;
        FriBidiStrIndex count = (FriBidiStrIndex)len;
        fribidi_get_bidi_types(text, count, b->types);
        if (with_brackets) fribidi_get_bracket_types(text, count, b->types, b->brackets);
        size_t pars = 0;
        FriBidiStrIndex start = 0;
        for (FriBidiStrIndex i = 0; i < count; i++) {
            if (i != count - 1 && b->types[i] != FRIBIDI_TYPE_BS) continue;
            FriBidiParType dir = base;
            if (!fribidi_get_par_embedding_levels_ex(b->types + start,
                                                     with_brackets ? b->brackets + start : NULL,
                                                     i - start + 1, &dir, b->levels + start))
                return 2;
            b->pars[pars++] = dir;
            start = i + 1;
        }
        for (FriBidiStrIndex i = 0; i < count; i++) b->map[i] = i;
        start = 0;
        size_t par = 0;
        for (FriBidiStrIndex i = 0; i < count; i++) {
            int bs = b->types[i] == FRIBIDI_TYPE_BS;
            if (i != count - 1 && !b->line_starts[i + 1] && !bs) continue;
            if (!fribidi_reorder_line(0, b->types, i - start + 1, start, b->pars[par], b->levels,
                                      NULL, b->map))
                return 2;
            start = i + 1;
            if (bs && par + 1 < pars) par++;
        }
        shape_record1(w, BIDI_LINE, "line", count);
        for (size_t i = 0; i < pars; i++)
            shape_record1(w, BIDI_PARAGRAPH, "paragraph", FRIBIDI_IS_RTL(b->pars[i]) ? 1 : 0);
        for (FriBidiStrIndex i = 0; i < count; i++) shape_record1(w, BIDI_LEVEL, "level", b->levels[i]);
        for (FriBidiStrIndex i = 0; i < count; i++) shape_record1(w, BIDI_MAP, "map", b->map[i]);
        w->packets++;
        w->bytes += len;
    }
    return 0;
}

static int shape_simple(const char *data, size_t size, FriBidiParType base, uint32_t *text,
                        ShapeBidi *b, ShapeWork *w) {
    size_t at = 0, n, len;
    const char *line;
    int64_t index = 0;
    while (shape_next_line(data, size, &at, &line, &n)) {
        if (!shape_parse_line(line, n, 0, text, &len, b)) {
            shape_record1(w, SIMPLE_LINE_ERROR, "line-error", index++);
            continue;
        }
        index++;
        FriBidiStrIndex count = (FriBidiStrIndex)len;
        FriBidiParType dir = base;
        fribidi_get_bidi_types(text, count, b->types);
        if (!fribidi_get_par_embedding_levels_ex(b->types, NULL, count, &dir, b->levels)) return 2;
        fribidi_get_joining_types(text, count, b->joins);
        fribidi_join_arabic(b->types, count, b->levels, b->joins);
        fribidi_shape(FRIBIDI_FLAGS_DEFAULT | FRIBIDI_FLAGS_ARABIC, b->levels, count, b->joins,
                      text);
        shape_record1(w, SIMPLE_LINE, "line", count);
        for (FriBidiStrIndex i = 0; i < count; i++) shape_record1(w, SIMPLE_CHAR, "char", text[i]);
        w->packets++;
        w->bytes += len;
    }
    return 0;
}

/* ---------------------------------------------------------------- unicode */

static void shape_unicode_point(hb_unicode_funcs_t *funcs, uint32_t u, ShapeWork *w) {
    FriBidiChar mirror;
    hb_codepoint_t a, b;
    int decomposed = hb_unicode_decompose(funcs, u, &a, &b);
    int mirrored = fribidi_get_mirror_char(u, &mirror);
    int64_t v[11] = {
        u,
        hb_unicode_general_category(funcs, u),
        hb_unicode_combining_class(funcs, u),
        (int64_t)hb_unicode_script(funcs, u),
        hb_unicode_mirroring(funcs, u),
        fribidi_get_bidi_type(u),
        fribidi_get_bracket(u),
        mirrored ? (int64_t)mirror : -1,
        fribidi_get_joining_type(u),
        decomposed ? (int64_t)a : -1,
        decomposed ? (int64_t)b : -1,
    };
    shape_record(w, UNICODE_POINT, "point", v, 11);
}

static int shape_unicode(const char *data, size_t size, ShapeWork *w) {
    hb_unicode_funcs_t *funcs = hb_unicode_funcs_get_default();
    size_t at = 0, n;
    const char *line;
    int64_t index = 0;
    while (shape_next_line(data, size, &at, &line, &n)) {
        const char *space = memchr(line, ' ', n);
        uint32_t first, last;
        if (!space ||
            !shape_parse_scalar(line, (size_t)(space - line), &first) ||
            !shape_parse_scalar(space + 1, n - (size_t)(space - line) - 1, &last) || first > last) {
            shape_record1(w, UNICODE_RANGE_ERROR, "range-error", index++);
            continue;
        }
        index++;
        int64_t v[2] = {first, last};
        shape_record(w, UNICODE_RANGE, "range", v, 2);
        /* Both ends are scalar values; a range between them may span the surrogates,
           which are queried like every other code point. */
        for (uint32_t u = first;; u++) {
            shape_unicode_point(funcs, u, w);
            w->packets++;
            if (u == last) break;
        }
    }
    return 0;
}

/* ------------------------------------------------------------------- main */

typedef struct {
    ShapeOperation operation;
    ShapeSettings settings;
    FriBidiParType base;
    int brackets;
    long height;
    FT_Library library;
} ShapeCase;

static int shape_run(const ShapeCase *c, const char *data, size_t size, hb_blob_t *blob,
                     uint32_t *text, ShapeBidi *b, ShapeWork *w) {
    switch (c->operation) {
    case SHAPE_SHAPE:
        return shape_text(blob, &c->settings, text, w);
    case SHAPE_LIBASS:
        return shape_libass(c->library, data, size, c->height, &c->settings, text, w);
    case SHAPE_BIDI:
        return shape_bidi(data, size, c->base, c->brackets, text, b, w);
    case SHAPE_SIMPLE:
        return shape_simple(data, size, c->base, text, b, w);
    case SHAPE_UNICODE:
        return shape_unicode(data, size, w);
    }
    return 2;
}

int main(int argc, char **argv) {
    const char *usage = "usage: reference OPERATION-fold FILE ARGUMENT...\n"
                        "       | OPERATION-bench FILE N ARGUMENT...\n"
                        "       | OPERATION-memory FILE ARGUMENT... | OPERATION-dump FILE ARGUMENT...\n"
                        "  OPERATION: shape (DIRECTION SCRIPT LANGUAGE FEATURES TEXT...),\n"
                        "  shape-libass (SIZE DIRECTION SCRIPT LANGUAGE FEATURES TEXT...),\n"
                        "  bidi (BASE BRACKETS), simple (BASE) or unicode\n";
    const char *mode = argc > 1 ? argv[1] : "";
    const char *action = strrchr(mode, '-');
    size_t prefix = action ? (size_t)(action - mode) : 0;
    static const char *const names[] = {"shape", "bidi", "simple", "unicode", "shape-libass"};
    ShapeCase c;
    memset(&c, 0, sizeof c);
    int known = 0;
    for (int i = 0; i < 5; i++)
        if (prefix == strlen(names[i]) && !strncmp(mode, names[i], prefix)) {
            c.operation = (ShapeOperation)i;
            known = 1;
        }
    if (!known) {
        fputs(usage, stderr);
        return 2;
    }
    action = action + 1;
    int bench = !strcmp(action, "bench"), memory = !strcmp(action, "memory");
    int dump = !strcmp(action, "dump"), fold = !strcmp(action, "fold");
    if (!(bench || memory || dump || fold) || argc < 3 + bench) {
        fputs(usage, stderr);
        return 2;
    }
    char *end = NULL;
    unsigned long iterations = bench ? strtoul(argv[3], &end, 10) : 1;
    if ((end && *end) || iterations < 1 || iterations > 1000000) {
        fputs(usage, stderr);
        return 2;
    }
    char *const *args = argv + 3 + bench;
    int count = argc - 3 - bench;
    int valid;
    switch (c.operation) {
    case SHAPE_SHAPE:
        valid = shape_settings(args, count, &c.settings);
        break;
    case SHAPE_LIBASS: {
        unsigned long height = 0;
        valid = count > 0 && shape_parse_decimal(args[0], strlen(args[0]), &height) && height > 0 &&
                shape_settings(args + 1, count - 1, &c.settings);
        c.height = (long)height;
        break;
    }
    case SHAPE_BIDI:
        valid = count == 2 && shape_parse_base(args[0], &c.base) &&
                (!strcmp(args[1], "0") || !strcmp(args[1], "1"));
        c.brackets = valid && !strcmp(args[1], "1");
        break;
    case SHAPE_SIMPLE:
        valid = count == 1 && shape_parse_base(args[0], &c.base);
        break;
    default:
        valid = count == 0;
        break;
    }
    if (!valid) {
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
    char *data = size >= 0 && size <= SHAPE_MAX_INPUT ? malloc((size_t)size + 1) : NULL;
    int read_ok = data && fread(data, 1, (size_t)size, file) == (size_t)size;
    fclose(file);
    uint32_t *text = malloc(SHAPE_MAX_TEXT * sizeof *text);
    ShapeBidi *b = malloc(sizeof *b);
    /* The FreeType library is created once, outside the operation. */
    int library_ok = c.operation != SHAPE_LIBASS || !FT_Init_FreeType(&c.library);
    if (!read_ok || !text || !b || !library_ok) {
        fprintf(stderr, "%s: unreadable or too large\n", argv[2]);
        free(data);
        free(text);
        free(b);
        return 2;
    }
    hb_blob_t *blob = c.operation == SHAPE_SHAPE
                          ? hb_blob_create(data, (unsigned)size, HB_MEMORY_MODE_READONLY, NULL, NULL)
                          : NULL;
    int status = 0;
    if (dump) {
        ShapeWork work = {0, 0, 0, stdout};
        status = shape_run(&c, data, (size_t)size, blob, text, b, &work);
    } else {
        if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18) status = 2;
        uint64_t start = shape_now_ns(), packets = 0, bytes = 0, digest = 0;
        for (unsigned long i = 0; i < iterations && !status; i++) {
            ShapeWork work = {0, 0, 0, NULL};
            status = shape_run(&c, data, (size_t)size, blob, text, b, &work);
            packets += work.packets;
            bytes += work.bytes;
            digest = digest * 31 + work.digest;
        }
        uint64_t elapsed = bench ? shape_now_ns() - start : 0;
        if (memory && !status && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16) status = 2;
        if (!status)
            printf("{\"schema\":1,\"operation\":\"%s\",\"iterations\":%lu,\"packets\":%" PRIu64
                   ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true}\n",
                   shape_operation_names[c.operation], iterations, packets, bytes, digest, elapsed);
    }
    if (status) fprintf(stderr, "%s: the operation failed\n", argv[2]);
    hb_blob_destroy(blob);
    if (c.library) FT_Done_FreeType(c.library);
    free(data);
    free(text);
    free(b);
    return status;
}
