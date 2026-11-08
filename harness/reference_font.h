/*
 * Font reference: the FreeType build of harness/reference.c (tooling only; never
 * linked into the library). Public FreeType API only.
 *
 * Usage:
 *   reference font-fold   FILE
 *   reference font-bench  FILE ITERATIONS
 *   reference font-memory FILE
 *   reference font-dump   FILE
 *
 * One operation opens every face of FILE from memory and reads, per face in this
 * order: the face metrics and style flags; the family, style and PostScript names;
 * the OS/2 weight; every name record; every cmap subtable of formats 0, 4, 6, 10, 12
 * and 13 with all of its mappings; the text charmap libass selects; the kerning of
 * every pair the `kern` table lists; and for every glyph its hmtx advance, its
 * loaded advance and its unhinted outline at 1/64 font unit. Each group starts with
 * a tag and folds as crates/font-worker/src/record.rs folds it; `packets` counts
 * glyph outlines and `bytes` their points. The file is read before the operation
 * and the FreeType library is created once, outside it.
 *
 * Kerning: the pairs of every format 0 subtable of the raw version 0 `kern` table,
 * within the subtable's length, sorted and unique, each passed to FT_Get_Kerning;
 * nonzero values are folded. `font-dump` prints the same values as lines:
 *   face INDEX GLYPHS UPEM ASCENDER DESCENDER HEIGHT MAX-ADVANCE BOLD ITALIC
 *   family|style|postscript NAME     weight N     name P E L ID HEX
 *   charmap P E FORMAT     map CODE GLYPH     textmap P E FORMAT
 *   kern LEFT RIGHT VALUE  glyph G ADVANCE LOADED CONTOURS | ENDS | X Y TAG ...
 */
#define _POSIX_C_SOURCE 200809L
#include <ft2build.h>
#include FT_FREETYPE_H
#include FT_ADVANCES_H
#include FT_SFNT_NAMES_H
#include FT_TRUETYPE_TABLES_H
#include FT_TRUETYPE_TAGS_H

#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define FONT_OPERATION "font-outlines"
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
};

typedef struct {
    uint64_t packets, bytes, digest;
    FILE *dump; /* font-dump: print lines instead of folding */
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

static void font_kerning(FT_Face face, FontWork *w) {
    uint32_t *keys;
    size_t count = font_kern_pairs(face, &keys);
    for (size_t i = 0; i < count; i++) {
        FT_UInt left = keys[i] >> 16, right = keys[i] & 0xffff;
        FT_Vector delta;
        if (FT_Get_Kerning(face, left, right, FT_KERNING_UNSCALED, &delta) || !delta.x) continue;
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

static void font_glyph(FontWork *w, long g, long advance, long loaded, const FT_Outline *o) {
    if (w->dump) {
        fprintf(w->dump, "glyph %ld %ld %ld %d |", g, advance, loaded, o->n_contours);
        for (int c = 0; c < o->n_contours; c++) fprintf(w->dump, " %d", o->contours[c] + 1);
        fprintf(w->dump, " |");
        for (int p = 0; p < o->n_points; p++)
            fprintf(w->dump, " %ld %ld %d", o->points[p].x, o->points[p].y, FT_CURVE_TAG(o->tags[p]));
        fprintf(w->dump, "\n");
        return;
    }
    font_fold(w, FONT_GLYPH);
    font_fold(w, (uint64_t)g);
    font_fold(w, font_signed(advance));
    font_fold(w, font_signed(loaded));
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

static void font_error(FontWork *w, int tag, const char *key, long index) {
    if (w->dump) {
        fprintf(w->dump, "%s %ld error\n", key, index);
    } else {
        font_fold(w, (uint64_t)tag);
        font_fold(w, (uint64_t)index);
    }
}

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
            (uint64_t)face->max_advance_width,
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

    font_kerning(face, w);

    /* Unhinted outlines at one pixel per font unit, so 26.6 values are 1/64 units. */
    int sized = !FT_Set_Char_Size(face, 0, (FT_F26Dot6)face->units_per_EM * 64, 72, 72);
    const FT_Int32 outline_flags =
        FT_LOAD_NO_HINTING | FT_LOAD_NO_BITMAP | FT_LOAD_NO_AUTOHINT | FT_LOAD_NO_SVG;
    for (long g = 0; g < face->num_glyphs; g++) {
        FT_Fixed advance = 0;
        if (FT_Get_Advance(face, (FT_UInt)g, FT_LOAD_NO_SCALE, &advance)) advance = -1;
        FT_Error error =
            FT_Load_Glyph(face, (FT_UInt)g, FT_LOAD_NO_SCALE | FT_LOAD_NO_BITMAP | FT_LOAD_NO_SVG);
        long loaded = error ? -1 : face->glyph->metrics.horiAdvance;
        if (!error) error = !sized || FT_Load_Glyph(face, (FT_UInt)g, outline_flags);
        if (error)
            font_error(w, FONT_GLYPH_ERROR, "glyph", g);
        else
            font_glyph(w, g, (long)advance, loaded, &face->glyph->outline);
    }
}

/* One operation over the font in memory; nonzero when FreeType opens no font. */
static int font_read(FT_Library library, const FT_Byte *data, FT_Long size, FontWork *w) {
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
        font_face(face, i, w);
        FT_Done_Face(face);
    }
    return 0;
}

int main(int argc, char **argv) {
    const char *usage = "usage: reference font-fold FILE | font-bench FILE N | font-memory FILE | font-dump FILE\n";
    const char *mode = argc > 1 ? argv[1] : "";
    int bench = !strcmp(mode, "font-bench"), memory = !strcmp(mode, "font-memory");
    int dump = !strcmp(mode, "font-dump"), fold = !strcmp(mode, "font-fold");
    if (argc != (bench ? 4 : 3) || !(bench || memory || dump || fold)) {
        fputs(usage, stderr);
        return 2;
    }
    char *end = NULL;
    unsigned long iterations = bench ? strtoul(argv[3], &end, 10) : 1;
    if ((bench && *end) || iterations < 1 || iterations > 1000000) {
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
        status = font_read(library, data, size, &work);
    } else {
        if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18) status = 2;
        uint64_t start = font_now_ns(), packets = 0, bytes = 0, digest = 0;
        for (unsigned long i = 0; i < iterations && !status; i++) {
            FontWork work = {0, 0, 0, NULL};
            status = font_read(library, data, size, &work);
            packets += work.packets;
            bytes += work.bytes;
            digest = digest * 31 + work.digest;
        }
        uint64_t elapsed = bench ? font_now_ns() - start : 0;
        if (memory && !status && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16) status = 2;
        if (!status)
            printf("{\"schema\":1,\"operation\":\"" FONT_OPERATION "\",\"iterations\":%lu,\"packets\":%" PRIu64
                   ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true}\n",
                   iterations, packets, bytes, digest, elapsed);
    }
    if (status == 1) fprintf(stderr, "%s: not a font FreeType opens\n", argv[2]);
    FT_Done_FreeType(library);
    free(data);
    return status;
}
