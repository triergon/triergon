/*
 * Font reference: the FreeType build of harness/reference.c (tooling only; never
 * linked into the library). Public FreeType API only.
 *
 * Usage:
 *   reference OPERATION-fold   FILE [HEIGHT]
 *   reference OPERATION-bench  FILE ITERATIONS [HEIGHT]
 *   reference OPERATION-memory FILE [HEIGHT]
 *   reference OPERATION-dump   FILE [HEIGHT]
 *
 * OPERATION is one of:
 *   font         per face: the face metrics and style flags; the family, style and
 *                PostScript names; the OS/2 weight; every name record; every cmap
 *                subtable of formats 0, 4, 6, 10, 12 and 13 with all of its
 *                mappings; the text charmap libass selects; the unscaled kerning of
 *                every pair the `kern` table lists; and for every glyph its hmtx
 *                advance, its loaded advance and its unhinted outline at 1/64 font
 *                unit (loaded at one pixel per font unit).
 *   font-scaled  per face, as libass loads it at the 26.6 pixel HEIGHT: libass's GDI
 *                face metrics (`set_font_metrics`), the size request libass makes
 *                (`ass_face_set_size`: REAL_DIM), the size metrics, libass's ascent
 *                and descent, HarfBuzz scale, emboldening strength, PostScript flag
 *                and decoration bands; the kerning at the size of every pair the
 *                `kern` table lists (FT_KERNING_DEFAULT); and for every glyph the
 *                unhinted outline and advance libass draws, then that outline
 *                emboldened, slanted, and both (libass's order).
 *   font-hinted  per face at HEIGHT as above: every glyph's default-load (hinted)
 *                advance, which libass's HarfBuzz callbacks report.
 *
 * Each group of values starts with a tag and folds as crates/font-worker/src/
 * record.rs folds it; `packets` counts outlines (or hinted advances) and `bytes`
 * outline points. The file is read before the operation and the FreeType library
 * is created once, outside it. Kerning pairs: those of every format 0 subtable of
 * the raw version 0 `kern` table, within the subtable's length, sorted and unique;
 * nonzero values are folded. `-dump` prints the same values as lines, identical
 * to font-worker's.
 */
#define _POSIX_C_SOURCE 200809L
#include <ft2build.h>
#include FT_FREETYPE_H
#include FT_ADVANCES_H
#include FT_OUTLINE_H
#include FT_SFNT_NAMES_H
#include FT_TRUETYPE_TABLES_H
#include FT_TRUETYPE_TAGS_H
#include FT_TYPE1_TABLES_H

#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define FONT_ABSENT UINT64_MAX

enum {
    FONT_FACE = 1,
    FONT_FACE_ERROR,
    FONT_FAMILY,
    FONT_STYLE,
    FONT_POSTSCRIPT,
    FONT_WEIGHT,
    FONT_NAME,
    FONT_CHARMAP,
    FONT_MAP,
    FONT_TEXTMAP,
    FONT_KERN,
    FONT_GLYPH,
    FONT_GLYPH_ERROR,
    FONT_SIZED_FACE = 20,
    FONT_SIZE,
    FONT_SIZE_ERROR,
    FONT_SCALED_GLYPH,
    FONT_STYLED,
    FONT_HINTED,
};

typedef enum { FONT_FACES, FONT_SCALED, FONT_HINTED_ADVANCES } FontOperation;

static const char *const font_operation_names[] = {"font-outlines", "font-scaled", "font-hinted"};

typedef struct {
    uint64_t packets, bytes, digest;
    FILE *dump; /* -dump: print lines instead of folding */
} FontWork;

static void font_fold(FontWork *w, uint64_t v) { w->digest = w->digest * 31 + v; }

static void font_fold_framed(FontWork *w, const uint8_t *bytes, size_t n) {
    font_fold(w, n);
    for (size_t i = 0; i < n; i++) font_fold(w, bytes[i]);
}

static uint64_t font_signed(int64_t v) { return (uint64_t)v; }

static unsigned font_u16(const FT_Byte *p) { return (unsigned)p[0] << 8 | p[1]; }

static uint64_t font_now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * UINT64_C(1000000000) + (uint64_t)t.tv_nsec;
}

static void font_text(FontWork *w, int tag, const char *key, const char *value) {
    if (w->dump) {
        fprintf(w->dump, "%s %s\n", key, value ? value : "-");
        return;
    }
    font_fold(w, (uint64_t)tag);
    if (value)
        font_fold_framed(w, (const uint8_t *)value, strlen(value));
    else
        font_fold(w, FONT_ABSENT);
}

static void font_charmap(FontWork *w, int tag, const char *key, FT_CharMap c) {
    if (w->dump) {
        fprintf(w->dump, "%s %u %u %ld\n", key, c->platform_id, c->encoding_id, FT_Get_CMap_Format(c));
        return;
    }
    font_fold(w, (uint64_t)tag);
    font_fold(w, c->platform_id);
    font_fold(w, c->encoding_id);
    font_fold(w, font_signed(FT_Get_CMap_Format(c)));
}

static void font_error(FontWork *w, int tag, const char *key, long value) {
    if (w->dump) {
        fprintf(w->dump, "%s %ld error\n", key, value);
    } else {
        font_fold(w, (uint64_t)tag);
        font_fold(w, font_signed(value));
    }
}

/* Contours, points, contour ends, then x, y and tag of every point; one packet. */
static void font_outline(FontWork *w, const FT_Outline *o) {
    if (w->dump) {
        fprintf(w->dump, "%d |", o->n_contours);
        for (int c = 0; c < o->n_contours; c++) fprintf(w->dump, " %d", o->contours[c] + 1);
        fprintf(w->dump, " |");
        for (int p = 0; p < o->n_points; p++)
            fprintf(w->dump, " %ld %ld %d", o->points[p].x, o->points[p].y, FT_CURVE_TAG(o->tags[p]));
        fprintf(w->dump, "\n");
        return;
    }
    font_fold(w, (uint64_t)o->n_contours);
    font_fold(w, (uint64_t)o->n_points);
    for (int c = 0; c < o->n_contours; c++) font_fold(w, (uint64_t)o->contours[c] + 1);
    for (int p = 0; p < o->n_points; p++) {
        font_fold(w, font_signed(o->points[p].x));
        font_fold(w, font_signed(o->points[p].y));
        font_fold(w, (uint64_t)FT_CURVE_TAG(o->tags[p]));
    }
    w->packets++;
    w->bytes += (uint64_t)o->n_points;
}

static int font_supported(FT_CharMap charmap) {
    switch (FT_Get_CMap_Format(charmap)) {
    case 0: case 4: case 6: case 10: case 12: case 13: return 1;
    default: return 0;
    }
}

/* The charmap libass selects: the first Windows Unicode subtable, else the first
 * Windows subtable, else FreeType's default, else the first. */
static FT_CharMap font_text_charmap(FT_Face face) {
    FT_CharMap windows = NULL;
    for (int i = 0; i < face->num_charmaps; i++) {
        FT_CharMap c = face->charmaps[i];
        if (c->platform_id == 3 && (c->encoding_id == 1 || c->encoding_id == 10)) return c;
        if (c->platform_id == 3 && !windows) windows = c;
    }
    if (windows) return windows;
    if (face->charmap) return face->charmap;
    return face->num_charmaps ? face->charmaps[0] : NULL;
}

static int font_key_order(const void *a, const void *b) {
    uint32_t x = *(const uint32_t *)a, y = *(const uint32_t *)b;
    return (x > y) - (x < y);
}

/* Every pair the raw `kern` table lists, as left << 16 | right, sorted and unique. */
static size_t font_kern_pairs(FT_Face face, uint32_t **keys) {
    FT_ULong length = 0;
    *keys = NULL;
    if (FT_Load_Sfnt_Table(face, TTAG_kern, 0, NULL, &length) || length < 4) return 0;
    FT_Byte *table = malloc(length);
    if (!table) return 0;
    size_t count = 0;
    if (!FT_Load_Sfnt_Table(face, TTAG_kern, 0, table, &length) && font_u16(table) == 0 &&
        (*keys = malloc(length / 6 * sizeof **keys))) {
        unsigned subtables = font_u16(table + 2);
        size_t at = 4;
        for (unsigned t = 0; t < subtables && at + 14 <= length; t++) {
            size_t size = font_u16(table + at + 2), end = at + size < length ? at + size : length;
            if (size < 14) break;
            if (font_u16(table + at + 4) >> 8 == 0) {
                unsigned pairs = font_u16(table + at + 6);
                for (size_t p = 0, o = at + 14; p < pairs && o + 6 <= end; p++, o += 6)
                    (*keys)[count++] = (uint32_t)font_u16(table + o) << 16 | font_u16(table + o + 2);
            }
            at += size;
        }
        qsort(*keys, count, sizeof **keys, font_key_order);
        size_t unique = 0;
        for (size_t i = 0; i < count; i++)
            if (!unique || (*keys)[unique - 1] != (*keys)[i]) (*keys)[unique++] = (*keys)[i];
        count = unique;
    }
    free(table);
    return count;
}

/* The nonzero kerning of every listed pair, unscaled or at the current size. */
static void font_kerning(FT_Face face, FT_UInt mode, FontWork *w) {
    uint32_t *keys;
    size_t count = font_kern_pairs(face, &keys);
    for (size_t i = 0; i < count; i++) {
        FT_UInt left = keys[i] >> 16, right = keys[i] & 0xffff;
        FT_Vector delta;
        if (FT_Get_Kerning(face, left, right, mode, &delta) || !delta.x) continue;
        if (w->dump) {
            fprintf(w->dump, "kern %u %u %ld\n", left, right, delta.x);
        } else {
            font_fold(w, FONT_KERN);
            font_fold(w, left);
            font_fold(w, right);
            font_fold(w, font_signed(delta.x));
        }
    }
    free(keys);
}

/* ---- Face data and unscaled outlines -------------------------------------- */

static void font_face(FT_Face face, long index, FontWork *w) {
    if (w->dump) {
        fprintf(w->dump, "face %ld %ld %u %d %d %d %d %d %d\n", index, face->num_glyphs,
                face->units_per_EM, face->ascender, face->descender, face->height,
                face->max_advance_width, !!(face->style_flags & FT_STYLE_FLAG_BOLD),
                !!(face->style_flags & FT_STYLE_FLAG_ITALIC));
    } else {
        const uint64_t words[] = {
            FONT_FACE,
            (uint64_t)index,
            (uint64_t)face->num_glyphs,
            face->units_per_EM,
            font_signed(face->ascender),
            font_signed(face->descender),
            font_signed(face->height),
            font_signed(face->max_advance_width), /* FT_Short */
            !!(face->style_flags & FT_STYLE_FLAG_BOLD),
            !!(face->style_flags & FT_STYLE_FLAG_ITALIC),
        };
        for (size_t i = 0; i < sizeof words / sizeof *words; i++) font_fold(w, words[i]);
    }
    font_text(w, FONT_FAMILY, "family", face->family_name);
    font_text(w, FONT_STYLE, "style", face->style_name);
    font_text(w, FONT_POSTSCRIPT, "postscript", FT_Get_Postscript_Name(face));
    TT_OS2 *os2 = FT_Get_Sfnt_Table(face, FT_SFNT_OS2);
    unsigned weight = os2 ? os2->usWeightClass : 0;
    if (w->dump) {
        fprintf(w->dump, "weight %u\n", weight);
    } else {
        font_fold(w, FONT_WEIGHT);
        font_fold(w, weight);
    }

    FT_UInt names = FT_Get_Sfnt_Name_Count(face);
    for (FT_UInt i = 0; i < names; i++) {
        FT_SfntName name;
        if (FT_Get_Sfnt_Name(face, i, &name)) continue;
        if (w->dump) {
            fprintf(w->dump, "name %u %u %u %u ", name.platform_id, name.encoding_id,
                    name.language_id, name.name_id);
            for (FT_UInt k = 0; k < name.string_len; k++) fprintf(w->dump, "%02x", name.string[k]);
            fprintf(w->dump, "\n");
        } else {
            font_fold(w, FONT_NAME);
            font_fold(w, name.platform_id);
            font_fold(w, name.encoding_id);
            font_fold(w, name.language_id);
            font_fold(w, name.name_id);
            font_fold_framed(w, name.string, name.string_len);
        }
    }

    FT_CharMap textmap = font_text_charmap(face);
    for (int i = 0; i < face->num_charmaps; i++) {
        FT_CharMap c = face->charmaps[i];
        if (!font_supported(c)) continue;
        font_charmap(w, FONT_CHARMAP, "charmap", c);
        FT_Set_Charmap(face, c);
        FT_UInt glyph;
        FT_ULong code = FT_Get_First_Char(face, &glyph);
        while (glyph != 0) {
            if (w->dump) {
                fprintf(w->dump, "map %lu %u\n", code, glyph);
            } else {
                font_fold(w, FONT_MAP);
                font_fold(w, code);
                font_fold(w, glyph);
            }
            code = FT_Get_Next_Char(face, code, &glyph);
        }
    }
    if (textmap) {
        FT_Set_Charmap(face, textmap);
        font_charmap(w, FONT_TEXTMAP, "textmap", textmap);
    } else if (w->dump) {
        fprintf(w->dump, "textmap -\n");
    } else {
        font_fold(w, FONT_TEXTMAP);
        font_fold(w, FONT_ABSENT);
    }

    font_kerning(face, FT_KERNING_UNSCALED, w);

    /* Unscaled, unhinted outlines in whole font units, with their advances. */
    for (long g = 0; g < face->num_glyphs; g++) {
        FT_Fixed advance = 0;
        if (FT_Get_Advance(face, (FT_UInt)g, FT_LOAD_NO_SCALE, &advance)) advance = -1;
        FT_Error error =
            FT_Load_Glyph(face, (FT_UInt)g, FT_LOAD_NO_SCALE | FT_LOAD_NO_BITMAP | FT_LOAD_NO_SVG);
        long loaded = error ? -1 : face->glyph->metrics.horiAdvance;
        if (error) {
            font_error(w, FONT_GLYPH_ERROR, "glyph", g);
            continue;
        }
        if (w->dump) {
            fprintf(w->dump, "glyph %ld %ld %ld ", g, (long)advance, loaded);
        } else {
            font_fold(w, FONT_GLYPH);
            font_fold(w, (uint64_t)g);
            font_fold(w, font_signed(advance));
            font_fold(w, font_signed(loaded));
        }
        font_outline(w, &face->glyph->outline);
    }
}

/* ---- Faces at a size, as libass loads them -------------------------------- */

/* libass `set_font_metrics`: GDI's ascender, descender and height. */
static void font_gdi_metrics(FT_Face face) {
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

/* A libass decoration band from a table position and size, y up, or absent. */
static void font_band(FontWork *w, const char *key, int present, long position, long size,
                      long y_scale) {
    if (present) {
        long long pos = ((long long)position * y_scale + 0x8000) >> 16;
        long long thick = ((long long)size * y_scale + 0x8000) >> 16;
        pos = -pos - (thick >> 1);
        const long long max = (1LL << 28) - 1;
        if (pos >= -max && pos + thick <= max) {
            if (w->dump) {
                fprintf(w->dump, " %s %lld %lld", key, -(pos + thick), -pos);
            } else {
                font_fold(w, font_signed(-(pos + thick)));
                font_fold(w, font_signed(-pos));
            }
            return;
        }
    }
    if (w->dump)
        fprintf(w->dump, " %s -", key);
    else
        font_fold(w, FONT_ABSENT);
}

/* The size header; returns the emboldening strength and PostScript flag. */
static void font_size(FT_Face face, long height, FontWork *w, int *strength, int *postscript) {
    FT_Size_Metrics *m = &face->size->metrics;
    PS_FontInfoRec info;
    *postscript = !FT_Get_PS_Font_Info(face, &info);
    /* ass_font_get_asc_desc, update_hb_size and ass_glyph_embolden */
    long ascent = (int)FT_MulFix(face->ascender, m->y_scale);
    long descent = (int)FT_MulFix(-face->descender, m->y_scale);
    long hb_x = (int)(((unsigned long long)m->x_scale * face->units_per_EM) >> 16);
    long hb_y = (int)(((unsigned long long)m->y_scale * face->units_per_EM) >> 16);
    *strength = (int)(FT_MulFix(face->units_per_EM, m->y_scale) / 64);
    if (w->dump) {
        fprintf(w->dump, "size %ld %u %u %ld %ld %ld %ld %ld %ld %ld %ld %ld %ld %d %d", height,
                m->x_ppem, m->y_ppem, m->x_scale, m->y_scale, m->ascender, m->descender, m->height,
                m->max_advance, ascent, descent, hb_x, hb_y, *strength, *postscript);
    } else {
        const uint64_t words[] = {
            FONT_SIZE,
            font_signed(height),
            m->x_ppem,
            m->y_ppem,
            font_signed(m->x_scale),
            font_signed(m->y_scale),
            font_signed(m->ascender),
            font_signed(m->descender),
            font_signed(m->height),
            font_signed(m->max_advance),
            font_signed(ascent),
            font_signed(descent),
            font_signed(hb_x),
            font_signed(hb_y),
            font_signed(*strength),
            (uint64_t)*postscript,
        };
        for (size_t i = 0; i < sizeof words / sizeof *words; i++) font_fold(w, words[i]);
    }
    TT_Postscript *ps = FT_Get_Sfnt_Table(face, FT_SFNT_POST);
    font_band(w, "underline", ps && ps->underlinePosition <= 0 && ps->underlineThickness > 0,
              ps ? ps->underlinePosition : 0, ps ? ps->underlineThickness : 0, m->y_scale);
    TT_OS2 *os2 = FT_Get_Sfnt_Table(face, FT_SFNT_OS2);
    font_band(w, "strikeout", os2 && os2->yStrikeoutPosition >= 0 && os2->yStrikeoutSize > 0,
              os2 ? os2->yStrikeoutPosition : 0, os2 ? os2->yStrikeoutSize : 0, m->y_scale);
    if (w->dump) fprintf(w->dump, "\n");
}

/* One synthetic style of the glyph just loaded, or its absence. */
static void font_styled(FontWork *w, int style, long g, const FT_Outline *o, int applied) {
    static const char *const names[] = {"bold", "oblique", "both"};
    if (w->dump) {
        if (!applied) {
            fprintf(w->dump, "%s %ld error\n", names[style], g);
            return;
        }
        fprintf(w->dump, "%s %ld ", names[style], g);
    } else {
        font_fold(w, FONT_STYLED);
        font_fold(w, (uint64_t)style);
        font_fold(w, (uint64_t)g);
        if (!applied) {
            font_fold(w, FONT_ABSENT);
            return;
        }
    }
    font_outline(w, o);
}

static const FT_Int32 font_libass_flags =
    FT_LOAD_NO_BITMAP | FT_LOAD_IGNORE_GLOBAL_ADVANCE_WIDTH | FT_LOAD_IGNORE_TRANSFORM;

static void font_scaled(FT_Library library, FT_Face face, long height, FontWork *w) {
    int strength, postscript;
    font_size(face, height, w, &strength, &postscript);
    font_kerning(face, FT_KERNING_DEFAULT, w);
    /* ass_glyph_italicize's shear */
    FT_Matrix shear = {0x10000L, postscript ? 0x02d24L : 0x05700L, 0, 0x10000L};
    for (long g = 0; g < face->num_glyphs; g++) {
        if (FT_Load_Glyph(face, (FT_UInt)g, font_libass_flags | FT_LOAD_NO_HINTING)) {
            font_error(w, FONT_GLYPH_ERROR, "glyph", g);
            continue;
        }
        FT_Outline *o = &face->glyph->outline;
        if (w->dump) {
            fprintf(w->dump, "glyph %ld %ld ", g, face->glyph->advance.x);
        } else {
            font_fold(w, FONT_SCALED_GLYPH);
            font_fold(w, (uint64_t)g);
            font_fold(w, font_signed(face->glyph->advance.x));
        }
        font_outline(w, o);
        FT_Outline copy;
        if (FT_Outline_New(library, (FT_UInt)o->n_points, o->n_contours, &copy)) abort();
        int applied = !FT_Outline_Copy(o, &copy) && !FT_Outline_Embolden(&copy, strength);
        font_styled(w, 0, g, &copy, applied);
        applied = !FT_Outline_Copy(o, &copy);
        if (applied) FT_Outline_Transform(&copy, &shear);
        font_styled(w, 1, g, &copy, applied);
        applied = applied && !FT_Outline_Embolden(&copy, strength);
        font_styled(w, 2, g, &copy, applied);
        FT_Outline_Done(library, &copy);
    }
}

static void font_hinted(FT_Face face, FontWork *w) {
    for (long g = 0; g < face->num_glyphs; g++) {
        /* What HarfBuzz receives: the default (hinted) advance. */
        int error = FT_Load_Glyph(face, (FT_UInt)g, FT_LOAD_DEFAULT | FT_LOAD_IGNORE_GLOBAL_ADVANCE_WIDTH |
                                                        FT_LOAD_IGNORE_TRANSFORM) != 0;
        long advance = error ? 0 : face->glyph->metrics.horiAdvance;
        if (w->dump) {
            if (error)
                fprintf(w->dump, "hinted %ld error\n", g);
            else
                fprintf(w->dump, "hinted %ld %ld\n", g, advance);
        } else {
            font_fold(w, FONT_HINTED);
            font_fold(w, (uint64_t)g);
            font_fold(w, error ? FONT_ABSENT : font_signed(advance));
            w->packets++;
        }
    }
}

/* ---- One operation -------------------------------------------------------- */

/* One operation over the font in memory; nonzero when FreeType opens no font. */
static int font_read(FT_Library library, const FT_Byte *data, FT_Long size, FontOperation operation,
                     long height, FontWork *w) {
    FT_Face probe;
    if (FT_New_Memory_Face(library, data, size, -1, &probe)) return 1;
    long faces = probe->num_faces;
    FT_Done_Face(probe);
    for (long i = 0; i < faces; i++) {
        FT_Face face;
        if (FT_New_Memory_Face(library, data, size, i, &face)) {
            font_error(w, FONT_FACE_ERROR, "face", i);
            continue;
        }
        if (operation == FONT_FACES) {
            font_face(face, i, w);
        } else {
            if (w->dump) {
                fprintf(w->dump, "face %ld\n", i);
            } else {
                font_fold(w, FONT_SIZED_FACE);
                font_fold(w, (uint64_t)i);
            }
            font_gdi_metrics(face);
            FT_CharMap textmap = font_text_charmap(face);
            if (textmap) FT_Set_Charmap(face, textmap);
            FT_Size_RequestRec rq;
            memset(&rq, 0, sizeof rq);
            rq.type = FT_SIZE_REQUEST_TYPE_REAL_DIM;
            rq.height = height;
            FT_Error error = FT_Request_Size(face, &rq);
            if (error || !face->size->metrics.x_ppem || !face->size->metrics.y_ppem)
                font_error(w, FONT_SIZE_ERROR, "size", height);
            else if (operation == FONT_SCALED)
                font_scaled(library, face, height, w);
            else
                font_hinted(face, w);
        }
        FT_Done_Face(face);
    }
    return 0;
}

int main(int argc, char **argv) {
    const char *usage = "usage: reference OPERATION-fold FILE [HEIGHT] | OPERATION-bench FILE N [HEIGHT]\n"
                        "       | OPERATION-memory FILE [HEIGHT] | OPERATION-dump FILE [HEIGHT]\n"
                        "  OPERATION: font, font-scaled HEIGHT, font-hinted HEIGHT\n";
    const char *mode = argc > 1 ? argv[1] : "";
    const char *action = strrchr(mode, '-');
    size_t prefix = action ? (size_t)(action - mode) : 0;
    FontOperation operation;
    if (prefix == 4 && !strncmp(mode, "font", 4))
        operation = FONT_FACES;
    else if (prefix == 11 && !strncmp(mode, "font-scaled", 11))
        operation = FONT_SCALED;
    else if (prefix == 11 && !strncmp(mode, "font-hinted", 11))
        operation = FONT_HINTED_ADVANCES;
    else {
        fputs(usage, stderr);
        return 2;
    }
    action = action + 1;
    int bench = !strcmp(action, "bench"), memory = !strcmp(action, "memory");
    int dump = !strcmp(action, "dump"), fold = !strcmp(action, "fold");
    int sized = operation != FONT_FACES;
    if (!(bench || memory || dump || fold) || argc != 3 + bench + sized) {
        fputs(usage, stderr);
        return 2;
    }
    char *end = NULL, *height_end = NULL;
    unsigned long iterations = bench ? strtoul(argv[3], &end, 10) : 1;
    long height = sized ? strtol(argv[3 + bench], &height_end, 10) : 0;
    if ((end && *end) || (height_end && *height_end) || iterations < 1 || iterations > 1000000) {
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
    FT_Byte *data = size > 0 && size <= (64L << 20) ? malloc((size_t)size) : NULL;
    int read_ok = data && fread(data, 1, (size_t)size, file) == (size_t)size;
    fclose(file);
    FT_Library library;
    if (!read_ok || FT_Init_FreeType(&library)) {
        fprintf(stderr, "%s: unreadable or too large\n", argv[2]);
        free(data);
        return 2;
    }
    int status = 0;
    if (dump) {
        FontWork work = {0, 0, 0, stdout};
        status = font_read(library, data, size, operation, height, &work);
    } else {
        if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18) status = 2;
        uint64_t start = font_now_ns(), packets = 0, bytes = 0, digest = 0;
        for (unsigned long i = 0; i < iterations && !status; i++) {
            FontWork work = {0, 0, 0, NULL};
            status = font_read(library, data, size, operation, height, &work);
            packets += work.packets;
            bytes += work.bytes;
            digest = digest * 31 + work.digest;
        }
        uint64_t elapsed = bench ? font_now_ns() - start : 0;
        if (memory && !status && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16) status = 2;
        if (!status)
            printf("{\"schema\":1,\"operation\":\"%s\",\"iterations\":%lu,\"packets\":%" PRIu64
                   ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true}\n",
                   font_operation_names[operation], iterations, packets, bytes, digest, elapsed);
    }
    if (status == 1) fprintf(stderr, "%s: not a font FreeType opens\n", argv[2]);
    FT_Done_FreeType(library);
    free(data);
    return status;
}
