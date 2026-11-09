//! One glyph's bitmaps: the 3D transform of `\frx` `\fry` `\frz` `\fax` `\fay`,
//! its quantisation against the glyph's run, the border or opaque box, and their
//! rasterisation, as libass's `get_bitmap_glyph` draws them.

use std::sync::Arc;

use triergon_core::{Error, Result, WorkBudget, round_ties_even_i32};

use crate::bitmap::Bitmap;
use crate::compose::{Border, Effects};
use crate::fp;
use crate::geometry::{OUTLINE_MAX, Rect, Vector};
use crate::raster::Raster;
use crate::shape::Shape;
use crate::stroke::{Pen, stroke as stroke_outline};
use crate::transform::{BitmapTransform, MAX_PERSP_SCALE, Matrix, Quantized, RunOffset};

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
    /// The width of the opaque box before its border, 26.6: the glyph's
    /// advance plus the scaled `\fsp` letter spacing.
    pub advance: i32,
    /// The font scale correction (libass's `scale_fix`): `scale` times it is
    /// the scale the style asks for, which the opaque box is scaled by again,
    /// as VSFilter does. 1 for a drawing.
    pub scale_fix: f64,
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
    /// The effects of the glyph's run, whose border is drawn alongside the
    /// glyph: the stroked outline, or the opaque box when it is shown or the
    /// run casts a shadow.
    pub effects: &'a Effects,
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
    fn opaque_box(&self, p: &Placement, frame: &FrameScale, size: [f64; 2]) -> Matrix {
        let mut bord = [
            64.0 * size[0] * frame.border[0] / frame.pixel_aspect,
            64.0 * size[1] * frame.border[1],
        ];
        let mut width = f64::from(p.advance);
        let mut height = f64::from(p.ascender.wrapping_add(p.descender));
        let orig = [p.scale[0] * p.scale_fix, p.scale[1] * p.scale_fix];
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

/// How a glyph's border is stroked: the scale its outline is stroked at and
/// the border widths at that scale. The border of a shape under equal strokes
/// is the same outline, so a renderer keys the stroked outline by the shape
/// and this, as libass's outline cache keys a border.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BorderStroke {
    /// The outline is scaled by 2^order (x and y) before stroking.
    scale_order: [i32; 2],
    /// Border widths in 1/16 outline units at that scale.
    size: Vector,
}

impl BorderStroke {
    /// The stroke of a border `size` pixels wide and the matrix of the
    /// stroked outline, from the glyph's centred matrix `m` and its outline
    /// matrix: the outline is scaled by powers of two so the stroker's fixed
    /// tolerance stays near 1/8 pixel after the transform. `None` when the
    /// border is out of range.
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
        let stroke = Self {
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
        Some((stroke, matrix))
    }

    /// Whether the border rounds to nothing, so the fill stands in for it.
    fn is_empty(&self) -> bool {
        self.size == Vector::default()
    }
}

/// The state of one run of glyphs composited together: the sub-pixel origin
/// its first drawn glyph sets, how many glyphs it has drawn, and the least
/// transformed x of its karaoke glyphs.
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

/// Where a glyph's border bitmap comes from once the glyph is placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlacedBorder {
    /// No border: none is drawn, or it leaves the coordinate range.
    None,
    /// The border rounds to nothing, and the fill's bitmap stands in for it.
    Fill,
    /// The opaque box, which does not depend on the glyph's shape: the
    /// [`Shape::unit_box`] under this transform.
    Box(Quantized),
    /// The glyph's first outline stroked by [`Raster::stroke_border`], then
    /// placed by [`GlyphPlacement::place_stroke`].
    Stroke(BorderStroke),
}

/// A glyph quantised and placed in its run by [`Run::place`]: what its fill
/// and border bitmaps are drawn from and under which transforms, before any
/// is drawn.
#[derive(Clone, Copy, Debug)]
pub struct GlyphPlacement {
    fill: Quantized,
    border: PlacedBorder,
    /// The stroked border's matrix, for [`PlacedBorder::Stroke`].
    stroke_matrix: Matrix,
    /// The run's sub-pixel origin, which the border is placed against.
    offset: RunOffset,
}

impl GlyphPlacement {
    /// The glyph's fill: its shape under this transform, drawn by
    /// [`Raster::draw`].
    pub fn fill(&self) -> Quantized {
        self.fill
    }

    /// The glyph's border.
    pub fn border(&self) -> PlacedBorder {
        self.border
    }

    /// The border of [`PlacedBorder::Stroke`] placed in the run, `stroked`
    /// being the glyph's outline as [`Raster::stroke_border`] strokes it.
    /// `None` when its transform leaves the coordinate range; the glyph then
    /// has no border, as in libass.
    pub fn place_stroke(&self, stroked: &Shape) -> Option<Quantized> {
        let quantized =
            BitmapTransform::quantize(&self.stroke_matrix, stroked.cbox(), Some(self.offset))?;
        Some(quantized.placed)
    }
}

impl Run {
    /// Places one glyph of the run as libass's `get_bitmap_glyph` does
    /// (`ass_render.c:1383-1542` in 0.17.5), without drawing it: a `\kf` glyph moves the run's karaoke leftmost x,
    /// its transform is quantised against the run's sub-pixel origin (a glyph
    /// that comes before any drawn one sets that origin), and its border is
    /// placed against the same origin. `None` when the glyph's transform is
    /// degenerate or out of range: nothing of it is drawn, as in libass.
    ///
    /// Draw its bitmaps, then [`add`](Run::add) them before placing the next
    /// glyph, which sets the run's origin again if this one draws nothing.
    pub fn place(&mut self, request: &GlyphRequest<'_>) -> Option<GlyphPlacement> {
        let GlyphRequest {
            shape,
            placement,
            frame,
            effects,
            karaoke,
        } = request;
        let transform = GlyphTransform::new(placement, frame);
        if *karaoke {
            self.leftmost_x =
                shape.outlines()[0].min_transformed_x(transform.matrix(), self.leftmost_x);
        }
        let starts = self.drawn == 0;
        let continued = (!starts).then_some(self.offset);
        let quantized = BitmapTransform::quantize(transform.matrix(), shape.cbox(), continued)?;
        if starts {
            self.offset = quantized.remainder;
        }
        let mut out = GlyphPlacement {
            fill: quantized.placed,
            border: PlacedBorder::None,
            stroke_matrix: Matrix::IDENTITY,
            offset: self.offset,
        };
        match effects.border {
            Border::None => {}
            Border::OpaqueBox { size, shown } => {
                if shown || effects.casts_shadow() {
                    let m = transform.opaque_box(placement, frame, size);
                    if let Some(q) = BitmapTransform::quantize(&m, &UNIT_BOX, Some(self.offset)) {
                        out.border = PlacedBorder::Box(q.placed);
                    }
                }
            }
            Border::Outline { size, .. } => {
                let stroke = BorderStroke::new(
                    &quantized.centred,
                    transform.matrix(),
                    shape.cbox(),
                    placement,
                    frame,
                    size,
                );
                if let Some((stroke, matrix)) = stroke {
                    out.border = if stroke.is_empty() {
                        PlacedBorder::Fill
                    } else {
                        out.stroke_matrix = matrix;
                        PlacedBorder::Stroke(stroke)
                    };
                }
            }
        }
        Some(out)
    }

    /// Adds a placed glyph's drawn bitmaps to the run: its fill, and its
    /// border (the fill's own bitmap for [`PlacedBorder::Fill`]). `None`
    /// when neither draws anything: libass takes a bitmap without pixels as
    /// none (`ass_render.c:1412-1414`, `1536-1541`), and
    /// `render_and_combine_glyphs` leaves a glyph without bitmaps out of the
    /// run (`2548-2549`), so the next glyph sets the run's sub-pixel origin.
    pub fn add(
        &mut self,
        fill: Option<PlacedBitmap>,
        border: Option<PlacedBitmap>,
    ) -> Option<GlyphBitmaps> {
        if fill.is_none() && border.is_none() {
            return None;
        }
        self.drawn += 1;
        Some(GlyphBitmaps { fill, border })
    }

    /// The least x, 26.6, of the transformed outlines of the run's karaoke
    /// glyphs; [`OUTLINE_MAX`] without any.
    pub fn leftmost_x(&self) -> i32 {
        self.leftmost_x
    }
}

/// The control box of [`Shape::unit_box`], which every opaque box is drawn
/// from.
const UNIT_BOX: Rect = Rect::new(0, 0, 64, 64);

/// A bitmap placed in the frame: its [`position`](Bitmap::position) counts
/// from `origin`. The bitmap may be shared with a renderer's caches.
#[derive(Clone, Debug)]
pub struct PlacedBitmap {
    /// The bitmap.
    pub bitmap: Arc<Bitmap>,
    /// The whole-pixel frame position the bitmap's position counts from.
    pub origin: Vector,
}

/// The bitmaps of one glyph of a run: a fill, a border, or both.
#[derive(Clone, Debug)]
pub struct GlyphBitmaps {
    fill: Option<PlacedBitmap>,
    border: Option<PlacedBitmap>,
}

impl GlyphBitmaps {
    /// The glyph's fill, `None` when it draws nothing.
    pub fn fill(&self) -> Option<&PlacedBitmap> {
        self.fill.as_ref()
    }

    /// The border: the stroked outline or the opaque box, or the fill's
    /// bitmap when the border rounds to nothing. `None` without a border.
    pub fn border(&self) -> Option<&PlacedBitmap> {
        self.border.as_ref()
    }

    /// The glyph's place in its run: the fill's origin, or the border's
    /// without a fill (libass's `pos`).
    pub fn origin(&self) -> Vector {
        match (&self.fill, &self.border) {
            (Some(placed), _) | (None, Some(placed)) => placed.origin,
            (None, None) => Vector::default(),
        }
    }

    /// The border's origin, or the fill's without a border (libass's
    /// `pos_o`).
    pub fn border_origin(&self) -> Vector {
        self.border
            .as_ref()
            .map_or_else(|| self.origin(), |b| b.origin)
    }
}

#[cfg(test)]
impl GlyphBitmaps {
    /// A glyph bitmap without a border, at `origin`.
    pub(crate) fn of_fill(bitmap: Bitmap, origin: Vector) -> Self {
        Self {
            fill: Some(PlacedBitmap {
                bitmap: Arc::new(bitmap),
                origin,
            }),
            border: None,
        }
    }
}

/// Where [`Raster::render_with`] gets a placed glyph's bitmaps: drawn on the
/// spot, as [`Raster::render`] draws them, or kept by a renderer between
/// frames. Each is `None` when it draws nothing.
pub trait GlyphSource {
    /// The fill: the glyph's `shape` under `at`.
    ///
    /// # Errors
    ///
    /// Those of [`Raster::draw`], and the source's own.
    fn fill(
        &mut self,
        raster: &mut Raster,
        shape: &Shape,
        at: Quantized,
        work: &mut WorkBudget,
    ) -> Result<Option<PlacedBitmap>>;

    /// The opaque box: [`Shape::unit_box`] under `at`
    /// ([`Raster::draw_box`]).
    ///
    /// # Errors
    ///
    /// Those of [`Raster::draw`], and the source's own.
    fn opaque_box(
        &mut self,
        raster: &mut Raster,
        at: Quantized,
        work: &mut WorkBudget,
    ) -> Result<Option<PlacedBitmap>>;

    /// The border: `shape` stroked by `stroke` ([`Raster::stroke_border`]),
    /// placed by `placement` once stroked ([`GlyphPlacement::place_stroke`]).
    ///
    /// # Errors
    ///
    /// Those of [`Raster::stroke_border`] and [`Raster::draw`], and the
    /// source's own.
    fn border(
        &mut self,
        raster: &mut Raster,
        shape: &Shape,
        stroke: BorderStroke,
        placement: &GlyphPlacement,
        work: &mut WorkBudget,
    ) -> Result<Option<PlacedBitmap>>;
}

/// The bitmaps of a glyph drawn as they are asked for.
struct Drawing;

impl Drawing {
    fn placed(bitmap: Option<Bitmap>, at: Quantized) -> Option<PlacedBitmap> {
        bitmap.map(|bitmap| PlacedBitmap {
            bitmap: Arc::new(bitmap),
            origin: at.position,
        })
    }
}

impl GlyphSource for Drawing {
    fn fill(
        &mut self,
        raster: &mut Raster,
        shape: &Shape,
        at: Quantized,
        work: &mut WorkBudget,
    ) -> Result<Option<PlacedBitmap>> {
        Ok(Self::placed(raster.draw(shape, &at.transform, work)?, at))
    }

    fn opaque_box(
        &mut self,
        raster: &mut Raster,
        at: Quantized,
        work: &mut WorkBudget,
    ) -> Result<Option<PlacedBitmap>> {
        Ok(Self::placed(raster.draw_box(&at.transform, work)?, at))
    }

    fn border(
        &mut self,
        raster: &mut Raster,
        shape: &Shape,
        stroke: BorderStroke,
        placement: &GlyphPlacement,
        work: &mut WorkBudget,
    ) -> Result<Option<PlacedBitmap>> {
        let Some(stroked) = raster.stroke_border(shape, &stroke, work)? else {
            return Ok(None);
        };
        let Some(at) = placement.place_stroke(&stroked) else {
            return Ok(None);
        };
        Ok(Self::placed(
            raster.draw(&stroked, &at.transform, work)?,
            at,
        ))
    }
}

impl Raster {
    /// Draws one glyph of `run` as libass does: places it
    /// ([`Run::place`]), draws its fill and its border outline or opaque box
    /// ([`Raster::draw`], [`Raster::stroke_border`]), and adds them to the
    /// run ([`Run::add`]). A glyph or border that leaves the coordinate
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
        self.render_with(request, run, &mut Drawing, work)
    }

    /// [`Raster::render`] with the bitmaps from `source`: a renderer that
    /// keeps the bitmaps it draws hands out those it has.
    ///
    /// # Errors
    ///
    /// Those of [`Raster::render`] and of `source`.
    pub fn render_with(
        &mut self,
        request: &GlyphRequest<'_>,
        run: &mut Run,
        source: &mut impl GlyphSource,
        work: &mut WorkBudget,
    ) -> Result<Option<GlyphBitmaps>> {
        let Some(placement) = run.place(request) else {
            return Ok(None);
        };
        let shape = request.shape;
        let fill = source.fill(self, shape, placement.fill(), work)?;
        let border = match placement.border() {
            PlacedBorder::None => None,
            PlacedBorder::Fill => fill.clone(),
            PlacedBorder::Box(at) => source.opaque_box(self, at, work)?,
            PlacedBorder::Stroke(stroke) => source.border(self, shape, stroke, &placement, work)?,
        };
        Ok(run.add(fill, border))
    }

    /// The bitmap of `shape` under the quantised `transform`, placed at the
    /// transform's position: a glyph's fill, its stroked border, or a
    /// drawing quantised on its own. An outline that leaves the coordinate
    /// range under the transform is dropped, as libass drops it. `None` when
    /// nothing is drawn.
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

    /// The bitmap of an opaque box: [`Shape::unit_box`] under the quantised
    /// `transform` of [`PlacedBorder::Box`], drawn from the one unit square
    /// the raster keeps; see [`Raster::draw`].
    ///
    /// # Errors
    ///
    /// Those of [`Raster::draw`].
    pub fn draw_box(
        &mut self,
        transform: &BitmapTransform,
        work: &mut WorkBudget,
    ) -> Result<Option<Bitmap>> {
        let unit_box = match self.unit_box.take() {
            Some(unit_box) => unit_box,
            None => Shape::unit_box(&self.budget)?,
        };
        let drawn = self.draw(&unit_box, transform, work);
        self.unit_box = Some(unit_box);
        drawn
    }

    /// The border of `glyph` for [`PlacedBorder::Stroke`]: its first outline
    /// scaled and stroked, as libass builds a border outline. `None` when it
    /// cannot be stroked within the coordinate range; the glyph then has no
    /// border.
    ///
    /// # Errors
    ///
    /// `Policy` when `work` or the limits run out, and the budget's errors.
    pub fn stroke_border(
        &mut self,
        glyph: &Shape,
        stroke: &BorderStroke,
        work: &mut WorkBudget,
    ) -> Result<Option<Shape>> {
        let source = &glyph.outlines()[0];
        let mut border = Shape::new(&self.budget);
        if stroke.is_empty() || source.is_empty() {
            return Ok(Some(border));
        }
        let pen = Pen {
            width: Vector::new(
                stroke.size.x.wrapping_mul(STROKER_PRECISION),
                stroke.size.y.wrapping_mul(STROKER_PRECISION),
            ),
            tolerance: STROKER_PRECISION,
        };
        let scaled = &mut self.transformed;
        let points = self.limits.outline_points;
        let stroked = scaled
            .rebuild(|[first, _]| source.scale_pow2_into(first, stroke.scale_order))
            .and_then(|()| stroke_outline(&scaled.outlines()[0], &pen, points, &mut border, work));
        match stroked {
            Ok(()) => Ok(Some(border)),
            Err(Error::Invalid(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use triergon_core::{Budget, WorkBudget};

    use super::{FrameScale, GlyphRequest, PlacedBorder, Placement, Run};
    use crate::{Border, Effects, Limits, Outline, Raster, Shape, Vector};

    fn placement(x: i32) -> Placement {
        Placement {
            rotation: [0.0; 3],
            shear: [0.0; 2],
            scale: [1.0, 1.0],
            shift: Vector::new(0, 0),
            ascender: 640,
            descender: 128,
            advance: 640,
            scale_fix: 1.0,
            position: Vector::new(x, 50 * 64),
            outline_scale: [1.0, 1.0],
            outline_offset: [0.0, 0.0],
        }
    }

    const FRAME: FrameScale = FrameScale {
        blur: [1.0, 1.0],
        border: [1.0, 1.0],
        pixel_aspect: 1.0,
    };

    fn square(budget: &Budget) -> Shape {
        let mut square = Outline::new(budget);
        square
            .push_rect(Vector::new(0, -640), Vector::new(640, 0))
            .unwrap();
        Shape::from_outlines(square, Outline::new(budget))
    }

    #[test]
    fn opaque_boxes_are_placed_by_the_unit_box() {
        let budget = Budget::new(1 << 20).unwrap();
        assert_eq!(*Shape::unit_box(&budget).unwrap().cbox(), super::UNIT_BOX);
    }

    #[test]
    fn an_opaque_box_is_placed_without_the_shape() {
        let budget = Budget::new(16 << 20).unwrap();
        let effects = Effects {
            border: Border::OpaqueBox {
                size: [2.0, 2.0],
                shown: true,
            },
            ..Effects::default()
        };
        let border = |shape: &Shape| {
            let request = GlyphRequest {
                shape,
                placement: placement(100 * 64),
                frame: FRAME,
                effects: &effects,
                karaoke: false,
            };
            Run::default().place(&request).unwrap().border()
        };
        let boxed = border(&Shape::new(&budget));
        assert!(matches!(boxed, PlacedBorder::Box(_)));
        assert_eq!(boxed, border(&square(&budget)));
    }

    #[test]
    fn a_glyph_that_draws_nothing_leaves_the_run_to_the_next() {
        let budget = Budget::new(16 << 20).unwrap();
        let mut raster = Raster::new(&budget, Limits::default());
        let mut work = WorkBudget::new(1 << 24);
        let effects = Effects::default();
        let (empty, square) = (Shape::new(&budget), square(&budget));
        let request = |shape, x| GlyphRequest {
            shape,
            placement: placement(x),
            frame: FRAME,
            effects: &effects,
            karaoke: false,
        };
        let mut run = Run::default();
        let nothing = raster.render(&request(&empty, 100 * 64 + 13), &mut run, &mut work);
        assert!(nothing.unwrap().is_none());
        let after_nothing = run.place(&request(&square, 200 * 64 + 5)).unwrap();
        let first = Run::default()
            .place(&request(&square, 200 * 64 + 5))
            .unwrap();
        assert_eq!(after_nothing.fill(), first.fill());
    }
}
