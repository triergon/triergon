//! Subtitle receivers over playback packets: text cues ([`text`]), closed
//! captions ([`caption`]), PGS and DVD bitmaps ([`bitmap`]) and the tracks and
//! streams that feed them in Matroska ([`matroska`]), MP4 ([`mp4_dvd`]) and MPEG-TS
//! ([`ts`]). Of the access-unit half they use only the Matroska demux clock.
pub mod bitmap;
pub mod caption;
pub mod matroska;
pub mod mp4_dvd;
pub mod text;
pub mod ts;
