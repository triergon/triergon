/*
 * FreeType reference for the triergon-font comparison.
 *
 * Usage: freetype-font FILE
 *        freetype-font --scaled HEIGHT,... FILE
 *
 * Prints, for every face of FILE, the lines that `triergon-font`'s `font-dump`
 * example prints for the same face (see crates/font/examples/font-dump.rs):
 * face metrics and style, FreeType's family, style and PostScript names, the name
 * records, the supported cmap subtables and their mappings, the text charmap,
 * kerning among the first 256 glyphs and the glyphs the text charmap maps from
 * U+0020..U+00FF, and every glyph's advances and unhinted outline.
 *
 * Outlines are loaded unhinted at a size whose pixels per em equal the units per
 * em, so 26.6 coordinates are font units times 64. Advances are font units.
 *
 * With --scaled, it prints for every face what libass obtains from FreeType at
 * each 26.6 height (crates/font/examples/font-dump.rs --scaled prints the same):
 * after libass's GDI face metrics (`set_font_metrics`), the size request libass
 * makes (`ass_face_set_size`: REAL_DIM, no width or resolution), the size
 * metrics, libass's ascent and descent, HarfBuzz scale, emboldening strength,
 * PostScript flag and decoration lines; kerning at the size; and per glyph the
 * default-load advance HarfBuzz receives, then the unhinted outline libass draws
 * with its advance, emboldened, slanted, and both (libass's order). The first
 * height prints whole outlines, the others and the synthetic variants FNV-1a
 * digests of the same text.
 */
#include <ft2build.h>
#include FT_FREETYPE_H
#include FT_ADVANCES_H
#include FT_OUTLINE_H
#include FT_TYPE1_TABLES_H
#include FT_SFNT_NAMES_H
#include FT_TRUETYPE_TABLES_H

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void text(const char *key, const char *value) {
  printf("%s %s\n", key, value ? value : "-");
}

static int supported(FT_CharMap charmap) {
  switch (FT_Get_CMap_Format(charmap)) {
  case 0: case 4: case 6: case 10: case 12: case 13: return 1;
  default: return 0;
  }
}

/* The charmap libass selects: the first Windows Unicode subtable, else the first
 * Windows subtable, else FreeType's default, else the first. */
static FT_CharMap text_charmap(FT_Face face) {
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

static FT_UInt text_glyph(FT_Face face, FT_CharMap charmap, FT_ULong code) {
  if (charmap->encoding == FT_ENCODING_MS_SYMBOL) code |= 0xF000;
  return FT_Get_Char_Index(face, code);
}

/* The glyphs whose kerning is compared: the first 256 and the text glyphs of
 * U+0020..U+00FF. */
static int kerned_glyphs(FT_Face face, FT_CharMap textmap, FT_UInt kerned[512]) {
  int count = 0;
  for (FT_UInt g = 0; g < 256 && g < (FT_UInt)face->num_glyphs; g++) kerned[count++] = g;
  if (textmap) {
    for (FT_ULong code = 0x20; code <= 0xff; code++) {
      FT_UInt g = text_glyph(face, textmap, code);
      if (g >= 256) kerned[count++] = g;
    }
  }
  return count;
}

static int dump(FT_Library library, const char *path, long index) {
  FT_Face face;
  FT_Error error = FT_New_Face(library, path, index, &face);
  if (error) {
    printf("face %ld error %d\n", index, error);
    return 1;
  }
  printf("face %ld %ld %u %d %d %d %d %d %d\n", index, face->num_glyphs,
         face->units_per_EM, face->ascender, face->descender, face->height,
         face->max_advance_width, !!(face->style_flags & FT_STYLE_FLAG_BOLD),
         !!(face->style_flags & FT_STYLE_FLAG_ITALIC));
  text("family", face->family_name);
  text("style", face->style_name);
  text("postscript", FT_Get_Postscript_Name(face));
  TT_OS2 *os2 = FT_Get_Sfnt_Table(face, FT_SFNT_OS2);
  printf("weight %u\n", os2 ? os2->usWeightClass : 0);

  FT_UInt names = FT_Get_Sfnt_Name_Count(face);
  for (FT_UInt i = 0; i < names; i++) {
    FT_SfntName name;
    if (FT_Get_Sfnt_Name(face, i, &name)) continue;
    printf("name %u %u %u %u ", name.platform_id, name.encoding_id, name.language_id,
           name.name_id);
    for (FT_UInt k = 0; k < name.string_len; k++) printf("%02x", name.string[k]);
    printf("\n");
  }

  FT_CharMap textmap = text_charmap(face);
  for (int i = 0; i < face->num_charmaps; i++) {
    FT_CharMap c = face->charmaps[i];
    if (!supported(c)) continue;
    printf("charmap %u %u %ld\n", c->platform_id, c->encoding_id, FT_Get_CMap_Format(c));
    FT_Set_Charmap(face, c);
    FT_UInt glyph;
    FT_ULong code = FT_Get_First_Char(face, &glyph);
    while (glyph != 0) {
      printf("map %lu %u\n", code, glyph);
      code = FT_Get_Next_Char(face, code, &glyph);
    }
  }
  if (textmap) {
    FT_Set_Charmap(face, textmap);
    printf("textmap %u %u %ld\n", textmap->platform_id, textmap->encoding_id,
           FT_Get_CMap_Format(textmap));
  } else {
    printf("textmap -\n");
  }

  FT_UInt kerned[512];
  int count = kerned_glyphs(face, textmap, kerned);
  if (FT_HAS_KERNING(face)) {
    for (int a = 0; a < count; a++) {
      for (int b = 0; b < count; b++) {
        FT_Vector delta;
        if (!FT_Get_Kerning(face, kerned[a], kerned[b], FT_KERNING_UNSCALED, &delta) && delta.x)
          printf("kern %u %u %ld\n", kerned[a], kerned[b], delta.x);
      }
    }
  }

  error = FT_Set_Char_Size(face, 0, (FT_F26Dot6)face->units_per_EM * 64, 72, 72);
  if (error) {
    printf("size error %d\n", error);
    FT_Done_Face(face);
    return 1;
  }
  const FT_Int32 outline_flags = FT_LOAD_NO_HINTING | FT_LOAD_NO_BITMAP | FT_LOAD_NO_AUTOHINT |
                                 FT_LOAD_NO_SVG;
  for (long g = 0; g < face->num_glyphs; g++) {
    FT_Fixed advance = 0;
    if (FT_Get_Advance(face, (FT_UInt)g, FT_LOAD_NO_SCALE, &advance)) advance = -1;
    error = FT_Load_Glyph(face, (FT_UInt)g, FT_LOAD_NO_SCALE | FT_LOAD_NO_BITMAP | FT_LOAD_NO_SVG);
    long loaded = error ? -1 : face->glyph->metrics.horiAdvance;
    if (!error) error = FT_Load_Glyph(face, (FT_UInt)g, outline_flags);
    if (error) {
      printf("glyph %ld error %d\n", g, error);
      continue;
    }
    FT_Outline *o = &face->glyph->outline;
    printf("glyph %ld %ld %ld %d |", g, (long)advance, loaded, o->n_contours);
    for (int c = 0; c < o->n_contours; c++) printf(" %d", o->contours[c] + 1);
    printf(" |");
    for (int p = 0; p < o->n_points; p++)
      printf(" %ld %ld %d", o->points[p].x, o->points[p].y, FT_CURVE_TAG(o->tags[p]));
    printf("\n");
  }
  FT_Done_Face(face);
  return 0;
}

/* ---- Scaled glyphs, as libass loads them ---------------------------------- */

/* A growing text buffer for one outline, so that it can be printed or hashed. */
static char *text_buf;
static size_t text_len, text_cap;

static void put(const char *format, long a, long b, long c, int args) {
  char item[96];
  int n = args == 3 ? snprintf(item, sizeof item, format, a, b, c)
                    : snprintf(item, sizeof item, format, a);
  if (text_len + (size_t)n + 1 > text_cap) {
    text_cap = (text_len + (size_t)n + 1) * 2;
    text_buf = realloc(text_buf, text_cap);
    if (!text_buf) abort();
  }
  memcpy(text_buf + text_len, item, (size_t)n + 1);
  text_len += (size_t)n;
}

/* `n | ends | x y tag ...`, the outline part of a `glyph` line. */
static void outline_text(const FT_Outline *o) {
  text_len = 0;
  put("%ld |", o->n_contours, 0, 0, 1);
  for (int c = 0; c < o->n_contours; c++) put(" %ld", o->contours[c] + 1, 0, 0, 1);
  put(" |", 0, 0, 0, 1);
  for (int p = 0; p < o->n_points; p++)
    put(" %ld %ld %ld", o->points[p].x, o->points[p].y, FT_CURVE_TAG(o->tags[p]), 3);
}

/* FNV-1a, 64 bits, of the outline text. */
static unsigned long long digest(void) {
  unsigned long long h = 0xcbf29ce484222325ULL;
  for (size_t i = 0; i < text_len; i++) h = (h ^ (unsigned char)text_buf[i]) * 0x100000001b3ULL;
  return h;
}

/* libass `set_font_metrics`: GDI's ascender, descender and height. */
static void gdi_metrics(FT_Face face) {
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

/* A libass decoration line from a table position and size, y up. */
static void band(const char *key, int present, long position, long size, long y_scale) {
  if (present) {
    long long pos = ((long long)position * y_scale + 0x8000) >> 16;
    long long thick = ((long long)size * y_scale + 0x8000) >> 16;
    pos = -pos - (thick >> 1);
    const long long max = (1LL << 28) - 1;
    if (pos >= -max && pos + thick <= max) {
      printf(" %s %lld %lld", key, -(pos + thick), -pos);
      return;
    }
  }
  printf(" %s -", key);
}

static const FT_Int32 libass_flags = FT_LOAD_NO_BITMAP | FT_LOAD_IGNORE_GLOBAL_ADVANCE_WIDTH |
                                     FT_LOAD_IGNORE_TRANSFORM;

static void dump_size(FT_Library library, FT_Face face, long height, int detailed,
                      const FT_UInt *kerned, int count) {
  FT_Size_RequestRec rq;
  memset(&rq, 0, sizeof rq);
  rq.type = FT_SIZE_REQUEST_TYPE_REAL_DIM;
  rq.height = height;
  FT_Error error = FT_Request_Size(face, &rq);
  FT_Size_Metrics *m = &face->size->metrics;
  if (error || !m->x_ppem || !m->y_ppem) {
    printf("size %ld error\n", height);
    return;
  }
  PS_FontInfoRec info;
  int postscript = !FT_Get_PS_Font_Info(face, &info);
  printf("size %ld %u %u %ld %ld %ld %ld %ld %ld", height, m->x_ppem, m->y_ppem, m->x_scale,
         m->y_scale, m->ascender, m->descender, m->height, m->max_advance);
  /* ass_font_get_asc_desc, update_hb_size and ass_glyph_embolden */
  printf(" %d %d", (int)FT_MulFix(face->ascender, m->y_scale),
         (int)FT_MulFix(-face->descender, m->y_scale));
  printf(" %d %d", (int)(((unsigned long long)m->x_scale * face->units_per_EM) >> 16),
         (int)(((unsigned long long)m->y_scale * face->units_per_EM) >> 16));
  int strength = (int)(FT_MulFix(face->units_per_EM, m->y_scale) / 64);
  printf(" %d %d", strength, postscript);
  TT_Postscript *ps = FT_Get_Sfnt_Table(face, FT_SFNT_POST);
  band("underline", ps && ps->underlinePosition <= 0 && ps->underlineThickness > 0,
       ps ? ps->underlinePosition : 0, ps ? ps->underlineThickness : 0, m->y_scale);
  TT_OS2 *os2 = FT_Get_Sfnt_Table(face, FT_SFNT_OS2);
  band("strikeout", os2 && os2->yStrikeoutPosition >= 0 && os2->yStrikeoutSize > 0,
       os2 ? os2->yStrikeoutPosition : 0, os2 ? os2->yStrikeoutSize : 0, m->y_scale);
  printf("\n");

  if (FT_HAS_KERNING(face)) {
    for (int a = 0; a < count; a++) {
      for (int b = 0; b < count; b++) {
        FT_Vector delta;
        if (!FT_Get_Kerning(face, kerned[a], kerned[b], FT_KERNING_DEFAULT, &delta) && delta.x)
          printf("kern %u %u %ld\n", kerned[a], kerned[b], delta.x);
      }
    }
  }

  /* ass_glyph_italicize's shear */
  FT_Matrix shear = {0x10000L, postscript ? 0x02d24L : 0x05700L, 0, 0x10000L};
  FT_Outline copy;
  for (long g = 0; g < face->num_glyphs; g++) {
    /* What HarfBuzz receives: the default (hinted) advance. */
    if (FT_Load_Glyph(face, (FT_UInt)g, FT_LOAD_DEFAULT | FT_LOAD_IGNORE_GLOBAL_ADVANCE_WIDTH |
                                            FT_LOAD_IGNORE_TRANSFORM))
      printf("hinted %ld error\n", g);
    else
      printf("hinted %ld %ld\n", g, face->glyph->metrics.horiAdvance);

    /* What libass draws with hinting off. */
    if (FT_Load_Glyph(face, (FT_UInt)g, libass_flags | FT_LOAD_NO_HINTING)) {
      printf("glyph %ld error\n", g);
      continue;
    }
    FT_Outline *o = &face->glyph->outline;
    outline_text(o);
    if (detailed)
      printf("glyph %ld %ld %s", g, face->glyph->advance.x, text_buf);
    else
      printf("glyph %ld %ld %016llx", g, face->glyph->advance.x, digest());
    if (FT_Outline_New(library, (FT_UInt)o->n_points, o->n_contours, &copy)) abort();
    FT_Outline_Copy(o, &copy);
    FT_Outline_Embolden(&copy, strength);
    outline_text(&copy);
    printf(" bold %016llx", digest());
    FT_Outline_Copy(o, &copy);
    FT_Outline_Transform(&copy, &shear);
    outline_text(&copy);
    printf(" oblique %016llx", digest());
    FT_Outline_Embolden(&copy, strength);
    outline_text(&copy);
    printf(" both %016llx\n", digest());
    FT_Outline_Done(library, &copy);
  }
}

/* Every face at every size in `sizes` (26.6 heights, comma-separated); the
 * first size prints whole outlines, the others their digests. */
static int dump_scaled(FT_Library library, const char *path, long index, const char *sizes) {
  FT_Face face;
  if (FT_New_Face(library, path, index, &face)) {
    printf("face %ld error\n", index);
    return 1;
  }
  printf("face %ld\n", index);
  gdi_metrics(face);
  FT_CharMap textmap = text_charmap(face);
  if (textmap) FT_Set_Charmap(face, textmap);
  FT_UInt kerned[512];
  int count = kerned_glyphs(face, textmap, kerned);
  char *list = strdup(sizes);
  if (!list) abort();
  int first = 1;
  for (char *t = strtok(list, ","); t; t = strtok(NULL, ","), first = 0)
    dump_size(library, face, strtol(t, NULL, 10), first, kerned, count);
  free(list);
  FT_Done_Face(face);
  return 0;
}

int main(int argc, char **argv) {
  const char *sizes = NULL;
  if (argc == 4 && !strcmp(argv[1], "--scaled")) {
    sizes = argv[2];
  } else if (argc != 2) {
    fprintf(stderr, "usage: freetype-font [--scaled HEIGHT,...] FILE\n");
    return 2;
  }
  const char *path = argv[argc - 1];
  FT_Library library;
  if (FT_Init_FreeType(&library)) return 2;
  FT_Face probe;
  if (FT_New_Face(library, path, -1, &probe)) {
    fprintf(stderr, "%s: not a font FreeType opens\n", path);
    return 2;
  }
  long faces = probe->num_faces;
  FT_Done_Face(probe);
  int failed = 0;
  for (long i = 0; i < faces; i++)
    failed |= sizes ? dump_scaled(library, path, i, sizes) : dump(library, path, i);
  FT_Done_FreeType(library);
  free(text_buf);
  return failed;
}
