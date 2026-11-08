//! Mutated and random inputs must produce errors or empty results, never
//! panics or unbounded work. Run in the dev profile, arithmetic overflow is
//! checked too.

mod support;

use support::{Rng, Source, exercise};
use triergon_raster::Limits;

fn limits() -> Limits {
    Limits {
        work: 1 << 20,
        outline_points: 1 << 16,
        bitmap_bytes: 1 << 22,
    }
}

#[test]
fn mutated_outlines_never_panic() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    for _ in 0..40 {
        let mut source = Source::random(&mut rng);
        for _ in 0..8 {
            exercise(&source, &mut rng, limits());
            source.mutate(&mut rng);
        }
    }
}

#[test]
fn a_square_survives_every_operation() {
    use triergon_raster::{PointKind, SourcePoint};
    let corner = |x, y| SourcePoint {
        x,
        y,
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
        exercise(&source, &mut rng, limits());
    }
}
