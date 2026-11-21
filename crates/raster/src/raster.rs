//! The rasterising context: the scratch every stage reuses.

use triergon_core::{Budget, Error, Result, WorkBudget};

use crate::bitmap::Bitmap;
use crate::blur::Blur;
use crate::limits::Limits;
use crate::outline::Outline;
use crate::rasterizer::Rasterizer;
use crate::shape::Shape;
use crate::stroke::{Pen, stroke};
use crate::transform::BitmapTransform;

/// The stroker, the rasteriser and the blur with the scratch they reuse: the
/// rasteriser's segment buffers, transformed and stroked shapes and the
/// blur's planes, all charged to one [`Budget`]. Every operation takes the [`WorkBudget`] of the
/// frame it belongs to; running out of it, or a size beyond [`Limits`], is
/// `Error::Policy`.
#[derive(Debug)]
pub struct Raster {
    pub(crate) budget: Budget,
    pub(crate) limits: Limits,
    pub(crate) rasterizer: Rasterizer,
    pub(crate) transformed: Shape,
    /// The border being stroked, grown as the stroker goes.
    pub(crate) stroked: Shape,
    pub(crate) blur: Blur,
    /// The unit square every opaque box is drawn from, once one is.
    pub(crate) unit_box: Option<Shape>,
}

impl Raster {
    /// A context whose scratch and results are charged to `budget`.
    pub fn new(budget: &Budget, limits: Limits) -> Self {
        Self {
            budget: budget.clone(),
            limits,
            rasterizer: Rasterizer::new(budget),
            transformed: Shape::new(budget),
            stroked: Shape::new(budget),
            blur: Blur::new(budget),
            unit_box: None,
        }
    }

    /// Bytes of scratch storage currently charged.
    pub fn retained_bytes(&self) -> usize {
        self.rasterizer.retained_bytes()
            + self.transformed.retained_bytes()
            + self.stroked.retained_bytes()
            + self.blur.retained_bytes()
            + self.unit_box.as_ref().map_or(0, Shape::retained_bytes)
    }

    /// Strokes `outline` with `pen`, writing the outer (+offset) and inner
    /// (−offset) border outlines into `border`, which is cleared first.
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
    /// nonzero rule, in 16×16 tiles, into a bitmap of whole tiles placed at
    /// its pixel origin. The bitmap covers the polyline's bounds widened by
    /// 1/64 pixel on the top-left and a pixel on the bottom-right. Returns
    /// `None` when there is nothing to draw, or the bitmap would exceed
    /// [`MAX_BITMAP_BYTES`](crate::MAX_BITMAP_BYTES), as libass draws nothing
    /// then.
    ///
    /// # Errors
    ///
    /// `Policy` when `work` or the limits run out, and the budget's errors.
    pub fn rasterize(&mut self, shape: &Shape, work: &mut WorkBudget) -> Result<Option<Bitmap>> {
        self.rasterizer.rasterize(shape, &self.limits, work)
    }

    /// Rasterises `shape` under the quantised `transform`, as
    /// [`Raster::rasterize`] does, into a bitmap placed against the
    /// transform's [`position`](crate::Quantized::position). An outline that
    /// leaves the coordinate range under the transform is dropped. `None`
    /// when nothing is drawn.
    ///
    /// # Errors
    ///
    /// `Policy` when `work` or the limits run out, and the budget's errors.
    pub fn draw(
        &mut self,
        shape: &Shape,
        transform: &BitmapTransform,
        work: &mut WorkBudget,
    ) -> Result<Option<Bitmap>> {
        let m = transform.restore(shape.cbox());
        let projection = transform.projection();
        self.transformed.rebuild(|outputs| {
            for (source, out) in shape.outlines().iter().zip(outputs) {
                match source.transform_into(&m, projection, out) {
                    Ok(()) | Err(Error::Invalid(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        })?;
        self.rasterizer
            .rasterize(&self.transformed, &self.limits, work)
    }

    /// Blurs `bitmap`: with the Gaussian of `variance` (x and y, in pixels²)
    /// when either exceeds 0.001, which replaces the bitmap by a larger one
    /// covering the blur's reach, then with `be` passes of the 3×3 box blur
    /// in place.
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
