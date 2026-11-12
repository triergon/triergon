//! The canvas sizes the reference accepts for a bitmap subtitle stream.
//!
//! Neither the DVD subpicture format nor the PGS presentation composition
//! bounds the display area; the reference refuses the same sizes for both, a
//! DVD `size:` line (its decoder does not open) and a PGS presentation's video
//! size (the composition is ignored). Observed on both: 1 × 1, 65,535 × 1,
//! 1 × 65,535, 16,255 × 16,257 and 16,257 × 16,255 are accepted; 0 × 480,
//! 720 × 0, 16,256 × 16,256, 3,961 × 65,535 and 65,535 × 65,535 are refused. On
//! DVD, whose sizes go beyond 16 bits, 2,080,767 × 1 is accepted and
//! 2,080,768 × 1 and negative sizes are refused.

/// The bound an accepted canvas's `(width + 128) × (height + 128)` stays below.
const AREA_BOUND: u64 = 1 << 28;

/// Whether the reference accepts a `width` × `height` canvas: both sides at
/// least one, and `(width + 128) × (height + 128)` below 2²⁸. That product is
/// 2²⁸ − 1 for 2,080,767 × 1 and 16,255 × 16,257, which are accepted, and
/// above it for 2,080,768 × 1 and 16,256 × 16,256, which are not.
pub(crate) fn accepts(width: u32, height: u32) -> bool {
    let padded = |side: u32| u64::from(side) + 128;
    width != 0
        && height != 0
        && padded(width)
            .checked_mul(padded(height))
            .is_some_and(|area| area < AREA_BOUND)
}

#[cfg(test)]
mod tests {
    use super::accepts;

    /// The boundary the reference shows at several aspect ratios: the widest
    /// canvas it accepts at each height, and one column more.
    #[test]
    fn the_boundary_follows_the_padded_area() {
        for (height, widest) in [
            (1, 2_080_767),
            (2, 2_064_760),
            (100, 1_177_220),
            (480, 441_377),
            (1080, 222_086),
            (4096, 63_422),
            (16_384, 16_128),
            (32_767, 8_032),
            (65_535, 3_960),
            (1_000_000, 140),
        ] {
            assert!(accepts(widest, height), "{widest}x{height}");
            assert!(!accepts(widest + 1, height), "{}x{height}", widest + 1);
        }
        assert!(accepts(16_255, 16_257));
        assert!(accepts(16_257, 16_255));
        assert!(!accepts(16_256, 16_256));
        assert!(!accepts(1, 268_435_455));
    }

    #[test]
    fn a_side_of_zero_is_refused() {
        assert!(accepts(1, 1));
        assert!(accepts(720, 480));
        assert!(accepts(65_535, 1));
        assert!(!accepts(0, 480));
        assert!(!accepts(720, 0));
        assert!(!accepts(0, 0));
        assert!(!accepts(u32::MAX, u32::MAX));
    }
}
