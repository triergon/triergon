# raster-worker

Rasterising measurement worker for the harness. It performs libass's outline,
stroker, rasteriser, blur, composition and clipping stages through
[triergon-raster](../raster/README.md), with glyph outlines loaded by
[triergon-font](../font/README.md), as the libass reference worker
(`harness/reference_raster.h`) does with libass 0.17.5 and FreeType 2.15.0. It is
tooling (`publish = false`), not part of the library.

## Command line

```text
raster-worker raster-fold   FILE MODE...
raster-worker raster-bench  FILE ITERATIONS MODE...
raster-worker raster-memory FILE MODE...
raster-worker raster-dump   FILE MODE...
```

| MODE | FILE | Cases |
|---|---|---|
| `glyphs HEIGHT COUNT CASE...` | a font file | each argument one case; `g K` names the k-th of `COUNT` glyph ids spread evenly over face 0, loaded unhinted at the 26.6 pixel `HEIGHT` as libass loads them with hinting off |
| `lines` | a case file | one case per line, outlines given inline |

The case grammar is in `src/case.rs`: outline conversion, rotation, stroking,
transforms, rasterisation, runs of glyphs through the glyph pipeline and the
compositor, blur, the bitmap blends, the border's fill subtraction, shifts and
clipping. A case outside the grammar is recorded as unparsed, a source or
parameter libass rejects as invalid, and a case the crate refuses where libass
draws as failed, so the case fails. The fold, bench and memory modes print the
harness protocol's one JSON line, whose `digest` folds every record in a fixed
order (`src/record.rs`); the bench mode adds the elapsed time of the timed loop
and the memory mode brackets one operation with the allocation-window markers.
Reading the file, loading the glyphs and parsing the cases are outside both.
The dump mode prints the same records as lines, identical to the reference's
dump. The features are `harness/features/raster-*/`.
