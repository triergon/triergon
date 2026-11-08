//! Random inputs for every operation: outlines with valid and broken structure
//! and extreme coordinates, transforms, borders, blurs, filters and clips.

use triergon_core::Budget;
use triergon_raster::{
    Bitmap, Blur, BorderRequest, Clip, ClipFrame, Compositor, Effects, Filter, FilterFlags,
    FrameScale, GlyphRaster, GlyphRef, GlyphRequest, GlyphSource, Limits, Matrix, Outline, Pen,
    PixelRect, Placement, PointKind, Projection, Rasterizer, Run, Shape, SourcePoint, Vector,
};

/// xorshift64.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn signed(&mut self, magnitude: i64) -> i64 {
        (self.below(2 * magnitude as u64 + 1) as i64) - magnitude
    }

    fn real(&mut self) -> f64 {
        match self.below(12) {
            0 => 0.0,
            1 => f64::NAN,
            2 => f64::INFINITY,
            3 => -1e300,
            4 => 1e-300,
            5 => self.signed(1 << 20) as f64,
            _ => (self.next() >> 11) as f64 / (1u64 << 53) as f64 * 8.0 - 4.0,
        }
    }

    fn scale(&mut self) -> f64 {
        if self.below(4) == 0 {
            self.real()
        } else {
            0.25 + (self.below(1000) as f64) / 400.0
        }
    }
}

/// A glyph source: points and contour ends, valid or not.
pub struct Source {
    pub points: Vec<SourcePoint>,
    pub ends: Vec<u32>,
}

impl Source {
    pub fn random(rng: &mut Rng) -> Self {
        let magnitude = [64, 4096, 1 << 20, (1 << 28) - 1, 1 << 30][rng.below(5) as usize];
        let n = rng.below(48) as usize;
        let kinds = [
            PointKind::On,
            PointKind::On,
            PointKind::Quadratic,
            PointKind::Cubic,
        ];
        let points = (0..n)
            .map(|_| SourcePoint {
                x: rng.signed(magnitude) as i32,
                y: rng.signed(magnitude) as i32,
                kind: kinds[rng.below(4) as usize],
            })
            .collect();
        let mut ends = Vec::new();
        let mut end = 0;
        while end < n as u32 && ends.len() < 6 {
            end = (end + 1 + rng.below(16) as u32).min(n as u32 + u32::from(rng.below(8) == 0));
            ends.push(end);
        }
        if rng.below(10) == 0 {
            ends.reverse();
        }
        Self { points, ends }
    }

    /// One of four edits: tag flips, coordinate bursts, a contour end moved, or
    /// points removed.
    pub fn mutate(&mut self, rng: &mut Rng) {
        let n = self.points.len();
        match rng.below(4) {
            0 if n > 0 => {
                let at = rng.below(n as u64) as usize;
                self.points[at].kind =
                    [PointKind::On, PointKind::Quadratic, PointKind::Cubic][rng.below(3) as usize];
            }
            1 if n > 0 => {
                let at = rng.below(n as u64) as usize;
                self.points[at].x ^= 1 << rng.below(31);
                self.points[at].y = self.points[at].y.wrapping_neg();
            }
            2 if !self.ends.is_empty() => {
                let at = rng.below(self.ends.len() as u64) as usize;
                self.ends[at] = self.ends[at]
                    .wrapping_add(rng.below(5) as u32)
                    .wrapping_sub(2);
            }
            _ if n > 0 => {
                let keep = rng.below(n as u64) as usize;
                self.points.truncate(keep);
            }
            _ => {}
        }
    }

    pub fn outline(&self, budget: &Budget) -> Outline {
        let mut outline = Outline::new(budget);
        let glyph = GlyphSource {
            points: &self.points,
            contour_ends: &self.ends,
        };
        if outline.push_glyph(&glyph).is_err() {
            assert!(
                outline.is_empty(),
                "a rejected glyph leaves the outline unchanged"
            );
        }
        outline
    }
}

fn matrix(rng: &mut Rng) -> Matrix {
    let mut m = Matrix::IDENTITY;
    for row in &mut m.0 {
        for v in row.iter_mut() {
            if rng.below(3) == 0 {
                *v = rng.real();
            }
        }
    }
    m
}

/// Runs every operation on `source` with parameters drawn from `rng`. Errors
/// are fine; panics and runaway work are not.
pub fn exercise(source: &Source, rng: &mut Rng, limits: Limits) {
    let budget = Budget::new(256 << 20).expect("budget");
    let outline = source.outline(&budget);
    let _ = outline.control_box();
    let _ = outline.min_transformed_x(&matrix(rng), 0);

    let mut stroked = Shape::new(&budget);
    let pen = Pen {
        width: Vector::new(rng.signed(1 << 12) as i32, rng.signed(1 << 12) as i32),
        tolerance: 1 + rng.below(32) as i32,
    };
    let _ = outline.stroke_into(&pen, &limits, &mut stroked);

    let mut moved = Outline::new(&budget);
    let m = matrix(rng);
    let _ = outline.transform_into(&m, Projection::Affine, &mut moved);
    let _ = outline.transform_into(&m, Projection::Perspective, &mut moved);
    let mut rotated = Outline::new(&budget);
    if rotated.copy_from(&outline).is_ok() {
        let _ = rotated.rotate_90(Vector::new(
            rng.signed(1 << 29) as i32,
            rng.signed(1 << 29) as i32,
        ));
    }

    let mut rasterizer = Rasterizer::new(&budget, limits);
    let bitmap = rasterizer.rasterize(&stroked).ok().flatten();

    let mut glyphs = GlyphRaster::new(&budget, limits);
    let shape = Shape::from_outlines(outline, Outline::new(&budget));
    let placement = Placement {
        rotation: [rng.real(), rng.real(), rng.real()],
        shear: [rng.real(), rng.real()],
        scale: [rng.scale(), rng.scale()],
        shift: Vector::new(rng.signed(1 << 16) as i32, rng.signed(1 << 16) as i32),
        ascender: rng.signed(1 << 12) as i32,
        descender: rng.signed(1 << 12) as i32,
        position: Vector::new(rng.signed(1 << 17) as i32, rng.signed(1 << 17) as i32),
        outline_scale: [rng.scale(), rng.scale()],
        outline_offset: [rng.real(), rng.real()],
    };
    let frame = FrameScale {
        blur: [rng.scale(), rng.scale()],
        border: [rng.scale(), rng.scale()],
        pixel_aspect: rng.scale(),
    };
    let size = [rng.scale() * 4.0, rng.scale() * 4.0];
    let border = match rng.below(3) {
        0 => BorderRequest::None,
        1 => BorderRequest::Outline { size },
        _ => BorderRequest::OpaqueBox {
            size,
            spacing: rng.signed(256) as i32,
            advance: rng.signed(1 << 12) as i32,
            scale_fix: rng.scale(),
        },
    };
    let request = GlyphRequest {
        shape: &shape,
        placement,
        frame,
        border,
        karaoke: rng.below(2) == 0,
    };
    let mut run = Run::default();
    let Ok(rendered) = glyphs.render(&request, &mut run) else {
        return;
    };

    let flags = FilterFlags {
        opaque_box: rng.below(2) == 0,
        border: rng.below(2) == 0,
        shadow: rng.below(2) == 0,
        fill_in_shadow: rng.below(2) == 0,
        fill_in_border: rng.below(2) == 0,
    };
    let effects = Effects {
        flags,
        be: rng.below(12) as u32,
        blur: rng.scale() * 3.0,
        shadow: [rng.real(), rng.real()],
    };
    let filter = Filter::new(&effects, &frame);
    let glyph_ref = GlyphRef {
        glyph: rendered.glyph.as_ref(),
        border: rendered.border_bitmap().or(bitmap.as_ref()),
        position: Vector::new(rng.signed(64) as i32, rng.signed(64) as i32),
        border_position: Vector::new(rng.signed(64) as i32, rng.signed(64) as i32),
    };
    let mut compositor = Compositor::new(&budget, limits);
    let composite = compositor.compose(&filter, &[glyph_ref, glyph_ref]);

    let mut blur = Blur::new(&budget, limits);
    if let Ok(mut plain) = Bitmap::new(
        &budget,
        rng.below(80) as usize,
        rng.below(80) as usize,
        &limits,
    ) {
        for row in plain.rows_mut() {
            for v in row {
                *v = rng.next() as u8;
            }
        }
        let variance = [rng.scale().abs(), rng.scale().abs()];
        let _ = blur.apply(&mut plain, rng.below(8) as u32, variance);
        plain.shift_by(Vector::new(rng.signed(200) as i32, rng.signed(200) as i32));
        if let Some(other) = composite.ok().and_then(|c| c.fill) {
            plain.subtract_fill(&other);
            plain.add(&other, rng.signed(100) as i32, rng.signed(100) as i32);
            plain.multiply(&other, rng.signed(100) as i32, rng.signed(100) as i32);
            plain.multiply_inverse(&other, rng.signed(100) as i32, rng.signed(100) as i32);
        }
        let clip = Clip {
            rect: PixelRect {
                left: rng.signed(1 << 31) as i32,
                top: rng.signed(400) as i32,
                right: rng.signed(400) as i32,
                bottom: rng.signed(1 << 31) as i32,
            },
            inverse: rng.below(2) == 0,
        };
        let frame = ClipFrame {
            width: rng.signed(400) as i32,
            height: rng.signed(400) as i32,
            content: PixelRect {
                left: 0,
                top: 0,
                right: 400,
                bottom: 400,
            },
        };
        let origin = Vector::new(rng.signed(1 << 31) as i32, rng.signed(400) as i32);
        let parts = clip.parts(&plain, origin, rng.signed(400) as i32, &frame);
        for part in parts.as_slice() {
            assert!(
                part.source_x + part.width <= plain.width()
                    && part.source_y + part.height <= plain.height()
            );
        }
    }
}
