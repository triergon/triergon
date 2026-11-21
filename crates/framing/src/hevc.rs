//! H.265: `nal_unit_type` values and their classes, the `hvcC` configuration
//! record and its Annex B forms, and length-prefixed samples as Annex B access
//! units.
//!
//! | Need | Entry points |
//! |---|---|
//! | `hvcC` record | [`HvcConfigParser`], [`HvcConfiguration`], [`inspect_config_prefix`] |
//! | One sample as an Annex B access unit | [`HevcAnnexBConfig`], [`HevcAnnexBJob`] |
//! | A record's parameter sets as Annex B | [`HvcAnnexBWriter`], [`HvcDecoderConfigWriter`] |
//!
//! The constants are the `nal_unit_type` values of H.265 Table 7-1; the
//! functions classify a type value.

mod access_unit;
pub(crate) mod config;
mod config_annexb;
mod config_decoder;

pub use access_unit::{HevcAnnexBAccessUnit, HevcAnnexBConfig, HevcAnnexBJob, HevcAnnexBLimits};
pub use config::{
    HvcConfigArray, HvcConfigFields, HvcConfigLimits, HvcConfigNal, HvcConfigParser,
    HvcConfigSource, HvcConfiguration, inspect_config_prefix,
};
pub use config_annexb::{HvcAnnexBOutput, HvcAnnexBWriter};
pub use config_decoder::{HvcDecoderConfigOutput, HvcDecoderConfigRefusal, HvcDecoderConfigWriter};

pub const RADL_N: u8 = 6;
pub const RADL_R: u8 = 7;
pub const RASL_N: u8 = 8;
pub const RASL_R: u8 = 9;
pub const RSV_VCL_N14: u8 = 14;
pub const BLA_W_LP: u8 = 16;
pub const BLA_N_LP: u8 = 18;
pub const IDR_W_RADL: u8 = 19;
pub const IDR_N_LP: u8 = 20;
pub const CRA: u8 = 21;
/// The last type reserved for IRAP pictures.
pub const RSV_IRAP_23: u8 = 23;
pub const VPS: u8 = 32;
pub const SPS: u8 = 33;
pub const PPS: u8 = 34;
pub const AUD: u8 = 35;
pub const EOS: u8 = 36;
pub const EOB: u8 = 37;
pub const PREFIX_SEI: u8 = 39;
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
/// TS, the reference also starts it with an end of sequence (36) or end
/// of bitstream (37), which the specification places last in the access
/// unit they end. Every other non-VCL type stays in the access unit before
/// it: filler data (38), the suffix SEI (40), the reserved types 45 to 47
/// and the unspecified types 56 to 63, as do the reserved VCL types. A slice ([`is_slice`]) starts a picture by its
/// `first_slice_segment_in_pic_flag` instead, and is `false` here.
#[inline]
pub const fn begins_access_unit(t: u8) -> bool {
    matches!(t, VPS..=EOB | PREFIX_SEI | 41..=44 | 48..=55)
}

/// Intra random access point, reserved IRAP types included (7.4.2.2).
#[inline]
pub const fn is_irap(t: u8) -> bool {
    matches!(t, BLA_W_LP..=RSV_IRAP_23)
}

#[inline]
pub const fn is_idr(t: u8) -> bool {
    matches!(t, IDR_W_RADL | IDR_N_LP)
}

#[inline]
pub const fn is_bla(t: u8) -> bool {
    matches!(t, BLA_W_LP..=BLA_N_LP)
}

#[inline]
pub const fn is_rasl(t: u8) -> bool {
    matches!(t, RASL_N | RASL_R)
}

#[inline]
pub const fn is_radl(t: u8) -> bool {
    matches!(t, RADL_N | RADL_R)
}

/// A sub-layer non-reference picture (7.4.2.2): the even types up to 14.
#[inline]
pub const fn is_sub_layer_non_ref(t: u8) -> bool {
    t <= RSV_VCL_N14 && t.is_multiple_of(2)
}
