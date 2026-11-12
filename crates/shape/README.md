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
visual order, positioned in the units of the metrics' `Scale`. `Metrics` is the
font-function interface. `SizedMetrics` answers as libass's FreeType-backed
functions do over a face at a size (`triergon_font::SizedFont`): the character
map libass selects, or with `SizedMetrics::with_charmap` the one a face has been
left on, read as libass reads it, default-load advances and `FT_KERNING_DEFAULT`
kerning, at the size's scale and pixels per em, loading charstrings within the
caller's `WorkBudget`. For a face of an `@` font, `SizedMetrics::rotating` takes
one face's entries (`FaceRotations`) of libass's glyph metrics cache,
`GlyphRotations`, which its owner keys by font, face and size as libass does and
keeps as long as it shapes: a glyph whose first lookup is through a code point
that `rotates` (U+02F1 and above) advances by its default-load vertical advance,
as libass gives HarfBuzz that glyph, and a glyph first asked for an advance or
extents stays horizontal. The cache's storage is charged before it grows, and a
charge that fails ends the shaping call. `OtMetrics` answers as HarfBuzz's built-in
functions do at units per em (its character map choice among the raw encoding
records, which it sanitizes but does not validate, its `cmap` lookups, the
last (0, 5) format 14 subtable's variation sequences, `hmtx` advances, `glyf` and CFF extents). A callback that
cannot answer as its font would ends the shaping call with its error. `Scale` reproduces HarfBuzz's scaling arithmetic,
including hinting device tables when a ppem is set.

The bidi API mirrors libass's `ass_shaper_shape` and `ass_shaper_reorder`:
`bidi::BidiClass::of` and `bidi::Bracket::of` per character, `bidi::paragraphs`,
`bidi::Resolver::resolve` per paragraph (base direction left-to-right,
right-to-left or automatic, with or without bracket pairing), `bidi::reorder_line`
per line (both within `bidi::Limits`), and `bidi::shape_simple` for libass's
simple shaping level (cursive joining, Arabic presentation forms and mirroring,
as FriBidi applies them).

The character properties shaping reads are public too, typed: HarfBuzz's
(Unicode 18.0.0) are `unicode::GeneralCategory::of`, `unicode::combining_class`,
`Script::of`, `unicode::mirroring` and `unicode::decompose` (a
`unicode::Decomposition` into one or two characters); the bidi ones (Unicode
14.0.0) are `bidi::BidiClass::of`, `bidi::Bracket::of` with `Bracket::pair`,
`bidi::mirror` and `bidi::JoiningType::of`.

## What it covers

**Pipeline.** The glyph ids, clusters and positions `hb_shape` gives for a
horizontal run with default buffer flags, but not the glyph flags it also sets
(`HB_GLYPH_FLAG_UNSAFE_TO_BREAK` and the others `hb_glyph_info_get_glyph_flags`
reports), by the same steps: Unicode properties and grapheme clusters (emoji modifiers, regional
indicators, ZWJ sequences), shaping in the script's native direction, mirroring
or `rtlm`, normalisation against the font (decomposition, mark reordering with
HarfBuzz's modified combining classes, recomposition, variation selectors, space
and non-breaking-hyphen fallbacks), fraction masks, the feature map (merging,
mask bits, stages, required features, global feature search, `rand`), script and
language system selection, GDEF classes or synthesised ones, mark zeroing,
attachment resolution, default ignorables shown as the space glyph or removed.

**OpenType layout.** GDEF glyph classes, mark attachment classes and mark glyph
sets (with HarfBuzz's GDEF blocklist); every GSUB lookup type (single, multiple,
alternate, ligature, context, chaining context, extension, reverse chaining) and
every GPOS lookup type (single, pair formats 1 and 2, cursive, mark-to-base,
mark-to-ligature, mark-to-mark, context, chaining context, extension) with
HarfBuzz's skipping rules, ligature component tracking, nested lookups and their
position bookkeeping; feature variations at the default instance; the legacy
`kern` table (format 0) when GPOS has no kerning; fallback mark positioning by
combining class when the font has no GPOS.

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

**Unicode data.** Generated by `cargo xtask shape ucd` from hash-pinned files:
Unicode 18.0.0 for HarfBuzz's tables (general category, combining class, script,
mirroring, decomposition and composition, extended pictographic, Arabic joining
and presentation forms, Indic and USE categories with Microsoft's overrides,
vowel constraints, the scripts that join), each exactly as HarfBuzz's shipped
tables hold them, and Unicode 14.0.0, the data of libass's FriBidi, for the bidi
ones (bidi class, bracket pairs, mirroring, joining type, presentation forms).
Two bidi properties take FriBidi's values where they differ from the UCD's: the
defaults of unassigned code points in right-to-left blocks, and the joining
type of format characters. The scripts HarfBuzz gives the Universal Shaping
Engine are read from its `hb-ot-shaper.hh`, which lists them by hand.

**Languages.** Every BCP 47 language as HarfBuzz maps it to OpenType language
systems (`hb_ot_tags_from_script_and_language`): canonicalised as
`hb_language_from_string` canonicalises it, then the rules for languages that
need more than their first subtag, extended language subtags, the two- and
three-letter language tables, ISO 639-3 codes taken for their own tags, and the
private-use `-hbot` and `-hbsc` subtags. A language HarfBuzz does not register
selects no language system. The tables are generated by `cargo xtask shape ucd`
from the tables in HarfBuzz's `hb-ot-tag-table.hh`.

**Bidi.** UAX #9, revision 44, written from the annex with the Unicode
Consortium's C reference implementation of that version as its model (see
[THIRD-PARTY.md](THIRD-PARTY.md)); it passes every case of BidiTest.txt and
BidiCharacterTest.txt. Where FriBidi 1.0.17's results differ from the annex's
on text those files do not test, the results are FriBidi's, each deviation found
by comparing the two and named in the module of the rule it changes: explicit
levels and isolate depths where embeddings and isolates reach level 125;
isolating run sequences grouped by isolate depth; bracket pairs per isolate
depth over the whole paragraph, with FriBidi's stack limits; marks that join the
run of a neutral before them; the end of a line, where an isolate formatting
character keeps the whitespace before it; and text after a paragraph separator
inside the paragraph. Simple shaping joins characters by visual sides at even
levels and forms the LAM-ALEF ligature only from adjacent letters, as FriBidi
does.

## What it does not cover

- Vertical directions (libass shapes `@` fonts horizontally with `vert`).
- Apple Advanced Typography (`morx`, `kerx`, `trak` with `STAT`) and Apple `kern`
  version 1; in an OpenType `kern` table, subtables of formats 1, 2 and 3, which
  HarfBuzz applies too (format 1 as Apple's state machine), and cross-stream
  kerning. Shaping a face that would need them is refused with
  `Error::Unsupported`.
- `stch` stretching (refused when it would apply) and the Windows-1256 Arabic
  fallback lookups.
- What libass reads from FreeType's hinted load of some TrueType glyphs:
  `SizedMetrics` takes glyph extents and contour points from the font crate's
  default load (`SizedFont::load` with `Load::Default`), which hints CFF, CFF2
  and CID-keyed glyphs as FreeType does, and TrueType glyphs without bytecode
  as its auto-hinter does in the Latin and dummy styles, but not instructed
  TrueType glyphs or those the auto-hinter gives a CJK or Indic style
  (`Unsupported`), but for the empty extents of a glyph without points, which
  every FreeType load gives it. So fallback mark positioning in such a font
  without GPOS marks fails where it measures a glyph with an outline.
  Faces whose character map libass would convert (Windows multibyte encodings)
  are refused.
- `OtMetrics`'s CFF glyph extents where HarfBuzz's charstring interpreter
  departs from FreeType's: it lacks the Type 2 arithmetic and storage operators
  and reads some curve operands differently. `OtMetrics` takes CFF extents from
  FreeType's interpretation, so fallback mark positioning in a CFF font without
  GPOS whose glyphs use them can differ. libass never asks for these extents.
- Variable font instances other than the default; the 24-bit "beyond 64K"
  layout formats.
- Malformed layout tables are read leniently (an out-of-range structure reads
  as absent, a count as the records that are present) where HarfBuzz's
  sanitizer would reject them, so results on damaged fonts can differ; they
  never panic. `cmap` is the exception: as HarfBuzz's sanitizer, which never
  edits a table, `OtMetrics` takes a `cmap` with any subtable or
  variation-sequence table outside it, or whose checks exceed its operation
  budget, for no table. It reads a Windows Symbol subtable through the OS/2
  font page's remapping and a Macintosh Roman one through `unicode_to_macroman`.

## Behaviour and limits

Faces read their tables from the font bytes without copying and do no work of
their own. Plans, buffers and bidi resolvers charge their storage to the
`Budget` they are given and keep it for reuse.

Planning is bounded by the face: a plan may spend 65536 units of work and 4 per
byte of the face's GSUB and GPOS, charged for every feature variation condition
evaluated, every language system and feature record searched, every lookup a
feature lists and every coverage record a lookup's digest reads. That is 13
times the most a plan of the shaping corpus needs (0.3 per byte); a table whose
shared structures would multiply the work, such as nested feature variation
conditions that share their children, makes `Plan::new` fail with
`Error::Policy`.

A buffer's `Limits` bound one shaping call. Its work: every lookup's visit of a
glyph, every subtable and rule tried, every glyph a match skips, every nested
lookup, every glyph a buffer edit moves or a cluster merge touches, every link
of an attachment chain and every subtable and rule asked whether it would apply
is charged, by default within HarfBuzz's 4096 operations per character (at
least 65536). Its length: by default HarfBuzz's 64 glyphs per character (at
least 16384). Running out of either, or nesting lookups deeper than HarfBuzz's
64 levels, is `Error::Policy`. Each lookup carries HarfBuzz's digest of the
glyphs it can start at, computed once per plan, so lookups and glyphs that
cannot match are skipped without reading the font.

Bidi resolution and line reordering run under `bidi::Limits`: each paragraph a
`Resolver` resolves and each line `reorder_line` reorders may spend
`work_per_char` units per character, and at least `minimum_work`, charged for
every character a rule looks at. Resolving looks at each character in about a
dozen passes, and N0 again for each bracket pair that encloses it (at most 63
per isolate depth, at every depth); line reordering looks at each character once
per level it reverses at (at most 126). The default, 4096 units per character
and 65536 in all, is 347 times the most a paragraph of the bidi corpora needs
(11.8 per character, a six-character random paragraph with bracket pairs);
running out is `Error::Policy`.
Paragraphs are limited to 2³² characters.

## Verification

- `cargo test -p triergon-shape`: unit tests per module, and `tests/mutation.rs`,
  which shapes with mutated copies of a font built in memory (GDEF, GSUB
  ligature, single and chaining lookups, GPOS pair and mark attachment), resolves
  random text of every bidi class and checks that the work limit stops a call.
- `cargo xtask shape conformance` fetches Unicode 14.0.0's hash-pinned
  conformance files and runs the `bidi_conformance` example on them: 770,241
  cases of classes in BidiTest.txt and 91,707 of characters in
  BidiCharacterTest.txt, all passing.
- Six harness features (`harness/features/shape-*`) run through
  [`shape-worker`](../shape-worker/README.md) and the HarfBuzz 14.6.0 and FriBidi
  1.0.17 reference worker (`harness/reference_shape.h`, FriBidi as libass calls
  it), and require both to fold the same output:
  - `shape-libass`: 201 cases over 26 corpus files and 22 script profiles, 3,105
    runs with libass's features and with kerning, ligatures, direction and
    language varied, shaped as libass 0.17.5 shapes them with hinting off; every
    glyph's id, cluster, advances and offsets. The reference opens each face
    with FreeType 2.15.0 and sets it up as libass does (`ass_charmap_magic`,
    `set_font_metrics`, `ass_face_set_size` at 256 pixels, 16384 in 26.6), then
    shapes with a HarfBuzz font over the face's tables carrying libass's font
    functions at the size's scale and ppem, each restated over the pinned
    FreeType: `ass_font_index_magic` with `FT_Get_Char_Index` and
    `FT_Face_GetCharVariantIndex`, cached `FT_LOAD_DEFAULT` metrics for advances
    and extents, that load's outline for contour points, and
    `FT_KERNING_DEFAULT` kerning. The candidate shapes with `SizedMetrics` over
    `Font::at_size`. This compares device tables, contour-point anchors and
    FreeType's advances. Six cases are open: NotoEmoji's, whose fallback mark
    positioning reads the extents of glyphs FreeType's auto-hinter gives a CJK
    style.
  - `shape-text` and `shape-text-all`: the same cases, and every corpus face and
    script (1,768 cases, 103,031 runs; 1,746 and 101,691 before the charmap face
    joined them), with HarfBuzz's built-in font functions
    at units per em against `OtMetrics`. They cover the font tables' layout
    without FreeType; each lists one open case, the CFF font whose glyphs use
    operators HarfBuzz's charstring interpreter reads differently (above).
  - `shape-bidi`: 24 cases, authored and seeded random paragraphs and lines with
    each base direction, with and without bracket pairing; levels, visual order
    and paragraph directions.
  - `shape-simple`: 6 cases of libass's simple shaping level.
  - `shape-unicode`: every Unicode scalar value's properties as HarfBuzz and
    FriBidi report them.

  `cargo xtask check` requires `shape-libass`, `shape-text`, `shape-bidi`,
  `shape-simple` and `shape-unicode` (`cargo xtask shape compare`);
  `shape-text-all` runs on demand (`cargo xtask shape compare --all`). The
  crate's acceptance rests on `shape-libass`, `shape-bidi`, `shape-simple` and
  `shape-unicode`; the units-per-em features are additional coverage, and their
  open case does not block it. `check` still runs and reports every open case.
  The case rows are
  generated (`cargo xtask shape manifest`) from the font corpus in
  `harness/fonts/corpus.json`, authored texts and seeded random strings, and each
  case's expected output is pinned from the reference in `pins.json`.
  `cargo xtask report` runs each feature on its own and with `--benchmark` and
  `--memory` measures runtime and memory against the reference; `shape-dump`,
  `bidi-dump`, `simple-dump` and `unicode-dump` print the folded values as lines
  in both workers, for diffing a failing case.
- `cargo xtask shape ucd --check` regenerates the Unicode and language tables from
  the pinned files and fails if they would change.
- `cargo xtask fuzz --contract harness/features/<feature>/contract.json` mutates
  each case's input through the release and dev builds of the worker (the dev
  build checks overflow); `cargo xtask check` runs one round of it on a few cases
  of the text, bidi and simple features.

The comparison establishes agreement with HarfBuzz and FriBidi on that corpus,
not conformance with the OpenType or Unicode specifications; for bidi, the
conformance files check UAX #9 itself.
