# triergon-shape

Text shaping for a subtitle renderer, giving exactly what libass 0.17.5 gets from
HarfBuzz 14.6.0 and FriBidi 1.0.17: OpenType layout and the script shapers, the
Unicode bidirectional algorithm as FriBidi resolves it, and libass's simple
shaping level. It depends on [`triergon-core`](../core) and reads fonts through
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
let plan = Plan::new(&face, Direction::RightToLeft, Script::from_iso15924(*b"Arab"), &Language::new("ar")?, &features, &pool)?;

let text: Vec<char> = "مرحبا بالعالم".chars().collect();
// HarfBuzz's default scale: font units.
let metrics = OtMetrics::new(&font, Scale::units(font.units_per_em()), &pool);
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
font-function interface: `OtMetrics` answers as HarfBuzz's built-in functions do
(its character map choice, `cmap` format 14 variation sequences, `hmtx` advances,
`glyf` and CFF extents), and a renderer can implement it from its rasteriser, as
libass does with FreeType. `Scale` reproduces HarfBuzz's scaling arithmetic,
including hinting device tables when a ppem is set.

The bidi API mirrors libass's `ass_shaper_shape` and `ass_shaper_reorder`:
`bidi::BidiClass::of` and `bidi::Bracket::of` per character, `bidi::paragraphs`,
`bidi::Resolver::resolve` per paragraph (base direction left-to-right,
right-to-left or automatic, with or without bracket pairing), `bidi::reorder_line`
per line (both within `bidi::Limits`), and `bidi::shape_simple` for libass's simple shaping level (FriBidi's
joining, Arabic presentation forms and mirroring).

## What it covers

**Pipeline.** Everything `hb_shape` does for a horizontal run with default buffer
flags: Unicode properties and grapheme clusters (emoji modifiers, regional
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
vowel constraints, the scripts that join) and Unicode 14.0.0 for FriBidi's (bidi
class, bracket pairs, mirroring, joining type, presentation forms), each exactly
as that reference's shipped tables hold them; the scripts HarfBuzz gives the
Universal Shaping Engine are read from its `hb-ot-shaper.hh`, which lists them
by hand.

**Bidi.** UAX #9 as FriBidi 1.0.17 computes it, including its choices: runs as
FriBidi forms them, the level-125 limit and overflow counting, bracket pairing
within 63 open brackets per isolate depth, FriBidi's L1 rules, and stopping at a
paragraph separator inside the text.

## What it does not cover

- Vertical directions (libass shapes `@` fonts horizontally with `vert`).
- Apple Advanced Typography (`morx`, `kerx`, `trak` with `STAT`) and Apple `kern`
  version 1; `kern` formats other than 0 and cross-stream kerning. A face that
  would need them is refused with `Error::Unsupported`.
- `stch` stretching (refused when it would apply), the Windows-1256 Arabic
  fallback lookups, fallback kerning through `Metrics::h_kerning` (HarfBuzz's
  built-in functions have none).
- CFF glyph extents where HarfBuzz's charstring interpreter departs from
  FreeType's: it lacks the Type 2 arithmetic and storage operators and reads some
  curve operands differently. `OtMetrics` takes CFF extents from FreeType's
  interpretation, so fallback mark positioning in a CFF font without GPOS whose
  glyphs use them can differ.
- Variable font instances other than the default; the 24-bit "beyond 64K"
  layout formats.
- Languages outside the table of `Language`: libass's script languages, the major
  European, Middle Eastern and Asian languages and `c`.
- Malformed tables are read leniently (an out-of-range structure reads as
  absent, a count as the records that are present) where HarfBuzz's sanitizer
  would reject or neuter them, so results on damaged fonts can differ; they
  never panic.

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
every run a rule looks at. Most rules look at a run once; it is looked at again
for each enclosing isolate depth (at most 125) that a context search passes it
at, for each bracket pair that encloses it (at most 63 per isolate depth), and
once per level line reordering reverses at (at most 126). The default, 4096
units per character and 65536 in all, is 19 times the most a paragraph of the
bidi and shaping corpora needs (216 per character, a seeded random paragraph of
every class); running out is `Error::Policy`. Paragraphs are limited to 2³²
characters.

## Verification

- `cargo test -p triergon-shape`: unit tests per module, and `tests/mutation.rs`,
  which shapes with mutated copies of a font built in memory (GDEF, GSUB
  ligature, single and chaining lookups, GPOS pair and mark attachment), resolves
  random text of every bidi class and checks that the work limit stops a call.
- Five harness features (`harness/features/shape-*`) run through
  [`shape-worker`](../shape-worker/README.md) and the HarfBuzz 14.6.0 and FriBidi 1.0.17
  reference worker (`harness/reference_shape.h`, HarfBuzz with its built-in font
  functions at its default scale, FriBidi as libass calls it), and require both
  to fold the same output:
  - `shape-text`: 200 cases over 25 corpus files and 22 script profiles, 3,029
    runs with libass's features and with kerning, ligatures, direction and
    language varied; every glyph's id, cluster, advances and offsets.
  - `shape-text-all`: every corpus face and script, 1,724 cases and 100,351 runs.
  - `shape-bidi`: 24 cases, authored and seeded random paragraphs and lines with
    each base direction, with and without bracket pairing; levels, visual order
    and paragraph directions.
  - `shape-simple`: 6 cases of libass's simple shaping level.
  - `shape-unicode`: every Unicode scalar value's properties as HarfBuzz and
    FriBidi report them.

  `cargo xtask check` requires `shape-text`, `shape-bidi`, `shape-simple` and
  `shape-unicode` (`cargo xtask shape compare`); `shape-text-all` runs on demand
  (`cargo xtask shape compare --all`). Both text features list one open case, a
  CFF font whose glyphs use operators HarfBuzz's charstring interpreter reads
  differently (above), which `check` still runs and reports. The case rows are
  generated (`cargo xtask shape manifest`) from the font corpus in
  `harness/fonts/corpus.json`, authored texts and seeded random strings, and each
  case's expected output is pinned from the reference in `pins.json`.
  `cargo xtask report` runs each feature on its own and with `--benchmark` and
  `--memory` measures runtime and memory against the reference; `shape-dump`,
  `bidi-dump`, `simple-dump` and `unicode-dump` print the folded values as lines
  in both workers, for diffing a failing case.
- `cargo xtask shape ucd --check` regenerates the Unicode tables from the pinned
  files and fails if they would change.
- `cargo xtask fuzz --contract harness/features/<feature>/contract.json` mutates
  each case's input through the release and dev builds of the worker (the dev
  build checks overflow); `cargo xtask check` runs one round of it on a few cases
  of the text, bidi and simple features.

The comparison establishes agreement with HarfBuzz and FriBidi on that corpus,
not conformance with the OpenType or Unicode specifications.
