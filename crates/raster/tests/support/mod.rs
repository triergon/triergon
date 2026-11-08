//! Random inputs for every operation: outlines with valid and broken structure
//! and extreme coordinates, transforms, borders, blurs, filters and clips.

use triergon_core::{Budget, WorkBudget};
use triergon_font::{OutlinePoint, Point, PointKind};
use triergon_raster::{
    Bitmap, Border, Clip, ClipFrame, Effects, FrameScale, GlyphRequest, Limits, Matrix, Outline,
    Pen, Placement, Projection, Raster, Rect, Run, RunBorder, Shadow, Shape, Vector,
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
    pub points: Vec<OutlinePoint>,
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
            .map(|_| OutlinePoint {
                point: Point::new(rng.signed(magnitude) as i32, rng.signed(magnitude) as i32),
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
                let point = &mut self.points[at].point;
                point.x ^= 1 << rng.below(31);
                point.y = point.y.wrapping_neg();
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
        if outline.push_glyph(&self.points, &self.ends).is_err() {
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
/// are fine; panics and runaway work are not: every operation shares `work`
/// units, as the stages of one frame do.
pub fn exercise(source: &Source, rng: &mut Rng, limits: Limits, work: u64) {
    let budget = Budget::new(256 << 20).expect("budget");
    let mut work = WorkBudget::new(work);
    let mut raster = Raster::new(&budget, limits);
    let outline = source.outline(&budget);
    let _ = outline.min_transformed_x(&matrix(rng), 0);

    let mut stroked = Shape::new(&budget);
    let pen = Pen {
        width: Vector::new(rng.signed(1 << 12) as i32, rng.signed(1 << 12) as i32),
        tolerance: 1 + rng.below(32) as i32,
    };
    let _ = raster.stroke(&outline, &pen, &mut stroked, &mut work);

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

    let _ = raster.rasterize(&stroked, &mut work);

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
        0 => Border::None,
        1 => Border::Outline { size },
        _ => Border::OpaqueBox {
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
    let mut rendered = Vec::new();
    for _ in 0..2 {
        match raster.render(&request, &mut run, &mut work) {
            Ok(Some(bitmaps)) => rendered.push(bitmaps),
            Ok(None) => {}
            Err(_) => return,
        }
    }
    if let Some(bitmaps) = rendered.first() {
        let _ = (bitmaps.key(), bitmaps.origin());
        if let Some(border) = bitmaps.border() {
            assert!(border.width() > 0 && border.height() > 0);
        }
    }

    let effects = Effects {
        border: match rng.below(3) {
            0 => RunBorder::None,
            1 => RunBorder::Outline {
                keeps_fill: rng.below(2) == 0,
            },
            _ => RunBorder::OpaqueBox {
                shown: rng.below(2) == 0,
            },
        },
        shadow: Shadow {
            offset: [rng.real(), rng.real()],
            keeps_fill: rng.below(2) == 0,
        },
        be: rng.below(12) as u32,
        blur: rng.scale() * 3.0,
    };
    let _ = effects.key(&frame);
    let composite = raster.compose(&effects, &frame, &rendered, &mut work);

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
        let _ = raster.blur(&mut plain, rng.below(8) as u32, variance, &mut work);
        plain.shift_by(Vector::new(rng.signed(200) as i32, rng.signed(200) as i32));
        if let Some(other) = composite.ok().and_then(|c| c.fill) {
            plain.subtract_fill(&other);
            plain.add(&other, rng.signed(100) as i32, rng.signed(100) as i32);
            plain.multiply(&other, rng.signed(100) as i32, rng.signed(100) as i32);
            plain.multiply_inverse(&other, rng.signed(100) as i32, rng.signed(100) as i32);
        }
        let clip = Clip {
            rect: Rect::new(
                rng.signed(1 << 31) as i32,
                rng.signed(400) as i32,
                rng.signed(400) as i32,
                rng.signed(1 << 31) as i32,
            ),
            inverse: rng.below(2) == 0,
        };
        let frame = ClipFrame {
            width: rng.signed(400) as i32,
            height: rng.signed(400) as i32,
            content: Rect::new(0, 0, 400, 400),
        };
        let origin = Vector::new(rng.signed(1 << 31) as i32, rng.signed(400) as i32);
        let parts = clip.parts(&plain, origin, rng.signed(400) as i32, &frame);
        for part in parts {
            assert!(
                part.source_x + part.width <= plain.width()
                    && part.source_y + part.height <= plain.height()
            );
        }
    }
}
