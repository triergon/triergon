//! Descriptor tags: H.222.0 Table 2-45 and EN 300 468 Table 12. Tags from 0x40 are user
//! private in H.222.0; the DVB values below mean what they say only in a DVB namespace.

/// Registration descriptor: a 32-bit format identifier (H.222.0 2.6.8).
pub const REGISTRATION: u8 = 0x05;
/// ISO 639 language descriptor (H.222.0 2.6.18).
pub const ISO_639_LANGUAGE: u8 = 0x0a;
/// AVC video descriptor (H.222.0 2.6.64).
pub const AVC_VIDEO: u8 = 0x28;
/// HEVC video descriptor (H.222.0 2.6.95).
pub const HEVC_VIDEO: u8 = 0x38;
/// DVB teletext descriptor (EN 300 468 6.2.43).
pub const DVB_TELETEXT: u8 = 0x56;
/// DVB subtitling descriptor (EN 300 468 6.2.41).
pub const DVB_SUBTITLING: u8 = 0x59;
/// DVB AC-3 descriptor (EN 300 468 Annex D).
pub const DVB_AC3: u8 = 0x6a;
/// DVB enhanced AC-3 descriptor (EN 300 468 Annex D).
pub const DVB_EAC3: u8 = 0x7a;
/// DVB DTS descriptor (EN 300 468 Annex G.2).
pub const DVB_DTS: u8 = 0x7b;
/// DVB extension descriptor; its first body byte is a `descriptor_tag_extension`
/// (EN 300 468 6.2.16).
pub const DVB_EXTENSION: u8 = 0x7f;
/// The `descriptor_tag_extension` of the DVB DTS-HD descriptor (EN 300 468 Annex G.3).
pub const DVB_DTS_HD_EXTENSION: u8 = 0x0e;
/// ATSC AC-3 audio descriptor (A/52 Annex A).
pub const ATSC_AC3: u8 = 0x81;
/// ATSC E-AC-3 audio descriptor (A/52 Annex G).
pub const ATSC_EAC3: u8 = 0xcc;
