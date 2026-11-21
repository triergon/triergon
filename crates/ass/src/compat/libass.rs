//! The renderer settings a player gives libass through its setters, as
//! [`Settings`](crate::Settings) holds them: the cache limits
//! `ass_set_cache_limits` sets ([`cache_limits`]).
//!
//! Reference: libass 0.17.5.

use crate::CacheLimits;

/// The share of the megabytes that composites get: one part in
/// `COMPOSITE_RATIO + 1`.
const COMPOSITE_RATIO: usize = 2;

/// The limits `ass_set_cache_limits(glyphs, megabytes)` sets: `glyphs`
/// outlines, and `megabytes` MiB split two thirds to glyph bitmaps and one
/// third to composites. libass reads 0 as its default for either value; here
/// 0 keeps nothing beyond what the last frame holds. Font descriptions keep
/// their bound, [`CacheLimits::FONTS`].
pub const fn cache_limits(glyphs: usize, megabytes: usize) -> CacheLimits {
    let total = megabytes.saturating_mul(1 << 20);
    let composite_bytes = total / (COMPOSITE_RATIO + 1);
    CacheLimits {
        outlines: glyphs,
        bitmap_bytes: total - composite_bytes,
        composite_bytes,
        fonts: CacheLimits::FONTS,
    }
}

#[cfg(test)]
mod tests {
    use super::cache_limits;
    use crate::CacheLimits;

    #[test]
    fn cache_limits_split_as_libass_splits_them() {
        let default = CacheLimits::default();
        assert_eq!(
            (
                default.outlines,
                default.bitmap_bytes,
                default.composite_bytes
            ),
            (10_000, 128 << 20, 64 << 20)
        );
        let player = cache_limits(1000, 24);
        assert_eq!(
            (player.outlines, player.bitmap_bytes, player.composite_bytes),
            (1000, 16 << 20, 8 << 20)
        );
    }
}
