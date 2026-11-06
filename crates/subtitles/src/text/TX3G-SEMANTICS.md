# tx3g sample semantics

Rules implemented by `text::tx3g_semantics::SemanticJob` against ETSI TS 126 245
V18.0.0 §§5.1–5.2, 5.6–5.8 and 5.15–5.18. The output is style and effect intent
for a renderer; no glyph rendering, font substitution, bidi reordering or ASS
output happens here. The source `Cue` and its modifier boxes stay attached.

## Inputs

`SemanticJob::new` takes an `Input`, an optional `ConfigLayout` for the sample
description, an optional non-zero track timescale, a `Budget`, `SemanticLimits`
and the expected generation. It runs the tx3g `Extractor` (ISO-box form) and
`ConfigJob` itself; caller-built `Cue` or `Configuration` values are not trusted.
A rejected constructor leaves the caller's buffers untouched.

## Grammar

- Interpreted boxes: `styl`, `hlit`, `hclr`, `krok`, `dlay`, `href`, `tbox`,
  `blnk`, `twrp`, `disp`. Unknown boxes stay in the source `Cue` with no effect.
- Field lengths and counts must match the box body exactly; trailing bytes,
  short records and invalid UTF-8 URLs are errors.
- Offsets count Unicode scalars, including line breaks; a surrogate pair is one
  scalar and the UTF-16 BOM is excluded. Ranges are half-open.
- Highlight and karaoke may end at N+1 (one past the last character); this is
  flagged explicitly and not applied to other boxes.
- Records of one type must be ordered and non-overlapping across boxes.
- `hclr`, `dlay`, `tbox`, `twrp`, `disp` and `krok` may appear at most once.
  A duplicate is an error; there is no last-box-wins repair.

## Resolution

- Static runs cover every character exactly once, with raw and UTF-8 byte spans,
  the requested style, font binding, highlight, hyperlink and blink membership.
- A `styl` record overrides the description style only over its range; face
  bits outside bold/italic/underline are kept as `uninterpreted_face_bits`.
- Hyperlink presentation is left to the renderer; the requested style stays
  visible. Static highlight with `hclr` is a background-colour intent; without
  it, an inverse intent. Dynamic (karaoke) highlight is a foreground intent.
- Karaoke intervals are half-open; zero-length records never activate. Dynamic
  highlight may not overlap static highlight or hyperlinks.
- `tbox` and `disp` in the sample override the description. Disparity stays in
  signed 1/16 pixels; `disparity_pixels()` truncates toward zero.
- Scroll delay without `dlay` is the specified default of zero.
- Font IDs must exist in a supplied description. Without one, fonts stay
  unresolved. Font names are exposed raw and as strict UTF-8, untrimmed.

## Time and queries

Karaoke and scroll delays are track ticks. Exact `Time` is derived only from a
supplied timescale. `PointQuery` returns the `PointStyle` at a character and
optional tick without allocating; an unknown time yields `Truth::Unknown` where
a timed effect could apply.

## Resources

All storage comes from the caller's `Budget`. Every loop is paid from the
`WorkBudget`; zero work makes no progress or allocation. Cancellation and output
pressure are resumable. Fatal errors are sticky and publish nothing.
