/*
 * ASS rendering measurement reference: libass 0.17.5 linked as a player links
 * it (tooling only; never linked into the library). None of libass's sources is
 * compiled into this worker: every libass function it runs is the one in the
 * built libass.a, so its time and memory are libass's. It renders frames as
 * reference_ass_render.h describes and folds the same values as the parity
 * reference (reference_ass.h), which compiles libass's rendering, drawing and
 * parsing sources into itself with checks of its own and is never timed or
 * measured.
 *
 * Usage: the ass-render-fold, -bench, -memory and -dump modes of
 * reference_ass_render.h.
 *
 * libass's private headers give the renderer's bitmap engine, which the
 * reference replaces with the plain C one, and ASS_ImagePriv, which tells the
 * box behind a BorderStyle 4 event by its buffer. They are included after
 * config.h and ass_compat.h, as libass's own sources include them, so every
 * structure has the library's layout.
 */
#include "config.h"
#include "ass_compat.h"

#include "ass.h"
#include "ass_bitmap_engine.h"
#include "ass_rasterizer.h"
#include "ass_render.h"

/* The rasteriser's spline approximation error, in 1/64 pixel, that
 * the renderer gives it. */
#define RASTERIZER_PRECISION 16

#include "reference_ass_fixture.h"
#include "reference_ass_render.h"

int main(int argc, char **argv) {
    if (argc > 1 && !strncmp(argv[1], "ass-render-", 11)) return assr_render_main(argc, argv);
    fputs("usage: reference ass-render-fold | ass-render-bench | ass-render-memory | ass-render-dump ...\n", stderr);
    return 2;
}
