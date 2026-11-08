# font-worker

Font measurement worker for the harness. It reads every face of a font file through
[triergon-font](../font/README.md), as the FreeType reference worker
(`harness/reference_font.h`) does with FreeType. It is tooling (`publish = false`),
not part of the library.

## Command line

```text
font-worker font-fold   FILE
font-worker font-bench  FILE ITERATIONS
font-worker font-memory FILE
font-worker font-dump   FILE
```

One operation parses every face of the file from memory and reads, per face: the
metrics and style flags, the family, style and PostScript names in FreeType's ASCII
forms, the `OS/2` weight, every name record, every supported cmap subtable with all
of its mappings, the text charmap, the kerning of every pair the `kern` subtables
list (sorted, unique, nonzero values only), and for every glyph its `hmtx` advance,
its loaded advance and its outline at 1/64 font unit. A face or glyph the reader
rejects is recorded as such and reading goes on.

The fold, bench and memory modes print the harness protocol's one JSON line:
`packets` counts glyph outlines, `bytes` their points and `digest` folds every value
above in a fixed order (`src/record.rs`). `font-bench` adds the elapsed time of the
timed loop and `font-memory` brackets one operation with the allocation-window
markers; reading the file is outside both. `font-dump` prints the same values as
lines, identical to the reference's `font-dump`, so a failing case can be diffed.
The corpus is `harness/features/font-outlines/`.
