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
    use triergon_core::{Budget, Error, WorkBudget};
    use triergon_raster::{
        Bitmap, Effects, Filter, FrameScale, GlyphRef, MAX_BITMAP_BYTES, Raster, Vector,
    };
    let budget = Budget::new(1 << 20).unwrap();
    for (width, height) in [
        (usize::MAX, 1),
        (usize::MAX - 3, 2),
        (1, usize::MAX),
        (1 << 40, 1 << 40),
    ] {
        let refused = Bitmap::new(&budget, width, height, &limits());
        assert!(
            matches!(refused, Err(Error::Limit { maximum, .. }) if maximum == MAX_BITMAP_BYTES),
            "{width} x {height}"
        );
    }
    // Glyphs placed as far apart as positions reach compose into nothing.
    let glyph = Bitmap::new(&budget, 4, 4, &limits()).unwrap();
    let at = |x| GlyphRef {
        glyph: Some(&glyph),
        border: None,
        position: Vector::new(x, x),
        border_position: Vector::new(x, x),
    };
    let frame = FrameScale {
        blur: [1.0, 1.0],
        border: [1.0, 1.0],
        pixel_aspect: 1.0,
    };
    let filter = Filter::new(&Effects::default(), &frame);
    let composite = Raster::new(&budget, limits())
        .compose(
            &filter,
            &[at(i32::MIN), at(i32::MAX)],
            &mut WorkBudget::new(WORK),
        )
        .unwrap();
    assert!(composite.fill.is_none());
}
