//! Fonts as libass uses them to render ASS subtitles: which face a font name,
//! weight and slant select, the charmap text is read through, the line metrics
//! and the size a font size means, the synthetic oblique and bold, and the
//! underline and strikeout bands.
//!
//! Every function takes the [`Font`] or [`SizedFont`] it speaks of. Glyph
//! loading, hinting, advances and kerning are the crate root's.
//!
//! Reference: libass 0.17.5, with FreeType 2.15.0 and HarfBuzz 14.6.0.

mod matching;

pub use matching::{FaceInfo, Request, select};

use crate::cmap::{self, Charmap};
use crate::fixed::mul_fix;
use crate::name::Platform;
use crate::outline::Outline;
use crate::sized::SizedFont;
use crate::{Font, GlyphId, OutlineFormat, VerticalMetrics};
use triergon_core::{Error, Result};

/// The largest outline coordinate libass draws.
const OUTLINE_MAX: i64 = (1 << 28) - 1;

/// The weight libass assigns the face: `OS/2.usWeightClass`, with classes 1
/// to 9 read as 100 to 900 (4 as 350), and 400 or 700 by the bold style when
/// it is zero or `OS/2` is absent.
pub fn weight(font: &Font) -> u32 {
    match font.os2.map_or(0, |o| o.weight_class) {
        0 if font.style().bold => 700,
        0 => 400,
        4 => 350,
        class @ 1..=9 => u32::from(class) * 100,
        class => u32::from(class),
    }
}

/// The face's vertical metrics as libass sets them, after GDI: the Windows
/// metrics of `OS/2` when their (signed) sum is not zero; else
/// [`Font::vertical_metrics`] when ascender minus descender and the height
/// are not zero; else the typographic metrics when they span a height; else
/// the `head` bounding box. Values are 16-bit.
pub fn vertical_metrics(font: &Font) -> VerticalMetrics {
    let short = |v: i32| i32::from(v as i16);
    let span = |ascender: i32, descender: i32| VerticalMetrics {
        ascender: short(ascender),
        descender: short(descender),
        height: short(short(ascender) - short(descender)),
    };
    let mut m = font.vertical_metrics();
    let os2 = font.os2();
    if let Some(o) = os2 {
        let (ascent, descent) = (o.win_ascent.cast_signed(), o.win_descent.cast_signed());
        if i32::from(ascent) + i32::from(descent) != 0 {
            m = span(i32::from(ascent), -i32::from(descent));
        }
    }
    if m.ascender - m.descender == 0 || m.height == 0 {
        m = match os2 {
            Some(o) if i32::from(o.typo_ascender) - i32::from(o.typo_descender) != 0 => {
                span(i32::from(o.typo_ascender), i32::from(o.typo_descender))
            }
            _ => span(
                i32::from(font.bounds().y_max),
                i32::from(font.bounds().y_min),
            ),
        };
    }
    m
}

/// The face at a font size, as libass requests it: `height` is the 26.6
/// pixel height from the ascender to the descender of [`vertical_metrics`]
/// ([`Font::at_height`]). libass asks for 256 pixels (16384) unless hinting
/// is on, rounding pixels to 26.6 half to even, and for 0 when a glyph's size
/// is 0.
///
/// # Errors
///
/// Those of [`Font::at_height`].
pub fn at_size(font: &Font, height: i32) -> Result<SizedFont<'_>> {
    font.at_height(height, vertical_metrics(font))
}

/// libass's ascent and descent of the size in 26.6 pixels: the ascender and
/// negated descender of [`vertical_metrics`] times the y scale.
pub fn ascent_descent(sized: &SizedFont<'_>) -> (i32, i32) {
    let face = vertical_metrics(sized.font());
    let y_scale = sized.metrics().y_scale;
    let scaled = |v: i32| mul_fix(i64::from(v), y_scale) as i32;
    (scaled(face.ascender), scaled(-face.descender))
}

/// The scale libass gives HarfBuzz for the size: units per em times the
/// 16.16 scales, shifted down by 16.
pub fn harfbuzz_scale(sized: &SizedFont<'_>) -> (i32, i32) {
    let upem = u64::from(sized.font().units_per_em());
    let scale = |s: i64| ((s.cast_unsigned().wrapping_mul(upem)) >> 16) as i32;
    let metrics = sized.metrics();
    (scale(metrics.x_scale), scale(metrics.y_scale))
}

/// The index in [`Font::charmaps`] of the charmap libass uses for text, as it
/// selects one when it opens a face: the first Windows full-Unicode charmap
/// (3, 10); else the first Windows BMP charmap (3, 1), which may be the one
/// built from glyph names; else the first other Windows charmap, such as
/// Symbol (3, 0). A face without Windows charmaps keeps
/// [`Font::unicode_charmap`]. Without one of those, it is the first charmap.
pub fn text_charmap_index(font: &Font) -> Option<usize> {
    text_charmap_among(font.charmaps())
}

/// The charmap at [`text_charmap_index`].
pub fn text_charmap(font: &Font) -> Option<Charmap<'_>> {
    text_charmap_index(font).and_then(|index| font.charmap(index))
}

/// [`text_charmap_index`] over a face's charmaps in their order.
pub(crate) fn text_charmap_among<'f>(
    charmaps: impl Iterator<Item = Charmap<'f>> + Clone,
) -> Option<usize> {
    let windows = |encoding: Option<u16>| {
        charmaps.clone().position(|c| {
            c.platform() == Platform::Windows && encoding.is_none_or(|e| c.encoding_id() == e)
        })
    };
    windows(Some(10))
        .or_else(|| windows(Some(1)))
        .or_else(|| windows(None))
        .or_else(|| cmap::unicode_charmap(charmaps.clone()))
        .or_else(|| charmaps.clone().next().map(|_| 0))
}

/// `charmap`, one of `font`'s, read as libass reads text through it.
///
/// # Errors
///
/// `Unsupported` where libass converts codes for the charmap and this
/// crate does not: through iconv for the Windows Shift-JIS, PRC, Big5,
/// Wansung and Johab charmaps (encodings 2 to 6), and by its legacy Arabic
/// tables for the Symbol charmap of a face declaring an Arabic character
/// set (`OS/2.fsSelection` high byte 178 or 179).
pub fn text_lookup<'f>(font: &'f Font, charmap: Charmap<'f>) -> Result<TextLookup<'f>> {
    if charmap.platform() != Platform::Windows {
        return Ok(TextLookup::new(charmap, false));
    }
    match charmap.encoding_id() {
        2..=6 => Err(Error::Unsupported("libass's multibyte charmap conversion")),
        0 if font
            .os2
            .is_some_and(|os2| matches!(os2.selection.bits() >> 8, 178 | 179)) =>
        {
            Err(Error::Unsupported("libass's legacy Arabic charmaps"))
        }
        encoding => Ok(TextLookup::new(charmap, encoding == 0)),
    }
}

/// A charmap read as libass reads text through it ([`text_lookup`]): a
/// character is looked up at the code libass gives it.
#[derive(Clone, Copy, Debug)]
pub struct TextLookup<'f> {
    charmap: Charmap<'f>,
    /// The bits set in every code looked up: `0xF000` in a Windows Symbol
    /// charmap, none in others.
    prefix: u32,
}

impl<'f> TextLookup<'f> {
    const fn new(charmap: Charmap<'f>, symbol: bool) -> Self {
        let prefix = if symbol { 0xf000 } else { 0 };
        Self { charmap, prefix }
    }

    /// The charmap codes are looked up in.
    pub const fn charmap(&self) -> Charmap<'f> {
        self.charmap
    }

    /// The code `c` is looked up at: `0xF000 | c` in a Windows Symbol charmap,
    /// `c` in any other; `None` for 0, which libass never looks up.
    pub const fn code(&self, c: u32) -> Option<u32> {
        let code = c | self.prefix;
        if code == 0 { None } else { Some(code) }
    }

    /// The glyph `c` maps to.
    pub fn glyph(&self, c: u32) -> Option<GlyphId> {
        self.charmap.glyph(self.code(c)?)
    }
}

/// Which synthetic styles libass applies to a glyph of a face.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Synthesis {
    /// Slant the outline (see [`Synthesis::shear`]).
    pub oblique: bool,
    /// Embolden the outline by the size's [`embolden_strength`].
    pub embolden: bool,
}

impl Synthesis {
    /// The 16.16 horizontal shear libass applies for `oblique`: tan 10° for
    /// faces with CFF (not CFF2) outlines, and 0x5700, as GDI slants, for the
    /// others.
    pub fn shear(font: &Font) -> i32 {
        if font.outline_format() == OutlineFormat::Cff {
            0x2d24
        } else {
            0x5700
        }
    }
}

/// The synthetic styles libass applies for a requested weight and slant (as
/// [`Request::weight`] and [`Request::slant`] give them): oblique when the
/// face is not italic and the slant exceeds 55, bold when the face is not
/// bold and the weight exceeds [`weight`] by more than 150. Italic and bold
/// come from `OS/2.fsSelection` bits 0 and 5, or [`Font::style`] without
/// `OS/2`.
pub fn synthesis(font: &Font, weight: u32, slant: u32) -> Synthesis {
    let (italic, bold) = match font.os2() {
        Some(os2) => (os2.selection.italic(), os2.selection.bold()),
        None => (font.style().italic, font.style().bold),
    };
    Synthesis {
        oblique: !italic && slant > 55,
        embolden: !bold && weight > self::weight(font) + 150,
    }
}

/// The emboldening strength libass uses at the size, in 26.6 units: the em
/// in pixels over 64.
pub fn embolden_strength(sized: &SizedFont<'_>) -> i32 {
    (mul_fix(
        i64::from(sized.font().units_per_em()),
        sized.metrics().y_scale,
    ) / 64) as i32
}

/// Applies libass's synthetic styles to an outline loaded by
/// [`SizedFont::outline_into`]: the [`Synthesis::shear`] first, then
/// emboldening by [`embolden_strength`]. Metrics are unchanged. Returns
/// `false` when emboldening was asked for and refused: the outline then
/// keeps the shear alone (see [`Outline::embolden`]).
///
/// # Errors
///
/// `Overflow` for coordinates beyond `i32`.
pub fn synthesize(
    sized: &SizedFont<'_>,
    outline: &mut Outline,
    synthesis: Synthesis,
) -> Result<bool> {
    if synthesis.oblique {
        outline.transform([0x1_0000, Synthesis::shear(sized.font()), 0, 0x1_0000])?;
    }
    if synthesis.embolden {
        return outline.embolden(embolden_strength(sized));
    }
    Ok(true)
}

/// A horizontal band in 26.6 pixels, y up: an underline or strikeout as libass
/// adds it, a rectangle from x 0 to the glyph's advance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Band {
    /// The lower edge.
    pub y_min: i32,
    /// The upper edge.
    pub y_max: i32,
}

/// The underline libass draws at the size: from `post`, when its position is
/// at or below the baseline and its thickness positive.
pub fn underline(sized: &SizedFont<'_>) -> Option<Band> {
    let (position, thickness) = sized.font().underline;
    (position <= 0 && thickness > 0)
        .then(|| band(sized, position, thickness))
        .flatten()
}

/// The strikeout libass draws at the size: from `OS/2`, when its position is
/// at or above the baseline and its size positive.
pub fn strikeout(sized: &SizedFont<'_>) -> Option<Band> {
    let os2 = sized.font().os2()?;
    (os2.strikeout_position >= 0 && os2.strikeout_size > 0)
        .then(|| band(sized, os2.strikeout_position, os2.strikeout_size))
        .flatten()
}

/// A decoration line centred on `position`, rounded as libass rounds it.
fn band(sized: &SizedFont<'_>, position: i16, size: i16) -> Option<Band> {
    let y_scale = sized.metrics().y_scale;
    let scale = |v: i16| (i64::from(v) * y_scale + 0x8000) >> 16;
    let size = scale(size);
    // libass works y down: the band's top edge and its height.
    let top = -scale(position) - (size >> 1);
    (top >= -OUTLINE_MAX && top + size <= OUTLINE_MAX).then(|| Band {
        y_min: -(top + size) as i32,
        y_max: -top as i32,
    })
}
