//! A face at a pixel size, set up as FreeType sets it up when libass requests
//! the size: scales, size metrics, scaled unhinted outlines and advances, the
//! hinted advances HarfBuzz receives, vertical advances, kerning and decoration
//! lines.

use crate::fixed::Scale;
use crate::fixed::{div_fix, mul_div, mul_fix, pix_ceil, pix_floor, pix_round};
use crate::glyf::{self, Glyf};
use crate::outline::Outline;
use crate::synthesis::Synthesis;
use crate::{Advances, Font, GlyphId, GlyphMetrics, Outlines, VerticalMetrics};
use triergon_core::{Error, Result, WorkBudget};

/// libass's largest outline coordinate, `OUTLINE_MAX`.
const OUTLINE_MAX: i64 = (1 << 28) - 1;

/// FreeType's size metrics (`FT_Size_Metrics`) after `FT_Request_Size`: scales
/// from font units to 26.6 pixels in 16.16, pixels per em, and the face's
/// vertical metrics and largest advance scaled and grid-fitted, in 26.6.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizeMetrics {
    pub x_ppem: u16,
    pub y_ppem: u16,
    pub x_scale: i64,
    pub y_scale: i64,
    /// Rounded up to a whole pixel.
    pub ascender: i64,
    /// Rounded down to a whole pixel.
    pub descender: i64,
    pub height: i64,
    pub max_advance: i64,
}

/// A horizontal band in 26.6 pixels, y up: an underline or strikeout as libass
/// adds it, a rectangle from x 0 to the glyph's advance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Band {
    pub y_min: i32,
    pub y_max: i32,
}

/// How FreeType's default (hinted) loading treats the face's advances.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hinting {
    /// The unhinted advance, fitted to the size's grid: CFF, TrueType whose
    /// programs keep the v40 interpreter's backward compatibility, and any
    /// face at zero pixels per em, which loads unhinted.
    Unhinted,
    /// Advances that depend on what this crate does not run.
    Unsupported(&'static str),
}

/// A face at a size: [`Font::at_size`].
#[derive(Clone, Copy, Debug)]
pub struct SizedFont<'f> {
    font: &'f Font,
    metrics: SizeMetrics,
    /// The scales glyphs load at: the size's, or none at all (font units) at
    /// zero pixels per em.
    scale: Scale,
    /// The scales hinted loading uses: the size's, but for TrueType faces
    /// that ask for integer pixels per em (`head.flags` bit 3), which scale by
    /// the rounded ppem on both axes.
    hinted_scale: Scale,
    /// The grid hinted loading fits advances to, as the mask of a 26.6
    /// value's fraction: whole pixels (63), or none (0) at zero pixels per
    /// em.
    grid: i64,
    hinting: Hinting,
}

impl Font {
    /// The face's vertical metrics as libass sets them, after GDI: the Windows
    /// metrics of `OS/2` when their (signed) sum is not zero; else FreeType's
    /// [`Font::vertical_metrics`] when ascender minus descender and the height
    /// are not zero; else the typographic metrics when they span a height; else
    /// the `head` bounding box. Values are 16-bit, as FreeType stores them.
    pub fn gdi_metrics(&self) -> VerticalMetrics {
        let short = |v: i32| i32::from(v as i16);
        let span = |ascender: i32, descender: i32| VerticalMetrics {
            ascender: short(ascender),
            descender: short(descender),
            height: short(short(ascender) - short(descender)),
        };
        let mut m = self.vertical_metrics();
        let os2 = self.os2();
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
                    i32::from(self.bounds().y_max),
                    i32::from(self.bounds().y_min),
                ),
            };
        }
        m
    }

    /// Whether FreeType reports PostScript font information for the face: CFF
    /// (not CFF2) outlines.
    pub fn is_postscript(&self) -> bool {
        self.outline_format() == crate::OutlineFormat::Cff
    }

    /// The face at a pixel size, as libass requests it from FreeType
    /// (`FT_Request_Size`, `FT_SIZE_REQUEST_TYPE_REAL_DIM`): `height` is the
    /// 26.6 pixel height of [`Font::gdi_metrics`]' ascender to descender, and
    /// x scales as y. libass asks for 256 pixels (16384) unless hinting is
    /// on, rounding pixels to 26.6 half to even, and for 0 when a glyph's size
    /// is 0.
    ///
    /// A size of zero pixels per em, such as height 0, keeps its scales and
    /// metrics, but FreeType then loads glyphs unscaled, in font units, and
    /// unhinted (`FT_Load_Glyph`: "undefined scale means no scale").
    ///
    /// # Errors
    ///
    /// `Invalid` for a negative height, a face whose ascender equals its
    /// descender, and sizes FreeType refuses: more than 65535 pixels per em.
    pub fn at_size(&self, height: i32) -> Result<SizedFont<'_>> {
        if height < 0 {
            return Err(Error::Invalid("negative size"));
        }
        let face = self.gdi_metrics();
        let span = i64::from(face.ascender - face.descender).abs();
        if span == 0 {
            return Err(Error::Invalid("face ascender equals its descender"));
        }
        let scale = div_fix(i64::from(height), span);
        let upem = i64::from(self.units_per_em());
        let ppem = (mul_fix(upem, scale) + 32) >> 6;
        let ppem =
            u16::try_from(ppem).map_err(|_| Error::Invalid("more than 65535 pixels per em"))?;
        let metrics = SizeMetrics {
            x_ppem: ppem,
            y_ppem: ppem,
            x_scale: scale,
            y_scale: scale,
            ascender: pix_ceil(mul_fix(i64::from(face.ascender), scale)),
            descender: pix_floor(mul_fix(i64::from(face.descender), scale)),
            height: pix_round(mul_fix(i64::from(face.height), scale)),
            max_advance: pix_round(mul_fix(
                i64::from(self.horizontal_header().advance_max),
                scale,
            )),
        };
        if ppem == 0 {
            return Ok(SizedFont {
                font: self,
                metrics,
                scale: Scale::UNITS,
                hinted_scale: Scale::UNITS,
                grid: 0,
                hinting: Hinting::Unhinted,
            });
        }
        let integer_ppem = matches!(self.outlines, Outlines::Glyf(_)) && self.head.flags & 8 != 0;
        let hinted_scale = if integer_ppem {
            div_fix(i64::from(ppem) << 6, upem)
        } else {
            scale
        };
        Ok(SizedFont {
            font: self,
            metrics,
            scale: Scale { x: scale, y: scale },
            hinted_scale: Scale {
                x: hinted_scale,
                y: hinted_scale,
            },
            grid: 63,
            hinting: self.default_load,
        })
    }

    /// How FreeType's default loading treats this face's advances, found
    /// once when the face is opened: it reads every `fpgm` and `prep`
    /// instruction and the family name.
    pub(crate) fn hinting(&self) -> Hinting {
        if !matches!(self.outlines, Outlines::Glyf(_)) {
            return Hinting::Unhinted;
        }
        if self.is_tricky() {
            return Hinting::Unsupported("hinted advances of a tricky TrueType font");
        }
        let data = self.data.as_ref();
        let table = |t: Option<crate::sfnt::Table>| t.map(|t| t.bytes(data));
        let (fpgm, prep) = (table(self.tables.fpgm), table(self.tables.prep));
        // FreeType auto-hints TrueType faces without bytecode.
        if fpgm.is_none_or(<[u8]>::is_empty) && prep.is_none_or(|p| p.len() <= 7) {
            return Hinting::Unsupported("hinted advances of an auto-hinted TrueType font");
        }
        if [fpgm, prep].into_iter().flatten().all(keeps_compatibility) {
            Hinting::Unhinted
        } else {
            Hinting::Unsupported("hinted advances under a TrueType instruction control")
        }
    }

    /// FreeType's family-name test for fonts that need their bytecode to render.
    fn is_tricky(&self) -> bool {
        const NAMES: [&str; 20] = [
            "cpop",
            "DFGirl-W6-WIN-BF",
            "DFGothic-EB",
            "DFGyoSho-Lt",
            "DFHei",
            "DFHSGothic-W5",
            "DFHSMincho-W3",
            "DFHSMincho-W7",
            "DFKaiSho-SB",
            "DFKaiShu",
            "DFKai-SB",
            "DFMing",
            "DLC",
            "HuaTianKaiTi?",
            "HuaTianSongTi?",
            "Ming(for ISO10646)",
            "MingLiU",
            "MingMedium",
            "PMingLiU",
            "MingLi43",
        ];
        let Some(name) = self.family_name() else {
            return false;
        };
        // FreeType's ASCII form of the name, up to a NUL: UTF-16 units (bytes on
        // Macintosh) outside 32..=127 read as `?`; the first 128 are enough.
        let mut ascii = [0_u8; 128];
        let mut len = 0;
        for unit in name.code_units().take_while(|&u| u != 0).take(ascii.len()) {
            ascii[len] = u8::try_from(unit)
                .ok()
                .filter(|b| (32..=127).contains(b))
                .unwrap_or(b'?');
            len += 1;
        }
        let mut family = &ascii[..len];
        // A PDF subset tag ("ABCDEF+") precedes the name in subsetted fonts.
        if family.len() > 7 && family[..6].iter().all(u8::is_ascii_uppercase) && family[6] == b'+' {
            family = &family[7..];
        }
        NAMES
            .iter()
            .any(|n| family.windows(n.len()).any(|w| w == n.as_bytes()))
    }
}

/// The metrics of a `glyf` glyph without loading its points, passing each
/// program it runs to `program`.
fn glyf_metrics(
    font: &Font,
    table: &Glyf,
    index: u16,
    program: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<glyf::Metrics> {
    let mut work = WorkBudget::new(font.limits.work);
    table.walk(&font.glyf_context(), index, &mut work, program)
}

/// Whether a TrueType program can only issue `INSTCTRL` with a constant
/// selector 1 or 2: then FreeType's v40 interpreter stays in backward
/// compatibility mode, where hinting never changes an advance.
fn keeps_compatibility(program: &[u8]) -> bool {
    let mut i = 0;
    let mut last: Option<i32> = None;
    while let Some(&op) = program.get(i) {
        let pushed = |count: usize, words: bool, at: usize| -> Option<(usize, i32)> {
            let size = if words { 2 } else { 1 };
            let bytes = program.get(at..at + count * size)?;
            let top = match bytes.len().checked_sub(size) {
                Some(t) if words => i32::from(i16::from_be_bytes([bytes[t], bytes[t + 1]])),
                Some(t) => i32::from(bytes[t]),
                None => return Some((at + count * size, i32::MIN)),
            };
            Some((at + count * size, top))
        };
        let step = match op {
            0x40 | 0x41 => program
                .get(i + 1)
                .and_then(|&n| pushed(usize::from(n), op == 0x41, i + 2)),
            0xB0..=0xB7 => pushed(usize::from(op - 0xAF), false, i + 1),
            0xB8..=0xBF => pushed(usize::from(op - 0xB7), true, i + 1),
            0x8E if !matches!(last, Some(1 | 2)) => return false,
            _ => Some((i + 1, i32::MIN)),
        };
        let Some((next, top)) = step else {
            // A push running off the end: the program stops there.
            return true;
        };
        last = (top != i32::MIN).then_some(top);
        i = next;
    }
    true
}

impl<'f> SizedFont<'f> {
    pub const fn font(&self) -> &'f Font {
        self.font
    }

    /// The size metrics: `face->size->metrics` after the request.
    pub const fn metrics(&self) -> SizeMetrics {
        self.metrics
    }

    /// libass's ascent and descent in 26.6 pixels: [`Font::gdi_metrics`]
    /// ascender and negated descender times the y scale.
    pub fn ascent_descent(&self) -> (i32, i32) {
        let face = self.font.gdi_metrics();
        let scaled = |v: i32| mul_fix(i64::from(v), self.metrics.y_scale) as i32;
        (scaled(face.ascender), scaled(-face.descender))
    }

    /// The scale libass gives HarfBuzz: units per em times the 16.16 scales,
    /// shifted down by 16.
    pub fn harfbuzz_scale(&self) -> (i32, i32) {
        let upem = u64::from(self.font.units_per_em());
        let scale = |s: i64| ((s.cast_unsigned().wrapping_mul(upem)) >> 16) as i32;
        (scale(self.metrics.x_scale), scale(self.metrics.y_scale))
    }

    /// The emboldening strength libass uses, in 26.6 units: the em in pixels
    /// over 64.
    pub fn embolden_strength(&self) -> i32 {
        (mul_fix(i64::from(self.font.units_per_em()), self.metrics.y_scale) / 64) as i32
    }

    /// Loads `glyph` into `outline` as libass loads it with hinting off
    /// (`FT_LOAD_NO_HINTING | FT_LOAD_NO_BITMAP`): unhinted, scaled to 26.6
    /// pixels (unscaled at zero pixels per em), y up, the origin at the
    /// glyph's left side bearing as FreeType places it, with the unhinted
    /// advance in 26.6 pixels (`advance.x` of the glyph slot), the vertical
    /// advance and the linear vertical advance in 16.16 pixels.
    ///
    /// TrueType points scale with `FT_MulFix` by the size's scales (unhinted
    /// loading ignores `head`'s integer-ppem flag); composites transform and
    /// move components in pixels. CFF outlines are built in whole font units by
    /// FreeType's CFF engine, put through the FontMatrix, and then scaled.
    ///
    /// # Errors
    ///
    /// Those of [`Font::outline_into`]; `Overflow` for coordinates beyond
    /// `i32`.
    pub fn outline_into(&self, glyph: GlyphId, outline: &mut Outline) -> Result<GlyphMetrics> {
        let mut metrics = self.font.load(glyph, self.scale, outline)?;
        // FreeType scales the linear advance by the size even when the glyph
        // loads unscaled.
        metrics.linear_vertical_advance =
            mul_div(metrics.linear_vertical_advance, self.metrics.y_scale, 64);
        Ok(metrics)
    }

    /// Applies libass's synthetic styles to an outline loaded by
    /// [`Self::outline_into`]: the [`Synthesis::shear`] first, then emboldening
    /// by [`Self::embolden_strength`]. Metrics are unchanged.
    ///
    /// # Errors
    ///
    /// `Overflow` for coordinates beyond `i32`.
    pub fn synthesize(&self, outline: &mut Outline, synthesis: Synthesis) -> Result<()> {
        if synthesis.oblique {
            outline.transform([0x1_0000, Synthesis::shear(self.font), 0, 0x1_0000])?;
        }
        if synthesis.embolden {
            outline.embolden(self.embolden_strength())?;
        }
        Ok(())
    }

    /// The advance FreeType's default loading reports (`FT_LOAD_DEFAULT`, the
    /// one libass's HarfBuzz callbacks use): the unhinted advance rounded to a
    /// whole pixel; at zero pixels per em, the unscaled advance.
    ///
    /// # Errors
    ///
    /// `Unsupported` where hinting could change the advance and this crate does
    /// not hint: TrueType faces FreeType auto-hints (no `fpgm`, `prep` of at most
    /// 7 bytes), tricky fonts known by family name, and programs that may leave
    /// backward compatibility (an `INSTCTRL` without a constant selector 1 or 2
    /// in `fpgm`, `prep` or the glyph's programs). Otherwise the errors of
    /// [`Self::outline_into`].
    pub fn hinted_advance(&self, glyph: GlyphId) -> Result<i32> {
        self.fit(self.default_advances(glyph)?.horizontal)
    }

    /// The vertical advance FreeType's default loading reports
    /// (`metrics.vertAdvance` of `FT_LOAD_DEFAULT`, which libass gives
    /// HarfBuzz as the advance of a glyph it draws rotated): as
    /// [`GlyphMetrics::vertical_advance`] at the hinted scale, rounded to a
    /// whole pixel.
    ///
    /// # Errors
    ///
    /// As [`Self::hinted_advance`].
    pub fn hinted_vertical_advance(&self, glyph: GlyphId) -> Result<i32> {
        self.fit(self.default_advances(glyph)?.vertical)
    }

    /// The advances FreeType's default loading computes for `glyph`, before
    /// it fits them to the grid, read without the glyph's points.
    fn default_advances(&self, glyph: GlyphId) -> Result<Advances> {
        if let Hinting::Unsupported(what) = self.hinting {
            return Err(Error::Unsupported(what));
        }
        let font = self.font;
        let index = font.check(glyph)?;
        match &font.outlines {
            Outlines::Glyf(table) => {
                let m = self.hinted_glyf_metrics(table, index)?;
                Ok(font.glyf_advances(m, self.hinted_scale).1)
            }
            Outlines::Cff(cff) => font.cff_advances(cff, glyph, index, self.hinted_scale),
        }
    }

    /// The `glyf` metrics of a glyph that hinted loading runs the programs of:
    /// `Unsupported` where one may leave backward compatibility.
    fn hinted_glyf_metrics(&self, table: &Glyf, index: u16) -> Result<glyf::Metrics> {
        let mut compatible = true;
        let m = glyf_metrics(self.font, table, index, &mut |program| {
            compatible &= keeps_compatibility(program);
            Ok(())
        })?;
        // Unhinted loading runs no program.
        if !compatible && self.grid != 0 {
            return Err(Error::Unsupported(
                "hinted advances under a TrueType instruction control",
            ));
        }
        Ok(m)
    }

    /// A hinted 26.6 value fitted to the grid as FreeType's default loading
    /// fits advances (`FT_PIX_ROUND`).
    fn fit(&self, value: i64) -> Result<i32> {
        glyf::narrow((value + (self.grid + 1) / 2) & !self.grid)
    }

    /// The `kern` value for the pair as libass's HarfBuzz callback reads it
    /// (`FT_KERNING_DEFAULT`): scaled, reduced below 25 pixels per em, and
    /// rounded to a whole pixel.
    pub fn kerning(&self, left: GlyphId, right: GlyphId) -> i32 {
        let mut x = mul_fix(
            i64::from(self.font.kerning(left, right)),
            self.metrics.x_scale,
        );
        if self.metrics.x_ppem < 25 {
            x = mul_div(x, i64::from(self.metrics.x_ppem), 25);
        }
        pix_round(x) as i32
    }

    /// The underline libass draws: from `post`, when its position is at or
    /// below the baseline and its thickness positive.
    pub fn underline(&self) -> Option<Band> {
        let (position, thickness) = self.font.underline;
        (position <= 0 && thickness > 0)
            .then(|| self.band(position, thickness))
            .flatten()
    }

    /// The strikeout libass draws: from `OS/2`, when its position is at or above
    /// the baseline and its size positive.
    pub fn strikeout(&self) -> Option<Band> {
        let os2 = self.font.os2()?;
        (os2.strikeout_position >= 0 && os2.strikeout_size > 0)
            .then(|| self.band(os2.strikeout_position, os2.strikeout_size))
            .flatten()
    }

    /// A decoration line centred on `position`, rounded as libass rounds it.
    fn band(&self, position: i16, size: i16) -> Option<Band> {
        let scale = |v: i16| (i64::from(v) * self.metrics.y_scale + 0x8000) >> 16;
        let size = scale(size);
        // libass works y down: the band's top edge and its height.
        let top = -scale(position) - (size >> 1);
        (top >= -OUTLINE_MAX && top + size <= OUTLINE_MAX).then(|| Band {
            y_min: -(top + size) as i32,
            y_max: -top as i32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::keeps_compatibility;

    #[test]
    fn instruction_control_needs_a_constant_harmless_selector() {
        // PUSHB_2 1 1; INSTCTRL
        assert!(keeps_compatibility(&[0xB1, 1, 1, 0x8E]));
        // PUSHW_1 3; INSTCTRL
        assert!(!keeps_compatibility(&[0xB8, 0, 3, 0x8E]));
        // NPUSHB 2 4 3; INSTCTRL
        assert!(!keeps_compatibility(&[0x40, 2, 4, 3, 0x8E]));
        // A computed selector.
        assert!(!keeps_compatibility(&[0xB0, 1, 0x60, 0x8E]));
        assert!(keeps_compatibility(&[0x40, 200]));
    }
}
