# font-worker

Font measurement worker for the harness. It reads every face of a font file through
[triergon-font](../font/README.md), as the FreeType reference worker
(`harness/reference_font.h`) does with FreeType. It is tooling (`publish = false`),
not part of the library.

## Command line

```text
font-worker OPERATION-fold   FILE [MODE HEIGHT]
font-worker OPERATION-bench  FILE ITERATIONS [MODE HEIGHT]
font-worker OPERATION-memory FILE [MODE HEIGHT]
font-worker OPERATION-dump   FILE [MODE HEIGHT]
```

One operation parses every face of the file from memory and reads, per face:

| OPERATION | MODE HEIGHT | Read |
|---|---|---|
| `font` | none | the metrics and style flags, the family, style and PostScript names in FreeType's ASCII forms, the `OS/2` weight, every name record, every supported cmap subtable with all of its mappings, the text charmap, the kerning of every pair the `kern` subtables list (sorted, unique, nonzero values only), and for every glyph its `hmtx` advance, its loaded advance and its unscaled outline in font units (`FT_LOAD_NO_SCALE`) |
| `font-sized` | `outlines`, 26.6 pixels | the face at that size as libass loads it: the size metrics, ascent and descent, HarfBuzz scale, emboldening strength, PostScript flag, underline and strikeout bands and the kerning of every listed pair; for every glyph its unhinted outline and advance, then that outline emboldened, slanted and both |
| `font-sized` | `advances`, 26.6 pixels | every glyph's default-load (hinted) advance at that size |
| `font-sized` | `vertical`, 26.6 pixels | every glyph's `linearVertAdvance` and `metrics.vertAdvance` of the unhinted load, then `metrics.vertAdvance` of the default load |
| `font-sized` | `default`, 26.6 pixels | every glyph's default load (`FT_LOAD_DEFAULT`): `metrics.horiAdvance`, `vertAdvance`, `horiBearingX`, `horiBearingY`, `width` and `height`, then its hinted outline |

A face, size or glyph the reader rejects is recorded as such and reading goes on.
A default-load value the crate declares `Unsupported` is not a value: the
`advances`, `vertical` and `default` modes then exit with an error naming the
face, size and glyph.

The fold, bench and memory modes print the harness protocol's one JSON line:
`packets` counts outlines (or glyphs in the `advances`, `vertical` and `default`
modes), `bytes` outline points and
`digest` folds every value above in a fixed order (`src/record.rs`). The bench
mode adds the elapsed time of the timed loop and the memory mode brackets one
operation with the allocation-window markers; reading the file is outside both.
The dump mode prints the same values as lines, identical to the reference's
dump, so a failing case can be diffed. The features are
`harness/features/font-outlines/`, `font-hinting-none/`, `font-hinting-default/`
and `font-hinting-modes/`.
