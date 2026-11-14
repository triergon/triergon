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

**Container.** Single fonts with sfnt version `0x00010000`, `true` or `OTTO`, and
TrueType/OpenType collections (`ttcf`, any version), face by index. A table that
starts beyond the file is absent; one that ends beyond it is absent too, except
`hmtx`, which keeps its whole four-byte records. The first record of a tag with a
nonzero length is used, as FreeType sanitises the directory.

**Face tables.** `head` (unitsPerEm 16 to 16384, bounding box, macStyle, loca
format: any value but 0 means long offsets), `hhea`, `maxp` (any version),
`hmtx` (an advance or bearing cut off by the table's end reads as zero, as do all
advances without records) and `OS/2` (version 0 and later, at least 78 bytes;
shorter is treated as absent). `head`, `hhea`, `maxp` and `hmtx` are required.
`vhea` and `vmtx` are read together: FreeType reads vertical metrics only when
both load and `vhea` lists a record (`has_vertical_metrics`). As in FreeType,
the extended `HHEA` and `VHEA`, with a 32-bit record count, and their `HMTX`
and `VMTX` stand in for missing lower-case headers; a header's 36 bytes (38 for
an extended one) are read from the file at its offset, so a short header reads
on into the bytes after it and one cut off by the end of the file fails the
face; and `vmtx` is cut as `hmtx` is when it runs past the file.
As in FreeType, a `name` table that cannot be read counts as absent and the `cmap`
version is not checked.

**Metrics and style.** `vertical_metrics` and `style` follow FreeType's choices:
typographic metrics when `fsSelection` sets USE_TYPO_METRICS, else `hhea`, else
(both zero) typographic and then Windows metrics, kept to 16 bits with a
negative height made positive, as FreeType stores them; bold from `fsSelection` bit 5,
italic from bits 0 or 9, or `macStyle` without `OS/2`.

**Names.** Naming tables of any version, read as format 0 (format 1 language
tags are not read). Records decode lazily as `char`s:
platforms 0 and 3 as UTF-16BE (unpaired surrogates become U+FFFD), Macintosh Roman
as Mac OS Roman; other encodings yield nothing. Empty records and records outside
the string storage are skipped. `family_name`, `style_name` and `postscript_name`
select the record FreeType reports.

**Character mapping.** FreeType's charmaps in its order (`charmaps`): `cmap`
formats 0, 2, 4, 6, 8, 10, 12 and 13 on any platform, in directory order, each
listed only when FreeType's default validation accepts it (lengths that fit,
ascending non-overlapping groups for formats 8, 12 and 13, segments that start
at or before their end, in any order, and in-table glyph arrays for format 4,
subheaders and glyph arrays inside the table for format 2); then for a CFF face
(a CID-keyed one's CIDs read as string IDs, as FreeType reads them) without a
listed subtable on Windows BMP (3, 1) or the Unicode platform, of any format
including the variation-sequence formats 14 and 15, the Unicode charmap
FreeType builds from its glyph names when one maps a code point
(`CharmapKind::GlyphNames`, (3, 1)): each name's code point through FreeType's compiled Adobe Glyph List, `uniXXXX` and `uXXXX` forms,
suffixed names as variants after their base, and FreeType's second mappings for
`space`, `hyphen`, `Delta` and seven more, looked up with FreeType's own probes
(which on duplicate names differ between 32- and 64-bit targets as FreeType's
pointer arithmetic does); then for a name-keyed CFF face whose encoding maps a
code, the charmap of that encoding (`CharmapKind::CffEncoding`, platform 7): a
custom encoding of format 0 or 1 with supplements, or the Standard or Expert
encoding through the charset's lowest glyph of each SID. Subtables of formats 14
and 15, which FreeType lists but cannot select, are left out of the list and
read as variation sequences instead (`variation_sequences`, FreeType's first
valid (0, 5) subtable of either format; `VariationSequences::format14` reads one
another reader selects). Format 4 is read as FreeType reads it: a linear search
through segments its validation finds unsorted, else a binary search that for
overlapping segments takes the first holding the code, and `char_next`'s walk
from segment to segment.
Records stop at the table's end. Lookups are bounded by the end of the `cmap`
table rather than the declared subtable length, and `mappings` steps through
formats 2, 4 and 8 and the added charmaps as `FT_Get_Next_Char` does. A glyph at or
above the glyph count is unmapped. `text_charmap` is the charmap libass selects
when it opens a face (`ass_charmap_magic`): the first Windows full-Unicode
charmap (3, 10), else the first (3, 1), which may be the one built from glyph
names, else the first other Windows charmap; without Windows charmaps, the one
FreeType selects (the last (0, 4) or format 13 (0, 6), else the last Unicode- or
ISO-platform charmap), else the first.
`charmap` gives a charmap by its index in `charmaps`, and `text_charmap_index`
the index of `text_charmap`. `text_lookup` reads a
charmap as libass reads text through it (`ass_font_index_magic`): a Windows
Symbol charmap at `0xF000 | c`, any other unchanged, and `Unsupported` for the
charmaps whose codes libass converts (Windows encodings 2 to 6 through iconv, and
a Symbol charmap under a legacy Arabic character set); `glyph_index` looks `c` up
that way through `text_charmap`.

**TrueType outlines.** `glyf` with short or long `loca`: simple glyphs and
composites with offsets, point matching, one scale, x and y scales or a 2×2
matrix, nested to the depth limit. Offsets scale only with
SCALED_COMPONENT_OFFSET, by the length of each matrix row, rounded to whole units.
A composite takes the advance of its last USE_MY_METRICS component; a component
that adds no points is not placed. As FreeType does, the outline is shifted
horizontally by `lsb - xMin` of the glyph whose metrics apply. `loca` is read
as FreeType sizes it (at most 65536 entries; one too short for `maxp` is read on
into the bytes before the next table when they hold every entry, else the glyph
count drops to the glyphs it covers), and broken `loca` data is repaired: a glyph starting, or (except the last) ending, past `glyf` is
empty, the last glyph is cut at the table's end, and one ending before it starts
runs to the end. Instructions are skipped. Outlines are whole font units, as
FreeType loads them with `FT_LOAD_NO_SCALE`: component matrices apply as
`FT_Outline_Transform` and offsets with `FT_MulFix`, the same fixed-point
arithmetic as at a size with a scale of one.

**CFF and CFF2 outlines.** `CFF ` version 1 with Type 2 charstrings, name-keyed or
CID-keyed (FDArray, FDSelect formats 0 and 3), and `CFF2` (FDSelect format 4 too)
at the default instance of a variable font: `blend` applies the region scalars
FreeType's `cff_blend_build_vector` builds at the origin, in charstrings and
Private DICTs. A face whose `fvar` FreeType accepts has a design vector of its
axes, all zero: a region weighs one unless an axis peaks away from zero, which
scales it down where that axis's range spans zero and excludes it otherwise.
Without `fvar` every region weighs zero. A variation store with other axes
than `fvar` fails each charstring `blend`; in the Private DICT it does not stop
the face from opening, as FreeType first reads that DICT without a design
vector, and the hinter then takes what the DICT's second reading, with the
vector, gives before its first `blend`, as FreeType ignores that reading's
error. Supported: all path operators, flex,
stem hints, `hintmask` and `cntrmask` (the stems are fixed by the first mask or
move: later stem operators are ignored, as is a later mask operator with more
than one operand, whose mask bytes then run as charstring, and a mask over more
than 96 stems fails the glyph, all as in FreeType), local and global subroutines (16 nested
calls, FreeType's limit; TN 5177 states 10), arithmetic, storage and `random` operators (CFF only, as in
FreeType), and `seac` accents through StandardEncoding and the charset, the
accent drawn first. The charset (formats 0, 1 and 2, or ISOAdobe, Expert and
ExpertSubset, which must cover the glyphs) and a name-keyed face's encoding are
read when the face is parsed, as FreeType reads them; one it cannot read makes
the face `Error::Invalid`. Charstrings are evaluated in 16.16 fixed
point as FreeType's CFF engine evaluates them: sums wrap at 32 bits, `mul`, `div`
and `sqrt` round as FreeType's do, integer operands round. Outlines are built as
that engine builds unhinted ones: points are whole font units (the 16.16 value at
1/64 scale, rounded, then shifted down), a contour starts at its first segment,
lines of zero length are dropped, and closing a contour drops a final on-curve
point equal to its first and a contour left with one point. The FontMatrix is
read, normalised and applied in fixed point as FreeType does on 32-bit ARM
(coefficients rounded to a common power of ten saturate at the 32-bit
`FT_LONG_MIN` and `FT_LONG_MAX`, and `FT_Matrix_Check` scales them to 15
significant bits and tests them in 32 bits): a Font DICT's matrix
follows the Top DICT's, outlines and advances go through it, and a Font DICT whose
matrix implies other units per em scales its glyphs by the ratio. Drawing
operators read operand counts the format does not allow as FreeType's engine reads
them, and a glyph whose operator reads past its operands fails; operand types are
not tracked.

**Hinted CFF outlines.** The crate also builds a CFF or CFF2 glyph as FreeType's
default load (`FT_LOAD_DEFAULT`) leaves it in the glyph slot, which
`SizedFont::load` with `Load::Default` returns (below). The same
interpreter and glyph path run with Adobe's hinter as FreeType 2.15's CFF driver
runs it by default, without stem darkening: at a 64th of the size's 16.16 scale,
rounded (adjusted first for a Font DICT with other units per em), the blue
zones of the glyph's Private DICT (BlueValues, OtherBlues, FamilyBlues,
FamilyOtherBlues, BlueScale, BlueShift, BlueFuzz, and LanguageGroup 1's
synthetic em box hints, read when the face is parsed with FreeType's defaults,
number conversions and corrections, so that an entry without its operand makes
the face `Error::Invalid`) capture horizontal stem edges, and hint maps built from the masks
move the other stems' edges onto pixel boundaries; each element of the path is
mapped with the hint map in force when the next one is drawn, as in FreeType.
The 26.6 points then go through the FontMatrix and are moved by its offset in
font units, without further scaling. Sizes beyond 2000 pixels per em, which the
hinter refuses, load unhinted and scaled, as FreeType retries them; a 16.16
scale below 32 fails the glyph. The arithmetic is cf2's, 16.16 in 32 bits.
DICTs are read as FreeType's `cff_parser_run` reads them: a Top or Font DICT
entry takes up to 95 operands in CFF and 512 in CFF2, a Private DICT entry 96
and 513 (the Top DICT's `maxstack`, which FreeType clamps to 513, plus one);
27 is a one-byte number, operators FreeType does not know in a DICT are
skipped with their operands, and a number cut short at the end ends the DICT.
Operands are kept as encoded and converted where read, as FreeType's parser
converts them on 32-bit ARM, the five-byte form signed; offsets and counts
take the first operand, cast to 32 unsigned bits. `vsindex` and `blend` apply
only in a CFF2 Private DICT, where `blend` sums each default and its deltas
times the region scalars in 16.16, as `cff_blend_doBlend` does, and `vsindex`
may not follow it; a charstring's `vsindex` ignores a negative index, and an
index outside the variation store fails at its `blend`. A Private DICT that
is missing, or at offset or size zero, is not read, as `cff_load_private_dict`
skips it; an entry FreeType reads in any DICT, used here or not, fails the
face with fewer operands than FreeType reads (`cfftoken.h` and the field
readers of `cffparse.c`), as does a MultipleMaster entry with other than 2 to
16 designs, and a ROS entry with registry 0xFFFF marks no CID-keyed face. A
CFF Font DICT is read as a Top DICT, as FreeType reads it, and one with its own
ROS has no Private DICT. One difference remains: a CFF2 face whose `fvar`
passes FreeType's header check but which `TT_Get_MM_Var` then refuses opens
here, with the header's axes, where FreeType fails to open it (`cff_face_init`
sets the default instance).

**Control boxes.** `Font::control_box` reads a glyph's box without building its
outline, within the caller's `WorkBudget`, so no point limit applies: a TrueType glyph's `glyf` header box as
stored, or the box of every point a charstring draws, control points included,
in unrounded 16.16 before the FontMatrix (`ControlBox::Empty` when there are no
points). HarfBuzz takes glyph extents from them.

**At a size.** `Font::at_size` takes a 26.6 pixel height and sets up the face as
FreeType does when libass requests the size (`ass_face_set_size`:
`FT_Request_Size`, REAL_DIM, no width or resolution), after libass's GDI face
metrics (`Font::gdi_metrics`: the signed Windows ascent and descent when their sum
is not zero, else FreeType's, else the typographic, else the `head` box, all
16-bit). The scale is that height over ascender minus descender in `FT_DivFix`,
for x and y; pixels per em, the grid-fitted size metrics and every product follow
FreeType's fixed-point rounding (`FT_MulFix`, `FT_DivFix`, `FT_MulDiv`). A size
of zero pixels per em, such as height 0, keeps its scales and metrics, but
glyphs then load unscaled and unhinted, in font units, as `FT_Load_Glyph` loads
them when the scale is undefined. A `SizedFont` then gives:

- `outline_into`: the outline libass draws with hinting off
  (`FT_LOAD_NO_HINTING | FT_LOAD_NO_BITMAP`), in an `Outline` of 26.6 points,
  y up, and its unhinted advance (`advance.x`, `pp2 - pp1`). TrueType
  points, component offsets (scaled with FreeType's CORDIC `FT_Hypot` under
  SCALED_COMPONENT_OFFSET) and the `xMin - lsb` origin are scaled with
  `FT_MulFix`; component matrices apply as `FT_Outline_Transform`. CFF outlines
  are the font-unit outlines above, then scaled point by point.
- `load`: one glyph loaded as `FT_Load_Glyph` loads it, with
  `Load::Unhinted` (the load above) or `Load::Default`, the load libass's
  HarfBuzz callbacks make (`FT_LOAD_DEFAULT`), into an `Outline` and the full
  `GlyphMetrics`: the advances and the control box (`horiBearingX`,
  `horiBearingY`, `width`, `height`), fitted to the pixel grid by the default
  load. A CFF, CFF2 or CID-keyed glyph is hinted there as FreeType's CFF driver
  hints it with Adobe's hinter (`cf2`, without stem darkening): its stems and
  hint masks, counter masks and blue zones at the hinted scale, then the
  FontMatrix and its offset; above 2000 pixels per em it loads unhinted, as
  FreeType retries it. A glyph of a TrueType face without bytecode (no `fpgm`,
  a `prep` of at most 7 bytes) goes through FreeType's auto-hinter, as below.
  Other TrueType glyphs are `Unsupported` there (see below).
- The auto-hinter (`autofit`), as FreeType 2.15 builds it without HarfBuzz and
  runs it for `FT_LOAD_DEFAULT`: the glyph is loaded in font units and hinted
  at the size's scales (not the integer-ppem ones). Each glyph takes the style
  of the first script, in FreeType's order, whose character ranges contain a
  character the face's Unicode charmap (`find_unicode_charmap`) maps to it;
  uncovered glyphs take the CJK fallback. A style of the latin writing system
  measures, once per face, its standard stem widths on the script's standard
  characters, its blue zones on its blue strings and whether the digits share
  one advance; a style without blue zones, and the dummy style, leave the
  outline unhinted. Latin hinting finds segments, stems, serifs and edges in
  both dimensions (vertically only in an italic face), fits the vertical scale
  to the x height, aligns horizontal edges to blue zones (base characters
  only), fits stems to whole pixels, interpolates strong and weak points, and
  applies the accent adjustments of FreeType's database (tildes stretched,
  blue zones kept off diacritics, diacritics kept a pixel from their base).
  The outline is then moved by its hinted left phantom point; the advance
  runs between the hinted phantom points (the scaled advance in a fixed-pitch
  face and for digits of one width), the vertical advance is scaled by the
  style's vertical scale, and the box is fitted outwards to pixels. Glyphs of
  the CJK and Indic styles have their advances (the scaled advance, rounded,
  which those writing systems leave unhinted) and, without points, their empty
  outlines, but outlines with points are `Unsupported`. The style of every glyph and each style's metrics are
  computed on the first load that needs them and kept with the face, charged
  to its budget, as is the storage one load hints in, which the next reuses
  and which stays charged at the largest glyph's size.
- `synthesize`: libass's synthetic styles, decided by `Font::synthesis` from the
  requested weight and slant against `OS/2.fsSelection` bits 0 and 5 and
  `Font::weight`: the oblique shear (0x2d24 for CFF faces, 0x5700 for others, as
  `FT_Outline_Transform` applies it), then `FT_Outline_EmboldenXY` with libass's
  strength (`embolden_strength`, the em in 26.6 pixels over 64), including
  `FT_Vector_NormLen` and the orientation test. `Outline::transform`,
  `embolden` and `orientation` are public.
- `default_advance`: the advance of FreeType's default load, which libass's
  HarfBuzz callbacks report, as `load` with `Load::Default` gives it: for an
  auto-hinted face the auto-hinter's, found with only the vertical edges of a
  latin glyph hinted and without the points of other glyphs; otherwise the
  unhinted advance at the hinting scale (TrueType faces with `head.flags` bit 3
  scale by the integer pixels per em there) rounded to a pixel. That is exact
  for CFF and for TrueType whose `fpgm`, `prep` and glyph programs can only
  issue `INSTCTRL` with a constant selector 1 or 2, so that FreeType's v40
  interpreter keeps backward compatibility and hinting cannot move an advance;
  other instructed faces are `Unsupported` (see below). At zero pixels per em
  it is the unscaled advance.
- The vertical advances libass uses for a glyph of an `@` font it draws
  rotated: the outline's `GlyphMetrics` carry the unhinted load's
  `metrics.vertAdvance` (26.6) and `linearVertAdvance` (16.16, scaled by the
  size's y scale with `FT_MulDiv`, in 32 bits as on 32-bit ARM, after either
  load), and `default_vertical_advance` gives the
  default load's `metrics.vertAdvance`, which libass gives HarfBuzz, rounded to
  a pixel at the hinting scale (the auto-hinter's vertical scale for a face it
  hints) and `Unsupported` where `default_advance` is.
  The advance in font units is the `vmtx` advance height with vertical metrics,
  else the one FreeType makes up from the typographic ascender and descender
  (`OS/2`, else `hhea`). A TrueType glyph measures it between its phantom
  points (the header's `yMax` plus the top side bearing, down by the advance
  height, of the glyph whose metrics apply), scaled and read back in font units
  as a 16-bit value; a CFF glyph puts it through the FontMatrix.
- `metrics` (`FT_Size_Metrics`), `ascent_descent` (libass's line ascent and
  descent), `harfbuzz_scale` (the scale libass sets on its HarfBuzz font),
  `kerning` (`FT_KERNING_DEFAULT`: scaled, reduced below 25 pixels per em,
  rounded), and `underline` and `strikeout` (the bands libass adds from `post`
  and `OS/2`, rounded as it rounds them).

**Kerning.** Microsoft `kern` version 0: horizontal, non-minimum format 0
subtables, at most 32, summed, with the override bit replacing the sum. As in
FreeType the version is not checked; Apple's version 1 reads as no subtables.
`kerning_pairs` lists the pairs those subtables contain.

**Face matching.** `FaceInfo` and `select` score faces as libass 0.17.5 does for
fonts it loads itself. A `Request` carries the weight and slant of libass's font
description (700 for the ASS bold values 1 and -1, 100 for the italic value 1,
other values unsigned); any slant but 0 asks for an italic face. Windows family names (or FreeType's family name when a face
has none) match the request ignoring ASCII case and score 1 for a missing italic, 4
for an unwanted one, plus 73/256 of the weight difference, where a face not flagged
bold counts as 120 heavier when the request is more than 150 heavier. A full or
PostScript name match scores 0 and ends the search; when only one of the two
matches, CFF faces go by the PostScript name and others by the full name. A face
whose text charmap lacks the requested code, looked up unchanged and not
necessarily a scalar value, is skipped. Weights come from
`usWeightClass`, with classes 1 to 9 read as 100 to 900; the italic and bold flags
from `fsSelection` bits 0 and 5. `FaceInfo::has_family_name` tells the faces
libass registers at all.

**Store.** `Store` caches built values, such as faces and outlines, by typed key.
Held values keep a reservation on the store's `Budget`; when a reservation fails,
the least recently used values that only the store holds are dropped.

## What it does not read

WOFF and WOFF2, bare CFF, Type 1 and Type 42, bitmap and colour glyphs (`EBDT`,
`CBDT`, `sbix`, `COLR`, `SVG `), variation instances other than the default (`fvar`,
`gvar`, `HVAR`, `avar`), vertical origins and bearings (`VORG`, top side
bearings beyond the advance), `post` beyond the
underline, TrueType hinting (`cvt ` is not read, `fpgm`, `prep` and glyph
programs are only scanned for `INSTCTRL`), CFF stem darkening, StdHW and StdVW
(which only darkening reads), Apple `kern` version 1 and
kern formats other than 0, GPOS and GSUB, and CFF features outside the above: Type
1 charstrings (CharstringType 1) and synthetic fonts.
A face without a usable `glyf` or CFF table is `Error::Invalid`, where FreeType
opens it and fails each glyph.

At a size, the default load of an instructed TrueType glyph is `Unsupported`,
hence its glyph extents and contour points, which libass's HarfBuzz callbacks
report: it needs the TrueType interpreter, which this crate does not run. So is
the default-load outline, with points, of an auto-hinted glyph of a CJK or Indic style, whose
writing systems the auto-hinter here does not run, and HarfBuzz's coverage of
glyphs through OpenType features, which FreeType's auto-hinter has only with
HarfBuzz built in. The auto-hinter runs only the normal render mode, without
the light mode's stem darkening.
`default_advance` is `Unsupported` for tricky fonts known by family name and
programs that may leave backward compatibility; fonts FreeType finds tricky only
by their table checksums are not recognised.
Sizes beyond 65535 pixels per em are refused. Coordinates are `i32`; a scaled coordinate beyond it is
`Error::Overflow` in an unhinted or CFF load, where FreeType's `FT_Pos` wraps at
32 bits on 32-bit ARM (libass rejects anything beyond 2^28 itself); the
auto-hinter wraps its `FT_Pos` and `FT_Fixed` arithmetic as FreeType does there. The `random` operator does not follow FreeType's
generator.

## Hinting

The gate covers libass's default configuration, hinting off: unhinted outlines at
256 pixels and the default-load advances libass's HarfBuzz callbacks read at that
size, with the vertical advances of `@` fonts (`font-hinting-none`), all of which
match FreeType, the auto-hinted faces' included. The default load itself, every
glyph's metrics and hinted outline at that size, is gated too
(`font-hinting-default`): every CFF, CFF2 and CID-keyed face matches FreeType, and
so does the auto-hinted authored-cmap-formats. DejaVuMathTeXGyre is open under the
auto-hinter's CJK writing system, which styles most of its glyphs, and the
instructed TrueType faces under the TrueType interpreter. For libass's default
configuration that gap is in what HarfBuzz reads to position marks without GPOS
anchors (glyph extents and contour points) in those fonts; the outlines libass
draws are unhinted and compared. For libass's hinting-enabled modes (NATIVE, LIGHT,
NORMAL), the unhinted outlines and default-load advances at nine other sizes are
only measured (`font-hinting-modes`), and the hinted outlines those modes draw
are not compared.

## Behaviour and limits

Parsing a face reads the directory and fixed headers and checks every structure a
lookup depends on; glyph programs, cmap entries, names and kerning pairs are read
from the font bytes when asked for. A face retains the shared `Bytes` plus small
tables charged to the `Budget` it was created with: the supported cmap records, one
Private DICT per CFF FDArray entry, variation region scalars and kern subtable
records. A face FreeType auto-hints also keeps, once a default load needs
them, the auto-hinter's style of each glyph, its reverse charmap of accented
characters and each latin style's metrics, charged to that budget, as is the
scratch storage of an auto-hinted load. An `Outline` charges its points and
contour ends to its own budget and keeps its capacity across glyphs; a
`SizedFont` is a small copyable view that allocates nothing.

`Limits` bounds the work of one parse and of one outline (`work`), nested
composites and subroutine calls (`depth`) and the points of one outline
(`points`); exceeding one is `Error::Policy`. Format ceilings (48 or 513 operands,
96 stems, 32 transient slots, 16 nested subroutine calls in CFF, 64 nested
composites) are
`Error::Invalid`, as is malformed data. Unsupported files and features are
`Error::Unsupported`. No input panics.

## Verification

- `cargo test -p triergon-font`: unit tests per table, faces built in memory
  (TrueType, CFF and a collection) and a deterministic mutation test over them.
- Four harness features (`harness/features/font-*`) run every file of the corpus
  in `harness/fonts/corpus.json` through [`font-worker`](../font-worker/README.md)
  and the FreeType 2.15.0 reference worker (`harness/reference_font.h`), and require
  both to fold exactly what FreeType folds:
  - `font-outlines`, per face: metrics, style, family, style and PostScript names,
    every name record, every cmap mapping, the text charmap, the kerning of every
    pair the `kern` table lists, and for every glyph its `hmtx` and loaded advances
    and its unscaled outline in font units (`FT_LOAD_NO_SCALE`).
  - `font-hinting-none`, libass 0.17.5's default (ASS_HINTING_NONE) at 256 pixels
    (16384 in 26.6), after libass's GDI face metrics, as three cases per file. The
    outlines case folds the size metrics, ascent and descent, HarfBuzz scale,
    emboldening strength, PostScript flag, underline and strikeout bands and the
    kerning of every listed pair, and every glyph's unhinted outline and advance,
    plain, emboldened, slanted and both, every point. The advances case folds every
    glyph's default-load advance; one `default_advance` declares `Unsupported` ends
    the candidate's operation with an error, so the case fails. The vertical case
    folds every glyph's `linearVertAdvance` and `metrics.vertAdvance` of the
    unhinted load and `metrics.vertAdvance` of the default load, failing the same
    way.
  - `font-hinting-default`, one case per file at 256 pixels: every glyph's
    default load (`FT_LOAD_DEFAULT`, as libass's shaper loads it), folding
    `metrics.horiAdvance` and `vertAdvance`, `horiBearingX` and `horiBearingY`,
    `width` and `height`, then the hinted outline, every point. A glyph `load`
    declares `Unsupported` ends the candidate's operation with an error.
  - `font-hinting-modes`, the outlines and advances cases at the nine other heights libass
    requests with hinting on: 75, 82.5, 50, 48, 37.5, 33.75, 13.33, 24.5 and 8.4
    pixels (common ASS sizes on 1080- and 720-line frames, below 25 pixels per em,
    and small).

  `cargo xtask check` requires `font-outlines`, `font-hinting-none` and
  `font-hinting-default` (`cargo xtask fonts compare`); `font-hinting-modes` runs
  on demand (`cargo xtask fonts compare --all`) and is not gated yet. The
  required features list their open cases, which `check` still runs and reports. The case rows are
  generated (`cargo xtask fonts manifest`) and each case's expected work is pinned
  from FreeType's output in `pins.json`. `cargo xtask report` runs each feature on
  its own and with `--benchmark` and `--memory` measures runtime and memory
  against FreeType. `font-dump` and `font-sized-dump` print the folded values as
  lines in both workers, for diffing a failing case.
- The corpus includes five fonts the crate's test support writes
  (`examples/authored-fonts.rs`): `seac`, the arithmetic, logic and storage operators,
  the flex forms, counter masks, subroutines nested twelve deep, curve operators with
  operand counts the format does not allow, a skewing FontMatrix with an offset,
  one cmap subtable of each common format, a collection with a face for each
  step of the text charmap selection, a collection with a face for each way
  FreeType reads vertical advances (long `vmtx` records with level phantom
  points, an advance height of 65535 and `USE_MY_METRICS`, a `vhea` without
  records, a short `vhea`, and `VHEA` with `VMTX`), and a CFF face with format 2
  and 8 subtables, glyph names of every form FreeType reads (with a variant, a
  repeated name and `space` and `Delta`, which it maps twice) and a custom
  encoding with a supplement, used by the font and shaping features and by
  `ass-render`. With them the
  comparison exercises every charstring operator except `vsindex` (CFF2 fonts in the
  corpus do not use it) and `random`, every supported cmap format, every step of
  the text charmap selection and every vertical-metrics path;
  `examples/font-coverage.rs` reports the operators and formats.
- `cargo xtask fuzz --contract harness/features/<feature>/contract.json` mutates
  every case's file through the release and dev builds of the worker (the dev build
  checks overflow), with a timeout per input, and with `--reference` compares each
  result with FreeType's. `cargo xtask check` runs one round of it on a few cases
  of each required feature.

The comparison establishes agreement with FreeType on that corpus, not conformance
with the OpenType specification. Face matching is checked by unit tests and
through the renderer's comparison with libass (`ass-render`).

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
