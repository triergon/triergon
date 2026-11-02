//! Primary-source colour facts and an explicitly named integer projection.
//! EN300743 7.2.4/10.1-10.3; BT.601-7 studio coding. No reference-code port.
//! General native equality is unqualified; source facts remain independent.
use super::Depth;
use safe_media_core::{Bytes, Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColourSource {
    Default {
        depth: Depth,
        index: u8,
    },
    /// Encoded values, not already shifted/normalized. Reduced fields are 6/4/4/2.
    YCrCbT {
        y: u8,
        cr: u8,
        cb: u8,
        transparency: u8,
        full_resolution: bool,
    },
}

#[derive(Clone, Debug)]
pub struct PaletteEntry {
    pub source: ColourSource,
    pub original_segment: Option<Bytes>,
}

pub const SOURCE_PROJECTION: &str = "BT601-source-nearest8-straight-alpha-v1";
pub const STUDIO_UNREPAIRED_PROJECTION: &str = "BT601-unrepaired-RGB-nearest-alpha-floor8-v1";

/// Explicit output policies, not interchangeable source facts or an assertion
/// that a particular native decoder uses these exact coefficients everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColourProjection {
    SourceNearest8,
    StudioUnrepaired8,
}

impl ColourProjection {
    pub fn identifier(self) -> &'static str {
        match self {
            Self::SourceNearest8 => SOURCE_PROJECTION,
            Self::StudioUnrepaired8 => STUDIO_UNREPAIRED_PROJECTION,
        }
    }
}

fn nearest(numerator: i64, denominator: i64) -> u8 {
    if numerator <= 0 {
        return 0;
    }
    if numerator >= denominator * 255 {
        return 255;
    }
    ((numerator + denominator / 2) / denominator) as u8
}

fn studio_rgb(y: u8, cr: u8, cb: u8, full_resolution: bool) -> [u8; 3] {
    let (y, cr, cb) = if full_resolution {
        (y, cr, cb)
    } else {
        (y << 2, cr << 4, cb << 4)
    };
    let yy = i64::from(y) - 16;
    let rr = i64::from(cr) - 128;
    let bb = i64::from(cb) - 128;
    // Exact decimal BT.601 coefficients and 219/224 studio ranges;
    // round once after the complete inverse transform, then clip.
    [
        nearest(255 * (yy * 224_000 + rr * 219 * 1402), 219 * 224_000),
        nearest(
            255 * (yy * 224 * 587_000 - bb * 219 * 202_008 - rr * 219 * 419_198),
            219 * 224 * 587_000,
        ),
        nearest(255 * (yy * 224_000 + bb * 219 * 1772), 219 * 224_000),
    ]
}

impl ColourSource {
    /// Retains the source-nearest view, adding a separate mathematical view that
    /// applies the studio inverse to encoded Y=0 without an RGB-zero convention
    /// or the informative EN300743 Y=16 repair, and floors 8-bit opacity.
    /// Observed native [0,136,0,0] and T=128 alpha 127 are consistent with this
    /// view; that observation does not establish general native equivalence.
    pub fn rgba(self, projection: ColourProjection) -> Result<[u8; 4]> {
        if projection == ColourProjection::SourceNearest8 {
            return self.rgba_source_nearest();
        }
        let (opaque, denominator) = self.opacity()?;
        let alpha = u8::try_from(u32::from(opaque) * 255 / u32::from(denominator))
            .map_err(|_| Error::Overflow)?;
        let rgb = match self {
            Self::Default { .. } => {
                let value = self.rgba_source_nearest()?;
                [value[0], value[1], value[2]]
            }
            Self::YCrCbT {
                y,
                cr,
                cb,
                full_resolution,
                ..
            } => studio_rgb(y, cr, cb, full_resolution),
        };
        Ok([rgb[0], rgb[1], rgb[2], alpha])
    }

    /// Exact source opacity fraction. No 255-T approximation is substituted.
    pub fn opacity(self) -> Result<(u16, u16)> {
        match self {
            Self::Default { depth, index } => {
                if index > depth.maximum() {
                    return Err(Error::Invalid("DVB default palette index"));
                }
                Ok(match (depth, index) {
                    (_, 0) => (0, 1),
                    (Depth::Eight, 1..=7) => (1, 4),
                    (Depth::Eight, i) if i & 0x88 == 0x08 => (1, 2),
                    _ => (1, 1),
                })
            }
            Self::YCrCbT {
                y,
                cr,
                cb,
                transparency,
                full_resolution,
            } => {
                if !full_resolution && (y > 63 || cr > 15 || cb > 15 || transparency > 3) {
                    return Err(Error::Invalid("DVB reduced palette component"));
                }
                if y == 0 {
                    return Ok((0, 1));
                }
                if full_resolution {
                    Ok((256 - u16::from(transparency), 256))
                } else {
                    Ok((4 - u16::from(transparency), 4))
                }
            }
        }
    }

    /// Source-nearest projection only, never advertised as a native normalizer.
    /// Original components/default identity remain available alongside it.
    pub fn rgba_source_nearest(self) -> Result<[u8; 4]> {
        let (opaque, denominator) = self.opacity()?;
        let alpha = nearest(i64::from(opaque) * 255, i64::from(denominator));
        let rgb = match self {
            Self::Default { depth, index } => {
                let bit = |n: u8| i64::from((index >> n) & 1);
                match depth {
                    Depth::Two => match index {
                        0 | 2 => [0, 0, 0],
                        1 => [255, 255, 255],
                        _ => [128, 128, 128],
                    },
                    Depth::Four => {
                        let strength = if index & 8 == 0 { 1000 } else { 500 };
                        [
                            nearest(255 * strength * bit(0), 1000),
                            nearest(255 * strength * bit(1), 1000),
                            nearest(255 * strength * bit(2), 1000),
                        ]
                    }
                    Depth::Eight => {
                        let component = |low, high| match index & 0x88 {
                            0 if index < 8 => nearest(255 * bit(low), 1),
                            0 | 8 => nearest(255 * (333 * bit(low) + 667 * bit(high)), 1000),
                            0x80 => nearest(255 * (500 + 167 * bit(low) + 333 * bit(high)), 1000),
                            _ => nearest(255 * (167 * bit(low) + 333 * bit(high)), 1000),
                        };
                        [component(0, 4), component(1, 5), component(2, 6)]
                    }
                }
            }
            Self::YCrCbT { y: 0, .. } => [0, 0, 0],
            Self::YCrCbT {
                y,
                cr,
                cb,
                full_resolution,
                ..
            } => studio_rgb(y, cr, cb, full_resolution),
        };
        Ok([rgb[0], rgb[1], rgb[2], alpha])
    }
}

pub(crate) fn palette_size(depth: Depth) -> usize {
    match depth {
        Depth::Two => 4,
        Depth::Four => 16,
        Depth::Eight => 256,
    }
}

pub(crate) fn palette_offset(depth: Depth) -> usize {
    match depth {
        Depth::Two => 0,
        Depth::Four => 4,
        Depth::Eight => 20,
    }
}
