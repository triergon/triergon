//! Glyph lookup and metrics: the font functions shaping asks for, and HarfBuzz's
//! scaling arithmetic.

use crate::ot::data::{Data, bsearch};
use std::cell::Cell;
use triergon_core::{Error, Result};
use triergon_font::{CharEncoding, Charmap, ControlBox, Font, GlyphId, Platform, SizedFont};

/// The scale positions are produced in, as HarfBuzz's `hb_font_set_scale` and
/// `hb_font_set_ppem` set it. The default is one unit per font unit and no ppem,
/// which disables hinting device tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scale {
    /// Units per em horizontally: positions are in em / `x`.
    pub x: i32,
    /// Units per em vertically.
    pub y: i32,
    /// Horizontal pixels per em, for hinting device tables; zero for none.
    pub x_ppem: u32,
    /// Vertical pixels per em; zero for none.
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
    /// From the origin to the box's left edge.
    pub x_bearing: i32,
    /// From the origin to the box's top edge, y up.
    pub y_bearing: i32,
    /// The box's width.
    pub width: i32,
    /// The box's height, negative as it extends downwards.
    pub height: i32,
}

/// What shaping asks of the font beyond its layout tables: HarfBuzz's font
/// functions and the [`Scale`] they answer in, which positions are produced in.
///
/// [`OtMetrics`] answers from the font's own tables, as HarfBuzz's built-in
/// functions do; [`SizedMetrics`] as libass's FreeType callbacks do at a size.
/// The functions that load a glyph return a `Result`: an error ends the shaping
/// call with it, while a glyph the font cannot load is an ordinary answer (zero
/// or `None`), as it is to HarfBuzz.
pub trait Metrics {
    /// The scale of every value the other functions return.
    fn scale(&self) -> Scale;
    /// The glyph for a character, if the font maps it.
    fn nominal_glyph(&self, c: char) -> Option<GlyphId>;
    /// The glyph for a character with a variation selector, if the font has one.
    fn variation_glyph(&self, c: char, selector: char) -> Option<GlyphId>;
    /// The horizontal advance of `glyph`; zero for one the font cannot load.
    ///
    /// # Errors
    ///
    /// What keeps the functions from answering as their font would: a work or
    /// budget limit, or a value they cannot compute.
    fn h_advance(&self, glyph: GlyphId) -> Result<i32>;
    /// The ink box of `glyph`; `None` when the font gives none.
    ///
    /// # Errors
    ///
    /// As [`Self::h_advance`].
    fn extents(&self, glyph: GlyphId) -> Result<Option<Extents>>;
    /// The position of outline point `point`, for anchors that name one (used
    /// only with a ppem); `None` when the glyph has no such point.
    ///
    /// # Errors
    ///
    /// As [`Self::h_advance`].
    fn contour_point(&self, _glyph: GlyphId, _point: u16) -> Result<Option<(i32, i32)>> {
        Ok(None)
    }
    /// Whether [`Self::h_kerning`] gives pair kerning, which shaping applies when
    /// a font has neither GPOS nor a `kern` table. HarfBuzz's built-in functions
    /// give none.
    fn has_h_kerning(&self) -> bool {
        false
    }
    /// The kerning of a glyph pair in visual order.
    ///
    /// # Errors
    ///
    /// As [`Self::h_advance`].
    fn h_kerning(&self, _left: GlyphId, _right: GlyphId) -> Result<i32> {
        Ok(0)
    }
}

/// A glyph the font cannot load (`Invalid`) answers `otherwise`, as it does to
/// HarfBuzz; other errors stop shaping.
fn unless_unloadable<T>(result: Result<T>, otherwise: T) -> Result<T> {
    match result {
        Err(Error::Invalid(_)) => Ok(otherwise),
        result => result,
    }
}

/// The font functions HarfBuzz builds in (`hb-ot`), at a [`Scale`]: the character
/// map HarfBuzz prefers, `cmap` format 14 variation sequences, `hmtx` advances,
/// and extents from the `glyf` header box or the charstring's control box.
#[derive(Debug)]
pub struct OtMetrics<'a> {
    font: &'a Font,
    charmap: Option<Charmap<'a>>,
    /// A symbol charmap also maps U+0000..U+00FF at U+F000..U+F0FF.
    symbol: bool,
    /// A Macintosh charmap maps only ASCII.
    ascii_only: bool,
    variations: Variations<'a>,
    scale: Scale,
    x_mult: i64,
    x_multf: f32,
    y_multf: f32,
}

impl<'a> OtMetrics<'a> {
    /// The font functions of `font` at `scale`. Each glyph they read is bounded
    /// by the font's [`Limits`](triergon_font::Limits).
    pub fn new(font: &'a Font, scale: Scale) -> Self {
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
            variations: Variations::of(font),
            scale,
            x_mult: i64::from(scale.x) * 0x10000 / upem,
            x_multf: scale.x as f32 / f32::from(font.units_per_em()),
            y_multf: scale.y as f32 / f32::from(font.units_per_em()),
        }
    }

    /// The unscaled ink box (x min, y max, width, height ≤ 0) as hb-ot reads it:
    /// for TrueType the `glyf` header box moved to the `hmtx` side bearing, for
    /// CFF the charstring's control box, each side rounded as `roundf` rounds it
    /// after HarfBuzz narrows its double to a float.
    fn unscaled_extents(&self, glyph: GlyphId) -> Result<Option<[i32; 4]>> {
        let Some(control) = unless_unloadable(self.font.control_box(glyph).map(Some), None)? else {
            return Ok(None);
        };
        Ok(Some(match control {
            ControlBox::Empty => [0; 4],
            ControlBox::Glyf(b) => {
                let [x_min, y_min, x_max, y_max] =
                    [b.x_min, b.y_min, b.x_max, b.y_max].map(i32::from);
                [
                    i32::from(self.font.side_bearing(glyph)?),
                    y_min.max(y_max),
                    (x_max - x_min).abs(),
                    -(y_max - y_min).abs(),
                ]
            }
            ControlBox::Charstring(b) => {
                let round = |v: i32| ((f64::from(v) / 65536.0) as f32).round() as i32;
                let [mut x, mut y, mut w, mut h] = [0; 4];
                if b.x_min < b.x_max {
                    x = round(b.x_min);
                    w = round(b.x_max) - x;
                }
                if b.y_min < b.y_max {
                    y = round(b.y_max);
                    h = round(b.y_min) - y;
                }
                [x, y, w, h]
            }
        }))
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
        match self.variations.lookup(u32::from(base), u32::from(selector)) {
            Variation::Default => self.nominal_glyph(base),
            Variation::Glyph(glyph) => Some(glyph),
            Variation::None => None,
        }
    }

    fn h_advance(&self, glyph: GlyphId) -> Result<i32> {
        let advance = unless_unloadable(self.font.advance(glyph), 0)?;
        Ok(((i64::from(advance) * self.x_mult + 32768) >> 16) as i32)
    }

    fn extents(&self, glyph: GlyphId) -> Result<Option<Extents>> {
        let Some([x, y, w, h]) = self.unscaled_extents(glyph)? else {
            return Ok(None);
        };
        // Scaled as HarfBuzz scales extents: outwards to whole units.
        let x1 = x as f32 * self.x_multf;
        let y1 = y as f32 * self.y_multf;
        let x2 = x1 + w as f32 * self.x_multf;
        let y2 = y1 + h as f32 * self.y_multf;
        let (x_bearing, y_bearing) = (x1.floor() as i32, y1.floor() as i32);
        Ok(Some(Extents {
            x_bearing,
            y_bearing,
            width: x2.ceil() as i32 - x_bearing,
            height: y2.ceil() as i32 - y_bearing,
        }))
    }
}

/// The font functions libass 0.17.5 installs on the HarfBuzz font of a face at a
/// size (`init_font_funcs` and `get_hb_font` in `ass_shaper.c`): the character
/// map libass selects (`ass_charmap_magic`) looked up as it looks codes up
/// (`ass_font_index_magic`), FreeType's variation sequences, default-load
/// advances ([`SizedFont::hinted_advance`]) and `FT_KERNING_DEFAULT` kerning
/// ([`SizedFont::kerning`]), in 26.6 pixels at the size's
/// [`SizedFont::harfbuzz_scale`] and pixels per em, so that hinting device
/// tables apply.
///
/// libass reads glyph extents and contour points from FreeType's hinted load,
/// which this crate does not run: for a glyph of the face both are
/// `Unsupported`, as is an advance `hinted_advance` cannot give. The vertical
/// advances libass substitutes in an `@` font are not covered.
#[derive(Debug)]
pub struct SizedMetrics<'a> {
    sized: SizedFont<'a>,
    charmap: Option<Charmap<'a>>,
    /// A Windows Symbol charmap, looked up at `0xF000 | c`.
    symbol: bool,
    /// A Unicode charmap, which FreeType needs for variation sequences.
    unicode: bool,
    variations: Variations<'a>,
    scale: Scale,
}

impl<'a> SizedMetrics<'a> {
    /// libass's font functions over `sized`.
    ///
    /// # Errors
    ///
    /// `Unsupported` when libass would convert codes for the charmap it selects,
    /// which this crate does not: Windows Shift-JIS, PRC, Big5, Wansung and Johab
    /// subtables, and the Symbol subtable of a face declaring a legacy Arabic
    /// character set (`OS/2.fsSelection` high byte 178 or 179).
    pub fn new(sized: SizedFont<'a>) -> Result<Self> {
        let font = sized.font();
        let charmap = font.text_charmap();
        let windows = charmap.filter(|c| c.platform() == Platform::Windows);
        if windows.is_some_and(|c| (2..=6).contains(&c.encoding_id())) {
            return Err(Error::Unsupported("libass's multibyte charmap conversion"));
        }
        let symbol = windows.is_some_and(|c| c.encoding() == CharEncoding::Symbol);
        let arabic = font
            .os2()
            .is_some_and(|os2| matches!(os2.selection.bits() >> 8, 178 | 179));
        if symbol && arabic {
            return Err(Error::Unsupported("libass's legacy Arabic charmaps"));
        }
        let unicode = charmap.is_some_and(|c| {
            c.platform() == Platform::Unicode
                || matches!(
                    c.encoding(),
                    CharEncoding::Unicode | CharEncoding::UnicodeFull
                )
        });
        let (x, y) = sized.harfbuzz_scale();
        let size = sized.metrics();
        Ok(Self {
            sized,
            charmap,
            symbol,
            unicode,
            variations: Variations::of(font),
            scale: Scale {
                x,
                y,
                x_ppem: u32::from(size.x_ppem),
                y_ppem: u32::from(size.y_ppem),
            },
        })
    }

    /// The code libass looks `c` up at; it never looks up zero.
    fn code(&self, c: char) -> Option<u32> {
        let c = u32::from(c);
        let code = if self.symbol { c | 0xf000 } else { c };
        (code != 0).then_some(code)
    }

    /// `Unsupported` for a glyph of the face, which FreeType would hint; a glyph
    /// outside it does not load, which libass answers with nothing.
    fn hinted<T>(&self, glyph: GlyphId, what: &'static str) -> Result<Option<T>> {
        if glyph.0 < self.sized.font().glyph_count() {
            Err(Error::Unsupported(what))
        } else {
            Ok(None)
        }
    }
}

impl Metrics for SizedMetrics<'_> {
    fn scale(&self) -> Scale {
        self.scale
    }

    fn nominal_glyph(&self, c: char) -> Option<GlyphId> {
        self.charmap?.glyph(self.code(c)?)
    }

    fn variation_glyph(&self, c: char, selector: char) -> Option<GlyphId> {
        let code = self.code(c).filter(|_| self.unicode)?;
        match self.variations.lookup(code, u32::from(selector)) {
            Variation::Default => self.charmap?.glyph(code),
            Variation::Glyph(glyph) => Some(glyph),
            Variation::None => None,
        }
    }

    fn h_advance(&self, glyph: GlyphId) -> Result<i32> {
        unless_unloadable(self.sized.hinted_advance(glyph), 0)
    }

    fn extents(&self, glyph: GlyphId) -> Result<Option<Extents>> {
        self.hinted(glyph, "glyph extents of FreeType's hinted load")
    }

    fn contour_point(&self, glyph: GlyphId, _point: u16) -> Result<Option<(i32, i32)>> {
        self.hinted(glyph, "contour points of FreeType's hinted load")
    }

    fn has_h_kerning(&self) -> bool {
        true
    }

    fn h_kerning(&self, left: GlyphId, right: GlyphId) -> Result<i32> {
        Ok(self.sized.kerning(left, right))
    }
}

/// How a variation sequence maps.
enum Variation {
    /// To the base character's nominal glyph.
    Default,
    Glyph(GlyphId),
    /// Not at all.
    None,
}

/// The `cmap` format 14 subtable of the Unicode variation-sequence encoding (0,
/// 5): the first one, as HarfBuzz and FreeType both take it.
#[derive(Clone, Copy, Debug)]
struct Variations<'a>(Data<'a>);

impl<'a> Variations<'a> {
    fn of(font: &'a Font) -> Self {
        let cmap = Data(font.table(*b"cmap").unwrap_or_default());
        let count = cmap.count(2, 4, 8);
        let table = (0..count)
            .map(|i| 4 + 8 * i)
            .find(|&at| cmap.u16(at) == 0 && cmap.u16(at + 2) == 5)
            .map(|at| cmap.offset32(at + 4))
            .filter(|table| table.u16(0) == 14);
        Self(table.unwrap_or_default())
    }

    /// The selector's default ranges first, then its mappings; a mapping to
    /// glyph 0 is none.
    fn lookup(self, c: u32, selector: u32) -> Variation {
        let table = self.0;
        let records = table.u32(6) as usize;
        let records = records.min(table.0.len().saturating_sub(10) / 11);
        let Some(record) = bsearch(records, |i| table.u24(10 + 11 * i).cmp(&selector)) else {
            return Variation::None;
        };
        let at = 10 + 11 * record;
        let defaults = table.at(table.u32(at + 3) as usize);
        let count = (defaults.u32(0) as usize).min(defaults.0.len().saturating_sub(4) / 4);
        let in_default = bsearch(count, |i| {
            let first = defaults.u24(4 + 4 * i);
            let last = first + u32::from(defaults.u8(4 + 4 * i + 3));
            if c < first {
                std::cmp::Ordering::Greater
            } else if c > last {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        });
        if in_default.is_some() {
            return Variation::Default;
        }
        let mappings = table.at(table.u32(at + 7) as usize);
        let count = (mappings.u32(0) as usize).min(mappings.0.len().saturating_sub(4) / 5);
        bsearch(count, |i| mappings.u24(4 + 5 * i).cmp(&c))
            .map(|i| mappings.u16(4 + 5 * i + 3))
            .filter(|&glyph| glyph != 0)
            .map_or(Variation::None, |glyph| Variation::Glyph(GlyphId(glyph)))
    }
}

/// The metrics and scale shaping uses, with HarfBuzz's scaling arithmetic.
///
/// A font function that fails answers as for a glyph that does not load, so the
/// pipeline goes on without it, as `std`'s formatting adapters go on after an
/// I/O error: the first error is kept, no further fallible function is called,
/// and [`Self::finish`] returns it when the call ends.
pub(crate) struct Scaled<'a> {
    pub(crate) metrics: &'a dyn Metrics,
    pub(crate) scale: Scale,
    x_mult: i64,
    y_mult: i64,
    x_multf: f32,
    y_multf: f32,
    error: Cell<Option<Error>>,
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
            error: Cell::new(None),
        }
    }

    /// The first error a font function returned, if any.
    pub(crate) fn finish(&self) -> Result<()> {
        self.error.get().map_or(Ok(()), Err)
    }

    /// The answer of a fallible font function, or `otherwise` once one failed.
    fn ask<T>(&self, otherwise: T, f: impl FnOnce(&dyn Metrics) -> Result<T>) -> T {
        if self.error.get().is_some() {
            return otherwise;
        }
        f(self.metrics).unwrap_or_else(|error| {
            self.error.set(Some(error));
            otherwise
        })
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
        u16::try_from(glyph).map_or(0, |g| self.ask(0, |m| m.h_advance(GlyphId(g))))
    }

    pub(crate) fn extents(&self, glyph: u32) -> Option<Extents> {
        let glyph = GlyphId(u16::try_from(glyph).ok()?);
        self.ask(None, |m| m.extents(glyph))
    }

    pub(crate) fn contour_point(&self, glyph: u32, point: u16) -> Option<(i32, i32)> {
        let glyph = GlyphId(u16::try_from(glyph).ok()?);
        self.ask(None, |m| m.contour_point(glyph, point))
    }

    /// Whether the font functions give pair kerning.
    pub(crate) fn has_h_kerning(&self) -> bool {
        self.metrics.has_h_kerning()
    }

    /// The font functions' kerning of two glyph indices the buffer holds.
    pub(crate) fn h_kerning(&self, left: u32, right: u32) -> i32 {
        match (u16::try_from(left), u16::try_from(right)) {
            (Ok(l), Ok(r)) => self.ask(0, |m| m.h_kerning(GlyphId(l), GlyphId(r))),
            _ => 0,
        }
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
            .field("error", &self.error.get())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{Extents, Metrics, Scale, Scaled};
    use std::cell::Cell;
    use triergon_core::{Error, Result};
    use triergon_font::GlyphId;

    /// Advances fail for glyph 2; every call is counted.
    struct Failing(Cell<u32>);

    impl Metrics for Failing {
        fn scale(&self) -> Scale {
            Scale::units(1000)
        }

        fn nominal_glyph(&self, _c: char) -> Option<GlyphId> {
            None
        }

        fn variation_glyph(&self, _c: char, _selector: char) -> Option<GlyphId> {
            None
        }

        fn h_advance(&self, glyph: GlyphId) -> Result<i32> {
            self.0.set(self.0.get() + 1);
            match glyph.0 {
                2 => Err(Error::Unsupported("advance")),
                g => Ok(i32::from(g) * 10),
            }
        }

        fn extents(&self, _glyph: GlyphId) -> Result<Option<Extents>> {
            self.0.set(self.0.get() + 1);
            Ok(Some(Extents::default()))
        }
    }

    #[test]
    fn the_first_font_function_error_is_kept_and_ends_the_calls() {
        let metrics = Failing(Cell::new(0));
        let scaled = Scaled::new(&metrics, 1000);
        assert_eq!(scaled.advance(1), 10);
        assert_eq!(scaled.finish(), Ok(()));
        assert_eq!(scaled.advance(2), 0);
        assert_eq!(scaled.advance(3), 0);
        assert_eq!(scaled.extents(1), None);
        assert_eq!(metrics.0.get(), 2);
        assert_eq!(scaled.finish(), Err(Error::Unsupported("advance")));
        // Glyph indices no face has are not asked for.
        assert_eq!(Scaled::new(&metrics, 1000).advance(0x1_0000), 0);
        assert_eq!(metrics.0.get(), 2);
    }
}
