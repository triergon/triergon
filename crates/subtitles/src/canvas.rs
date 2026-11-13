//! The display sizes the reference accepts for a subtitle canvas.

/// Whether the reference accepts a canvas of `width` × `height` pixels.
///
/// Neither the DVD subpicture nor the PGS specification bounds the display
/// size. FFmpeg accepts the same sizes through both paths (a VobSub `.idx`
/// `size:` line and a PGS presentation composition's video size): both
/// dimensions at least 1 and (`width` + 128) × (`height` + 128) below 2²⁸.
/// So 2,080,767 × 1, 63,422 × 4,096 and 16,255 × 16,257 are accepted, and
/// 2,080,768 × 1, 63,423 × 4,096, 16,256 × 16,256 and any size with a zero
/// dimension are not. The boundary comes from bisecting the largest accepted
/// width and height against 37 other dimensions from 1 to 65,535 on both
/// paths, and 6 more up to 2³¹ − 1 on the DVD path; no other offsets in
/// `(width + a)(height + a) < k` with `a` up to 4,096 fit those points.
pub(crate) fn accepts(width: u32, height: u32) -> bool {
    const AREA: u64 = 1 << 28;
    let (width, height) = (u64::from(width), u64::from(height));
    // Each side below 2²⁸ first, so the product cannot overflow.
    width > 0
        && height > 0
        && width < AREA
        && height < AREA
        && (width + 128) * (height + 128) < AREA
}

#[cfg(test)]
mod tests {
    use super::accepts;

    /// Largest accepted and first refused sizes bisected from the reference,
    /// as `(width, height, accepted)`; the probes found the same boundary
    /// with the dimensions swapped.
    const OBSERVED: [(u32, u32, bool); 22] = [
        (2_080_767, 1, true),
        (2_080_768, 1, false),
        (1_397_973, 64, true),
        (1_397_974, 64, false),
        (441_377, 480, true),
        (441_378, 480, false),
        (222_086, 1080, true),
        (222_087, 1080, false),
        (63_422, 4096, true),
        (63_423, 4096, false),
        (16_255, 16_257, true),
        (16_256, 16_256, false),
        (16_129, 16_383, true),
        (16_130, 16_383, false),
        (3960, 65_535, true),
        (3961, 65_535, false),
        (140, 1_000_000, true),
        (141, 1_000_000, false),
        (0, 480, false),
        (720, 0, false),
        (0, 0, false),
        (2_147_483_647, 2_147_483_647, false),
    ];

    #[test]
    fn matches_the_sizes_the_reference_takes() {
        for (width, height, accepted) in OBSERVED {
            assert_eq!(accepts(width, height), accepted, "{width} x {height}");
            assert_eq!(accepts(height, width), accepted, "{height} x {width}");
        }
    }

    #[test]
    fn takes_common_sizes_and_refuses_the_largest() {
        assert!(accepts(1, 1));
        assert!(accepts(720, 480));
        assert!(accepts(65_535, 2048));
        assert!(!accepts(65_535, 65_535));
        assert!(!accepts(u32::MAX, u32::MAX));
        assert!(!accepts(u32::MAX, 1));
    }
}
