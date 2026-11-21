//! H.265: `nal_unit_type` values and their classes, the `hvcC` configuration
//! record and its Annex B forms, and length-prefixed samples as Annex B access
//! units.
//!
//! | Need | Entry points |
//! |---|---|
//! | `hvcC` record | [`HvcConfigParser`], [`HvcConfiguration`], [`inspect_config_prefix`] |
//! | One sample as an Annex B access unit | [`HevcAnnexBConfig`], [`HevcAnnexBJob`] |
//! | A record's parameter sets as Annex B | [`HvcAnnexBWriter`], [`HvcDecoderConfigWriter`] |
//! | Identifiers, display raster, timing and reorder limit | [`read_vps_id`], [`read_vps_timing`], [`read_sps_prefix`], [`read_sps_timing`], [`read_sps_reorder`], [`read_pps_ids`], [`read_slice_prefix`], [`check_parameter_sets`] |
//!
//! The constants are the `nal_unit_type` values of H.265 Table 7-1; the
//! functions classify a type value.

mod access_unit;
pub(crate) mod config;
mod config_annexb;
mod config_decoder;
mod prefix;

pub use access_unit::{HevcAnnexBAccessUnit, HevcAnnexBConfig, HevcAnnexBJob, HevcAnnexBLimits};
pub use config::{
    HvcConfigArray, HvcConfigFields, HvcConfigLimits, HvcConfigNal, HvcConfigParser,
    HvcConfigSource, HvcConfiguration, inspect_config_prefix,
};
pub use config_annexb::{HvcAnnexBOutput, HvcAnnexBWriter};
pub use config_decoder::{HvcDecoderConfigOutput, HvcDecoderConfigRefusal, HvcDecoderConfigWriter};
pub use prefix::{
    HevcSpsPrefix, HevcVuiTiming, check_parameter_sets, read_pps_ids, read_slice_prefix,
    read_sps_prefix, read_sps_reorder, read_sps_timing, read_vps_id, read_vps_timing,
};

/// Slice segment of a random access decodable leading picture, sub-layer non-reference.
pub const RADL_N: u8 = 6;
/// Slice segment of a random access decodable leading picture, sub-layer reference.
pub const RADL_R: u8 = 7;
/// Slice segment of a random access skipped leading picture, sub-layer non-reference.
pub const RASL_N: u8 = 8;
/// Slice segment of a random access skipped leading picture, sub-layer reference.
pub const RASL_R: u8 = 9;
/// The last type reserved for non-IRAP sub-layer non-reference pictures.
pub const RSV_VCL_N14: u8 = 14;
/// Slice segment of a broken link access picture that may have leading pictures.
pub const BLA_W_LP: u8 = 16;
/// Slice segment of a broken link access picture without leading pictures.
pub const BLA_N_LP: u8 = 18;
/// Slice segment of an IDR picture that may have decodable leading (RADL) pictures.
pub const IDR_W_RADL: u8 = 19;
/// Slice segment of an IDR picture without leading pictures.
pub const IDR_N_LP: u8 = 20;
/// Slice segment of a clean random access picture.
pub const CRA: u8 = 21;
/// The last type reserved for IRAP pictures.
pub const RSV_IRAP_23: u8 = 23;
/// Video parameter set.
pub const VPS: u8 = 32;
/// Sequence parameter set.
pub const SPS: u8 = 33;
/// Picture parameter set.
pub const PPS: u8 = 34;
/// Access unit delimiter.
pub const AUD: u8 = 35;
/// End of sequence.
pub const EOS: u8 = 36;
/// End of bitstream.
pub const EOB: u8 = 37;
/// Prefix supplemental enhancement information.
pub const PREFIX_SEI: u8 = 39;
/// Suffix supplemental enhancement information.
pub const SUFFIX_SEI: u8 = 40;

/// A slice of a coded picture. The reserved VCL types (10 to 15, 22 to
/// 31) are not: FFmpeg's HEVC parser splits access units on these types
/// only, and its decoder skips the reserved ones.
#[inline]
pub const fn is_slice(t: u8) -> bool {
    matches!(t, 0..=9 | 16..=21)
}

/// Whether a base-layer NAL unit of type `t` after the last VCL NAL unit
/// of a picture starts the next access unit.
///
/// H.265 §7.4.2.4.4 starts it with a VPS, SPS, PPS, access unit
/// delimiter, prefix SEI, a reserved type 41 to 44 or an unspecified type
/// 48 to 55. With each type 0 to 63 inserted after a picture, raw and in
/// TS, FFmpeg also starts it with an end of sequence (36) or end
/// of bitstream (37), which the specification places last in the access
/// unit they end. Every other non-VCL type stays in the access unit before
/// it: filler data (38), the suffix SEI (40), the reserved types 45 to 47
/// and the unspecified types 56 to 63, as do the reserved VCL types. A slice
/// ([`is_slice`]) starts a picture by its `first_slice_segment_in_pic_flag`
/// instead, and is `false` here.
#[inline]
pub const fn begins_access_unit(t: u8) -> bool {
    matches!(t, VPS..=EOB | PREFIX_SEI | 41..=44 | 48..=55)
}

/// Intra random access point, reserved IRAP types included (7.4.2.2).
#[inline]
pub const fn is_irap(t: u8) -> bool {
    matches!(t, BLA_W_LP..=RSV_IRAP_23)
}

/// An instantaneous decoding refresh picture: [`IDR_W_RADL`] or [`IDR_N_LP`].
#[inline]
pub const fn is_idr(t: u8) -> bool {
    matches!(t, IDR_W_RADL | IDR_N_LP)
}

/// A broken link access picture: the types 16 to 18, [`BLA_W_LP`], `BLA_W_RADL`
/// and [`BLA_N_LP`].
#[inline]
pub const fn is_bla(t: u8) -> bool {
    matches!(t, BLA_W_LP..=BLA_N_LP)
}

/// A random access skipped leading picture: [`RASL_N`] or [`RASL_R`].
#[inline]
pub const fn is_rasl(t: u8) -> bool {
    matches!(t, RASL_N | RASL_R)
}

/// A random access decodable leading picture: [`RADL_N`] or [`RADL_R`].
#[inline]
pub const fn is_radl(t: u8) -> bool {
    matches!(t, RADL_N | RADL_R)
}

/// A sub-layer non-reference picture (7.4.2.2): the even types up to 14.
#[inline]
pub const fn is_sub_layer_non_ref(t: u8) -> bool {
    t <= RSV_VCL_N14 && t.is_multiple_of(2)
}
