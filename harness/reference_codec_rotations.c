/* Compare direct metrics with FreeType's public default load. Two libass
 * character orderings supply the advance selection through a rendered marker. */
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <ass/ass.h>
#include <ft2build.h>
#include FT_FREETYPE_H
#include FT_TRUETYPE_TABLES_H

#include "reference_digest.h"

typedef struct {
    uint64_t digest, records;
    int trace;
} RotationWork;

static void rotation_row(RotationWork *work, size_t count, const uint64_t *words)
{
    fold(&work->digest, 3);
    for (size_t i = 0; i < count; ++i)
        fold(&work->digest, words[i]);
    ++work->records;
    if (work->trace) {
        fputs("3", stdout);
        for (size_t i = 0; i < count; ++i)
            printf(" %" PRIu64, words[i]);
        putchar('\n');
    }
}

#define ROW(work, ...) rotation_row(work, sizeof((uint64_t[]){__VA_ARGS__}) / sizeof(uint64_t), \
                                     (uint64_t[]){__VA_ARGS__})

static void quiet(int level, const char *format, va_list args, void *opaque)
{
    (void) level;
    (void) format;
    (void) args;
    (void) opaque;
}

static int marker_advances(const char *font, int length, int size, const char *first,
                           const char *second, int positions[2])
{
    char script[2048];
    int n = snprintf(script, sizeof script,
        "[Script Info]\nScriptType: v4.00+\nPlayResX: 1280\nPlayResY: 720\nWrapStyle: 2\n"
        "[V4+ Styles]\n"
        "Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour,"
        " BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle,"
        " BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n"
        "Style: Alias,@Arimo,%d,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,"
        "0,0,0,0,100,100,0,0,1,0,0,7,10,10,10,1\n"
        "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n"
        "Dialogue: 0,0:00:00.00,0:00:01.00,Alias,,0,0,0,,{\\pos(100,300)\\1c&H0000FF&}A\n"
        "Dialogue: 0,0:00:00.00,0:00:01.00,Alias,,0,0,0,,{\\pos(100,100)}%s{\\1c&H0000FF&}A\n"
        "Dialogue: 0,0:00:00.00,0:00:01.00,Alias,,0,0,0,,{\\pos(100,500)}%s{\\1c&H0000FF&}A\n",
        size, first, second);
    if (n < 0 || (size_t)n >= sizeof script)
        return 0;
    ASS_Library *library = ass_library_init();
    if (!library)
        return 0;
    ass_set_message_cb(library, quiet, NULL);
    ass_add_font(library, "Arimo-Regular.ttf", font, length);
    ASS_Renderer *renderer = ass_renderer_init(library);
    ASS_Track *track = ass_read_memory(library, script, (size_t)n, NULL);
    int x[] = {-1, -1, -1};
    int ok = 0;
    if (renderer && track) {
        ass_set_frame_size(renderer, 1280, 720);
        ass_set_storage_size(renderer, 1280, 720);
        ass_set_fonts(renderer, NULL, "Arimo", ASS_FONTPROVIDER_NONE, NULL, 0);
        ass_set_shaper(renderer, ASS_SHAPING_COMPLEX);
        for (const ASS_Image *image = ass_render_frame(renderer, track, 250, NULL);
             image; image = image->next) {
            if (image->color != UINT32_C(0xff000000) || !image->w || !image->h)
                continue;
            int band = image->dst_y < 290 ? 0 : image->dst_y < 490 ? 1 : 2;
            if (x[band] >= 0)
                goto done;
            x[band] = image->dst_x;
        }
        ok = x[0] >= 0 && x[1] >= 0 && x[2] >= 0;
        positions[0] = x[0] - x[1];
        positions[1] = x[2] - x[1];
    }
done:
    if (track) ass_free_track(track);
    if (renderer) ass_renderer_done(renderer);
    ass_library_done(library);
    return ok;
}

/* Image origins are integer pixels and vary by one pixel with marker layout.
 * Use the image to distinguish the two advances, then compare the selected
 * FreeType value in 26.6 units. */
static int selected(int marker, long advance, long other)
{
    return labs(marker * 64L - advance) <= 64 && labs(marker * 64L - other) > 64;
}

static void extents(RotationWork *work, const FT_Glyph_Metrics *m)
{
    ROW(work, 4, 1, (uint64_t)m->horiBearingX, (uint64_t)m->horiBearingY,
        (uint64_t)m->width, (uint64_t)-m->height);
}

static int rotations(RotationWork *work, const char *font, int length, int size,
                     int small, const char *order)
{
    const uint32_t lower = small ? 0x259 : 0x18f;
    const uint32_t upper = small ? 0x4d9 : 0x4d8;
    const char *lower_text = small ? "ə" : "Ə";
    const char *upper_text = small ? "ә" : "Ә";
    FT_Library library;
    FT_Face face;
    if (FT_Init_FreeType(&library))
        return 0;
    int ok = 0;
    if (FT_New_Memory_Face(library, (const FT_Byte *)font, length, 0, &face))
        goto library_done;
    /* The public shape-libass reference sizes faces on the Windows cell. */
    const TT_OS2 *os2 = FT_Get_Sfnt_Table(face, FT_SFNT_OS2);
    if (!os2 || !(os2->usWinAscent + os2->usWinDescent))
        goto face_done;
    face->ascender = (FT_Short)os2->usWinAscent;
    face->descender = (FT_Short)-os2->usWinDescent;
    face->height = face->ascender - face->descender;
    FT_Size_RequestRec request = {.type = FT_SIZE_REQUEST_TYPE_REAL_DIM, .height = size * 64};
    if (FT_Request_Size(face, &request))
        goto face_done;
    FT_UInt glyph = FT_Get_Char_Index(face, lower);
    if (!glyph || FT_Get_Char_Index(face, upper) != glyph ||
        FT_Load_Glyph(face, glyph, FT_LOAD_DEFAULT | FT_LOAD_IGNORE_GLOBAL_ADVANCE_WIDTH |
                                 FT_LOAD_IGNORE_TRANSFORM))
        goto face_done;
    const FT_Glyph_Metrics metrics = face->glyph->metrics;
    const long horizontal = metrics.horiAdvance, vertical = metrics.vertAdvance;
    int low_first[2], upper_first[2];
    if (!marker_advances(font, length, size, lower_text, upper_text, low_first) ||
        !marker_advances(font, length, size, upper_text, lower_text, upper_first))
        goto face_done;
    if (!selected(low_first[0], horizontal, vertical) ||
        !selected(low_first[1], horizontal, vertical) ||
        !selected(upper_first[0], vertical, horizontal) ||
        !selected(upper_first[1], vertical, horizontal)) {
        fprintf(stderr, "marker advances: lower-first %d,%d upper-first %d,%d; FreeType %ld,%ld\n",
                low_first[0], low_first[1], upper_first[0], upper_first[1], horizontal, vertical);
        goto face_done;
    }

    const uint32_t points[] = {0, lower, 0x2f0, 0x2f1, upper, 0x10ffff};
    for (size_t i = 0; i < sizeof points / sizeof points[0]; ++i)
        ROW(work, 0, points[i], points[i] >= 0x2f1);
    ROW(work, 1, glyph, glyph);
    /* libass exposes no glyph-ID lookup before a character. The documented
     * first-lookup rule selects FreeType's horizontal advance in these orders. */
    long first_advance = horizontal;
    if (!strcmp(order, "lower"))
        ROW(work, 2, lower, glyph);
    else if (!strcmp(order, "upper")) {
        first_advance = vertical;
        ROW(work, 2, upper, glyph);
    } else if (!strcmp(order, "advance"))
        ROW(work, 3, (uint64_t)horizontal);
    else if (!strcmp(order, "extents"))
        extents(work, &metrics);
    else
        goto face_done;

    for (int key = 0; key < 3; ++key) {
        if (key)
            ROW(work, 2, upper, glyph);
        const long advance = key == 2 ? vertical : first_advance;
        ROW(work, 2, upper, glyph);
        ROW(work, 2, lower, glyph);
        ROW(work, 3, (uint64_t)advance);
        extents(work, &metrics);
        ROW(work, 3, (uint64_t)advance);
        ROW(work, 5);
    }
    ok = 1;
face_done:
    FT_Done_Face(face);
library_done:
    FT_Done_FreeType(library);
    return ok;
}

int main(int argc, char **argv)
{
    if (argc != 6 || (strcmp(argv[1], "rotations-fold") && strcmp(argv[1], "rotations-trace")))
        return 2;
    const int size = atoi(argv[3]);
    const int small = !strcmp(argv[4], "small");
    if (size <= 0 || size > 128 || (!small && strcmp(argv[4], "capital")))
        return 2;
    FILE *file = fopen(argv[2], "rb");
    if (!file)
        return 2;
    long size_bytes = fseek(file, 0, SEEK_END) == 0 ? ftell(file) : -1;
    char *font = size_bytes > 0 && size_bytes <= 16L * 1024 * 1024 ? malloc((size_t)size_bytes) : NULL;
    int loaded = font && !fseek(file, 0, SEEK_SET) &&
                 fread(font, 1, (size_t)size_bytes, file) == (size_t)size_bytes;
    fclose(file);
    RotationWork work = {.trace = !strcmp(argv[1], "rotations-trace")};
    int ok = loaded && rotations(&work, font, (int)size_bytes, size, small, argv[5]);
    /* The common digest header also provides the payload fold. This operation
     * compares only scalar glyph metrics. */
    fold_bytes(&work.digest, NULL, 0);
    free(font);
    if (!ok)
        return 2;
    printf("{\"schema\":1,\"operation\":\"libass-rotations\",\"iterations\":1,\"packets\":%" PRIu64
           ",\"bytes\":0,\"digest\":%" PRIu64 ",\"elapsed_ns\":0,\"eof\":true}\n",
           work.records, work.digest);
    return 0;
}
