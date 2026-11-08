//! Mutated and random inputs must produce errors or empty results, never
//! panics or unbounded work. Run in the dev profile, arithmetic overflow is
//! checked too.

mod support;

use support::{Rng, Source, exercise};
use triergon_raster::Limits;

fn limits() -> Limits {
    Limits {
        outline_points: 1 << 16,
        bitmap_bytes: 1 << 22,
    }
}

/// The work units each exercise may spend.
const WORK: u64 = 1 << 20;

#[test]
fn mutated_outlines_never_panic() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    for _ in 0..40 {
        let mut source = Source::random(&mut rng);
        for _ in 0..8 {
            exercise(&source, &mut rng, limits(), WORK);
            source.mutate(&mut rng);
        }
    }
}

#[test]
fn a_square_survives_every_operation() {
    use triergon_font::{OutlinePoint, Point, PointKind};
    let corner = |x, y| OutlinePoint {
        point: Point::new(x, y),
        kind: PointKind::On,
    };
    let source = Source {
        points: vec![
            corner(0, 0),
            corner(640, 0),
            corner(640, 640),
            corner(0, 640),
        ],
        ends: vec![4],
    };
    let mut rng = Rng(99);
    for _ in 0..50 {
        exercise(&source, &mut rng, limits(), WORK);
    }
}

#[test]
fn bitmaps_beyond_libass_s_bound_are_refused() {
    use triergon_core::{Budget, Error};
    use triergon_raster::{Bitmap, MAX_BITMAP_BYTES};
    let budget = Budget::new(1 << 20).unwrap();
    for (width, height) in [
        (usize::MAX, 1),
        (usize::MAX - 3, 2),
        (1, usize::MAX),
        (1 << 20, 1 << 20),
        // A product beyond `usize` on every target.
        (1 << (usize::BITS / 2), 1 << (usize::BITS / 2)),
    ] {
        let refused = Bitmap::new(&budget, width, height, &limits());
        assert!(
            matches!(refused, Err(Error::Limit { maximum, .. }) if maximum == MAX_BITMAP_BYTES),
            "{width} x {height}"
        );
    }
}

/// A 1×1 bitmap blurred with a variance of 5·10⁵ pixels² would need planes of
/// about 6000 × 6000 samples, beyond a 4 MiB bitmap limit: it is refused before
/// anything that size is allocated or worked on.
#[test]
fn a_blur_beyond_the_bitmap_limit_is_refused_before_it_allocates() {
    use triergon_core::{Budget, Error, WorkBudget};
    use triergon_raster::{Bitmap, Raster};
    let budget = Budget::new(256 << 20).unwrap();
    let limits = Limits {
        bitmap_bytes: 4 << 20,
        ..Limits::default()
    };
    let mut raster = Raster::new(&budget, limits);
    let mut bitmap = Bitmap::new(&budget, 1, 1, &limits).unwrap();
    bitmap.rows_mut().for_each(|row| row.fill(255));
    let mut work = WorkBudget::new(1 << 40);
    let refused = raster.blur(&mut bitmap, 0, [5e5, 5e5], &mut work);
    assert_eq!(refused, Err(Error::Policy("bitmap size")));
    assert_eq!(work.spent(), 0);
    assert!(budget.peak() < 1 << 20, "peak {}", budget.peak());
    assert_eq!(
        (bitmap.width(), bitmap.height(), bitmap.row(0)),
        (1, 1, &[255][..])
    );
}

/// Blurs within the limits charge their work in proportion to the samples
/// they write, so a frame's work budget refuses a large blur before it runs.
#[test]
fn blur_work_follows_the_blurred_area() {
    use triergon_core::{Budget, Error, WorkBudget};
    use triergon_raster::{Bitmap, Raster};
    let budget = Budget::new(1 << 30).unwrap();
    let mut raster = Raster::new(&budget, Limits::default());
    let blurred = |raster: &mut Raster, size, be, variance, work: &mut WorkBudget| {
        let mut bitmap = Bitmap::new(&budget, size, size, &Limits::default()).unwrap();
        raster
            .blur(&mut bitmap, be, variance, work)
            .map(|()| bitmap)
    };

    // A Gaussian blur charges at least a unit per 16 samples of its output.
    let mut work = WorkBudget::new(1 << 40);
    let out = blurred(&mut raster, 1, 0, [400.0, 400.0], &mut work).unwrap();
    assert!(out.width() > 100);
    assert!(work.spent() >= (out.width() * out.height() / 16) as u64);

    // The variance of the scenario above, within the default limits, is
    // refused by a frame's work before its planes are allocated.
    let peak = budget.peak();
    let mut work = WorkBudget::new(1 << 24);
    let refused = blurred(&mut raster, 1, 0, [5e5, 5e5], &mut work);
    assert_eq!(refused.err(), Some(Error::Policy("raster work")));
    assert!(budget.peak() < peak + (1 << 20), "peak {}", budget.peak());

    // `\be` charges every pass over the whole area.
    let mut work = WorkBudget::new(1 << 40);
    blurred(&mut raster, 1024, 100, [0.0, 0.0], &mut work).unwrap();
    assert!(work.spent() >= 100 * 1024 * 1024 / 16);
    let mut work = WorkBudget::new(1 << 16);
    let refused = blurred(&mut raster, 1024, 100, [0.0, 0.0], &mut work);
    assert_eq!(refused.err(), Some(Error::Policy("raster work")));
}

/// The rasteriser charges a unit per tile it fills, solid ones included.
#[test]
fn rasteriser_work_follows_the_tiles_filled() {
    use triergon_core::{Budget, Error, WorkBudget};
    use triergon_raster::{Outline, Raster, Shape, Vector};
    let budget = Budget::new(1 << 30).unwrap();
    let mut raster = Raster::new(&budget, Limits::default());
    let mut outline = Outline::new(&budget);
    outline
        .push_rect(Vector::new(0, 0), Vector::new(4000 * 64, 4000 * 64))
        .unwrap();
    let square = Shape::from_outlines(outline, Outline::new(&budget));
    let mut work = WorkBudget::new(1 << 40);
    let bitmap = raster.rasterize(&square, &mut work).unwrap().unwrap();
    let tiles = (bitmap.width() / 16 * (bitmap.height() / 16)) as u64;
    assert!(work.spent() >= tiles, "{} < {tiles}", work.spent());
    let mut work = WorkBudget::new(tiles / 2);
    assert_eq!(
        raster.rasterize(&square, &mut work).err(),
        Some(Error::Policy("raster work"))
    );
}

/// Composition charges the sum of a run's glyphs, a unit per 16 pixels of its
/// rows, before allocating it: a frame's work budget refuses glyphs placed far
/// apart before their sum takes memory.
#[test]
fn composition_work_follows_the_summed_area() {
    use triergon_core::{Budget, Error, WorkBudget};
    use triergon_raster::{
        Effects, FrameScale, GlyphRequest, Outline, Placement, Raster, Run, Shape, Vector,
    };
    let budget = Budget::new(1 << 30).unwrap();
    let mut raster = Raster::new(&budget, Limits::default());
    let mut outline = Outline::new(&budget);
    outline
        .push_rect(Vector::new(0, 0), Vector::new(640, 640))
        .unwrap();
    let square = Shape::from_outlines(outline, Outline::new(&budget));
    let frame = FrameScale {
        blur: [1.0, 1.0],
        border: [1.0, 1.0],
        pixel_aspect: 1.0,
    };
    let effects = Effects::default();
    let mut run = Run::default();
    let mut work = WorkBudget::new(1 << 40);
    let mut glyphs = Vec::new();
    for at in [0, 4000 * 64] {
        let placement = Placement {
            rotation: [0.0; 3],
            shear: [0.0; 2],
            scale: [1.0; 2],
            shift: Vector::new(0, 0),
            ascender: 640,
            descender: 0,
            position: Vector::new(at, at),
            advance: 640,
            scale_fix: 1.0,
            outline_scale: [1.0; 2],
            outline_offset: [0.0; 2],
        };
        let request = GlyphRequest {
            shape: &square,
            placement,
            frame,
            effects: &effects,
            karaoke: false,
        };
        glyphs.extend(raster.render(&request, &mut run, &mut work).unwrap());
    }
    assert_eq!(glyphs.len(), 2);

    // The fill spans the two glyphs, about 4000 × 4000 pixels: about 10⁶
    // units of work and 16 MB. Without the work for it, the sum is refused
    // before it is allocated.
    let peak = budget.peak();
    let mut work = WorkBudget::new(1 << 16);
    let refused = raster.compose(&effects, &frame, &glyphs, &mut work);
    assert_eq!(refused.err(), Some(Error::Policy("raster work")));
    assert!(budget.peak() < peak + (1 << 20), "peak {}", budget.peak());

    let mut work = WorkBudget::new(1 << 40);
    let fill = raster
        .compose(&effects, &frame, &glyphs, &mut work)
        .unwrap()
        .fill
        .unwrap();
    let area = (fill.width().div_ceil(16) * fill.height()) as u64;
    assert!(fill.width() > 4000 && fill.height() > 4000);
    assert!(work.spent() >= area, "{} < {area}", work.spent());
}
