//! The playback measurement runners. Each binary carries the container scope of the
//! reference build it is measured against ([`runner`]): `playback-worker` every
//! runner family, `playback-matroska-worker` the Matroska runners alone.
#![forbid(unsafe_code)]
mod hls;
mod matroska;
mod matroska_audio;
mod matroska_video;
mod mp4_forward;
mod mp4_range;
mod packet_fold;
mod progressive;
mod raw_video;
pub mod runner;
mod ts_program;
