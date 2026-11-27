//! PES `stream_id` values (H.222.0 Table 2-22).
use std::ops::RangeInclusive;

/// Program stream map.
pub const PROGRAM_STREAM_MAP: u8 = 0xbc;
/// Private stream 1: AC-3, DTS, DVB subtitles and other private data with a PES header.
pub const PRIVATE_STREAM_1: u8 = 0xbd;
/// Padding stream.
pub const PADDING_STREAM: u8 = 0xbe;
/// Private stream 2: private data with no PES header fields after the length.
pub const PRIVATE_STREAM_2: u8 = 0xbf;
/// MPEG audio streams 0 to 31 (ISO/IEC 11172-3, 13818-3, 13818-7, 14496-3).
pub const AUDIO: RangeInclusive<u8> = AUDIO_FIRST..=AUDIO_LAST;
/// The first MPEG audio stream ID.
pub const AUDIO_FIRST: u8 = 0xc0;
/// The last MPEG audio stream ID.
pub const AUDIO_LAST: u8 = 0xdf;
/// MPEG video streams 0 to 15 (H.262, ISO/IEC 11172-2, 14496-2, H.264, H.265).
pub const VIDEO: RangeInclusive<u8> = VIDEO_FIRST..=VIDEO_LAST;
/// The first MPEG video stream ID.
pub const VIDEO_FIRST: u8 = 0xe0;
/// The last MPEG video stream ID.
pub const VIDEO_LAST: u8 = 0xef;
/// ECM stream (conditional access).
pub const ECM_STREAM: u8 = 0xf0;
/// EMM stream (conditional access).
pub const EMM_STREAM: u8 = 0xf1;
/// DSM-CC stream (ISO/IEC 13818-6).
pub const DSMCC_STREAM: u8 = 0xf2;
/// H.222.1 type E stream.
pub const H222_1_TYPE_E: u8 = 0xf8;
/// Program stream directory.
pub const PROGRAM_STREAM_DIRECTORY: u8 = 0xff;
