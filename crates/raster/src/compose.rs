//! Combining a run's glyph bitmaps into its fill, border and shadow bitmaps:
//! blur, `\be`, the border's subtraction of the fill and the sub-pixel shadow.

use triergon_core::{Error, Result, WorkBudget, round_ties_even_i32};

use crate::bitmap::Bitmap;
use crate::fp;
use crate::geometry::{Vector, pixels_26_6};
use crate::glyph::{FrameScale, GlyphBitmaps};
use crate::limits::{charge, row_work};
use crate::raster::Raster;

/// Blur error as a fraction of the full range.
const BLUR_PRECISION: f64 = 1.0 / 256.0;
/// Transform error estimate, in 1/64 pixel.
const POSITION_PRECISION: f64 = 8.0;
/// Quantised blur steps per (64 · radius) unit.
const BLUR_SCALE: f64 = 64.0 * BLUR_PRECISION / POSITION_PRECISION;

/// A run's border (`\bord`, `\xbord`, `\ybord` and `BorderStyle`), and how
/// it combines with the fill. Every glyph of the run is drawn with it, and
/// the run is composed under it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Border {
    /// The run has no border.
    #[default]
    None,
    /// Stroked outlines (`\bord` style 1). A border that rounds to nothing
    /// takes the glyph's own bitmap. With `keeps_fill` the border also covers
    /// the fill's area, which libass leaves when the fill and karaoke colours
    /// are opaque and nothing fades; otherwise the fill is taken out of it
    /// where the fill shows through.
    Outline {
        /// `\xbord` and `\ybord` in script pixels, before the frame's border
        /// scale.
        size: [f64; 2],
        /// The border keeps its coverage under the fill.
        keeps_fill: bool,
    },
    /// Opaque boxes (`\bord` style 3) around each glyph's
    /// [`advance`](crate::Placement::advance) and height, `size` beyond them,
    /// which cover the fill's area. With `shown` (a nonzero `\bord`) the box
    /// is the border and the shadow is a copy of it; otherwise the box is the
    /// shadow alone, drawn only when the run casts one.
    OpaqueBox {
        /// `\xbord` and `\ybord` in script pixels, before the frame's border
        /// scale; the box reaches at least a pixel beyond the advance and the
        /// height before the transform.
        size: [f64; 2],
        /// The box is drawn as the border too.
        shown: bool,
    },
}

/// A run's shadow (`\xshad`, `\yshad`), cast when either offset is nonzero.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Shadow {
    /// The offset in script pixels, before the frame's border scale.
    pub offset: [f64; 2],
    /// The shadow also covers the fill's area, which libass leaves for a fill
    /// that is not fully transparent, `\kf` and `\ko` karaoke and opaque
    /// boxes; otherwise the fill is taken out of it. Without a border, a
    /// shadow that does not keep the fill would be empty and is not cast.
    pub keeps_fill: bool,
}

/// The effects of a run, as its style and override tags set them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Effects {
    /// The border, and how it combines with the fill.
    pub border: Border,
    /// The shadow.
    pub shadow: Shadow,
    /// `\blur` radius in script pixels, before the frame's blur scale.
    pub blur: f64,
    /// `\be` passes.
    pub be: u32,
}

impl Effects {
    /// What identifies the composite of these effects in a frame scaled by
    /// `frame`: the blur and the shadow offset quantised as libass quantises
    /// them. Glyphs with equal [`GlyphKey`](crate::GlyphKey)s and offsets
    /// compose into equal bitmaps under equal keys.
    pub fn key(&self, frame: &FrameScale) -> EffectsKey {
        EffectsKey(Filter::new(self, frame))
    }
}

/// What identifies a run's effects in a frame; see [`Effects::key`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EffectsKey(Filter);

/// libass's filter flags, from a run's effects as its renderer sets them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct FilterFlags {
    /// `\bord` style 3: the border is an opaque box.
    pub(crate) opaque_box: bool,
    /// The run has a border bitmap.
    pub(crate) border: bool,
    /// The run has a shadow bitmap.
    pub(crate) shadow: bool,
    /// The shadow covers the fill's area too.
    fill_in_shadow: bool,
    /// The border covers the fill's area too.
    fill_in_border: bool,
}

impl FilterFlags {
    pub(crate) fn new(effects: &Effects) -> Self {
        let (opaque_box, border, fill_in_border) = match effects.border {
            Border::None => (false, false, false),
            Border::Outline { keeps_fill, .. } => (false, true, keeps_fill),
            Border::OpaqueBox { shown, .. } => (true, shown, true),
        };
        let [x, y] = effects.shadow.offset;
        let cast = x != 0.0 || y != 0.0;
        let fill_in_shadow = cast && (effects.shadow.keeps_fill || opaque_box);
        Self {
            opaque_box,
            border,
            shadow: cast && (border || fill_in_shadow),
            fill_in_shadow,
            fill_in_border,
        }
    }
}

/// The post-processing of a run with its blur and shadow quantised: what
/// libass keys its composites by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
struct Filter {
    flags: FilterFlags,
    be: u32,
    /// Quantised Gaussian blur (x and y).
    blur: [i32; 2],
    /// Shadow offset, 26.6, rounded to the blur's resolution.
    shadow: Vector,
}

impl Filter {
    /// The filter of a run with `effects` in a frame scaled by `frame`: the blur
    /// under its blur scale, the shadow under its border scale.
    fn new(effects: &Effects, frame: &FrameScale) -> Self {
        let radius_scale = 2.0 / 256.0f64.ln().sqrt();
        let radius = effects.blur;
        let (blur_x, mask_x) = quantize_blur(radius * (frame.blur[0] * radius_scale));
        let (blur_y, mask_y) = quantize_blur(radius * (frame.blur[1] * radius_scale));
        let flags = FilterFlags::new(effects);
        let shadow = if flags.shadow {
            let snap = |v: f64, mask: i32| (pixels_26_6(v).wrapping_add(mask >> 1)) & !mask;
            let [x, y] = effects.shadow.offset;
            Vector::new(
                snap(x * frame.border[0], mask_x),
                snap(y * frame.border[1], mask_y),
            )
        } else {
            Vector::default()
        };
        Self {
            flags,
            be: effects.be,
            blur: [blur_x, blur_y],
            shadow,
        }
    }

    /// The Gaussian variances (x and y, pixels²) the quantised blur stands for.
    fn variances(&self) -> [f64; 2] {
        self.blur.map(restore_blur)
    }
}

/// The quantised blur index of `radius` (1/64 pixel units scaled), and the
/// mask of shadow offset bits below its resolution.
fn quantize_blur(radius: f64) -> (i32, i32) {
    let radius = radius * BLUR_SCALE;
    let (_, order) = fp::frexp((1.0 + radius) * (POSITION_PRECISION / 2.0));
    (
        round_ties_even_i32(radius.ln_1p() / BLUR_PRECISION),
        shadow_mask(order),
    )
}

/// libass's `((uint32_t) 1 << order) - 1`. C leaves a shift of 32 or more
/// undefined; this is what 32-bit ARM computes, whose register shifts use the
/// low byte of the amount and give 0 from 32 on, so the mask is then all ones.
/// AArch64 and x86 wrap the amount modulo 32 instead. Only blur radii beyond
/// 2³⁰ pixels reach an order of 32.
fn shadow_mask(order: i32) -> i32 {
    let amount = order as u32 & 0xFF;
    let power = if amount < 32 { 1u32 << amount } else { 0 };
    power.wrapping_sub(1) as i32
}

fn restore_blur(quantized: i32) -> f64 {
    let sigma = (BLUR_PRECISION * f64::from(quantized)).exp_m1() / BLUR_SCALE;
    sigma * sigma
}

/// The padding `\be` needs around a bitmap.
fn be_padding(be: u32) -> u32 {
    match be {
        0..=3 => be,
        4..=7 => 4,
        _ => 5,
    }
}

/// A run's composited bitmaps, placed against the run's origin.
#[derive(Debug, Default)]
pub struct Composite {
    /// The whole-pixel frame position the bitmaps'
    /// [`position`](Bitmap::position)s count from: the least
    /// [`origin`](GlyphBitmaps::origin) of the run's glyphs.
    pub origin: Vector,
    /// The summed glyph fills, blurred only without a stroked border.
    pub fill: Option<Bitmap>,
    /// The summed and blurred borders, the fill taken out unless the border
    /// keeps it; `None` when an opaque box is the shadow alone.
    pub border: Option<Bitmap>,
    /// The shadow, moved by the shadow offset: a copy of the border (the fill
    /// taken out unless the shadow keeps it), the opaque box, or a copy of the
    /// fill without a border.
    pub shadow: Option<Bitmap>,
}

impl Raster {
    /// Combines the glyphs of a run, in order as [`Raster::render`] drew
    /// them, under `effects` in a frame scaled by `frame`, as libass's
    /// composite cache builds a run (`ass_composite_construct`): fills and
    /// borders summed (one bitmap without `\be` is copied), blurred, the fill
    /// taken out of the border where it shows through, and the shadow copied
    /// from the border or fill and moved. Without glyphs the composite is
    /// empty.
    ///
    /// # Errors
    ///
    /// `Policy` when `work` or the limits run out, and the budget's errors.
    pub fn compose<'g, I>(
        &mut self,
        effects: &Effects,
        frame: &FrameScale,
        glyphs: I,
        work: &mut WorkBudget,
    ) -> Result<Composite>
    where
        I: IntoIterator<Item = &'g GlyphBitmaps>,
        I::IntoIter: Clone,
    {
        let glyphs = glyphs.into_iter();
        let Some(origin) = glyphs
            .clone()
            .map(GlyphBitmaps::origin)
            .reduce(|a, b| Vector::new(a.x.min(b.x), a.y.min(b.y)))
        else {
            return Ok(Composite::default());
        };
        let filter = Filter::new(effects, frame);
        let pad = be_padding(filter.be);
        let placed = |bitmap: fn(&GlyphBitmaps) -> Option<&Bitmap>| {
            glyphs.clone().filter_map(move |g| {
                let at = Vector::new(
                    g.origin().x.wrapping_sub(origin.x),
                    g.origin().y.wrapping_sub(origin.y),
                );
                Some((bitmap(g)?, at))
            })
        };
        let mut fill = self.combine(placed(GlyphBitmaps::glyph), pad, work)?;
        let mut border = self.combine(placed(GlyphBitmaps::border), pad, work)?;
        let flags = filter.flags;
        let variance = filter.variances();
        if (!flags.border || flags.opaque_box)
            && let Some(fill) = &mut fill
        {
            self.blur
                .apply(fill, filter.be, variance, &self.limits, work)?;
        }
        if let Some(border) = &mut border {
            self.blur
                .apply(border, filter.be, variance, &self.limits, work)?;
        }
        let subtract = |border: &mut Option<Bitmap>, fill: &Option<Bitmap>| {
            if let (Some(border), Some(fill)) = (border.as_mut(), fill) {
                border.subtract_fill(fill);
            }
        };
        if !flags.fill_in_border && !flags.fill_in_shadow {
            subtract(&mut border, &fill);
        }
        let mut shadow = None;
        if flags.shadow {
            let clone =
                |b: &Option<Bitmap>| b.as_ref().map(|b| b.try_clone(&self.budget)).transpose();
            if flags.border {
                shadow = clone(&border)?;
                if flags.fill_in_border && !flags.fill_in_shadow {
                    subtract(&mut shadow, &fill);
                }
            } else if flags.opaque_box {
                shadow = border.take();
            } else {
                shadow = clone(&fill)?;
            }
            if let Some(shadow) = &mut shadow {
                shadow.shift_by(filter.shadow);
            }
        }
        if flags.fill_in_shadow && !flags.fill_in_border {
            subtract(&mut border, &fill);
        }
        Ok(Composite {
            origin,
            fill,
            border,
            shadow,
        })
    }

    /// Sums the placed bitmaps into one, padded by `pad` pixels. The sum's
    /// rows and those of every bitmap added into it are charged to `work`
    /// before the sum is allocated; a sum beyond libass's bound is not drawn.
    fn combine<'b>(
        &self,
        parts: impl Iterator<Item = (&'b Bitmap, Vector)> + Clone,
        pad: u32,
        work: &mut WorkBudget,
    ) -> Result<Option<Bitmap>> {
        // Positions are the run's own, so they fit; 64 bits keep hostile ones
        // from overflowing.
        let (mut x_min, mut y_min, mut x_max, mut y_max) = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
        let mut count = 0;
        let mut added = 0u64;
        let mut last = None;
        for (bitmap, pos) in parts.clone() {
            let x = i64::from(pos.x) + i64::from(bitmap.position.x);
            let y = i64::from(pos.y) + i64::from(bitmap.position.y);
            (x_min, y_min) = (x_min.min(x), y_min.min(y));
            x_max = x_max.max(x + bitmap.width() as i64);
            y_max = y_max.max(y + bitmap.height() as i64);
            count += 1;
            added = added.saturating_add(row_work(bitmap.width(), bitmap.height()));
            last = Some((bitmap, pos));
        }
        let Some((last, last_pos)) = last else {
            return Ok(None);
        };
        if pad == 0 && count == 1 {
            charge(work, added)?;
            let mut copy = last.try_clone(&self.budget)?;
            copy.position.x = copy.position.x.wrapping_add(last_pos.x);
            copy.position.y = copy.position.y.wrapping_add(last_pos.y);
            return Ok(Some(copy));
        }
        let pad = i64::from(pad);
        let (left, top) = (x_min - pad, y_min - pad);
        let (Ok(left), Ok(top)) = (i32::try_from(left), i32::try_from(top)) else {
            return Ok(None);
        };
        let width = usize::try_from(x_max - x_min + 2 * pad).unwrap_or(usize::MAX);
        let height = usize::try_from(y_max - y_min + 2 * pad).unwrap_or(usize::MAX);
        match Bitmap::stride_within(width, height, &self.limits) {
            Ok(_) => {}
            Err(Error::Limit { .. }) => return Ok(None),
            Err(error) => return Err(error),
        }
        charge(work, row_work(width, height).saturating_add(added))?;
        let mut out = Bitmap::new(&self.budget, width, height, &self.limits)?;
        out.position = Vector::new(left, top);
        for (bitmap, pos) in parts {
            let x = i64::from(pos.x) + i64::from(bitmap.position.x) - i64::from(left);
            let y = i64::from(pos.y) + i64::from(bitmap.position.y) - i64::from(top);
            out.add(bitmap, x as i32, y as i32);
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::{Effects, be_padding, quantize_blur, restore_blur, shadow_mask};
    use crate::{Bitmap, FrameScale, GlyphBitmaps, Limits, Raster, Vector};
    use triergon_core::{Budget, WorkBudget};

    #[test]
    fn glyphs_as_far_apart_as_positions_reach_compose_into_nothing() {
        let budget = Budget::new(1 << 20).unwrap();
        let glyph = |at| {
            let mut bitmap = Bitmap::new(&budget, 4, 4, &Limits::default()).unwrap();
            bitmap.position = Vector::new(at, at);
            GlyphBitmaps::of_glyph(bitmap, Vector::default())
        };
        let frame = FrameScale {
            blur: [1.0, 1.0],
            border: [1.0, 1.0],
            pixel_aspect: 1.0,
        };
        let composite = Raster::new(&budget, Limits::default())
            .compose(
                &Effects::default(),
                &frame,
                &[glyph(i32::MIN), glyph(i32::MAX)],
                &mut WorkBudget::new(1 << 20),
            )
            .unwrap();
        assert!(composite.fill.is_none());
    }

    #[test]
    fn blur_quantisation_round_trips_closely() {
        for radius in [0.5, 2.0, 10.0, 100.0] {
            let r = radius * 64.0;
            let (q, _) = quantize_blur(r);
            let sigma = restore_blur(q).sqrt();
            assert!((sigma - r).abs() / r < 0.01, "{radius}: {sigma}");
        }
        assert_eq!(quantize_blur(0.0), (0, 7));
    }

    #[test]
    fn the_shadow_mask_shift_is_32_bit_arms() {
        assert_eq!(
            [3, 31, 32, 255, 256, -1].map(shadow_mask),
            [7, i32::MAX, -1, -1, 0, -1]
        );
    }

    #[test]
    fn be_padding_saturates() {
        assert_eq!([0, 3, 4, 7, 8, 100].map(be_padding), [0, 3, 4, 4, 5, 5]);
    }
}
