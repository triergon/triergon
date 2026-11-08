# shape-worker

Shaping measurement worker for the harness. It shapes text, resolves bidi and
reads Unicode properties through [triergon-shape](../shape/README.md), as the
HarfBuzz and FriBidi reference worker (`harness/reference_shape.h`) does with
HarfBuzz 14.6.0 and FriBidi 1.0.17, and with FreeType 2.15.0 for libass's font
functions. It is tooling (`publish = false`), not part
of the library.

## Command line

```text
shape-worker OPERATION-fold   FILE ARGUMENT...
shape-worker OPERATION-bench  FILE ITERATIONS ARGUMENT...
shape-worker OPERATION-memory FILE ARGUMENT...
shape-worker OPERATION-dump   FILE ARGUMENT...
```

| OPERATION | FILE | ARGUMENT... | Read |
|---|---|---|---|
| `shape` | a font file | `DIRECTION SCRIPT LANGUAGE FEATURES TEXT...` | every text shaped as one run with every face of the file at its default scale: the glyph count, then every glyph's id, cluster, advances and offsets; packets count glyphs and bytes code points |
| `shape-libass` | a font file | `SIZE DIRECTION SCRIPT LANGUAGE FEATURES TEXT...` | as `shape`, but every face at the 26.6 pixel height `SIZE` through `SizedMetrics`, libass's font functions; a face it cannot set up is recorded as rejected |
| `bidi` | text, one paragraph or line set per line | `BASE BRACKETS` | levels per paragraph and each line reordered as libass does: directions, levels and the visual-to-logical map; packets count lines |
| `simple` | text, one line each | `BASE` | libass's simple shaping level: the shaped code points; packets count lines |
| `unicode` | code point ranges | none | every code point's HarfBuzz and FriBidi properties; packets count code points |

`DIRECTION` and `BASE` are `ltr`, `rtl` (and `on` for automatic); `SCRIPT` an ISO
15924 tag; `FEATURES` `-` or comma-separated `TAG=VALUE`; a `TEXT` upper-case
hexadecimal code points separated by spaces, optionally prefixed by
`START,LENGTH:` to shape that item with the rest as context.

A face, text or line the crate rejects is recorded as such and reading goes on, so
the case fails. The fold, bench and memory modes print the harness protocol's one
JSON line, whose `digest` folds every value in a fixed order (`src/record.rs`);
the bench mode adds the elapsed time of the timed loop and the memory mode
brackets one operation with the allocation-window markers. Reading the file is
outside both. The dump mode prints the same values as lines, identical to the
reference's dump. The features are `harness/features/shape-*/`.
