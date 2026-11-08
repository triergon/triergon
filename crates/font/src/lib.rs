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
mod glyf;
mod kern;
mod matching;
mod metrics;
mod name;
mod outline;
mod read;
mod sfnt;
mod store;

pub use cmap::Charmap;
pub use matching::{FaceInfo, Request, select};
pub use metrics::Os2;
pub use name::{NameChars, NameRecord, id as name_id, platform};
pub use outline::{Outline, OutlinePoint, Point, PointKind, Segment};
pub use store::{Built, Store, StoreKey};

use cff::{Cff, Version};
use glyf::Glyf;
use kern::Kern;
use metrics::{Head, Hhea, Hmtx};
use name::Names;
use read::Meter;
use sfnt::{Flavor, Tables};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result};

/// A glyph index within one face.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GlyphId(pub u32);

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
    /// `USE_MY_METRICS` component.
    pub advance: u16,
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
    charmaps: BudgetVec<Charmap>,
    outlines: Outlines,
    kern: Kern,
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
        let mut meter = Meter::new(limits.work);
        let bytes = data.as_ref();
        let (flavor, tables) = sfnt::directory(bytes, index, &mut meter)?;
        let required =
            |table: Option<sfnt::Table>| table.ok_or(Error::Invalid("missing required table"));
        let head = metrics::head(required(tables.head)?.bytes(bytes))?;
        let hhea = metrics::hhea(required(tables.hhea)?.bytes(bytes))?;
        let maxp_glyphs = metrics::glyph_count(required(tables.maxp)?.bytes(bytes))?;
        let os2 = tables.os2.and_then(|t| metrics::os2(t.bytes(bytes)));
        let names = match tables.name {
            Some(t) => Names::new(bytes, t.offset, t.len)?,
            None => Names::default(),
        };
        let charmaps = match tables.cmap {
            Some(t) => cmap::parse(bytes, t, budget, &mut meter)?,
            None => BudgetVec::new(budget),
        };
        let (outlines, glyph_count) = match flavor {
            Flavor::TrueType => {
                let glyf = Glyf::new(
                    required(tables.glyf)?,
                    required(tables.loca)?,
                    head.long_offsets,
                    maxp_glyphs,
                )?;
                (Outlines::Glyf(glyf), maxp_glyphs)
            }
            Flavor::Cff => {
                let (table, version) = match (tables.cff, tables.cff2) {
                    (Some(t), _) => (t, Version::Cff1),
                    (None, Some(t)) => (t, Version::Cff2),
                    (None, None) => return Err(Error::Invalid("missing CFF table")),
                };
                let cff = Cff::parse(bytes, table, version, head.units_per_em, budget, &mut meter)?;
                let count = u16::try_from(cff.glyph_count()).map_err(|_| Error::Overflow)?;
                (Outlines::Cff(Box::new(cff)), count)
            }
        };
        let hmtx = Hmtx::new(required(tables.hmtx)?, hhea.metrics, glyph_count)?;
        let kern = match tables.kern {
            Some(t) => Kern::parse(bytes, t, budget, &mut meter)?,
            None => Kern::new(budget),
        };
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
            limits,
        })
    }

    /// The font bytes.
    pub fn data(&self) -> &Bytes {
        &self.data
    }

    /// The face's index in its file.
    pub const fn index(&self) -> u32 {
        self.index
    }

    pub const fn glyph_count(&self) -> u32 {
        self.glyph_count as u32
    }

    pub const fn units_per_em(&self) -> u16 {
        self.head.units_per_em
    }

    /// `head` bounding box: x min, y min, x max, y max.
    pub const fn bounds(&self) -> [i16; 4] {
        self.head.bounds
    }

    pub const fn mac_style(&self) -> u16 {
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

    /// `hhea` ascender, descender and line gap.
    pub const fn horizontal_header(&self) -> (i16, i16, i16) {
        (self.hhea.ascender, self.hhea.descender, self.hhea.line_gap)
    }

    pub const fn max_advance(&self) -> u16 {
        self.hhea.advance_max
    }

    pub fn vertical_metrics(&self) -> VerticalMetrics {
        let (a, d, g) = self.horizontal_header();
        let typo = |o: &Os2| VerticalMetrics {
            ascender: i32::from(o.typo_ascender),
            descender: i32::from(o.typo_descender),
            height: i32::from(o.typo_ascender) - i32::from(o.typo_descender)
                + i32::from(o.typo_line_gap),
        };
        match self.os2 {
            Some(os2) if os2.fs_selection & 0x80 != 0 => typo(&os2),
            Some(os2) if a == 0 && d == 0 => {
                if os2.typo_ascender != 0 || os2.typo_descender != 0 {
                    typo(&os2)
                } else {
                    // FreeType reads the unsigned Windows values as 16-bit signed.
                    let ascender = i32::from(os2.win_ascent.cast_signed());
                    let descender = -i32::from(os2.win_descent.cast_signed());
                    VerticalMetrics {
                        ascender,
                        descender,
                        height: ascender - descender,
                    }
                }
            }
            _ => VerticalMetrics {
                ascender: i32::from(a),
                descender: i32::from(d),
                height: i32::from(a) - i32::from(d) + i32::from(g),
            },
        }
    }

    pub fn style(&self) -> Style {
        match self.os2 {
            Some(os2) => Style {
                bold: os2.fs_selection & 0x20 != 0,
                italic: os2.fs_selection & 0x201 != 0,
            },
            None => Style {
                bold: self.head.mac_style & 1 != 0,
                italic: self.head.mac_style & 2 != 0,
            },
        }
    }

    /// The usable naming-table records in table order. Empty records and records
    /// outside the string storage are skipped.
    pub fn names(&self) -> impl Iterator<Item = NameRecord<'_>> + '_ {
        self.names.records(self.data.as_ref())
    }

    /// The record FreeType reports for `name_id` (see [`Self::family_name`]).
    pub fn preferred_name(&self, name_id: u16) -> Option<NameRecord<'_>> {
        self.names.preferred(self.data.as_ref(), name_id)
    }

    /// The family name FreeType reports: the WWS family (ID 21), then the
    /// typographic family (16), then the family (1); a face that sets the
    /// `fsSelection` WWS bit skips ID 21. Each ID's record is chosen as
    /// [`Self::preferred_name`] does.
    pub fn family_name(&self) -> Option<NameRecord<'_>> {
        self.first_name(&[
            name_id::WWS_FAMILY,
            name_id::TYPOGRAPHIC_FAMILY,
            name_id::FAMILY,
        ])
    }

    /// The style name FreeType reports, by the rule of [`Self::family_name`] over
    /// IDs 22, 17 and 2.
    pub fn style_name(&self) -> Option<NameRecord<'_>> {
        self.first_name(&[
            name_id::WWS_SUBFAMILY,
            name_id::TYPOGRAPHIC_SUBFAMILY,
            name_id::SUBFAMILY,
        ])
    }

    fn first_name(&self, ids: &[u16; 3]) -> Option<NameRecord<'_>> {
        let wws_only = self.os2.is_some_and(|o| o.fs_selection & 0x100 != 0);
        let ids = if wws_only { &ids[1..] } else { &ids[..] };
        ids.iter().find_map(|&id| self.preferred_name(id))
    }

    /// The PostScript name record (ID 6) FreeType reads for an sfnt face: among
    /// Windows records (encoding 0 or 1) the last US English one, else the first;
    /// failing that, among Macintosh Roman records the last English one, else the
    /// first. FreeType then keeps only printable ASCII other than `[](){}<>/%`.
    pub fn postscript_name(&self) -> Option<NameRecord<'_>> {
        let pick = |platform: u16, encodings: &[u16], language: u16| {
            let mut found: Option<NameRecord<'_>> = None;
            for r in self.names().filter(|r| {
                r.name_id == name_id::POSTSCRIPT
                    && r.platform == platform
                    && encodings.contains(&r.encoding)
            }) {
                if found.is_none() || r.language == language {
                    found = Some(r);
                }
            }
            found
        };
        pick(platform::WINDOWS, &[0, 1], 0x409).or_else(|| pick(platform::MACINTOSH, &[0], 0))
    }

    /// The supported cmap subtables, in directory order.
    pub fn charmaps(&self) -> &[Charmap] {
        self.charmaps.as_slice()
    }

    /// The glyph `code` maps to in `charmap`, which must be one of
    /// [`Self::charmaps`]. Codes the subtable does not map, and glyphs at or above
    /// the glyph count, give `None`.
    pub fn glyph_for_code(&self, charmap: &Charmap, code: u32) -> Option<GlyphId> {
        let glyph = charmap.lookup(self.data.as_ref(), code)?;
        (glyph != 0 && glyph < self.glyph_count()).then_some(GlyphId(glyph))
    }

    /// Every (code, glyph) pair of `charmap`, in code order, without glyph 0.
    pub fn mappings<'a>(&'a self, charmap: &Charmap) -> impl Iterator<Item = (u32, GlyphId)> + 'a {
        charmap.mappings(self.data.as_ref(), self.glyph_count())
    }

    /// The charmap for text: the first Windows Unicode subtable (3, 1) or (3, 10)
    /// in directory order, else the first Windows subtable, else the Unicode
    /// subtable FreeType selects by default, else the first subtable. This is the
    /// selection libass makes.
    pub fn text_charmap(&self) -> Option<&Charmap> {
        let all = self.charmaps();
        let windows = |c: &&Charmap| c.platform == platform::WINDOWS;
        all.iter()
            .find(|c| windows(c) && matches!(c.encoding, 1 | 10))
            .or_else(|| all.iter().find(windows))
            .or_else(|| {
                let full = |c: &&Charmap| matches!((c.platform, c.encoding), (3, 10) | (0, 4 | 6));
                let unicode = |c: &&Charmap| {
                    c.platform == 0 && c.encoding != 5 || (c.platform, c.encoding) == (3, 1)
                };
                all.iter()
                    .rev()
                    .find(full)
                    .or_else(|| all.iter().rev().find(unicode))
            })
            .or_else(|| all.first())
    }

    /// The glyph for `c` through [`Self::text_charmap`]. A Windows Symbol charmap
    /// is looked up at `0xF000 | c`, as libass does.
    pub fn glyph_index(&self, c: char) -> Option<GlyphId> {
        let charmap = self.text_charmap()?;
        let code = u32::from(c);
        let code = if (charmap.platform, charmap.encoding) == (3, 0) {
            code | 0xf000
        } else {
            code
        };
        self.glyph_for_code(charmap, code)
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

    /// Every pair the `kern` subtables used by [`Self::kerning`] list, in table
    /// order. A pair listed by several subtables appears once for each.
    pub fn kerning_pairs(&self) -> impl Iterator<Item = (GlyphId, GlyphId)> + '_ {
        self.kern.pairs(self.data.as_ref())
    }

    fn check(&self, glyph: GlyphId) -> Result<u16> {
        u16::try_from(glyph.0)
            .ok()
            .filter(|&g| g < self.glyph_count)
            .ok_or(Error::Invalid("glyph index outside the face"))
    }

    /// Loads `glyph` into `outline`, replacing its contents, and returns its
    /// metrics. Points are in font units, unhinted, at the default instance of a
    /// variable font.
    ///
    /// A TrueType outline is shifted horizontally so that its left side bearing is
    /// the `hmtx` value, by `lsb - xMin` of the glyph whose metrics apply, as
    /// FreeType places it. CFF outlines are unshifted.
    ///
    /// # Errors
    ///
    /// `Invalid` for a glyph outside the face or malformed glyph data, `Policy` for
    /// [`Limits`] and the outline budget's errors. On error the outline holds
    /// whatever was loaded before the failure.
    pub fn outline_into(&self, glyph: GlyphId, outline: &mut Outline) -> Result<GlyphMetrics> {
        let index = self.check(glyph)?;
        let data = self.data.as_ref();
        let mut meter = Meter::new(self.limits.work);
        match &self.outlines {
            Outlines::Glyf(glyf) => {
                outline.begin(self.limits.points);
                let cx = glyf::Context {
                    data,
                    glyph_count: self.glyph_count,
                    limits: &self.limits,
                };
                let metrics = glyf.outline(&cx, glyph, outline, &mut meter)?;
                let shift = self.side_bearing(metrics.glyph) - i32::from(metrics.x_min);
                if shift != 0 {
                    for p in outline.points_mut() {
                        p.point.x = (f64::from(p.point.x) + f64::from(shift)) as f32;
                    }
                }
                Ok(GlyphMetrics {
                    advance: self.hmtx.advance(data, metrics.glyph),
                })
            }
            Outlines::Cff(cff) => {
                cff.outline(data, glyph, outline, &self.limits, &mut meter)?;
                Ok(GlyphMetrics {
                    advance: self.hmtx.advance(data, index),
                })
            }
        }
    }

    /// Loads `glyph` into a new outline charged to `budget`.
    pub fn outline(&self, glyph: GlyphId, budget: &Budget) -> Result<(Outline, GlyphMetrics)> {
        let mut outline = Outline::new(budget);
        let metrics = self.outline_into(glyph, &mut outline)?;
        Ok((outline, metrics))
    }

    /// The `hmtx` left side bearing of `glyph`, or zero where the table lacks it.
    fn side_bearing(&self, glyph: u16) -> i32 {
        let Some(table) = self.tables.hmtx else {
            return 0;
        };
        let bytes = table.bytes(self.data.as_ref());
        let metrics = usize::from(self.hhea.metrics.min(self.glyph_count));
        let glyph = usize::from(glyph);
        let at = if glyph < metrics {
            4 * glyph + 2
        } else {
            4 * metrics + 2 * (glyph - metrics)
        };
        read::u16_at(bytes, at).map_or(0, |v| i32::from(v.cast_signed()))
    }

    /// Whether the face is CID-keyed CFF.
    pub fn is_cid_keyed(&self) -> bool {
        matches!(&self.outlines, Outlines::Cff(cff) if cff.is_cid_keyed())
    }
}
