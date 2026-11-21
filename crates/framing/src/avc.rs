//! H.264: `nal_unit_type` values, the `avcC` configuration record, the parameter
//! sets and slice headers of picture output, and length-prefixed samples as
//! Annex B access units.
//!
//! | Need | Entry points |
//! |---|---|
//! | `avcC` record | [`AvcConfigParser`], [`AvcConfiguration`] |
//! | One sample as an Annex B access unit | [`AvcAnnexBConfig`], [`AvcAnnexBJob`] |
//! | SPS, PPS and slice header fields | [`read_sps`], [`read_pps`], [`read_slice_header`], [`slice_pps_id`] |
//!
//! The constants are the `nal_unit_type` values of H.264 Table 7-1 that callers
//! act on.

mod access_unit;
pub(crate) mod config;
pub(crate) mod picture;

pub use access_unit::{AvcAnnexBAccessUnit, AvcAnnexBConfig, AvcAnnexBJob, AvcAnnexBLimits};
pub use config::{
    AvcConfigArray, AvcConfigArrayKind, AvcConfigFields, AvcConfigLimits, AvcConfigNal,
    AvcConfigParser, AvcConfigSource, AvcConfiguration,
};
pub use picture::{
    AvcPocSyntax, AvcPps, AvcReading, AvcSliceDepth, AvcSliceHeader, AvcSps, AvcVui, read_pps,
    read_slice_header, read_sps, slice_pps_id,
};

pub const SLICE: u8 = 1;
pub const IDR: u8 = 5;
pub const SEI: u8 = 6;
pub const SPS: u8 = 7;
pub const PPS: u8 = 8;
pub const AUD: u8 = 9;
