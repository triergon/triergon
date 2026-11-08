# triergon-font

OpenType and TrueType faces for a subtitle renderer: the table directory and font
collections, face metrics and names, character mapping, unhinted glyph outlines in
font units with their advances, `kern` pair kerning, and choosing a face for an ASS
font name, bold and italic value. It depends only on [`triergon-core`](../core) for
`Bytes`, `Budget` and errors. Shaping, rasterising and ASS parsing belong to other
crates.

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
let request = Request { family: "Arimo", bold: 1, italic: 0, character: Some('g') };
let font = &faces[select(&infos, &request).ok_or("no matching face")?];

let glyph = font.glyph_index('g').ok_or("unmapped")?;
let mut outline = Outline::new(&pool); // reuse it for every glyph
let metrics = font.outline_into(glyph, &mut outline)?;
for segment in outline.segments() {
    if let Segment::QuadTo(_control, _end) = segment { /* font units, y up */ }
}
let em = f32::from(font.units_per_em());
let _advance = f32::from(metrics.advance) / em;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## What it reads

**Container.** Single fonts with sfnt version `0x00010000`, `true` or `OTTO`, and
TrueType/OpenType collections (`ttcf`, any version), face by index. A table that
starts beyond the file is absent; one that ends beyond it is absent too, except
`hmtx`, which keeps its bytes. Only the first record of a repeated tag is used.

**Face tables.** `head` (unitsPerEm 1 to 16384, bounding box, macStyle, loca
format), `hhea`, `maxp` (version 0.5 or 1.0), `hmtx` and `OS/2` (version 0 and
later, at least 78 bytes; shorter is treated as absent). `head`, `hhea`, `maxp` and
`hmtx` are required.

**Metrics and style.** `vertical_metrics` and `style` follow FreeType's choices:
typographic metrics when `fsSelection` sets USE_TYPO_METRICS, else `hhea`, else
(both zero) typographic and then Windows metrics; bold from `fsSelection` bit 5,
italic from bits 0 or 9, or `macStyle` without `OS/2`.

**Names.** Format 0 and 1 naming tables. Records decode lazily as `char`s:
platforms 0 and 3 as UTF-16BE (unpaired surrogates become U+FFFD), Macintosh Roman
as Mac OS Roman; other encodings yield nothing. Empty records and records outside
the string storage are skipped. `family_name`, `style_name` and `postscript_name`
select the record FreeType reports.

**Character mapping.** `cmap` formats 0, 4, 6, 10, 12 and 13 on any platform, in
directory order; other formats (2, 8, 14) are not listed. Lookups are bounded by the
end of the `cmap` table rather than the declared subtable length. A glyph at or
above the glyph count is unmapped. `text_charmap` is libass's selection: the first
Windows Unicode subtable, else the first Windows subtable, else FreeType's default
Unicode subtable; `glyph_index` looks a Windows Symbol charmap up at `0xF000 | c`.

**TrueType outlines.** `glyf` with short or long `loca`: simple glyphs and
composites with offsets, point matching, one scale, x and y scales or a 2×2
matrix, nested to the depth limit. Offsets scale only with
SCALED_COMPONENT_OFFSET, by the length of each matrix column. A composite takes the
advance of its last USE_MY_METRICS component. As FreeType does, the outline is
shifted horizontally by `lsb - xMin` of the glyph whose metrics apply, and a glyph
range that runs past `glyf` is cut at its end. Instructions are skipped.

**CFF and CFF2 outlines.** `CFF ` version 1 with Type 2 charstrings, name-keyed or
CID-keyed (FDArray, FDSelect formats 0 and 3), and `CFF2` (FDSelect format 4 too)
at the default instance of a variable font: `blend` applies region scalars at the
origin, in charstrings and Private DICTs. Supported: all path operators, flex,
hints and masks (counted and skipped), local and global subroutines (10 nested
calls in CFF), arithmetic, storage and `random` operators, and `seac` accents
through StandardEncoding and the ISOAdobe or a custom charset. A top FontMatrix
other than 1/unitsPerEm is applied. As in FreeType's CFF engine, a contour starts
at its first segment, a final point repeating the start is dropped, and a line to
the current point is dropped unless a `hintmask` came just before. Drawing
operators ignore a malformed remainder of operands.

**Kerning.** Microsoft `kern` version 0: horizontal, non-minimum format 0
subtables, at most 32, summed, with the override bit replacing the sum.

**Face matching.** `FaceInfo` and `select` score faces as libass 0.17.5 does for
fonts it loads itself. Windows family names (or FreeType's family name when a face
has none) match the request ignoring ASCII case and score 1 for a missing italic, 4
for an unwanted one, plus 73/256 of the weight difference, where a face not flagged
bold counts as 120 heavier when the request is more than 150 heavier. A full or
PostScript name match scores 0 and ends the search; when only one of the two
matches, CFF faces go by the PostScript name and others by the full name. A face
whose text charmap lacks the requested character is skipped. Weights come from
`usWeightClass`, with classes 1 to 9 read as 100 to 900; the italic and bold flags
from `fsSelection` bits 0 and 5.

**Store.** `Store` caches built values, such as faces and outlines, by typed key.
Held values keep a reservation on the store's `Budget`; when a reservation fails,
the least recently used values that only the store holds are dropped.

## What it does not read

`cmap` format 14 (variation selectors, which shaping will need), WOFF and WOFF2, bare CFF, Type 1 and Type 42, bitmap and colour glyphs (`EBDT`,
`CBDT`, `sbix`, `COLR`, `SVG `), variation instances other than the default (`fvar`,
`gvar`, `HVAR`, `avar`), vertical metrics (`vhea`, `vmtx`), `post`, hinting
(`fpgm`, `prep`, `cvt `, CFF hints), Apple `kern` version 1 and kern formats other
than 0, GPOS and GSUB, and CFF features outside the above: Type 1 charstrings
(CharstringType 1), synthetic fonts, Expert charsets for `seac`, and FontMatrix
entries of CID Font DICTs.

## Behaviour and limits

Parsing a face reads the directory and fixed headers and checks every structure a
lookup depends on; glyph programs, cmap entries, names and kerning pairs are read
from the font bytes when asked for. A face retains the shared `Bytes` plus small
tables charged to the `Budget` it was created with: the supported cmap records, one
Private DICT per CFF FDArray entry, variation region scalars and kern subtable
records. An `Outline` charges its points and contour ends to its own budget and
keeps its capacity across glyphs.

`Limits` bounds the work of one parse and of one outline (`work`), nested
composites and subroutine calls (`depth`) and the points of one outline
(`points`); exceeding one is `Error::Policy`. Format ceilings (48 or 513 operands,
96 stems, 32 transient slots, 10 nested calls in CFF, 64 nested composites) are
`Error::Invalid`, as is malformed data. Unsupported files and features are
`Error::Unsupported`. No input panics.

## Verification

- `cargo test -p triergon-font`: unit tests per table, faces built in memory
  (TrueType, CFF and a collection) and a deterministic mutation test over them.
- `cargo xtask fonts compare` runs `examples/font-dump.rs` and the FreeType
  reference worker (`harness/freetype-font.c`, FreeType 2.15.0) on every face of
  the corpus in `harness/fonts/corpus.json` and requires identical output: metrics,
  style, family, style and PostScript names, every name record, every cmap mapping,
  the text charmap, kerning among the first 256 glyphs and the Latin-1 glyphs, and
  for every glyph its `hmtx` and loaded advances and its outline at 1/64 font unit.
- `cargo xtask fonts fuzz` runs `examples/fuzz.rs`, which mutates every corpus file
  and reads the results in the dev profile, so overflow is checked as well.

The comparison establishes agreement with FreeType on that corpus, not conformance
with the OpenType specification. Face matching is checked by unit tests only.
