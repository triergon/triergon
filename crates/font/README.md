# triergon-font

OpenType and TrueType faces for a subtitle renderer: the table directory and font
collections, face metrics and names, character mapping, unhinted glyph outlines in
font units with their advances, `kern` pair kerning, raw table bytes for other
readers, and choosing a face for an ASS font name, bold and italic value. At a
pixel size it gives what libass obtains from FreeType: scaled 26.6 outlines, size
metrics, advances, kerning, decoration lines and libass's synthetic oblique and
bold. It depends only on [`triergon-core`](../core) for `Bytes`, `Budget` and
errors. Shaping, rasterising and ASS parsing belong to other crates.

## Usage

```rust,no_run
use triergon_core::{Budget, Bytes};
use triergon_font::{FaceInfo, Font, Limits, Outline, Request, Segment, select};

let pool = Budget::new(64 << 20)?;
let file = Bytes::copy_from_slice(&pool, &std::fs::read("Arimo-Regular.ttf")?)?;
let faces: Vec<Font> = (0..Font::face_count(&file)?)
    .map(|i| Font::new(file.clone(), i, &pool, Limits::default()))
    .collect::<Result<_, _>>()?;

// \fnArimo\b1: the face libass's scoring would pick.
let infos: Vec<FaceInfo<'_>> = faces.iter().map(FaceInfo::new).collect();
let request = Request { family: "Arimo", weight: 700, slant: 0, character: Some(u32::from('g')) };
let font = &faces[select(&infos, &request).ok_or("no matching face")?];

let glyph = font.glyph_index('g').ok_or("unmapped")?;
let mut outline = Outline::new(&pool); // reuse it for every glyph
let metrics = font.outline_into(glyph, &mut outline)?;
for segment in outline.segments() {
    if let Segment::QuadTo(_control, _end) = segment { /* font units, y up */ }
}
let em = f64::from(font.units_per_em());
let _advance = f64::from(metrics.advance) / em;

// The glyph as libass draws it with hinting off: 256 pixels (16384 in 26.6),
// then scaled.
let sized = font.at_size(16_384)?;
let mut scaled = Outline::new(&pool); // reuse it too
let glyph_metrics = sized.outline_into(glyph, &mut scaled)?; // 26.6 pixels, y up
sized.synthesize(&mut scaled, font.synthesis(request.weight, request.slant))?;
let _shaping_advance = sized.default_advance(glyph)?; // what HarfBuzz receives
let _drawn_advance = glyph_metrics.advance;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## What it reads

| Area | Supported |
|---|---|
| Containers | sfnt version `0x00010000`, `true` or `OTTO`; TrueType/OpenType collections (`ttcf`, any version), face by index |
| Required tables | `head` (unitsPerEm 16 to 16384), `hhea`, `maxp` (any version), `hmtx` |
| Other tables | `OS/2` (version 0 and later, at least 78 bytes), `name` (records read as format 0), `cmap`, `kern`, `post` (the underline), `vhea` with `vmtx`, and the extended `HHEA`, `HMTX`, `VHEA` and `VMTX` where the lower-case tables are missing |
| Outlines | `glyf` with short or long `loca`; `CFF ` version 1 (Type 2 charstrings, name-keyed or CID-keyed); `CFF2` at the default instance |
| Character maps | `cmap` formats 0, 2, 4, 6, 8, 10, 12 and 13; variation sequences (formats 14 and 15); the Unicode charmap FreeType builds from CFF glyph names; CFF encodings |
| Kerning | Microsoft `kern` version 0, format 0 |

Damaged files are read as FreeType reads them: a table that starts or ends beyond
the file is absent (`hmtx` keeps its whole records), the first record of a tag
with a nonzero length is used, an unreadable `name` or a short `OS/2` table is
absent, and broken `loca` data is repaired.

**Metrics and style.** `vertical_metrics` and `style` follow FreeType's choices:
typographic metrics when `fsSelection` sets USE_TYPO_METRICS, else `hhea`, else
(both zero) typographic and then Windows metrics; bold from `fsSelection` bit 5, italic from
bits 0 or 9, or `macStyle` without `OS/2`. `has_vertical_metrics` tells whether
`vhea` and `vmtx` both load and `vhea` lists a record.

**Names.** `names` decodes records lazily as `char`s: platforms 0 and 3 as
UTF-16BE (unpaired surrogates become U+FFFD), Macintosh Roman as Mac OS Roman;
other encodings yield nothing. `family_name`, `style_name` and `postscript_name`
select the record FreeType reports.

**Character mapping.** `charmaps` lists FreeType's charmaps in its order: every
`cmap` subtable FreeType's default validation accepts, then for a CFF face
without a Unicode subtable the charmap built from its glyph names
(`CharmapKind::GlyphNames`), then for a name-keyed CFF face its encoding
(`CharmapKind::CffEncoding`). Formats 14 and 15 are read as variation sequences
instead (`variation_sequences`). `mappings` steps through a charmap as
`FT_Get_Next_Char` does; a glyph at or above the glyph count is unmapped.
`text_charmap` is the charmap libass selects when it opens a face
(`ass_charmap_magic`), and `text_lookup` reads a charmap as libass reads text
through it (`ass_font_index_magic`: a Windows Symbol charmap at `0xF000 | c`);
`glyph_index` looks a character up that way through `text_charmap`.

**Outlines.** `Font::outline_into` gives a glyph's unhinted outline in whole font
units, y up, as FreeType loads it with `FT_LOAD_NO_SCALE`, with its advance:

- TrueType simple and composite glyphs (offsets, point matching, one scale, x
  and y scales or a 2×2 matrix), taking the advance of the last USE_MY_METRICS
  component and shifted by `lsb - xMin` as FreeType shifts it. Instructions are
  skipped.
- CFF and CFF2 charstrings: all path operators, flex, stem hints, `hintmask`
  and `cntrmask`, local and global subroutines, arithmetic, storage and `random`
  operators (CFF only), and `seac` accents. Charstrings are evaluated in 16.16
  fixed point as FreeType's CFF engine evaluates them, and outlines and advances
  go through the FontMatrix. A CFF2 face is read at its default instance:
  `blend` applies the region scalars FreeType builds at the origin of the axes
  of an `fvar` it accepts, and zero without one.

`Font::control_box` reads a glyph's box without building its outline: the `glyf`
header box as stored, or the box of every point a charstring draws, control
points included, in unrounded 16.16 before the FontMatrix.

**At a size.** `Font::at_size` takes a 26.6 pixel height and sets up the face as
libass requests the size from FreeType (`ass_face_set_size`: `FT_Request_Size`,
REAL_DIM), after libass's GDI face metrics (`Font::gdi_metrics`), with FreeType's
fixed-point rounding. A `SizedFont` is a small copyable view that allocates
nothing. It gives:

- `outline_into`: the outline libass draws with hinting off
  (`FT_LOAD_NO_HINTING | FT_LOAD_NO_BITMAP`), in an `Outline` of 26.6 points,
  y up, and its `GlyphMetrics`.
- `load`: one glyph with `Load::Unhinted` (the load above) or `Load::Default`,
  the load libass's HarfBuzz callbacks make (`FT_LOAD_DEFAULT`), with the full
  `GlyphMetrics`: advances and the control box, fitted to the pixel grid by the
  default load. The default load hints as FreeType 2.15 does by default:
  - CFF, CFF2 and CID-keyed glyphs with Adobe's CFF hinter, without stem
    darkening. Sizes beyond 2000 pixels per em load unhinted and scaled, as
    FreeType retries them.
  - TrueType glyphs of a face with bytecode through the TrueType interpreter in
    backward compatibility mode: the font program once per face, the control
    value program at each size request, then each glyph's program. What the
    specifications leave open is reproduced as probes of FreeType establish it
    (`harness/fonts/probes`).
  - TrueType glyphs of a face without bytecode (no `fpgm`, a `prep` of at most 7
    bytes) through FreeType's auto-hinter as built without HarfBuzz: the latin
    and CJK writing systems, in the normal render mode.
- `synthesize`: libass's synthetic oblique and bold, as `Font::synthesis` decides
  them from the requested weight and slant: the oblique shear, then
  `FT_Outline_EmboldenXY` with libass's strength (`embolden_strength`). It
  returns `false` for an outline of zero area FreeType refuses to embolden.
  `Outline::transform`, `embolden` and `orientation` are public.
- `default_advance` and `default_vertical_advance`: the default load's advances,
  which libass's HarfBuzz callbacks report. `GlyphMetrics` also carries the
  vertical advances libass uses for the glyphs of an `@` font it draws rotated
  (`vertical_advance`, `linear_vertical_advance`).
- `metrics` (`FT_Size_Metrics`), `ascent_descent` (libass's line ascent and
  descent), `harfbuzz_scale` (the scale libass sets on its HarfBuzz font),
  `kerning` (`FT_KERNING_DEFAULT`), and `underline` and `strikeout` (the bands
  libass adds from `post` and `OS/2`).

At zero pixels per em, glyphs load unscaled and unhinted, in font units, as
FreeType loads them.

**Kerning.** `kerning` sums the horizontal, non-minimum format 0 subtables of a
`kern` table, at most 32, with the override bit replacing the sum; Apple's
version 1 reads as no subtables. `kerning_pairs` lists the pairs.

**Face matching.** `FaceInfo` and `select` score faces as libass 0.17.5 does for
fonts it loads itself. A `Request` carries the family, the weight and slant of
libass's font description (700 for the ASS bold values 1 and -1, 100 for the
italic value 1, other values unsigned) and optionally a code the face's text
charmap must map. Family names match ignoring ASCII case; a full or PostScript
name match ends the search. `FaceInfo::has_family_name` tells the faces libass
registers at all.

**Store.** `Store` caches built values, such as faces and outlines, by typed key.
Held values keep a reservation on the store's `Budget`; when a reservation fails,
the least recently used values that only the store holds are dropped.

## Not supported

- WOFF and WOFF2, bare CFF, Type 1 and Type 42; CFF Type 1 charstrings
  (CharstringType 1) and synthetic fonts.
- Bitmap and colour glyphs (`EBDT`, `CBDT`, `sbix`, `COLR`, `SVG `).
- Variation instances other than the default (`fvar`, `gvar`, `HVAR`, `avar`);
  vertical origins (`VORG`); `post` beyond the underline.
- Apple `kern` version 1 and `kern` formats other than 0; GPOS and GSUB, which
  `triergon-shape` reads.
- CFF stem darkening, and the auto-hinter's light mode, Indic writing systems
  and coverage through OpenType features. The default load of an auto-hinted
  glyph of an Indic style with points is `Unsupported`.
- TrueType hinting outside backward compatibility: for programs that may issue
  `INSTCTRL` with a selector other than a constant 1 or 2, and for tricky fonts
  known by family name, `default_advance` and the default load are
  `Unsupported`. Fonts FreeType finds tricky only by their table checksums are
  not recognised.
- `text_lookup` is `Unsupported` for charmaps whose codes libass converts
  (Windows encodings 2 to 6 through iconv, and a Symbol charmap under a legacy
  Arabic character set).

Known differences from FreeType: a face without a usable `glyf` or CFF table is
`Error::Invalid`, where FreeType opens it and fails each glyph; a CFF2 face whose
`fvar` FreeType's `TT_Get_MM_Var` refuses opens here at its default instance; the
`random` operator does not follow FreeType's generator; and the TrueType
interpreter's unit vectors along lines and from the stack (SPVTL, SFVTL, SDPVTL,
SPVFS, SFVFS) differ from FreeType's by one unit in a component for some
directions (13% of the probed vectors in `vectors-normalization`,
`vectors-from-stack` and `projections`), since no fixed-point rule tried
reproduces FreeType's normalisation.

## Behaviour and limits

Parsing a face reads the directory and fixed headers and checks every structure
a lookup depends on; glyph programs, cmap entries, names and kerning pairs are
read from the font bytes when asked for. A face retains the shared `Bytes` plus
small tables charged to the `Budget` it was created with (cmap records, CFF
Private DICTs, variation region scalars, kern subtable records) and, once hinted
loads need them, the auto-hinter's per-face metrics and scratch and the TrueType
interpreter's definitions and per-size state, sized by `maxp` as FreeType sizes
them. An `Outline` charges its points and contour ends to its own budget and
keeps its capacity across glyphs.

`Limits` bounds one parse and one glyph load: `work` (default 2²⁴ units, which a
hinted load's instructions also count against; the font and control value
programs run within a `work` of their own), nested composites and subroutine
calls (`depth`, default 16) and the points of one outline (`points`, default
2¹⁸). Exceeding one is `Error::Policy`. Format ceilings (48 or 513 operands, 96
stems, 16 nested subroutine calls in CFF, 64 nested composites, 32 nested calls
of TrueType functions) and malformed data are `Error::Invalid`, as are sizes
beyond 65535 pixels per em. Unsupported files and features are
`Error::Unsupported`. A scaled coordinate beyond `i32` is `Error::Overflow` in an
unhinted or CFF load. A font program that fails, or runs out of its work, fails
every hinted load of the face. No input panics.

## Verification

`cargo test -p triergon-font` runs unit tests per table and a mutation test over
faces built in memory. The comparison with FreeType 2.15.0 runs through
[`font-worker`](../font-worker/README.md) and the `harness/features/font-*`
features; [docs/status.md](../../docs/status.md) has the results.

## Licence

MIT or Apache-2.0, at your option, with FreeType 2.15.0's FreeType License,
HarfBuzz 14.6.0's "Old MIT" licence and libass 0.17.5's ISC licence for the modules
derived from them: most of the CFF hinter and the auto-hinter, the fixed-point
arithmetic, synthetic styles and glyph-name tables are translated from or generated
from FreeType, and face matching and the text charmap follow libass. `LICENSE`
holds all the notices, and [docs/provenance.md](../../docs/provenance.md#triergon-font)
lists the modules. Software that includes this crate credits FreeType in its
documentation:

> Portions of this software are copyright © 2026 The FreeType Project
> (<https://freetype.org>). All rights reserved.
