//! Values of a PMT entry's `stream_type`: H.222.0 Table 2-34, and the user-private values
//! ATSC A/53 Part 3 §5.7 and the HDMV (Blu-ray) format assign.

/// MPEG-1 video (ISO/IEC 11172-2).
pub const MPEG1_VIDEO: u8 = 0x01;
/// MPEG-2 video (H.262), or MPEG-1 constrained-parameter video.
pub const MPEG2_VIDEO: u8 = 0x02;
/// MPEG-1 audio (ISO/IEC 11172-3).
pub const MPEG1_AUDIO: u8 = 0x03;
/// MPEG-2 audio (ISO/IEC 13818-3).
pub const MPEG2_AUDIO: u8 = 0x04;
/// PES packets containing private data; the descriptors say what they carry.
pub const PES_PRIVATE_DATA: u8 = 0x06;
/// AAC audio with ADTS transport syntax (ISO/IEC 13818-7).
pub const AAC_ADTS: u8 = 0x0f;
/// MPEG-4 visual (ISO/IEC 14496-2).
pub const MPEG4_VISUAL: u8 = 0x10;
/// MPEG-4 audio with LATM transport syntax (ISO/IEC 14496-3).
pub const AAC_LATM: u8 = 0x11;
/// AVC video (H.264).
pub const H264: u8 = 0x1b;
/// An MVC sub-bitstream of AVC video (H.264 Annex H).
pub const H264_MVC: u8 = 0x20;
/// HEVC video (H.265).
pub const HEVC: u8 = 0x24;
/// An HEVC temporal video subset: the sub-layers above those of the base HEVC stream.
pub const HEVC_TEMPORAL_SUBSET: u8 = 0x25;
/// HEVC video with one or more tile substreams.
pub const HEVC_TILE_SUBSTREAMS: u8 = 0x31;
/// VVC video (H.266).
pub const VVC: u8 = 0x33;
/// AVS video (GB/T 20090.2), a user-private value.
pub const CAVS: u8 = 0x42;
/// AC-3 audio in the ATSC namespace (A/53 Part 3); user private elsewhere.
pub const ATSC_AC3: u8 = 0x81;
/// E-AC-3 audio in the ATSC namespace (A/53 Part 3); user private elsewhere.
pub const ATSC_EAC3: u8 = 0x87;
/// HDMV presentation graphics (PGS subtitles), a user-private value.
pub const HDMV_PGS: u8 = 0x90;
/// Dirac video, a user-private value.
pub const DIRAC: u8 = 0xd1;
/// AVS2 video (GB/T 33475.2), a user-private value.
pub const AVS2: u8 = 0xd2;
/// VC-1 video (SMPTE 421M), a user-private value.
pub const VC1: u8 = 0xea;
