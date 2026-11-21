//! A face at a pixel size: its scales and size metrics, glyphs loaded unhinted
//! or by the default, hinted load, the advances of that load, and kerning.
//!
//! A size is requested, scaled and rounded as FreeType 2.15.0 does it, so the
//! outlines and metrics are the ones a renderer built on FreeType draws with.

use crate::autofit::Autofit;
use crate::bytecode::{Bytecode, GlyphPoints, Hinter, Phantoms, Programs, Tables};
use crate::fixed::{Scale, div_fix, mul_div, mul_fix, pix_ceil, pix_floor, pix_round, wrap32};
use crate::glyf::{self, Glyf};
use crate::name::NameRecord;
use crate::outline::{Outline, Point};
use crate::{Advances, Font, GlyphId, GlyphMetrics, Outlines, VerticalMetrics};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

/// The metrics of a size: the scales from font units to 26.6 pixels, the
/// pixels per em, and the vertical metrics the size was requested with and
/// the face's largest advance, scaled and fitted to whole pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizeMetrics {
    /// Horizontal pixels per em, rounded to a whole number.
    pub x_ppem: u16,
    /// Vertical pixels per em, rounded to a whole number.
    pub y_ppem: u16,
    /// The 16.16 scale from font units to 26.6 pixels, horizontally.
    pub x_scale: i64,
    /// The 16.16 scale from font units to 26.6 pixels, vertically.
    pub y_scale: i64,
    /// The scaled ascender in 26.6 pixels, rounded up to a whole pixel.
    pub ascender: i64,
    /// The scaled descender in 26.6 pixels, rounded down to a whole pixel.
    pub descender: i64,
    /// The scaled baseline distance in 26.6 pixels, rounded to a whole pixel.
    pub height: i64,
    /// The scaled `hhea.advanceWidthMax` in 26.6 pixels, rounded to a whole
    /// pixel.
    pub max_advance: i64,
}

/// How [`SizedFont::load`] loads a glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Load {
    /// Scaled, without hinting or embedded bitmaps: the outline a renderer
    /// draws with hinting off.
    Unhinted,
    /// FreeType's default load: the hinted outline, with metrics fitted to
    /// the pixel grid. A text shaper reads its advances and extents.
    Default,
}

/// How the default (hinted) load hints a TrueType face, with what that
/// hinting keeps between loads. CFF faces need nothing: their hinter keeps
/// no state, and their advances are the unhinted ones.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "one per face, read in place by every load"
)]
pub(crate) enum TrueTypeHinting {
    /// Programs that keep the interpreter in backward compatibility mode,
    /// which leaves the advances unhinted: the interpreter.
    Instructed(Bytecode),
    /// No bytecode: the auto-hinter, which gives the advances too.
    Auto(Autofit),
    /// Advances that depend on what this crate does not run.
    Unsupported(&'static str),
}

/// A face at a size: [`Font::at_height`].
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
}

/// A linear advance in font units as 16.16 pixels at the 16.16 `scale`:
/// `units × scale / 64`, rounded, in 32 bits. Every load at a size reports
/// it so, whatever scale the glyph itself loads at.
pub(crate) fn linear_advance(units: i64, scale: i64) -> i64 {
    wrap32::mul_div(units, scale, 64)
}

impl Font {
    /// The face at the size where the span from `face.ascender` to
    /// `face.descender` is `height` 26.6 pixels, x scaling as y: the 16.16
    /// scale is `height` over that span, and the size's ascender, descender
    /// and height are `face`'s, scaled. Pass [`Font::vertical_metrics`] for
    /// the face's own line metrics, or the metrics a renderer substitutes for
    /// them.
    ///
    /// A size of zero pixels per em, such as height 0, keeps its scales and
    /// metrics, but glyphs then load unscaled, in font units, and unhinted,
    /// by the default load as by the unhinted one, with the linear vertical
    /// advance scaled by the size's scale. Observed: FreeType's request of
    /// such a size fails for a TrueType face and succeeds for a CFF one, and
    /// glyphs load so either way (probes `zero-ppem`,
    /// `zero-ppem-control-program`, `zero-ppem-uninstructed`).
    ///
    /// Each call is a new request of the size: the face's next hinted load
    /// runs its TrueType control value program again from freshly scaled
    /// control values, cleared storage and a fresh twilight zone, at the size
    /// the face is already at too (probe `sizes-same-size-request`). That
    /// load so does the work of scaling the `cvt ` table, clearing `maxp`'s
    /// storage area and twilight points and running the control value
    /// program, in the storage the face kept from its previous size, and pays
    /// for it from its own `WorkBudget`; a call itself does no work, so
    /// request a size once for the glyphs loaded at it.
    ///
    /// # Errors
    ///
    /// `Invalid` for a negative height, for `face` metrics whose ascender
    /// equals their descender, and for more than 65535 pixels per em.
    pub fn at_height(&self, height: i32, face: VerticalMetrics) -> Result<SizedFont<'_>> {
        if height < 0 {
            return Err(Error::Invalid("negative size"));
        }
        let span = (i64::from(face.ascender) - i64::from(face.descender)).abs();
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
        if let Outlines::Glyf(_, TrueTypeHinting::Instructed(bytecode)) = &self.outlines {
            bytecode.request();
        }
        if ppem == 0 {
            return Ok(SizedFont {
                font: self,
                metrics,
                scale: Scale::UNITS,
                hinted_scale: Scale::UNITS,
            });
        }
        let integer_ppem = matches!(self.outlines, Outlines::Glyf(..)) && self.head.flags & 8 != 0;
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
        })
    }
}

impl TrueTypeHinting {
    /// How the default load hints a TrueType face whose font and control
    /// value programs are `fpgm` and `prep` and whose family name is
    /// `family`, decided once when the face is opened by reading every
    /// instruction of the two programs. The hinter's storage is charged to
    /// `budget`.
    pub(crate) fn new(
        fpgm: Option<&[u8]>,
        prep: Option<&[u8]>,
        family: Option<NameRecord<'_>>,
        budget: &Budget,
    ) -> Self {
        if family.is_some_and(is_tricky) {
            return Self::Unsupported("hinted advances of a tricky TrueType font");
        }
        // A TrueType face without bytecode is auto-hinted.
        if fpgm.is_none_or(<[u8]>::is_empty) && prep.is_none_or(|p| p.len() <= 7) {
            return Self::Auto(Autofit::new(budget.clone()));
        }
        if [fpgm, prep].into_iter().flatten().all(keeps_compatibility) {
            Self::Instructed(Bytecode::new(budget.clone()))
        } else {
            Self::Unsupported("hinted advances under a TrueType instruction control")
        }
    }
}

/// Whether a family name is one of the fonts FreeType knows to need their
/// bytecode to render at all.
fn is_tricky(name: NameRecord<'_>) -> bool {
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
    // The first 128 characters of the name's ASCII form are enough.
    let mut ascii = [0_u8; 128];
    let mut len = 0;
    for c in name.ascii().take(ascii.len()) {
        ascii[len] = c as u8;
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
/// selector 1 or 2: then the interpreter stays in backward compatibility
/// mode, where hinting never changes an advance.
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
    /// The face.
    pub const fn font(&self) -> &'f Font {
        self.font
    }

    /// The size's metrics.
    pub const fn metrics(&self) -> SizeMetrics {
        self.metrics
    }

    /// Loads `glyph` into `outline` unhinted ([`Load::Unhinted`]): scaled to
    /// 26.6 pixels (unscaled at zero pixels per em), y up, the origin at the
    /// glyph's left side bearing, with the unhinted advance in 26.6 pixels,
    /// the vertical advance and the linear vertical advance in 16.16 pixels.
    ///
    /// TrueType points scale by the size's scales, each product rounded
    /// (unhinted loading ignores `head`'s integer-ppem flag); composites
    /// transform and move components in pixels. CFF outlines are built in
    /// whole font units, put through the FontMatrix, and then scaled.
    ///
    /// # Errors
    ///
    /// Those of [`Font::outline_into`]; `Overflow` for coordinates beyond
    /// `i32`.
    pub fn outline_into(&self, glyph: GlyphId, outline: &mut Outline) -> Result<GlyphMetrics> {
        let mut work = WorkBudget::new(self.font.limits.work);
        self.load(glyph, Load::Unhinted, outline, &mut work)
    }

    /// Loads `glyph` into `outline` by `load`, within `work`, and returns its
    /// metrics.
    ///
    /// [`Load::Unhinted`] is what [`Self::outline_into`] describes, which
    /// loads within the face's [`Limits`](crate::Limits). [`Load::Default`] is
    /// FreeType's default load, its metrics fitted to the pixel grid
    /// (bearings floored and ceiled, advances rounded):
    ///
    /// - a CFF glyph through Adobe's CFF hinter at the hinted scale;
    /// - a glyph of a TrueType face without bytecode (no `fpgm`, `prep` of at
    ///   most 7 bytes) through the auto-hinter at the size's scales:
    ///   loaded in font units, given a style by the characters the face's
    ///   Unicode charmap maps to it, hinted by the style's latin writing
    ///   system (stems and blue zones, both dimensions, but not horizontally
    ///   in an italic face) or left unhinted by the dummy one, and moved by
    ///   its hinted left side bearing; its advance runs between its hinted
    ///   phantom points (the scaled advance in a fixed-pitch face and for
    ///   digits of one width), its vertical advance is scaled by the style's
    ///   vertical scale, which the latin writing system fits to the x height;
    /// - a glyph of a TrueType face with bytecode through the TrueType
    ///   interpreter in backward compatibility mode:
    ///   loaded at the hinted scale, each glyph's program run on its points
    ///   as it loads, a composite's after its components'; its advances are
    ///   the unhinted ones;
    /// - at zero pixels per em, the unscaled, unhinted load.
    ///
    /// # Errors
    ///
    /// For [`Load::Default`], `Unsupported` for glyphs of the auto-hinter's
    /// CJK and Indic styles and where [`Self::default_advance`] is;
    /// `Invalid` for a face whose font or control value program stops on an
    /// error; `Policy` when `work` runs out; otherwise those of
    /// [`Self::outline_into`].
    pub fn load(
        &self,
        glyph: GlyphId,
        load: Load,
        outline: &mut Outline,
        work: &mut WorkBudget,
    ) -> Result<GlyphMetrics> {
        if load == Load::Unhinted || self.unscaled() {
            let mut metrics = self.font.load(glyph, self.scale, outline, work)?;
            // The linear advance scales by the size even when the glyph loads
            // unscaled.
            metrics.linear_vertical_advance =
                linear_advance(metrics.linear_vertical_advance, self.metrics.y_scale);
            return Ok(metrics);
        }
        let font = self.font;
        let cff = match &font.outlines {
            Outlines::Glyf(_, TrueTypeHinting::Unsupported(what)) => {
                return Err(Error::Unsupported(what));
            }
            Outlines::Glyf(_, TrueTypeHinting::Auto(autofit)) => {
                return autofit.load(font, glyph, self.scaler(), outline, work);
            }
            Outlines::Glyf(table, TrueTypeHinting::Instructed(bytecode)) => {
                font.check(glyph)?;
                return self.load_instructed(table, bytecode, glyph, outline, work);
            }
            Outlines::Cff(cff) => cff,
        };
        let index = font.check(glyph)?;
        cff.load_hinted(
            font.data.as_ref(),
            glyph,
            self.scale,
            outline,
            &font.limits,
            work,
        )?;
        let advances = font.cff_advances(cff, glyph, index, self.hinted_scale)?;
        self.fitted_metrics(advances, outline)
    }

    /// The default load of a glyph of a face with TrueType programs: its
    /// points loaded at the hinted scale, each glyph's program run on them
    /// as it loads, and the metrics fitted to the grid.
    // Out of line: inlined into `load`, it makes every other default load
    // dearer.
    #[inline(never)]
    fn load_instructed(
        &self,
        table: &Glyf,
        bytecode: &Bytecode,
        glyph: GlyphId,
        outline: &mut Outline,
        work: &mut WorkBudget,
    ) -> Result<GlyphMetrics> {
        let font = self.font;
        let data = font.data.as_ref();
        let bytes = |t: Option<crate::sfnt::Table>| t.map_or(&[][..], |t| t.bytes(data));
        let programs = Programs {
            font: bytes(font.tables.fpgm),
            control: bytes(font.tables.prep),
        };
        let (ppem, scale) = (self.metrics.y_ppem, self.hinted_scale);
        // The interpreter runs the font program once for the face and the
        // control value program once per size, and keeps the state they
        // leave for the size's glyphs.
        let tables = || {
            let table = |tag| font.table(tag).unwrap_or_default();
            Tables {
                programs,
                cvt: table(*b"cvt "),
                maxp: table(*b"maxp"),
                gasp: table(*b"gasp"),
            }
        };
        let m = bytecode.with_size(
            tables,
            (ppem, scale.y),
            font.limits.work,
            work,
            |state, work| {
                state.hinter.usable()?;
                state.units.clear();
                let mut hinting = GlyfHinting {
                    font,
                    hinter: &mut state.hinter,
                    programs,
                    units: &mut state.units,
                    ends: &mut state.ends,
                };
                outline.begin(font.limits.points);
                table.outline(
                    &font.glyf_context(),
                    glyph,
                    scale,
                    outline,
                    work,
                    &mut hinting,
                )
            },
        )?;
        let (origin, advances) = font.glyf_advances(m, scale);
        if origin != 0 {
            glyf::translate(outline.points_mut(), (-origin, 0))?;
        }
        self.fitted_metrics(advances, outline)
    }

    /// The metrics of a default load: `advances` and the box of `outline`,
    /// fitted to whole pixels for horizontal layout (the left and top edges
    /// floored and ceiled, the right and bottom ceiled and floored, the
    /// advances rounded), with the linear vertical advance at the size.
    fn fitted_metrics(&self, advances: Advances, outline: &Outline) -> Result<GlyphMetrics> {
        let mut m = GlyphMetrics::of(advances, outline)?;
        m.linear_vertical_advance = linear_advance(m.linear_vertical_advance, self.metrics.y_scale);
        let floor = |v: i32| v & !63;
        let ceil = |v: i32| floor(v.wrapping_add(63));
        let right = ceil(m.bearing_x.wrapping_add(m.width));
        let bottom = floor(m.bearing_y.wrapping_sub(m.height));
        m.bearing_x = floor(m.bearing_x);
        m.bearing_y = ceil(m.bearing_y);
        m.width = right.wrapping_sub(m.bearing_x);
        m.height = m.bearing_y.wrapping_sub(bottom);
        m.advance = glyf::narrow(pix_round(i64::from(m.advance)))?;
        m.vertical_advance = glyf::narrow(pix_round(i64::from(m.vertical_advance)))?;
        Ok(m)
    }

    /// The advance of the default load, as [`Self::load`] with
    /// [`Load::Default`] gives it: the unhinted advance rounded to a whole
    /// pixel, or for an auto-hinted face, the auto-hinter's; at zero pixels
    /// per em, the unscaled advance. Glyphs of the auto-hinter's CJK
    /// and Indic styles have their advances, though not their outlines.
    ///
    /// It loads within the face's [`Limits`](crate::Limits), and reads no
    /// points where the advance does not depend on them.
    ///
    /// # Errors
    ///
    /// `Unsupported` where hinting could change the advance and this crate does
    /// not hint: tricky fonts known by family name, and programs that may leave
    /// backward compatibility (an `INSTCTRL` without a constant selector 1 or 2
    /// in `fpgm`, `prep` or the glyph's programs). Otherwise the errors of
    /// [`Self::load`].
    pub fn default_advance(&self, glyph: GlyphId) -> Result<i32> {
        glyf::narrow(self.default_advances(glyph)?.0)
    }

    /// The vertical advance of the default load, as [`Self::load`] with
    /// [`Load::Default`] gives it: as
    /// [`GlyphMetrics::vertical_advance`] at the hinted scale (the
    /// auto-hinter's vertical scale for a face it hints), rounded to a
    /// whole pixel.
    ///
    /// # Errors
    ///
    /// As [`Self::default_advance`].
    pub fn default_vertical_advance(&self, glyph: GlyphId) -> Result<i32> {
        glyf::narrow(self.default_advances(glyph)?.1)
    }

    /// The horizontal and vertical advances of the default load of `glyph`,
    /// fitted to the grid.
    fn default_advances(&self, glyph: GlyphId) -> Result<(i64, i64)> {
        let font = self.font;
        let unscaled = self.unscaled();
        match &font.outlines {
            Outlines::Glyf(_, TrueTypeHinting::Unsupported(what)) if !unscaled => {
                return Err(Error::Unsupported(what));
            }
            Outlines::Glyf(_, TrueTypeHinting::Auto(autofit)) if !unscaled => {
                let mut work = WorkBudget::new(font.limits.work);
                return autofit.advances(font, glyph, self.scaler(), &mut work);
            }
            _ => {}
        }
        let index = font.check(glyph)?;
        let advances = match &font.outlines {
            Outlines::Glyf(table, _) => {
                let m = self.hinted_glyf_metrics(table, index)?;
                font.glyf_advances(m, self.hinted_scale).1
            }
            Outlines::Cff(cff) => font.cff_advances(cff, glyph, index, self.hinted_scale)?,
        };
        let (horizontal, vertical) = (advances.horizontal, advances.vertical);
        Ok(if unscaled {
            (horizontal, vertical)
        } else {
            (pix_round(horizontal), pix_round(vertical))
        })
    }

    /// Whether glyphs load unscaled and unhinted, in font units: at zero
    /// pixels per em.
    const fn unscaled(&self) -> bool {
        self.metrics.y_ppem == 0
    }

    /// The auto-hinter's scaler for the size: its scales and pixels per em.
    const fn scaler(&self) -> crate::autofit::Scaler {
        crate::autofit::Scaler {
            x_scale: self.metrics.x_scale,
            y_scale: self.metrics.y_scale,
            ppem: self.metrics.x_ppem,
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
        if !compatible && !self.unscaled() {
            return Err(Error::Unsupported(
                "hinted advances under a TrueType instruction control",
            ));
        }
        Ok(m)
    }

    /// The `kern` value for the pair in 26.6 pixels, as FreeType's default
    /// kerning mode gives it: scaled, reduced in proportion below 25 pixels
    /// per em, and rounded to a whole pixel.
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
}

/// The phantom points `units` (font units) as the programs see them at the
/// 16.16 `scale`. Observed: scaled, the vertical ones at half the advance,
/// they start there, and the horizontal ones' x and the vertical ones' y
/// are then moved to the pixel grid (`phantom-points`, `phantom-originals`).
/// `Overflow` for a point outside 32 bits.
fn scaled_phantom_points(units: [Point; 4], scale: i64) -> Result<Phantoms> {
    let scaled = |u: i32| i32::try_from(mul_fix(i64::from(u), scale)).map_err(|_| Error::Overflow);
    let fitted = |u: i32| {
        scaled(u)?
            .checked_add(32)
            .map(|v| v & !63)
            .ok_or(Error::Overflow)
    };
    let [pp1, pp2, pp3, pp4] = units;
    let middle = scaled(((i64::from(pp2.x) - i64::from(pp1.x)) / 2) as i32)?;
    Ok(Phantoms {
        current: [
            Point::new(fitted(pp1.x)?, 0),
            Point::new(fitted(pp2.x)?, 0),
            Point::new(middle, fitted(pp3.y)?),
            Point::new(middle, fitted(pp4.y)?),
        ],
        original: [
            Point::new(scaled(pp1.x)?, 0),
            Point::new(scaled(pp2.x)?, 0),
            Point::new(middle, scaled(pp3.y)?),
            Point::new(middle, scaled(pp4.y)?),
        ],
        units,
    })
}

/// Runs each glyph's program as a hinted TrueType load builds its outline.
struct GlyfHinting<'h> {
    font: &'h Font,
    hinter: &'h mut Hinter,
    programs: Programs<'h>,
    /// The font-unit positions of the outline's points.
    units: &'h mut BudgetVec<Point>,
    /// One glyph's contour ends, relative to its first point.
    ends: &'h mut BudgetVec<u32>,
}

impl glyf::Hint for GlyfHinting<'_> {
    const ACTIVE: bool = true;

    fn unit(&mut self, unit: Point) -> Result<()> {
        self.units.try_push(unit).map_err(|e| e.error)
    }

    fn grid_fitting(&self) -> bool {
        self.hinter.grid_fitting()
    }

    fn glyph(
        &mut self,
        glyph: glyf::Loaded<'_>,
        outline: &mut Outline,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let glyf::Loaded {
            metrics,
            composite,
            base,
            ends,
            instructions,
        } = glyph;
        let first = u32::try_from(base).map_err(|_| Error::Overflow)?;
        self.ends.clear();
        for &end in &outline.contour_ends()[ends..] {
            let end = end.checked_sub(first).ok_or(Error::Overflow)?;
            self.ends.try_push(end).map_err(|e| e.error)?;
        }
        let units = self.font.phantom_points(metrics);
        let points = GlyphPoints {
            outline,
            base,
            ends: self.ends.as_slice(),
            units: (!composite).then(|| &self.units.as_slice()[base..]),
            phantoms: scaled_phantom_points(units, self.hinter.scale())?,
        };
        self.hinter.hint(self.programs, instructions, points, work)
    }
}

#[cfg(test)]
mod tests {
    use super::{keeps_compatibility, linear_advance, scaled_phantom_points};
    use crate::outline::Point;
    use triergon_core::Error;

    #[test]
    fn phantom_points_outside_32_bits_are_an_overflow() {
        let units = [(0, 0), (1000, 0), (0, 800), (0, -200)].map(|(x, y)| Point::new(x, y));
        // Height 1000 at 2048 units per em.
        let points = |p: [(i32, i32); 4]| p.map(|(x, y)| Point::new(x, y));
        assert_eq!(
            scaled_phantom_points(units, 32000).map(|p| (p.current, p.original)),
            Ok((
                points([(0, 0), (512, 0), (244, 384), (244, -128)]),
                points([(0, 0), (488, 0), (244, 391), (244, -98)])
            ))
        );
        let wide = [(0, 0), (32767, 0), (0, 0), (0, 0)].map(|(x, y)| Point::new(x, y));
        assert_eq!(scaled_phantom_points(wide, 1 << 33), Err(Error::Overflow));
        // A scaled value just under 2^31 that rounding would carry over.
        let edge = 0x7FFF_FFF0_i64 * 65536 / 32767;
        assert_eq!(scaled_phantom_points(wide, edge + 1), Err(Error::Overflow));
    }

    #[test]
    fn linear_advances_wrap_in_32_bits() {
        // 65535 units at a y scale of 2^20 (a quarter pixel per unit): 2^36 /
        // 64 is just under 2^30, which fits; at 2^22 the product's low 32
        // bits are 0xFFFF0000, -65536.
        assert_eq!(linear_advance(65535, 1 << 20), 0x3fff_c000);
        assert_eq!(linear_advance(65535, 1 << 22), -0x1_0000);
        // A y scale of 2^31 is the smallest value 32 bits hold.
        assert_eq!(linear_advance(1, 1 << 31), -(1 << 25));
    }

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
