//! The rasterising context: the scratch every stage reuses.

use triergon_core::{Budget, Result, WorkBudget};

use crate::bitmap::Bitmap;
use crate::blur::Blur;
use crate::limits::Limits;
use crate::outline::Outline;
use crate::rasterizer::Rasterizer;
use crate::shape::Shape;
use crate::stroke::{Pen, stroke};

/// libass's outline and bitmap stages with the scratch they reuse: the
/// rasteriser's segment buffers, transformed shapes and the blur's planes, all
/// charged to one [`Budget`]. Every operation takes the [`WorkBudget`] of the
/// frame it belongs to; running out of it, or a size beyond [`Limits`], is
/// `Error::Policy`.
#[derive(Debug)]
pub struct Raster {
    pub(crate) budget: Budget,
    pub(crate) limits: Limits,
    pub(crate) rasterizer: Rasterizer,
    pub(crate) transformed: Shape,
    pub(crate) blur: Blur,
}

impl Raster {
    /// A context whose scratch and results are charged to `budget`.
    pub fn new(budget: &Budget, limits: Limits) -> Self {
        Self {
            budget: budget.clone(),
            limits,
            rasterizer: Rasterizer::new(budget),
            transformed: Shape::new(budget),
            blur: Blur::new(budget),
        }
    }

    /// Bytes of scratch storage currently charged.
    pub fn retained_bytes(&self) -> usize {
        self.rasterizer.retained_bytes()
            + self.transformed.retained_bytes()
            + self.blur.retained_bytes()
    }

    /// Strokes `outline` with `pen`, as libass strokes borders
    /// (`ass_outline_stroke`), writing the outer (+offset) and inner (−offset)
    /// border outlines into `border`, which is cleared first.
    ///
    /// # Errors
    ///
    /// `Invalid` when the larger width is below the tolerance, the tolerance
    /// below 1 or the width above [`OUTLINE_MAX`](crate::OUTLINE_MAX), or a
    /// border point leaves that range; `Policy` when `work` or the limits run
    /// out; and the budget's errors. `border` is then empty.
    pub fn stroke(
        &mut self,
        outline: &Outline,
        pen: &Pen,
        border: &mut Shape,
        work: &mut WorkBudget,
    ) -> Result<()> {
        stroke(outline, pen, self.limits.outline_points, border, work)
    }

    /// Rasterises `shape`, the union of its outlines each filled with the
    /// nonzero rule, as libass does (`ass_outline_to_bitmap`, 16×16 tiles),
    /// into a bitmap of whole tiles placed at its pixel origin. The bitmap
    /// covers the polyline's bounds widened by 1/64 pixel on the top-left and a
    /// pixel on the bottom-right. Returns `None` when there is nothing to draw,
    /// or the bitmap would exceed [`MAX_BITMAP_BYTES`](crate::MAX_BITMAP_BYTES),
    /// as libass draws nothing then.
    ///
    /// # Errors
    ///
    /// `Policy` when `work` or the limits run out, and the budget's errors.
    pub fn rasterize(&mut self, shape: &Shape, work: &mut WorkBudget) -> Result<Option<Bitmap>> {
        self.rasterizer.rasterize(shape, &self.limits, work)
    }

    /// Blurs `bitmap` as libass does (`ass_synth_blur`): with the Gaussian of
    /// `variance` (x and y, in pixels²) when either exceeds 0.001, which
    /// replaces the bitmap by a larger one covering the blur's reach, then with
    /// `be` passes of the 3×3 box blur in place.
    ///
    /// # Errors
    ///
    /// `Policy` when `work` or the limits run out, and the budget's errors; the
    /// bitmap is then unchanged or blurred by the Gaussian only.
    pub fn blur(
        &mut self,
        bitmap: &mut Bitmap,
        be: u32,
        variance: [f64; 2],
        work: &mut WorkBudget,
    ) -> Result<()> {
        self.blur.apply(bitmap, be, variance, &self.limits, work)
    }
}
