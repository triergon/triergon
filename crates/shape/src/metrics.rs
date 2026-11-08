//! Glyph lookup and metrics: the font functions shaping asks for, and HarfBuzz's
//! scaling arithmetic.

use crate::ot::data::{Data, bsearch};
use std::cell::RefCell;
use triergon_core::Budget;
use triergon_font::{Charmap, Font, GlyphId, Outline, OutlineFormat, Platform};

/// The scale positions are produced in, as HarfBuzz's `hb_font_set_scale` and
/// `hb_font_set_ppem` set it. The default is one unit per font unit and no ppem,
/// which disables hinting device tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scale {
    pub x: i32,
    pub y: i32,
    pub x_ppem: u32,
    pub y_ppem: u32,
}

impl Scale {
    /// HarfBuzz's default for a face: units per em on both axes.
    pub fn units(units_per_em: u16) -> Self {
        Self {
            x: i32::from(units_per_em),
            y: i32::from(units_per_em),
            x_ppem: 0,
            y_ppem: 0,
        }
    }
}

/// A glyph's ink box in scaled units: the top-left corner relative to the
/// origin, y up, and a width and (negative) height.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Extents {
    pub x_bearing: i32,
    pub y_bearing: i32,
    pub width: i32,
    pub height: i32,
}

/// What shaping asks of the font beyond its layout tables: HarfBuzz's font
/// functions and the [`Scale`] they answer in, which positions are produced in.
///
/// [`OtMetrics`] answers from the font's own tables, as HarfBuzz's built-in
/// functions do; a renderer can answer from its rasteriser instead, as libass does
/// with FreeType.
pub trait Metrics {
    /// The scale of every value the other functions return.
    fn scale(&self) -> Scale;
    /// The glyph for a character, if the font maps it.
    fn nominal_glyph(&self, c: char) -> Option<GlyphId>;
    /// The glyph for a character with a variation selector, if the font has one.
    fn variation_glyph(&self, c: char, selector: char) -> Option<GlyphId>;
    fn h_advance(&self, glyph: GlyphId) -> i32;
    fn extents(&self, glyph: GlyphId) -> Option<Extents>;
    /// The position of outline point `point`, for anchors that name one (used
    /// only with a ppem).
    fn contour_point(&self, _glyph: GlyphId, _point: u16) -> Option<(i32, i32)> {
        None
    }
    /// Pair kerning for the fallback used when a font has neither GPOS kerning
    /// nor a `kern` table, if the font functions provide it (HarfBuzz's built-in
    /// ones do not).
    fn h_kerning(&self) -> Option<&dyn Fn(GlyphId, GlyphId) -> i32> {
        None
    }
}

/// The font functions HarfBuzz builds in (`hb-ot`), at a [`Scale`]: the character
/// map HarfBuzz prefers, `cmap` format 14 variation sequences, `hmtx` advances.
#[derive(Debug)]
pub struct OtMetrics<'a> {
    font: &'a Font,
    charmap: Option<Charmap<'a>>,
    /// A symbol charmap also maps U+0000..U+00FF at U+F000..U+F0FF.
    symbol: bool,
    /// A Macintosh charmap maps only ASCII.
    ascii_only: bool,
    variations: Data<'a>,
    scale: Scale,
    x_mult: i64,
    x_multf: f32,
    y_multf: f32,
    /// Reused for CFF glyph bounds.
    outline: RefCell<Outline>,
}

impl<'a> OtMetrics<'a> {
    /// The font functions of `font` at `scale`; glyph outlines read for extents
    /// are charged to `budget`.
    pub fn new(font: &'a Font, scale: Scale, budget: &Budget) -> Self {
        use Platform::{Macintosh, Unicode, Windows};
        let find = |platform: Platform, encoding: Option<u16>| {
            font.charmaps()
                .find(|c| c.platform() == platform && encoding.is_none_or(|e| c.encoding_id() == e))
        };
        // HarfBuzz's preference: symbol, then full and BMP Unicode, then Mac.
        let symbol = find(Windows, Some(0));
        let unicode = [
            (Windows, 10),
            (Unicode, 6),
            (Unicode, 4),
            (Windows, 1),
            (Unicode, 3),
            (Unicode, 2),
            (Unicode, 1),
            (Unicode, 0),
        ]
        .into_iter()
        .find_map(|(p, e)| find(p, Some(e)));
        let mac = find(Macintosh, Some(0)).or_else(|| find(Macintosh, None));
        let ascii_only = symbol.is_none() && unicode.is_none() && mac.is_some();
        let is_symbol = symbol.is_some();
        let charmap = symbol.or(unicode).or(mac);
        let upem = i64::from(font.units_per_em());
        Self {
            font,
            charmap,
            symbol: is_symbol,
            ascii_only,
            variations: variation_subtable(font.table(*b"cmap").unwrap_or_default()),
            scale,
            x_mult: i64::from(scale.x) * 0x10000 / upem,
            x_multf: scale.x as f32 / f32::from(font.units_per_em()),
            y_multf: scale.y as f32 / f32::from(font.units_per_em()),
            outline: RefCell::new(Outline::new(budget)),
        }
    }

    /// The unscaled ink box (x min, y max, width, height ≤ 0) as hb-ot reads it:
    /// for TrueType the `glyf` header box moved to the `hmtx` side bearing, for
    /// CFF the charstring's bounds, each side rounded as `roundf` rounds it
    /// after HarfBuzz narrows its double to a float.
    fn unscaled_extents(&self, glyph: GlyphId) -> Option<[i32; 4]> {
        if glyph.0 >= self.font.glyph_count() {
            return None;
        }
        if self.font.outline_format() != OutlineFormat::TrueType {
            let mut outline = self.outline.try_borrow_mut().ok()?;
            let bounds = self.font.charstring_bounds(glyph, &mut outline).ok()?;
            let round = |v: i32| ((f64::from(v) / 65536.0) as f32).round() as i32;
            let [mut x, mut y, mut w, mut h] = [0; 4];
            if let Some(b) = bounds {
                if b.x_min < b.x_max {
                    x = round(b.x_min);
                    w = round(b.x_max) - x;
                }
                if b.y_min < b.y_max {
                    y = round(b.y_max);
                    h = round(b.y_min) - y;
                }
            }
            return Some([x, y, w, h]);
        }
        let header = self.glyph_header(glyph);
        let read = |at: usize| i32::from(header.i16(at));
        if read(0) == 0 {
            return Some([0; 4]);
        }
        let (x_min, y_min, x_max, y_max) = (read(2), read(4), read(6), read(8));
        let lsb = self.side_bearing(glyph).unwrap_or(x_min.min(x_max));
        Some([
            lsb,
            y_min.max(y_max),
            (x_max - x_min).abs(),
            -(y_max - y_min).abs(),
        ])
    }

    /// The `glyf` data of `glyph`, empty when `loca` gives none.
    fn glyph_header(&self, glyph: GlyphId) -> Data<'a> {
        let (Some(glyf), Some(loca), Some(head)) = (
            self.font.table(*b"glyf"),
            self.font.table(*b"loca"),
            self.font.table(*b"head"),
        ) else {
            return Data::EMPTY;
        };
        let (loca, g) = (Data(loca), usize::from(glyph.0));
        let (start, end) = if Data(head).i16(50) == 0 {
            (
                2 * usize::from(loca.u16(2 * g)),
                2 * usize::from(loca.u16(2 * g + 2)),
            )
        } else {
            (loca.u32(4 * g) as usize, loca.u32(4 * g + 4) as usize)
        };
        glyf.get(start..end).map_or(Data::EMPTY, Data)
    }

    /// The `hmtx` left side bearing of `glyph`, if the table has one for it.
    fn side_bearing(&self, glyph: GlyphId) -> Option<i32> {
        let hmtx = Data(self.font.table(*b"hmtx")?);
        let metrics = usize::from(Data(self.font.table(*b"hhea")?).u16(34));
        let g = usize::from(glyph.0);
        let at = if g < metrics {
            4 * g + 2
        } else {
            4 * metrics + 2 * (g - metrics)
        };
        (at + 2 <= hmtx.0.len()).then(|| i32::from(hmtx.i16(at)))
    }

    fn lookup(&self, code: u32) -> Option<GlyphId> {
        self.charmap?.glyph(code)
    }
}

impl Metrics for OtMetrics<'_> {
    fn scale(&self) -> Scale {
        self.scale
    }

    fn nominal_glyph(&self, c: char) -> Option<GlyphId> {
        let c = u32::from(c);
        if self.ascii_only && c >= 0x80 {
            return None;
        }
        self.lookup(c).or_else(|| {
            (self.symbol && c <= 0xff)
                .then(|| self.lookup(0xf000 + c))
                .flatten()
        })
    }

    fn variation_glyph(&self, base: char, selector: char) -> Option<GlyphId> {
        let (c, selector) = (u32::from(base), u32::from(selector));
        let table = self.variations;
        let records = table.u32(6) as usize;
        let records = records.min(table.0.len().saturating_sub(10) / 11);
        let record = bsearch(records, |i| table.u24(10 + 11 * i).cmp(&selector))?;
        let at = 10 + 11 * record;
        let defaults = table.at(table.u32(at + 3) as usize);
        let in_default = {
            let count = (defaults.u32(0) as usize).min(defaults.0.len().saturating_sub(4) / 4);
            bsearch(count, |i| {
                let first = defaults.u24(4 + 4 * i);
                let last = first + u32::from(defaults.u8(4 + 4 * i + 3));
                if c < first {
                    std::cmp::Ordering::Greater
                } else if c > last {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_some()
        };
        if in_default {
            return self.nominal_glyph(base);
        }
        let mappings = table.at(table.u32(at + 7) as usize);
        let count = (mappings.u32(0) as usize).min(mappings.0.len().saturating_sub(4) / 5);
        let i = bsearch(count, |i| mappings.u24(4 + 5 * i).cmp(&c))?;
        Some(GlyphId(mappings.u16(4 + 5 * i + 3)))
    }

    fn h_advance(&self, glyph: GlyphId) -> i32 {
        let advance = self.font.advance(glyph).map_or(0, i64::from);
        ((advance * self.x_mult + 32768) >> 16) as i32
    }

    fn extents(&self, glyph: GlyphId) -> Option<Extents> {
        let [x, y, w, h] = self.unscaled_extents(glyph)?;
        // Scaled as HarfBuzz scales extents: outwards to whole units.
        let x1 = x as f32 * self.x_multf;
        let y1 = y as f32 * self.y_multf;
        let x2 = x1 + w as f32 * self.x_multf;
        let y2 = y1 + h as f32 * self.y_multf;
        let (x_bearing, y_bearing) = (x1.floor() as i32, y1.floor() as i32);
        Some(Extents {
            x_bearing,
            y_bearing,
            width: x2.ceil() as i32 - x_bearing,
            height: y2.ceil() as i32 - y_bearing,
        })
    }
}

/// The format 14 subtable of the Unicode variation-sequence encoding (0, 5).
fn variation_subtable(cmap: &[u8]) -> Data<'_> {
    let cmap = Data(cmap);
    let count = cmap.count(2, 4, 8);
    (0..count)
        .map(|i| 4 + 8 * i)
        .find(|&at| cmap.u16(at) == 0 && cmap.u16(at + 2) == 5)
        .map(|at| cmap.offset32(at + 4))
        .filter(|table| table.u16(0) == 14)
        .unwrap_or_default()
}

/// The metrics and scale shaping uses, with HarfBuzz's scaling arithmetic.
pub(crate) struct Scaled<'a> {
    pub(crate) metrics: &'a dyn Metrics,
    pub(crate) scale: Scale,
    x_mult: i64,
    y_mult: i64,
    x_multf: f32,
    y_multf: f32,
}

impl<'a> Scaled<'a> {
    pub(crate) fn new(metrics: &'a dyn Metrics, units_per_em: u16) -> Self {
        let scale = metrics.scale();
        let upem = f32::from(units_per_em);
        let upem_int = i64::from(units_per_em);
        Self {
            metrics,
            scale,
            x_mult: i64::from(scale.x) * 0x10000 / upem_int,
            y_mult: i64::from(scale.y) * 0x10000 / upem_int,
            x_multf: scale.x as f32 / upem,
            y_multf: scale.y as f32 / upem,
        }
    }

    /// The nominal glyph of the character `c` (a code point the buffer holds).
    pub(crate) fn glyph(&self, c: u32) -> Option<u32> {
        let glyph = self.metrics.nominal_glyph(char::from_u32(c)?)?;
        Some(u32::from(glyph.0))
    }

    pub(crate) fn variation_glyph(&self, c: u32, selector: u32) -> Option<u32> {
        let (c, selector) = (char::from_u32(c)?, char::from_u32(selector)?);
        Some(u32::from(self.metrics.variation_glyph(c, selector)?.0))
    }

    /// The advance of a glyph index the buffer holds; zero for one no face has.
    pub(crate) fn advance(&self, glyph: u32) -> i32 {
        u16::try_from(glyph).map_or(0, |g| self.metrics.h_advance(GlyphId(g)))
    }

    pub(crate) fn extents(&self, glyph: u32) -> Option<Extents> {
        self.metrics.extents(GlyphId(u16::try_from(glyph).ok()?))
    }

    pub(crate) fn contour_point(&self, glyph: u32, point: u16) -> Option<(i32, i32)> {
        self.metrics
            .contour_point(GlyphId(u16::try_from(glyph).ok()?), point)
    }

    /// A font-unit distance in scaled units, rounded as HarfBuzz's `em_mult`.
    pub(crate) fn em_scale_x(&self, v: i32) -> i32 {
        ((i64::from(v) * self.x_mult + 32768) >> 16) as i32
    }

    pub(crate) fn em_scale_y(&self, v: i32) -> i32 {
        ((i64::from(v) * self.y_mult + 32768) >> 16) as i32
    }

    /// A font-unit coordinate in scaled units, unrounded, as `em_fscale`.
    pub(crate) fn em_fscale_x(&self, v: i32) -> f32 {
        v as f32 * self.x_multf
    }

    pub(crate) fn em_fscale_y(&self, v: i32) -> f32 {
        v as f32 * self.y_multf
    }
}

impl std::fmt::Debug for Scaled<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scaled")
            .field("scale", &self.scale)
            .finish_non_exhaustive()
    }
}
