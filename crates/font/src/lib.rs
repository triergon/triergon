#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used
)]
// Pending: `missing_docs`, `clippy::expect_used`.

mod cff;
mod cmap;
mod fixed;
mod glyf;
mod kern;
mod matching;
mod metrics;
mod name;
mod outline;
mod read;
mod sfnt;
mod sized;
mod store;
mod synthesis;
mod work;

pub use cff::{CharstringBounds, CharstringOperator, OperatorSet};
pub use cmap::{CharEncoding, Charmap, CmapFormat, Mappings};
pub use matching::{FaceInfo, Request, select};
pub use metrics::{FsSelection, MacStyle, Os2};
pub use name::{Language, NameChars, NameId, NameRecord, Platform};
pub use outline::{Outline, OutlinePoint, Point, PointKind, Segment};
pub use sized::{Band, SizeMetrics, SizedFont, pixels_26_6};
pub use store::{Built, Store, StoreKey};
pub use synthesis::{Orientation, Synthesis};

use cff::{Cff, Version};
use fixed::{Scale, mul_fix};
use glyf::Glyf;
use kern::Kern;
use metrics::{Head, Hhea, Hmtx};
use name::Names;
use sfnt::{Flavor, Tables};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

/// A glyph index within one face.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GlyphId(pub u16);

/// Format-policy limits. Allocation is bounded separately by the [`Budget`] a face
/// and its outlines are charged to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Work units of one parse or one outline: table records, cmap and kerning
    /// entries, charstring operators, glyph points.
    pub work: u64,
    /// Nested composite glyphs and charstring subroutine calls. The formats'
    /// own ceilings apply as well.
    pub depth: u32,
    /// Points of one outline.
    pub points: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            work: 1 << 24,
            depth: 16,
            points: 1 << 18,
        }
    }
}

/// The outline technology of a face.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutlineFormat {
    /// Quadratic contours in `glyf`.
    TrueType,
    /// Type 2 charstrings in `CFF `.
    Cff,
    /// CFF2 charstrings, read at the default instance.
    Cff2,
}

/// FreeType's style flags, from `OS/2.fsSelection` (italic or oblique, bold) or,
/// without `OS/2`, from `head.macStyle`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
}

/// Face-wide vertical metrics in font units, chosen as FreeType chooses them:
/// typographic metrics when `OS/2` sets USE_TYPO_METRICS, else `hhea`, else when
/// both `hhea` values are zero the typographic and then the Windows metrics.
/// Values are 16-bit, as FreeType stores them, and the height is positive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerticalMetrics {
    pub ascender: i32,
    pub descender: i32,
    /// Baseline to baseline.
    pub height: i32,
}

/// What loading a glyph reports besides its outline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlyphMetrics {
    /// The advance. A TrueType composite takes the advance of its last
    /// `USE_MY_METRICS` component; a CFF FontMatrix scales and offsets it.
    pub advance: i32,
}

/// The `head` bounding box of all glyphs, in font units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bounds {
    pub x_min: i16,
    pub y_min: i16,
    pub x_max: i16,
    pub y_max: i16,
}

/// The `hhea` line metrics, in font units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HorizontalHeader {
    pub ascender: i16,
    pub descender: i16,
    pub line_gap: i16,
    /// `advanceWidthMax`.
    pub advance_max: u16,
}

#[derive(Debug)]
enum Outlines {
    Glyf(Glyf),
    Cff(Box<Cff>),
}

/// One parsed face of an OpenType or TrueType font file or collection.
///
/// Parsing reads the table directory and the fixed headers and checks every
/// structure lookups depend on. Glyph data, cmap entries, names and kerning pairs
/// are read from the font bytes when asked for, so a face holds little beyond
/// the shared [`Bytes`].
#[derive(Debug)]
pub struct Font {
    data: Bytes,
    index: u32,
    tables: Tables,
    head: Head,
    hhea: Hhea,
    os2: Option<Os2>,
    glyph_count: u16,
    hmtx: Hmtx,
    names: Names,
    charmaps: BudgetVec<cmap::Subtable>,
    outlines: Outlines,
    kern: Kern,
    /// `post` underline position and thickness.
    underline: (i16, i16),
    limits: Limits,
}

impl Font {
    /// The number of faces in a font file: one, or a collection's count.
    pub fn face_count(data: &[u8]) -> Result<u32> {
        sfnt::face_count(data)
    }

    /// Parses face `index` of `data`. Retained structures are charged to `budget`.
    ///
    /// # Errors
    ///
    /// `Unsupported` for files that are not sfnt fonts or collections (WOFF, Type 1,
    /// bare CFF) and for CFF features outside this reader; `Invalid` for malformed
    /// required tables; `Policy` when `limits.work` runs out; and the budget's
    /// errors.
    pub fn new(data: Bytes, index: u32, budget: &Budget, limits: Limits) -> Result<Self> {
        let mut work = WorkBudget::new(limits.work);
        let bytes = data.as_ref();
        let (flavor, tables) = sfnt::directory(bytes, index, &mut work)?;
        let required =
            |table: Option<sfnt::Table>| table.ok_or(Error::Invalid("missing required table"));
        let head = metrics::head(required(tables.head)?.bytes(bytes))?;
        let hhea = metrics::hhea(required(tables.hhea)?.bytes(bytes))?;
        let maxp_glyphs = metrics::glyph_count(required(tables.maxp)?.bytes(bytes))?;
        let os2 = tables.os2.and_then(|t| metrics::os2(t.bytes(bytes)));
        // FreeType treats a broken `name` table as a missing one.
        let names = tables
            .name
            .and_then(|t| Names::new(bytes, t.offset, t.len).ok())
            .unwrap_or_default();
        // Tables already on core's work budget share it.
        let mut work = WorkBudget::new(limits.work);
        let charmaps = match tables.cmap {
            Some(t) => cmap::parse(bytes, t, budget, &mut work)?,
            None => BudgetVec::new(budget),
        };
        let (outlines, glyph_count) = match flavor {
            Flavor::TrueType => {
                let (glyf, count) = Glyf::new(
                    required(tables.glyf)?,
                    required(tables.loca)?,
                    head.long_offsets,
                    maxp_glyphs,
                    tables.loca_room,
                )?;
                (Outlines::Glyf(glyf), count)
            }
            Flavor::Cff => {
                let (table, version) = match (tables.cff, tables.cff2) {
                    (Some(t), _) => (t, Version::Cff1),
                    (None, Some(t)) => (t, Version::Cff2),
                    (None, None) => return Err(Error::Invalid("missing CFF table")),
                };
                let cff = Cff::parse(bytes, table, version, head.units_per_em, budget, &mut work)?;
                let count = u16::try_from(cff.glyph_count()).map_err(|_| Error::Overflow)?;
                (Outlines::Cff(Box::new(cff)), count)
            }
        };
        let hmtx = Hmtx::new(required(tables.hmtx)?, hhea.metrics, glyph_count);
        let kern = match tables.kern {
            Some(t) => Kern::parse(bytes, t, budget, &mut work)?,
            None => Kern::new(budget),
        };
        let underline = metrics::underline(tables.post.map(|t| t.bytes(bytes)));
        Ok(Self {
            data,
            index,
            tables,
            head,
            hhea,
            os2,
            glyph_count,
            hmtx,
            names,
            charmaps,
            outlines,
            kern,
            underline,
            limits,
        })
    }

    /// The font bytes.
    pub fn data(&self) -> &Bytes {
        &self.data
    }

    /// The bytes of table `tag`: the first nonempty record with that tag whose
    /// range lies inside the file, as FreeType's `FT_Load_Sfnt_Table` finds it.
    /// Tables this crate does not read, such as `GDEF`, `GSUB` and `GPOS`, are
    /// found the same way.
    pub fn table(&self, tag: [u8; 4]) -> Option<&[u8]> {
        let data = self.data.as_ref();
        sfnt::find(data, self.tables.records, tag).map(|t| t.bytes(data))
    }

    /// The face's index in its file.
    pub const fn index(&self) -> u32 {
        self.index
    }

    pub const fn glyph_count(&self) -> u16 {
        self.glyph_count
    }

    /// Every glyph of the face, in index order.
    pub fn glyphs(&self) -> impl Iterator<Item = GlyphId> + use<> {
        (0..self.glyph_count).map(GlyphId)
    }

    pub const fn units_per_em(&self) -> u16 {
        self.head.units_per_em
    }

    pub const fn bounds(&self) -> Bounds {
        let [x_min, y_min, x_max, y_max] = self.head.bounds;
        Bounds {
            x_min,
            y_min,
            x_max,
            y_max,
        }
    }

    pub const fn mac_style(&self) -> MacStyle {
        self.head.mac_style
    }

    pub fn outline_format(&self) -> OutlineFormat {
        match &self.outlines {
            Outlines::Glyf(_) => OutlineFormat::TrueType,
            Outlines::Cff(cff) if cff.version() == Version::Cff1 => OutlineFormat::Cff,
            Outlines::Cff(_) => OutlineFormat::Cff2,
        }
    }

    /// The `OS/2` table, when present and at least 78 bytes long.
    pub const fn os2(&self) -> Option<Os2> {
        self.os2
    }

    pub const fn horizontal_header(&self) -> HorizontalHeader {
        HorizontalHeader {
            ascender: self.hhea.ascender,
            descender: self.hhea.descender,
            line_gap: self.hhea.line_gap,
            advance_max: self.hhea.advance_max,
        }
    }

    pub fn vertical_metrics(&self) -> VerticalMetrics {
        let HorizontalHeader {
            ascender: a,
            descender: d,
            line_gap: g,
            ..
        } = self.horizontal_header();
        let short = |v: i32| i32::from(v as i16);
        let metrics = |ascender: i32, descender: i32, gap: i32| {
            let (ascender, descender) = (short(ascender), short(descender));
            // FT_Open_Face makes a negative height positive.
            let height = short(ascender - descender + gap);
            VerticalMetrics {
                ascender,
                descender,
                height: if height < 0 { short(-height) } else { height },
            }
        };
        let typo = |o: &Os2| {
            metrics(
                i32::from(o.typo_ascender),
                i32::from(o.typo_descender),
                i32::from(o.typo_line_gap),
            )
        };
        match self.os2 {
            Some(os2) if os2.selection.use_typo_metrics() => typo(&os2),
            Some(os2) if a == 0 && d == 0 => {
                if os2.typo_ascender != 0 || os2.typo_descender != 0 {
                    typo(&os2)
                } else {
                    // FreeType reads the unsigned Windows values as 16-bit signed.
                    metrics(
                        i32::from(os2.win_ascent.cast_signed()),
                        -i32::from(os2.win_descent.cast_signed()),
                        0,
                    )
                }
            }
            _ => metrics(i32::from(a), i32::from(d), i32::from(g)),
        }
    }

    pub fn style(&self) -> Style {
        match self.os2 {
            Some(os2) => Style {
                bold: os2.selection.bold(),
                italic: os2.selection.italic() || os2.selection.oblique(),
            },
            None => Style {
                bold: self.head.mac_style.bold(),
                italic: self.head.mac_style.italic(),
            },
        }
    }

    /// The usable naming-table records in table order. Empty records and records
    /// outside the string storage are skipped.
    pub fn names(&self) -> impl Iterator<Item = NameRecord<'_>> + '_ {
        self.names.records(self.data.as_ref())
    }

    /// The record FreeType reports for `name_id` (see [`Self::family_name`]).
    pub fn preferred_name(&self, name_id: NameId) -> Option<NameRecord<'_>> {
        self.names.preferred(self.data.as_ref(), name_id)
    }

    /// The family name FreeType reports: the WWS family (ID 21), then the
    /// typographic family (16), then the family (1); a face that sets the
    /// `fsSelection` WWS bit skips ID 21. Each ID's record is chosen as
    /// [`Self::preferred_name`] does.
    pub fn family_name(&self) -> Option<NameRecord<'_>> {
        self.first_name(&[NameId::WwsFamily, NameId::TypographicFamily, NameId::Family])
    }

    /// The style name FreeType reports, by the rule of [`Self::family_name`] over
    /// IDs 22, 17 and 2.
    pub fn style_name(&self) -> Option<NameRecord<'_>> {
        self.first_name(&[
            NameId::WwsSubfamily,
            NameId::TypographicSubfamily,
            NameId::Subfamily,
        ])
    }

    fn first_name(&self, ids: &[NameId; 3]) -> Option<NameRecord<'_>> {
        let wws_only = self.os2.is_some_and(|o| o.selection.wws());
        let ids = if wws_only { &ids[1..] } else { &ids[..] };
        ids.iter().find_map(|&id| self.preferred_name(id))
    }

    /// The PostScript name record (ID 6) FreeType reads for an sfnt face: among
    /// Windows records (encoding 0 or 1) the last US English one, else the first;
    /// failing that, among Macintosh Roman records the last English one, else the
    /// first. FreeType then keeps only printable ASCII other than `[](){}<>/%`.
    pub fn postscript_name(&self) -> Option<NameRecord<'_>> {
        let pick = |platform: Platform, encodings: &[CharEncoding], language: Language| {
            let mut found: Option<NameRecord<'_>> = None;
            for r in self.names().filter(|r| {
                r.name_id == NameId::PostScript
                    && r.platform == platform
                    && encodings.contains(&r.encoding())
            }) {
                if found.is_none() || r.language == language {
                    found = Some(r);
                }
            }
            found
        };
        pick(
            Platform::Windows,
            &[CharEncoding::Symbol, CharEncoding::Unicode],
            Language::WINDOWS_ENGLISH_US,
        )
        .or_else(|| {
            pick(
                Platform::Macintosh,
                &[CharEncoding::MacRoman],
                Language::MACINTOSH_ENGLISH,
            )
        })
    }

    /// The supported cmap subtables, in directory order.
    pub fn charmaps(&self) -> impl Iterator<Item = Charmap<'_>> + '_ {
        self.charmaps
            .as_slice()
            .iter()
            .map(|&subtable| self.charmap(subtable))
    }

    fn charmap(&self, subtable: cmap::Subtable) -> Charmap<'_> {
        Charmap::new(
            self.data.as_ref(),
            u32::from(self.glyph_count),
            subtable,
            self.limits.work,
        )
    }

    /// The charmap libass 0.17.5 uses for text, as it selects one when it opens
    /// a face (`ass_charmap_magic`). In directory order: the first Windows
    /// full-Unicode subtable (3, 10); else the first Windows BMP subtable (3, 1);
    /// else the first other Windows subtable, such as Symbol (3, 0). A face
    /// without Windows subtables keeps the charmap FreeType selects on opening
    /// it: the last full-Unicode subtable ((0, 4), or (0, 6) in format 13),
    /// else the last Unicode- or ISO-platform one. Without one of those, it is
    /// the first subtable.
    ///
    /// Only the subtables [`Self::charmaps`] lists take part. FreeType also
    /// lists formats 2, 8 and 14, and adds a Unicode charmap built from glyph
    /// names to a CFF face without a (3, 1) or Unicode-platform subtable; libass
    /// may select either, which this selection does not reproduce.
    pub fn text_charmap(&self) -> Option<Charmap<'_>> {
        cmap::text_subtable(self.charmaps.as_slice()).map(|&subtable| self.charmap(subtable))
    }

    /// The glyph for `c` through [`Self::text_charmap`]. A Windows Symbol charmap
    /// is looked up at `0xF000 | c`, as libass does.
    pub fn glyph_index(&self, c: char) -> Option<GlyphId> {
        let charmap = self.text_charmap()?;
        let code = u32::from(c);
        let code = if charmap.encoding() == CharEncoding::Symbol {
            code | 0xf000
        } else {
            code
        };
        charmap.glyph(code)
    }

    /// Every (left, right) pair the `kern` table lists, in table order, possibly
    /// repeated; [`Self::kerning`] gives each pair's value.
    pub fn kerning_pairs(&self) -> impl Iterator<Item = (GlyphId, GlyphId)> + '_ {
        self.kern.pairs(self.data.as_ref())
    }

    /// The `hmtx` advance of `glyph` in font units.
    pub fn advance(&self, glyph: GlyphId) -> Result<u16> {
        let glyph = self.check(glyph)?;
        Ok(self.hmtx.advance(self.data.as_ref(), glyph))
    }

    /// The `kern` table value for the pair, in font units.
    pub fn kerning(&self, left: GlyphId, right: GlyphId) -> i32 {
        self.kern.pair(self.data.as_ref(), left, right)
    }

    fn check(&self, glyph: GlyphId) -> Result<u16> {
        (glyph.0 < self.glyph_count)
            .then_some(glyph.0)
            .ok_or(Error::Invalid("glyph index outside the face"))
    }

    /// Loads `glyph` into `outline`, replacing its contents, and returns its
    /// metrics. Points are in font units, unhinted, at the default instance of a
    /// variable font.
    ///
    /// A TrueType outline is shifted horizontally so that its left side bearing is
    /// the `hmtx` value, by `lsb - xMin` of the glyph whose metrics apply, as
    /// FreeType places it. CFF outlines are unshifted and, as FreeType's CFF
    /// engine builds them, in whole font units (rounded down), then through the
    /// FontMatrix in fixed point. [`Font::at_size`] loads outlines scaled to
    /// pixels.
    ///
    /// # Errors
    ///
    /// `Invalid` for a glyph outside the face or malformed glyph data, `Policy` for
    /// [`Limits`] and the outline budget's errors. On error the outline holds
    /// whatever was loaded before the failure.
    pub fn outline_into(&self, glyph: GlyphId, outline: &mut Outline) -> Result<GlyphMetrics> {
        self.load(glyph, Scale::UNITS, outline)
    }

    /// Loads `glyph` as FreeType does without hinting, at the 16.16 `scale` (one
    /// for font units), and returns its advance at that scale.
    pub(crate) fn load(
        &self,
        glyph: GlyphId,
        scale: Scale,
        outline: &mut Outline,
    ) -> Result<GlyphMetrics> {
        let index = self.check(glyph)?;
        let data = self.data.as_ref();
        let mut work = WorkBudget::new(self.limits.work);
        let advance = match &self.outlines {
            Outlines::Glyf(table) => {
                outline.begin(self.limits.points);
                let m = table.outline(&self.glyf_context(), glyph, scale, outline, &mut work)?;
                // FreeType's phantom points: the origin and the advance.
                let origin = i64::from(m.x_min) - i64::from(self.side_bearing(m.glyph));
                let advance = i64::from(self.hmtx.advance(data, m.glyph));
                let pp1 = mul_fix(origin, scale.x);
                let pp2 = mul_fix(origin + advance, scale.x);
                if pp1 != 0 {
                    glyf::translate(outline.points_mut(), (-pp1, 0))?;
                }
                pp2 - pp1
            }
            Outlines::Cff(cff) => {
                cff.load(data, glyph, scale, outline, &self.limits, &mut work)?;
                cff.advance(data, glyph, self.hmtx.advance(data, index), scale)?
            }
        };
        Ok(GlyphMetrics {
            advance: glyf::narrow(advance)?,
        })
    }

    /// The charstring operators `glyph`'s program executes, with its subroutines
    /// and `seac` components; empty for TrueType outlines. Corpus tools use it to
    /// show which operators a comparison exercises.
    pub fn charstring_operators(
        &self,
        glyph: GlyphId,
        scratch: &mut Outline,
    ) -> Result<OperatorSet> {
        self.check(glyph)?;
        match &self.outlines {
            Outlines::Glyf(_) => Ok(OperatorSet::default()),
            Outlines::Cff(cff) => {
                let mut work = WorkBudget::new(self.limits.work);
                cff.operators(self.data.as_ref(), glyph, scratch, &self.limits, &mut work)
            }
        }
    }

    /// The box of every point `glyph`'s charstring draws, before the FontMatrix
    /// and unrounded; `None` for TrueType outlines and for a glyph that draws
    /// nothing. Shaping takes CFF glyph extents from it; `scratch` is reused
    /// storage.
    ///
    /// # Errors
    ///
    /// As [`Font::outline_into`].
    pub fn charstring_bounds(
        &self,
        glyph: GlyphId,
        scratch: &mut Outline,
    ) -> Result<Option<CharstringBounds>> {
        self.check(glyph)?;
        match &self.outlines {
            Outlines::Glyf(_) => Ok(None),
            Outlines::Cff(cff) => {
                let mut work = WorkBudget::new(self.limits.work);
                cff.bounds(self.data.as_ref(), glyph, scratch, &self.limits, &mut work)
            }
        }
    }

    /// Loads `glyph` into a new outline charged to `budget`.
    pub fn outline(&self, glyph: GlyphId, budget: &Budget) -> Result<(Outline, GlyphMetrics)> {
        let mut outline = Outline::new(budget);
        let metrics = self.outline_into(glyph, &mut outline)?;
        Ok((outline, metrics))
    }

    fn glyf_context(&self) -> glyf::Context<'_> {
        glyf::Context {
            data: self.data.as_ref(),
            glyph_count: self.glyph_count,
            limits: &self.limits,
        }
    }

    /// The weight libass assigns the face: `OS/2.usWeightClass`, with classes 1
    /// to 9 read as 100 to 900 (4 as 350), and 400 or 700 by the bold style when
    /// it is zero or `OS/2` is absent.
    pub fn weight(&self) -> u32 {
        match self.os2.map_or(0, |o| o.weight_class) {
            0 if self.style().bold => 700,
            0 => 400,
            4 => 350,
            class @ 1..=9 => u32::from(class) * 100,
            class => u32::from(class),
        }
    }

    /// The `hmtx` left side bearing of `glyph`.
    fn side_bearing(&self, glyph: u16) -> i32 {
        i32::from(self.hmtx.side_bearing(self.data.as_ref(), glyph))
    }

    /// Whether the face is CID-keyed CFF.
    pub fn is_cid_keyed(&self) -> bool {
        matches!(&self.outlines, Outlines::Cff(cff) if cff.is_cid_keyed())
    }
}
