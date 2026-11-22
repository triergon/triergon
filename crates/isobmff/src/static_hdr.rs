//! On-demand interpretation of visual sample-entry static HDR boxes.
use crate::{BoxView, fields};

/// Encoded mastering-display values from an `mdcv` box.
///
/// Chromaticities are in increments of 1/50,000 and luminances in units of
/// 1/10,000 cd/m². Values are retained even when outside their nominal ranges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MasteringDisplayColourVolume {
    /// Chromaticity pairs in encoded order; no RGB mapping is assumed.
    pub display_primaries: [[u16; 2]; 3],
    /// White-point chromaticity pair.
    pub white_point: [u16; 2],
    /// Nominal maximum mastering-display luminance.
    pub max_display_mastering_luminance: u32,
    /// Nominal minimum mastering-display luminance.
    pub min_display_mastering_luminance: u32,
}

/// Encoded content light levels from a `clli` box, in cd/m².
/// Zero indicates that the corresponding upper bound is not specified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentLightLevel {
    /// Upper bound on the maximum content light level.
    pub max_content_light_level: u16,
    /// Upper bound on the maximum picture-average light level.
    pub max_pic_average_light_level: u16,
}

impl BoxView {
    /// Interpret a complete `mdcv` box, or return `None` for another kind or
    /// a truncated payload. Additional trailing bytes remain available in the box.
    pub fn mastering_display_colour_volume(&self) -> Option<MasteringDisplayColourVolume> {
        if self.header().kind != *b"mdcv" {
            return None;
        }
        let b = self.payload();
        Some(MasteringDisplayColourVolume {
            display_primaries: [
                [fields::u16_at(b, 0).ok()?, fields::u16_at(b, 2).ok()?],
                [fields::u16_at(b, 4).ok()?, fields::u16_at(b, 6).ok()?],
                [fields::u16_at(b, 8).ok()?, fields::u16_at(b, 10).ok()?],
            ],
            white_point: [fields::u16_at(b, 12).ok()?, fields::u16_at(b, 14).ok()?],
            max_display_mastering_luminance: fields::u32_at(b, 16).ok()?,
            min_display_mastering_luminance: fields::u32_at(b, 20).ok()?,
        })
    }

    /// Interpret a complete `clli` box, or return `None` for another kind or
    /// a truncated payload. Additional trailing bytes remain available in the box.
    pub fn content_light_level(&self) -> Option<ContentLightLevel> {
        if self.header().kind != *b"clli" {
            return None;
        }
        let b = self.payload();
        Some(ContentLightLevel {
            max_content_light_level: fields::u16_at(b, 0).ok()?,
            max_pic_average_light_level: fields::u16_at(b, 2).ok()?,
        })
    }
}
