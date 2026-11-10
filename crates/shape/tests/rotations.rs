//! The font functions of an `@` font's face keep libass's rotation decisions:
//! the first lookup of a glyph decides, for its face only, and a rotated
//! glyph advances by its default-load vertical advance.

mod support;

use std::cell::RefCell;
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_font::{Font, GlyphId, Limits};
use triergon_shape::{Extents, FaceRotations, GlyphRotations, Metrics, SizedMetrics};

fn vertical_font() -> (Budget, Font) {
    let budget = Budget::new(16 << 20).expect("budget");
    let data = Bytes::copy_from_slice(&budget, &support::vertical_font()).expect("bytes");
    let font = Font::new(data, 0, &budget, Limits::default()).expect("font");
    (budget, font)
}

#[test]
fn a_glyph_first_found_through_a_rotating_code_point_advances_vertically() {
    let (budget, font) = vertical_font();
    let sized = font.at_size(16384).expect("size");
    let (across, down) = (GlyphId(28), GlyphId(1));
    let horizontal = sized.hinted_advance(across).expect("advance");
    let vertical = sized
        .hinted_vertical_advance(across)
        .expect("vertical advance");
    assert_ne!(horizontal, vertical);
    let rotations = RefCell::new(GlyphRotations::new(&budget));
    let (regular, bold) = (
        FaceRotations::new(&rotations, 0_u8),
        FaceRotations::new(&rotations, 1),
    );
    let mut work = WorkBudget::new(1 << 20);
    let metrics = SizedMetrics::new(sized, &mut work)
        .expect("metrics")
        .rotating(&regular);
    // U+0301 maps glyph 28 and rotates it; 'a' maps glyph 1, which stays.
    assert_eq!(metrics.nominal_glyph('\u{301}'), Some(across));
    assert_eq!(metrics.h_advance(across), Ok(vertical));
    assert_eq!(metrics.nominal_glyph('a'), Some(down));
    assert_eq!(
        metrics.h_advance(down),
        Ok(sized.hinted_advance(down).expect("advance"))
    );
    // Extents decide too, and keep a glyph horizontal.
    assert_eq!(metrics.extents(GlyphId(5)), Ok(Some(Extents::default())));
    assert_eq!(metrics.finish(), Ok(()));
    // Another face decides on its own: an advance first keeps glyph 28
    // horizontal there, whatever finds it later.
    let mut work = WorkBudget::new(1 << 20);
    let metrics = SizedMetrics::new(sized, &mut work)
        .expect("metrics")
        .rotating(&bold);
    assert_eq!(metrics.h_advance(across), Ok(horizontal));
    assert_eq!(metrics.nominal_glyph('\u{301}'), Some(across));
    assert_eq!(metrics.h_advance(across), Ok(horizontal));
    let kept = rotations.borrow();
    assert_eq!(
        [(0, 28), (0, 1), (0, 5), (1, 28)].map(|(face, g)| kept.get(face, GlyphId(g))),
        [Some(true), Some(false), Some(false), Some(false)]
    );
}

#[test]
fn glyphs_without_points_have_empty_extents_and_others_need_hinting() {
    let (_budget, font) = vertical_font();
    let sized = font.at_size(16384).expect("size");
    let mut work = WorkBudget::new(1 << 20);
    let metrics = SizedMetrics::new(sized, &mut work).expect("metrics");
    // Every glyph of the test font is empty; a glyph outside the face has no
    // extents.
    assert_eq!(metrics.extents(GlyphId(3)), Ok(Some(Extents::default())));
    assert_eq!(metrics.extents(GlyphId(30)), Ok(None));
    assert!(matches!(
        metrics.contour_point(GlyphId(3), 0),
        Err(Error::Unsupported(_))
    ));
}
