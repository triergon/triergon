# triergon-shape

Text shaping for a subtitle renderer, as libass 0.17.5 shapes with HarfBuzz 14.6.0
and FriBidi 1.0.17: OpenType layout and the script shapers, the Unicode
bidirectional algorithm with FriBidi's results, and libass's simple shaping
level. It depends on [`triergon-core`](../core) and reads fonts through
[`triergon-font`](../font). Run splitting, script resolution, font fallback and
line breaking belong to the renderer.

## Usage

```rust,no_run
use triergon_core::{Budget, Bytes};
use triergon_font::{Font, Limits};
use triergon_shape::{Buffer, Direction, Face, Feature, Language, OtMetrics, Plan, Scale, Script, Tag};

let pool = Budget::new(64 << 20)?;
let file = Bytes::copy_from_slice(&pool, &std::fs::read("NotoSansArabic-Regular.ttf")?)?;
let font = Font::new(file, 0, &pool, Limits::default())?;
let face = Face::new(&font);

// libass's features for a horizontal run without extra spacing.
let features = [b"vert", b"vkna", b"kern", b"liga", b"clig"]
    .iter()
    .zip([0, 0, 1, 1, 1])
    .map(|(tag, value)| Feature { tag: Tag::new(tag), value })
    .collect::<Vec<_>>();
// Build a plan once per face and run properties, then reuse it.
let plan = Plan::new(&face, Direction::RightToLeft, Script::from_iso15924(*b"Arab"), &Language::new("ar"), &features, &pool)?;

let text: Vec<char> = "مرحبا بالعالم".chars().collect();
// HarfBuzz's default scale: font units.
let metrics = OtMetrics::new(&font, Scale::units(font.units_per_em()));
let mut buffer = Buffer::new(&pool); // reuse it for every run
triergon_shape::shape(&face, &metrics, &plan, &text, 0..text.len(), &mut buffer)?;
for (glyph, position) in buffer.glyphs() {
    let _ = (glyph.id, glyph.cluster, position.x_advance, position.x_offset);
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

`shape` shapes `text[item]` with the rest of `text` as context, as
`hb_buffer_add_utf32` does; clusters are indices into `text` and glyphs come in
visual order, positioned in the units of the metrics' `Scale`. A `Plan` is built
once per face and run properties and reused; a `Buffer` keeps its storage
between runs.

## Font functions

`Metrics` is the font-function interface the shaper calls. A callback that cannot
answer as its font would ends the shaping call with its error. Two
implementations come with the crate:

- `OtMetrics` answers as HarfBuzz's built-in functions do at units per em: its
  character map choice among the raw encoding records, its `cmap` lookups, the
  last (0, 5) format 14 subtable's variation sequences, `hmtx` advances, and
  `glyf` and CFF extents. CFF and CFF2 extents are read as HarfBuzz's own
  charstring reading gives them, written from the CFF, Type 2 charstring and CFF2
  specifications and probes of HarfBuzz (`harness/shape/probes`,
  `cargo xtask shape probe`): the box holds every point a charstring draws,
  control points included, without the FontMatrix, each side rounded half up. A
  glyph has no extents when its charstring fails (more than ten nested
  subroutine calls, a moveto or flex with the wrong operands, a CFF charstring
  without `endchar` or a subroutine without `return`, more than 513 operands,
  its 200,000th operation, or an accented `endchar` naming a glyph the charset
  lacks). CFF2 `blend` keeps the default values.
- `SizedMetrics` answers as libass's FreeType-backed functions do over a face at
  a size (`triergon_font::SizedFont`): the character map libass selects, or with
  `SizedMetrics::with_charmap` the one a face has been left on, read as libass
  reads it; default-load advances, extents and contour points; and
  `FT_KERNING_DEFAULT` kerning, at the size's scale and pixels per em, loading
  glyphs within the caller's `WorkBudget`. For a face of an `@` font,
  `SizedMetrics::rotating` takes one face's entries (`FaceRotations`) of libass's
  glyph metrics cache, `GlyphRotations`, which its owner keys by font, face and
  size as libass does: a glyph whose first lookup is through a code point that
  `rotates` (U+02F1 and above) advances by its default-load vertical advance,
  and a glyph first asked for an advance or extents stays horizontal.

`Scale` reproduces HarfBuzz's scaling arithmetic, including hinting device tables
when a ppem is set.

## Bidi and character properties

The bidi API mirrors libass's `ass_shaper_shape` and `ass_shaper_reorder`:
`bidi::BidiClass::of` and `bidi::Bracket::of` per character, `bidi::paragraphs`,
`bidi::Resolver::resolve` per paragraph (base direction left-to-right,
right-to-left or automatic, with or without bracket pairing), `bidi::reorder_line`
per line, and `bidi::shape_simple` for libass's simple shaping level (cursive
joining, Arabic presentation forms and mirroring, as FriBidi applies them).

The resolver implements UAX #9, revision 44, written from the annex with the
Unicode Consortium's C reference implementation of that version as its model
(see [THIRD-PARTY.md](THIRD-PARTY.md)); `cargo xtask shape conformance` runs
Unicode 14.0.0's BidiTest.txt and BidiCharacterTest.txt against it. Where FriBidi
1.0.17's results differ from the annex's on text those files do not test (deep
embeddings and isolates, bracket pairs, marks after neutrals, line ends, text
after a paragraph separator), the results are FriBidi's; each deviation is named
in the module of the rule it changes. Simple shaping joins characters by visual
sides at even levels and forms the LAM-ALEF ligature only from adjacent letters,
as FriBidi does.

The character properties shaping reads are public too, typed: HarfBuzz's
(Unicode 18.0.0) are `unicode::GeneralCategory::of`, `unicode::combining_class`,
`Script::of`, `unicode::mirroring` and `unicode::decompose` (a
`unicode::Decomposition` into one or two characters); the bidi ones (Unicode
14.0.0) are `bidi::BidiClass::of`, `bidi::Bracket::of` with `Bracket::pair`,
`bidi::mirror` and `bidi::JoiningType::of`. The tables are generated by
`cargo xtask shape ucd` from hash-pinned UCD files, each as HarfBuzz's shipped
tables or libass's FriBidi hold them; two bidi properties take FriBidi's values
where they differ from the UCD's (the defaults of unassigned code points in
right-to-left blocks, and the joining type of format characters).

## What it covers

**Pipeline.** The glyph ids, clusters and positions `hb_shape` gives for a
horizontal run with default buffer flags (not the glyph flags it also sets, such
as `HB_GLYPH_FLAG_UNSAFE_TO_BREAK`): Unicode properties and grapheme clusters,
shaping in the script's native direction, mirroring or `rtlm`, normalisation
against the font, fraction masks, the feature map (required features, global
feature search, `rand`), script and language system selection, GDEF classes or
synthesised ones, mark zeroing, attachment resolution, and default ignorables
shown as the space glyph or removed.

**OpenType layout.** GDEF glyph classes, mark attachment classes and mark glyph
sets (with HarfBuzz's GDEF blocklist); every GSUB lookup type (single, multiple,
alternate, ligature, context, chaining context, extension, reverse chaining) and
every GPOS lookup type (single, pair formats 1 and 2, cursive, mark-to-base,
mark-to-ligature, mark-to-mark, context, chaining context, extension); feature
variations at the default instance; the legacy `kern` table (format 0) when GPOS
has no kerning; fallback mark positioning by combining class when the font has
no GPOS.

**Shapers**, chosen as `hb_ot_shaper_categorize` chooses them:

| Shaper | Scripts | Notes |
|---|---|---|
| default | all others | |
| Arabic | Arabic, Syriac | joining forms, `stch` recording, modifier mark order, fallback shaping from presentation forms for fonts without Arabic GSUB |
| Hebrew | Hebrew | presentation-form composition without GPOS marks, meteg order |
| Thai | Thai, Lao | sara am decomposition, private-use fallback shaping |
| Hangul | Hangul | jamo composition and decomposition, `ljmo`/`vjmo`/`tjmo`, tone marks |
| Indic | Bengali, Devanagari, Gujarati, Gurmukhi, Kannada, Malayalam, Oriya, Tamil, Telugu | old and new specification fonts, reph, pre-base forms, matras, dotted circles |
| Khmer, Myanmar | Khmer, Myanmar | |
| Universal Shaping Engine | the scripts HarfBuzz gives it (Sinhala, Tibetan, Balinese, Javanese and some ninety more), and Indic fonts tagged `dev3` etc. | including Arabic joining for joining scripts |

The scripts HarfBuzz gives the Universal Shaping Engine are read from its
`hb-ot-shaper.hh`, which lists them by hand.

**Languages.** Every BCP 47 language as HarfBuzz maps it to OpenType language
systems (`hb_ot_tags_from_script_and_language`), canonicalised as
`hb_language_from_string` canonicalises it, including extended language subtags,
ISO 639-3 codes and the private-use `-hbot` and `-hbsc` subtags. A language
HarfBuzz does not register selects no language system. The tables are generated
by `cargo xtask shape ucd` from HarfBuzz's `hb-ot-tag-table.hh`.

## Not supported

- Vertical directions (libass shapes `@` fonts horizontally with `vert`).
- Apple Advanced Typography (`morx`, `kerx`, `trak`) and Apple `kern` version 1;
  OpenType `kern` subtables of formats 1, 2 and 3 and cross-stream kerning. A
  face that would need them is refused with `Error::Unsupported`.
- `stch` stretching and the Windows-1256 Arabic fallback lookups, refused with
  `Error::Unsupported` when they would apply.
- With `SizedMetrics`, glyph extents and contour points of glyphs FreeType's
  auto-hinter gives an Indic style (unless the glyph has no points): the font
  crate's default load is `Unsupported` for them, so mark positioning that reads
  such a glyph's outline fails. Faces whose character map libass would convert
  (Windows multibyte encodings) are refused.
- Variable font instances other than the default; the 24-bit "beyond 64K" layout
  formats.
- Malformed layout tables are read leniently (an out-of-range structure reads as
  absent, a count as the records that are present) where HarfBuzz's sanitizer
  would reject them, so results on damaged fonts can differ; they never panic.
  `OtMetrics` takes a `cmap` with any subtable or variation-sequence table outside
  it, or whose checks exceed its operation budget, for no table, as HarfBuzz's
  sanitizer does.

## Behaviour and limits

Faces read their tables from the font bytes without copying and do no work of
their own. Plans, buffers and bidi resolvers charge their storage to the `Budget`
they are given and keep it for reuse.

`Plan::new` may spend 65536 units of work plus 4 per byte of the face's GSUB and
GPOS, charged for every feature variation condition evaluated, every language
system and feature record searched, every lookup a feature lists and every
coverage record a lookup's digest reads. A table whose shared structures would
multiply the work makes it fail with `Error::Policy`.

A buffer's `Limits` (set with `Buffer::with_limits`) bound one shaping call: its
work, charged for every glyph a lookup visits, every subtable and rule tried and
every glyph edit, by default HarfBuzz's 4096 operations per character and at
least 65536 (`work_per_char`, `minimum_work`); and its length, by default
HarfBuzz's 64 glyphs per character and at least 16384 (`glyphs_per_char`,
`minimum_glyphs`). Running out of either, or nesting lookups deeper than
HarfBuzz's 64 levels, is `Error::Policy`.

Bidi resolution and line reordering run under `bidi::Limits`: each paragraph a
`Resolver` resolves and each line `reorder_line` reorders may spend
`work_per_char` units per character (default 4096), and at least `minimum_work`
(default 65536); running out is `Error::Policy`. Paragraphs are limited to 2³²
characters.

## Verification

`cargo test -p triergon-shape` runs unit tests and a mutation test. The
comparison with HarfBuzz 14.6.0 and FriBidi 1.0.17 runs through
[`shape-worker`](../shape-worker/README.md) and the `harness/features/shape-*`
features; [docs/status.md](../../docs/status.md) has the results.

## Licence

MIT or Apache-2.0, at your option, with HarfBuzz 14.6.0's "Old MIT" licence and libass
0.17.5's ISC licence for the modules derived from them: the shaping pipeline, layout
tables, script shapers and their generated tables are translated from HarfBuzz, and the
sized font functions and `@` font rotations follow libass. `LICENSE` holds the
notices, including the Unicode data notices described in
[THIRD-PARTY.md](THIRD-PARTY.md), and
[docs/provenance.md](../../docs/provenance.md#triergon-shape) lists the modules.
