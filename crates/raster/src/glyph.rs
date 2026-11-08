//! One glyph's bitmaps: the 3D transform of `\frx` `\fry` `\frz` `\fax` `\fay`,
//! its quantisation against the glyph's run, the border or opaque box, and their
//! rasterisation, as libass's `get_bitmap_glyph` draws them.

use triergon_core::{Error, Result, WorkBudget, round_ties_even_i32};

use crate::bitmap::Bitmap;
use crate::fp;
use crate::geometry::{OUTLINE_MAX, Rect, Vector};
use crate::raster::Raster;
use crate::shape::Shape;
use crate::stroke::{Pen, stroke};
use crate::transform::{BitmapTransform, MAX_PERSP_SCALE, Matrix, RunOffset};

/// Stroker tolerance in outline units; outlines are scaled so it suffices.
const STROKER_PRECISION: i32 = 16;
/// Transform error estimate, in 1/64 pixel.
const POSITION_PRECISION: f64 = 8.0;

/// Where and how one glyph is placed: the per-glyph inputs of libass's
/// transform.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    /// `\frx`, `\fry`, `\frz` in degrees.
    pub rotation: [f64; 3],
    /// `\fax`, `\fay`.
    pub shear: [f64; 2],
    /// The glyph's horizontal and vertical scale.
    pub scale: [f64; 2],
    /// Offset of the glyph from the rotation origin, 26.6.
    pub shift: Vector,
    /// The ascender, 26.6: the distance from the baseline up to the top of the
    /// opaque box, which the `\fax` shear is also measured from.
    pub ascender: i32,
    /// The descender, 26.6, positive below the baseline: the distance down to
    /// the bottom of the opaque box.
    pub descender: i32,
    /// Position of the glyph origin in the frame, 26.6.
    pub position: Vector,
    /// Scale (x and y) from outline units to 26.6 before the transform: the
    /// font scale, or the drawing scale.
    pub outline_scale: [f64; 2],
    /// Offset (x and y), 26.6, added after `outline_scale`: zero for a glyph,
    /// the baseline offset of a drawing.
    pub outline_offset: [f64; 2],
}

/// Frame-wide scales of the renderer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameScale {
    /// Scales of `\blur` (x and y); y also sets the perspective distance.
    pub blur: [f64; 2],
    /// Scales of border widths and shadow offsets (x and y).
    pub border: [f64; 2],
    /// The pixel aspect correction applied to x.
    pub pixel_aspect: f64,
}

/// The border a glyph is drawn with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Border {
    /// No border bitmap.
    None,
    /// `\bord` style 1: the outline stroked by `size` (x and y) pixels.
    Outline {
        /// `\xbord` and `\ybord` in script pixels, before the frame's border
        /// scale.
        size: [f64; 2],
    },
    /// `\bord` style 3: an opaque box around the advance, `size` beyond it.
    /// `spacing` is the scaled `\fsp` and `advance` the glyph advance, both
    /// 26.6, and `scale_fix` the font scale correction the box is scaled by
    /// again.
    OpaqueBox {
        /// `\xbord` and `\ybord` in script pixels, before the frame's border
        /// scale; the box reaches at least a pixel beyond the advance and the
        /// height before the transform.
        size: [f64; 2],
        /// The scaled `\fsp` letter spacing, 26.6.
        spacing: i32,
        /// The glyph advance, 26.6.
        advance: i32,
        /// The font scale correction (libass's `scale_fix`).
        scale_fix: f64,
    },
}

/// What [`Raster::render`] draws: a glyph or drawing shape, where and how.
#[derive(Clone, Copy, Debug)]
pub struct GlyphRequest<'a> {
    /// The glyph or drawing in outline units, y down; its first outline is
    /// the one stroked for a border.
    pub shape: &'a Shape,
    /// Where and how the glyph is placed.
    pub placement: Placement,
    /// The renderer's frame-wide scales.
    pub frame: FrameScale,
    /// The border to draw alongside the glyph.
    pub border: Border,
    /// The glyph is timed by `\kf` karaoke, so its run tracks the least x of
    /// its transformed outline.
    pub karaoke: bool,
}

/// The matrices of libass's glyph transform (`calc_transform_matrix`).
#[derive(Clone, Copy, Debug, PartialEq)]
struct GlyphTransform {
    /// From 26.6 glyph space to the frame: shear, the three rotations about the
    /// origin `shift` away, perspective at distance 20000 · the y blur scale,
    /// then the move to the position.
    glyph: Matrix,
    /// `glyph` after the outline scale and offset.
    outline: Matrix,
}

impl GlyphTransform {
    fn new(p: &Placement, frame: &FrameScale) -> Self {
        let to_radians = std::f64::consts::PI / 180.0;
        let [frx, fry, frz] = p.rotation.map(|degrees| to_radians * degrees);
        let (sx, cx) = (-frx.sin(), frx.cos());
        let (sy, cy) = (fry.sin(), fry.cos());
        let (sz, cz) = (-frz.sin(), frz.cos());

        let fax = p.shear[0] * p.scale[0] / p.scale[1];
        let fay = p.shear[1] * p.scale[1] / p.scale[0];
        let x1 = [1.0, fax, f64::from(p.shift.x) + f64::from(p.ascender) * fax];
        let y1 = [fay, 1.0, f64::from(p.shift.y)];

        let mut x2 = [0.0; 3];
        let mut y2 = [0.0; 3];
        for i in 0..3 {
            x2[i] = x1[i] * cz - y1[i] * sz;
            y2[i] = x1[i] * sz + y1[i] * cz;
        }
        let y3 = y2.map(|v| v * cx);
        let z3 = y2.map(|v| v * sx);
        let mut x4 = [0.0; 3];
        let mut z4 = [0.0; 3];
        for i in 0..3 {
            x4[i] = x2[i] * cy - z3[i] * sy;
            z4[i] = x2[i] * sy + z3[i] * cy;
        }
        let dist = 20000.0 * frame.blur[1];
        z4[2] += dist;

        let scale_x = dist * frame.pixel_aspect;
        let offs_x = f64::from(p.position.x) - f64::from(p.shift.x) * frame.pixel_aspect;
        let offs_y = f64::from(p.position.y.wrapping_sub(p.shift.y));
        let mut m = [[0.0; 3]; 3];
        for i in 0..3 {
            m[0][i] = z4[i] * offs_x + x4[i] * scale_x;
            m[1][i] = z4[i] * offs_y + y3[i] * dist;
            m[2][i] = z4[i];
        }
        let glyph = Matrix(m);
        Self {
            glyph,
            outline: glyph.prescaled(p.outline_scale, p.outline_offset),
        }
    }

    /// The matrix from outline units to frame 26.6.
    fn matrix(&self) -> &Matrix {
        &self.outline
    }

    /// The opaque box's matrix: the unit square scaled to the advance and
    /// height plus the border, which VSFilter scales by the font scale a second
    /// time.
    fn opaque_box(
        &self,
        p: &Placement,
        frame: &FrameScale,
        size: [f64; 2],
        width: i32,
        scale_fix: f64,
    ) -> Matrix {
        let mut bord = [
            64.0 * size[0] * frame.border[0] / frame.pixel_aspect,
            64.0 * size[1] * frame.border[1],
        ];
        let mut width = f64::from(width);
        let mut height = f64::from(p.ascender.wrapping_add(p.descender));
        let orig = [p.scale[0] * scale_fix, p.scale[1] * scale_fix];
        bord[0] *= orig[0];
        bord[1] *= orig[1];
        width *= orig[0];
        height *= orig[1];
        // At least a pixel, to avoid gaps.
        let bord = bord.map(|b| fp::max(64.0, b));
        let scale = [
            (width + 2.0 * bord[0]) / 64.0,
            (height + 2.0 * bord[1]) / 64.0,
        ];
        let offset = [-bord[0], -bord[1] - f64::from(p.ascender)];
        self.glyph.prescaled(scale, offset)
    }
}

impl Matrix {
    /// The matrix with a per-axis scale and offset applied to its input first.
    fn prescaled(&self, scale: [f64; 2], offset: [f64; 2]) -> Self {
        Self(self.0.map(|r| {
            [
                r[0] * scale[0],
                r[1] * scale[1],
                r[0] * offset[0] + r[1] * offset[1] + r[2],
            ]
        }))
    }
}

/// A border outline's cache key: the scale it is stroked at and its widths in
/// stroker units.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct BorderKey {
    /// The outline is scaled by 2^order (x and y) before stroking.
    scale_order: [i32; 2],
    /// Border widths in 1/16 outline units at that scale.
    size: Vector,
}

impl BorderKey {
    /// The key of a border `size` pixels wide and the matrix of the scaled
    /// outline, from the glyph's centred matrix `m` and its outline matrix: the
    /// outline is scaled by powers of two so the stroker's fixed tolerance stays
    /// near 1/8 pixel after the transform. `None` when the border is out of
    /// range.
    fn new(
        m: &Matrix,
        outline: &Matrix,
        cbox: &Rect,
        p: &Placement,
        frame: &FrameScale,
        size: [f64; 2],
    ) -> Option<(Self, Matrix)> {
        let bord_x = 64.0 * frame.border[0] * size[0] / p.outline_scale[0] / frame.pixel_aspect;
        let bord_y = 64.0 * frame.border[1] * size[1] / p.outline_scale[1];
        // Half extents after stroking.
        let dx = f64::from(cbox.x_max - cbox.x_min) / 2.0 + (bord_x + 64.0);
        let dy = f64::from(cbox.y_max - cbox.y_min) / 2.0 + (bord_y + 64.0);
        let m = &m.0;
        let (mxx, mxy) = (m[0][0].abs(), m[0][1].abs());
        let (myx, myy) = (m[1][0].abs(), m[1][1].abs());
        let (mzx, mzy) = (m[2][0].abs(), m[2][1].abs());
        let z0 = m[2][2] - mzx * dx - mzy * dy;
        let mut w = 1.0 / fp::max(z0, m[2][2] / MAX_PERSP_SCALE);
        let x_lim = mxx * dx + mxy * dy;
        let y_lim = myx * dx + myy * dy;
        let rz = fp::max(x_lim, y_lim) * w;
        w *= f64::from(STROKER_PRECISION) / POSITION_PRECISION;
        let (_, order_x) = fp::frexp(w * (fp::max(mxx, myx) + mzx * rz));
        let (_, order_y) = fp::frexp(w * (fp::max(mxy, myy) + mzy * rz));
        let bord_x = fp::ldexp(bord_x, order_x);
        let bord_y = fp::ldexp(bord_y, order_y);
        let limit = f64::from(OUTLINE_MAX);
        if !(bord_x < limit && bord_y < limit) {
            return None;
        }
        let key = Self {
            scale_order: [order_x, order_y],
            size: Vector::new(
                round_ties_even_i32(bord_x / f64::from(STROKER_PRECISION)),
                round_ties_even_i32(bord_y / f64::from(STROKER_PRECISION)),
            ),
        };
        let matrix = Matrix(
            outline
                .0
                .map(|r| [fp::ldexp(r[0], -order_x), fp::ldexp(r[1], -order_y), r[2]]),
        );
        Some((key, matrix))
    }
}

/// The state of one run of glyphs composited together: the sub-pixel origin
/// its first drawn glyph sets, and the least transformed x of its karaoke
/// glyphs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Run {
    offset: RunOffset,
    drawn: usize,
    leftmost_x: i32,
}

impl Default for Run {
    fn default() -> Self {
        Self {
            offset: RunOffset::default(),
            drawn: 0,
            leftmost_x: OUTLINE_MAX,
        }
    }
}

impl Run {
    /// The least x, 26.6, of the transformed outlines of the run's karaoke
    /// glyphs; [`OUTLINE_MAX`] without any.
    pub fn leftmost_x(&self) -> i32 {
        self.leftmost_x
    }
}

/// A glyph's border bitmap.
#[derive(Debug)]
enum BorderBitmap {
    /// No border: none was requested, or it could not be drawn.
    None,
    /// The border rounded to nothing: the glyph bitmap stands in for it.
    Glyph,
    /// A bitmap of its own: the stroked outline or the opaque box.
    Own(Bitmap),
}

/// The bitmaps [`Raster::render`] draws for a glyph, placed against one
/// origin.
#[derive(Debug)]
pub struct GlyphBitmaps {
    glyph: Option<Bitmap>,
    border: BorderBitmap,
    origin: Vector,
    key: GlyphKey,
}

impl GlyphBitmaps {
    /// The glyph's fill, `None` when it has nothing to draw or leaves the
    /// coordinate range.
    pub fn glyph(&self) -> Option<&Bitmap> {
        self.glyph.as_ref()
    }

    /// The border: the stroked outline or the opaque box, or the glyph's own
    /// bitmap when the border rounds to nothing. `None` without a border.
    pub fn border(&self) -> Option<&Bitmap> {
        match &self.border {
            BorderBitmap::None => None,
            BorderBitmap::Glyph => self.glyph.as_ref(),
            BorderBitmap::Own(bitmap) => Some(bitmap),
        }
    }

    /// The whole-pixel frame position both bitmaps'
    /// [`position`](Bitmap::position)s count from: the glyph's quantised
    /// position, or the border's when only the border is drawn.
    pub fn origin(&self) -> Vector {
        self.origin
    }

    /// What identifies these bitmaps besides the shape they are drawn from;
    /// see [`GlyphKey`].
    pub fn key(&self) -> GlyphKey {
        self.key
    }
}

#[cfg(test)]
impl GlyphBitmaps {
    /// A glyph bitmap without a border, at `origin`.
    pub(crate) fn of_glyph(glyph: Bitmap, origin: Vector) -> Self {
        Self {
            glyph: Some(glyph),
            border: BorderBitmap::None,
            origin,
            key: GlyphKey {
                glyph: None,
                border: None,
                border_offset: Vector::default(),
            },
        }
    }
}

/// What identifies a glyph's bitmaps besides its shape: the quantised
/// transforms they were drawn with, the border's stroking scale and widths or
/// opaque box, and where the border sits against the glyph. Bitmaps of the
/// same shape with equal keys are equal, so a renderer can key its glyph and
/// run caches with it, as libass keys its bitmap and composite caches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    /// The glyph bitmap's transform, if it is drawn.
    glyph: Option<BitmapTransform>,
    /// The border bitmap, if it is drawn.
    border: Option<BorderId>,
    /// The border bitmap's own origin from the glyph's, in whole pixels.
    border_offset: Vector,
}

/// What a border bitmap is drawn from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum BorderId {
    /// The glyph bitmap stands in for it.
    Glyph,
    /// The glyph's first outline stroked under `BorderKey`.
    Stroke(BorderKey, BitmapTransform),
    /// The opaque box.
    Box(BitmapTransform),
}

impl Raster {
    /// Draws one glyph of `run` as libass does: quantises its transform against
    /// the run (the run's first drawn glyph sets the run's sub-pixel origin),
    /// rasterises the glyph, then its border outline or opaque box placed
    /// against the same origin. A glyph or border that leaves the coordinate
    /// range under its transform is not drawn, as in libass; `None` when
    /// nothing is drawn, and the glyph is then no part of the run.
    ///
    /// # Errors
    ///
    /// `Policy` when `work` or the limits run out, and the budget's errors.
    pub fn render(
        &mut self,
        request: &GlyphRequest<'_>,
        run: &mut Run,
        work: &mut WorkBudget,
    ) -> Result<Option<GlyphBitmaps>> {
        let GlyphRequest {
            shape,
            placement,
            frame,
            karaoke,
            ..
        } = request;
        let transform = GlyphTransform::new(placement, frame);
        if *karaoke {
            run.leftmost_x =
                shape.outlines()[0].min_transformed_x(transform.matrix(), run.leftmost_x);
        }
        let starts = run.drawn == 0;
        let continued = (!starts).then_some(run.offset);
        let Some(quantized) =
            BitmapTransform::quantize(transform.matrix(), shape.cbox(), continued)
        else {
            return Ok(None);
        };
        if starts {
            run.offset = quantized.remainder;
        }
        let glyph = self.bitmap(shape, &quantized.key, work)?;
        let mut out = GlyphBitmaps {
            key: GlyphKey {
                glyph: glyph.is_some().then_some(quantized.key),
                border: None,
                border_offset: Vector::default(),
            },
            glyph,
            border: BorderBitmap::None,
            origin: quantized.position,
        };
        self.render_border(request, &transform, &quantized.centred, run, &mut out, work)?;
        if out.glyph.is_none() && out.border().is_none() {
            return Ok(None);
        }
        run.drawn += 1;
        Ok(Some(out))
    }

    /// The border part of [`Raster::render`]: the border bitmap, placed
    /// against the glyph's origin.
    fn render_border(
        &mut self,
        request: &GlyphRequest<'_>,
        transform: &GlyphTransform,
        centred: &Matrix,
        run: &Run,
        out: &mut GlyphBitmaps,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let (placement, frame) = (&request.placement, &request.frame);
        let (shape, m, stroke) = match request.border {
            Border::None => return Ok(()),
            Border::OpaqueBox {
                size,
                spacing,
                advance,
                scale_fix,
            } => {
                let width = spacing.wrapping_add(advance);
                let m = transform.opaque_box(placement, frame, size, width, scale_fix);
                (Shape::unit_box(&self.budget)?, m, None)
            }
            Border::Outline { size } => {
                let glyph = request.shape;
                let Some((key, m)) = BorderKey::new(
                    centred,
                    transform.matrix(),
                    glyph.cbox(),
                    placement,
                    frame,
                    size,
                ) else {
                    return Ok(());
                };
                if key.size == Vector::default() {
                    out.border = BorderBitmap::Glyph;
                    out.key.border = out.glyph.is_some().then_some(BorderId::Glyph);
                    return Ok(());
                }
                let Some(stroked) = self.stroke_border(glyph, &key, work)? else {
                    return Ok(());
                };
                (stroked, m, Some(key))
            }
        };
        let Some(quantized) = BitmapTransform::quantize(&m, shape.cbox(), Some(run.offset)) else {
            return Ok(());
        };
        let Some(mut bitmap) = self.bitmap(&shape, &quantized.key, work)? else {
            return Ok(());
        };
        if out.glyph.is_none() {
            out.origin = quantized.position;
        }
        let offset = Vector::new(
            quantized.position.x.wrapping_sub(out.origin.x),
            quantized.position.y.wrapping_sub(out.origin.y),
        );
        bitmap.position.x = bitmap.position.x.wrapping_add(offset.x);
        bitmap.position.y = bitmap.position.y.wrapping_add(offset.y);
        out.border = BorderBitmap::Own(bitmap);
        out.key.border = Some(match stroke {
            Some(key) => BorderId::Stroke(key, quantized.key),
            None => BorderId::Box(quantized.key),
        });
        out.key.border_offset = offset;
        Ok(())
    }

    /// The bitmap of `shape` under the quantised transform `key`. An outline
    /// that leaves the coordinate range under the transform is dropped, as
    /// libass drops it.
    fn bitmap(
        &mut self,
        shape: &Shape,
        key: &BitmapTransform,
        work: &mut WorkBudget,
    ) -> Result<Option<Bitmap>> {
        let m = key.restore(shape.cbox());
        let projection = key.projection();
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

    /// The border of `glyph` for `key`: its first outline scaled and stroked.
    /// `None` when it cannot be stroked within the coordinate range.
    fn stroke_border(
        &mut self,
        glyph: &Shape,
        key: &BorderKey,
        work: &mut WorkBudget,
    ) -> Result<Option<Shape>> {
        let source = &glyph.outlines()[0];
        let mut border = Shape::new(&self.budget);
        if (key.size.x == 0 && key.size.y == 0) || source.is_empty() {
            return Ok(Some(border));
        }
        let pen = Pen {
            width: Vector::new(
                key.size.x.wrapping_mul(STROKER_PRECISION),
                key.size.y.wrapping_mul(STROKER_PRECISION),
            ),
            tolerance: STROKER_PRECISION,
        };
        let scaled = &mut self.transformed;
        let points = self.limits.outline_points;
        let stroked = scaled
            .rebuild(|[first, _]| source.scale_pow2_into(first, key.scale_order))
            .and_then(|()| stroke(&scaled.outlines()[0], &pen, points, &mut border, work));
        match stroked {
            Ok(()) => Ok(Some(border)),
            Err(Error::Invalid(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }
}
