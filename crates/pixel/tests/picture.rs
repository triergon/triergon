use triergon_pixel::{Error, Geometry, Orientation, Picture, rgba_to_yuv420};
#[test]
fn private_geometry_has_checked_dimensions_and_coded_extents() {
    let g = Geometry::new(18, 34).unwrap();
    assert_eq!(
        (
            g.width(),
            g.height(),
            g.macroblock_width(),
            g.macroblock_height()
        ),
        (18, 34, 2, 3)
    );
    assert_eq!(
        (g.luma_stride(), g.chroma_stride(), g.rgba_len()),
        (32, 16, 18 * 34 * 4)
    );
}
#[test]
fn largest_valid_geometry_is_32bit_representable_without_allocation() {
    let g = Geometry::new(4094, 4094).unwrap();
    assert_eq!((g.luma_stride(), g.chroma_stride()), (4096, 2048));
    assert!(g.rgba_len() < u32::MAX as usize);
}
#[test]
fn invalid_geometry_stays_explicit() {
    for (w, h) in [
        (0, 2),
        (2, 0),
        (1, 2),
        (2, 1),
        (4096, 2),
        (2, 4096),
        (u32::MAX, 2),
    ] {
        assert!(matches!(Geometry::new(w, h), Err(Error::Geometry { .. })));
    }
}
#[test]
fn mutable_planes_keep_exact_layout_and_convert_safely() {
    let mut p = Picture::new(Geometry::new(2, 2).unwrap());
    let before = (p.y().len(), p.cb().len(), p.cr().len());
    p.y_mut().fill(19);
    p.cb_mut().fill(128);
    p.cr_mut().fill(128);
    rgba_to_yuv420(&[0; 16], Orientation::TopDown, &mut p).unwrap();
    assert_eq!((p.y().len(), p.cb().len(), p.cr().len()), before);
    assert_eq!(p.geometry(), Geometry::new(2, 2).unwrap());
    assert_eq!(p.visible_yuv420(), [16, 16, 16, 16, 128, 128]);
}
#[test]
fn row_accessors_reject_end_and_overflow_indices() {
    let p = Picture::new(Geometry::new(18, 34).unwrap());
    assert_eq!(p.y_row(33).unwrap().len(), 18);
    assert!(p.y_row(34).is_none());
    assert!(p.y_row(usize::MAX).is_none());
    assert_eq!(p.cb_row(16).unwrap().len(), 9);
    assert!(p.cb_row(17).is_none());
    assert!(p.cr_row(usize::MAX).is_none());
}
