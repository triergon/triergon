//! H.264: `nal_unit_type` values, the `avcC` configuration record, the parameter
//! sets and slice headers of picture output, and length-prefixed samples as
//! Annex B access units.
//!
//! | Need | Entry points |
//! |---|---|
//! | `avcC` record | [`AvcConfigParser`], [`AvcConfiguration`] |
//! | One sample as an Annex B access unit | [`AvcAnnexBConfig`], [`AvcAnnexBJob`] |
//! | SPS, PPS and slice header fields | [`read_sps`], [`read_pps`], [`read_slice_header`], [`slice_pps_id`] |
//! | Identifiers, display raster, VUI timing and reorder limit | [`read_sps_raster`], [`read_sps_timing`], [`read_sps_reorder`], [`read_pps_ids`], [`read_slice_prefix`] |
//!
//! The constants are the `nal_unit_type` values of H.264 Table 7-1 that callers
//! act on.

mod access_unit;
pub(crate) mod config;
pub(crate) mod picture;
mod prefix;

pub use access_unit::{AvcAnnexBAccessUnit, AvcAnnexBConfig, AvcAnnexBJob, AvcAnnexBLimits};
pub use config::{
    AvcConfigArray, AvcConfigArrayKind, AvcConfigFields, AvcConfigLimits, AvcConfigNal,
    AvcConfigParser, AvcConfigSource, AvcConfiguration,
};
pub use picture::{
    AvcPocSyntax, AvcPps, AvcReading, AvcSliceDepth, AvcSliceHeader, AvcSps, AvcVui, read_pps,
    read_slice_header, read_sps, slice_pps_id,
};
pub use prefix::{
    AvcRaster, AvcVuiTiming, read_pps_ids, read_slice_prefix, read_sps_raster, read_sps_reorder,
    read_sps_timing,
};

/// Coded slice of a non-IDR picture.
pub const SLICE: u8 = 1;
/// Coded slice of an IDR picture.
pub const IDR: u8 = 5;
/// Supplemental enhancement information.
pub const SEI: u8 = 6;
/// Sequence parameter set.
pub const SPS: u8 = 7;
/// Picture parameter set.
pub const PPS: u8 = 8;
/// Access unit delimiter.
pub const AUD: u8 = 9;
