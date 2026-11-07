/*
 * FreeType reference for the triergon-font comparison.
 *
 * Usage: freetype-font FILE
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
 */
#include <ft2build.h>
#include FT_FREETYPE_H
#include FT_ADVANCES_H
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

  /* Kerning: the first 256 glyphs and the text glyphs of U+0020..U+00FF. */
  FT_UInt kerned[512];
  int count = 0;
  for (FT_UInt g = 0; g < 256 && g < (FT_UInt)face->num_glyphs; g++) kerned[count++] = g;
  if (textmap) {
    for (FT_ULong code = 0x20; code <= 0xff; code++) {
      FT_UInt g = text_glyph(face, textmap, code);
      if (g >= 256) kerned[count++] = g;
    }
  }
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

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: freetype-font FILE\n");
    return 2;
  }
  FT_Library library;
  if (FT_Init_FreeType(&library)) return 2;
  FT_Face probe;
  if (FT_New_Face(library, argv[1], -1, &probe)) {
    fprintf(stderr, "%s: not a font FreeType opens\n", argv[1]);
    return 2;
  }
  long faces = probe->num_faces;
  FT_Done_Face(probe);
  int failed = 0;
  for (long i = 0; i < faces; i++) failed |= dump(library, argv[1], i);
  FT_Done_FreeType(library);
  return failed;
}
