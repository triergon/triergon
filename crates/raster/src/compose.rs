//! Combining a run's glyph bitmaps into its fill, border and shadow bitmaps:
//! blur, `\be`, the border's subtraction of the fill and the sub-pixel shadow.

use triergon_core::{Budget, Result};

use crate::bitmap::Bitmap;
use crate::blur::Blur;
use crate::fp;
use crate::geometry::Vector;
use crate::glyph::FrameScale;
use crate::limits::Limits;

/// Blur error as a fraction of the full range.
const BLUR_PRECISION: f64 = 1.0 / 256.0;
/// Transform error estimate, in 1/64 pixel.
const POSITION_PRECISION: f64 = 8.0;
/// Quantised blur steps per (64 · radius) unit.
const BLUR_SCALE: f64 = 64.0 * BLUR_PRECISION / POSITION_PRECISION;

/// What a run's composite needs to do, as libass derives it from the style.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FilterFlags {
    /// `\bord` style 3: the border is an opaque box.
    pub opaque_box: bool,
    /// The run has a border bitmap (`FILTER_NONZERO_BORDER`).
    pub border: bool,
    /// The run has a shadow bitmap (`FILTER_NONZERO_SHADOW`).
    pub shadow: bool,
    /// The shadow covers the fill's area too (transparent fill, karaoke).
    pub fill_in_shadow: bool,
    /// The border covers the fill's area too (transparent fill and border).
    pub fill_in_border: bool,
}

/// The effects of a run, as its style and override tags set them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Effects {
    /// Which bitmaps the run's composite produces and how they overlap.
    pub flags: FilterFlags,
    /// `\be` passes.
    pub be: u32,
    /// `\blur` radius in pixels.
    pub blur: f64,
    /// `\xshad` and `\yshad` in pixels.
    pub shadow: [f64; 2],
}

/// The post-processing of a run with its blur and shadow quantised: what
/// libass keys its composites by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Filter {
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
    pub fn new(effects: &Effects, frame: &FrameScale) -> Self {
        let radius_scale = 2.0 / 256.0f64.ln().sqrt();
        let radius = effects.blur;
        let (blur_x, mask_x) = quantize_blur(radius * (frame.blur[0] * radius_scale));
        let (blur_y, mask_y) = quantize_blur(radius * (frame.blur[1] * radius_scale));
        let flags = effects.flags;
        let shadow = if flags.shadow {
            let snap = |v: f64, mask: i32| (fp::lrint32(v * 64.0).wrapping_add(mask >> 1)) & !mask;
            Vector::new(
                snap(effects.shadow[0] * frame.border[0], mask_x),
                snap(effects.shadow[1] * frame.border[1], mask_y),
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
        fp::lrint32(radius.ln_1p() / BLUR_PRECISION),
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

/// One glyph of a run: its bitmaps and their positions relative to the run.
#[derive(Clone, Copy, Debug)]
pub struct GlyphRef<'a> {
    /// The glyph's fill bitmap, if it has one.
    pub glyph: Option<&'a Bitmap>,
    /// The glyph's border bitmap, if it has one; it may be the fill bitmap.
    pub border: Option<&'a Bitmap>,
    /// Where `glyph`'s `left` and `top` count from, whole pixels relative to
    /// the run.
    pub position: Vector,
    /// Where `border`'s `left` and `top` count from, whole pixels relative to
    /// the run.
    pub border_position: Vector,
}

/// A run's composited bitmaps, positioned relative to the run.
#[derive(Debug, Default)]
pub struct Composite {
    /// The summed glyph fills, blurred only without a stroked border.
    pub fill: Option<Bitmap>,
    /// The summed and blurred borders with the fill subtracted, unless
    /// `fill_in_border` keeps it; `None` when an opaque box becomes the shadow.
    pub border: Option<Bitmap>,
    /// The shadow, shifted by the shadow offset: a copy of the stroked border
    /// (the fill subtracted unless `fill_in_shadow` keeps it), the opaque box
    /// itself, or a copy of the fill without a border.
    pub shadow: Option<Bitmap>,
}

/// Composites runs; owns the blur scratch.
#[derive(Debug)]
pub struct Compositor {
    blur: Blur,
    budget: Budget,
    limits: Limits,
}

impl Compositor {
    /// A compositor whose bitmaps and blur scratch are charged to `budget`,
    /// each operation bounded by `limits`.
    pub fn new(budget: &Budget, limits: Limits) -> Self {
        Self {
            blur: Blur::new(budget, limits),
            budget: budget.clone(),
            limits,
        }
    }

    /// Bytes of scratch storage currently charged.
    pub fn retained_bytes(&self) -> usize {
        self.blur.retained_bytes()
    }

    /// Combines `glyphs` under `filter`, as libass's composite cache builds a
    /// run: glyphs and borders summed (one bitmap without `\be` is copied),
    /// blurred, the fill taken out of the border where it shows through, and
    /// the shadow copied from the border or fill and shifted.
    pub fn compose(&mut self, filter: &Filter, glyphs: &[GlyphRef<'_>]) -> Result<Composite> {
        let pad = be_padding(filter.be);
        let mut fill = self.combine(
            glyphs.iter().filter_map(|g| Some((g.glyph?, g.position))),
            pad,
        )?;
        let mut border = self.combine(
            glyphs
                .iter()
                .filter_map(|g| Some((g.border?, g.border_position))),
            pad,
        )?;
        let flags = filter.flags;
        let variance = filter.variances();
        if (!flags.border || flags.opaque_box)
            && let Some(fill) = &mut fill
        {
            self.blur.apply(fill, filter.be, variance)?;
        }
        if let Some(border) = &mut border {
            self.blur.apply(border, filter.be, variance)?;
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
            fill,
            border,
            shadow,
        })
    }

    /// Sums the placed bitmaps into one, padded by `pad` pixels.
    fn combine<'b>(
        &self,
        parts: impl Iterator<Item = (&'b Bitmap, Vector)> + Clone,
        pad: u32,
    ) -> Result<Option<Bitmap>> {
        // Positions are the run's own, so they fit; 64 bits keep hostile ones
        // from overflowing.
        let (mut x_min, mut y_min, mut x_max, mut y_max) = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
        let mut count = 0;
        let mut last = None;
        for (bitmap, pos) in parts.clone() {
            let x = i64::from(pos.x) + i64::from(bitmap.left);
            let y = i64::from(pos.y) + i64::from(bitmap.top);
            (x_min, y_min) = (x_min.min(x), y_min.min(y));
            x_max = x_max.max(x + bitmap.width() as i64);
            y_max = y_max.max(y + bitmap.height() as i64);
            count += 1;
            last = Some((bitmap, pos));
        }
        let Some((last, last_pos)) = last else {
            return Ok(None);
        };
        if pad == 0 && count == 1 {
            let mut copy = last.try_clone(&self.budget)?;
            copy.left = copy.left.wrapping_add(last_pos.x);
            copy.top = copy.top.wrapping_add(last_pos.y);
            return Ok(Some(copy));
        }
        let pad = i64::from(pad);
        let (left, top) = (x_min - pad, y_min - pad);
        let (Ok(left), Ok(top)) = (i32::try_from(left), i32::try_from(top)) else {
            return Ok(None);
        };
        let width = usize::try_from(x_max - x_min + 2 * pad).unwrap_or(usize::MAX);
        let height = usize::try_from(y_max - y_min + 2 * pad).unwrap_or(usize::MAX);
        let Some(mut out) = Bitmap::drawable(&self.budget, width, height, &self.limits)? else {
            return Ok(None);
        };
        (out.left, out.top) = (left, top);
        for (bitmap, pos) in parts {
            let x = i64::from(pos.x) + i64::from(bitmap.left) - i64::from(left);
            let y = i64::from(pos.y) + i64::from(bitmap.top) - i64::from(top);
            out.add(bitmap, x as i32, y as i32);
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::{be_padding, quantize_blur, restore_blur, shadow_mask};

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
