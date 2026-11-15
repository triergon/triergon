//! The playback measurement runners. Each binary carries only the runner families
//! the reference build it is measured against can demux ([`runner`]):
//! `playback-worker` every runner family, `playback-matroska-worker` the Matroska
//! runners alone. Both link every backend of the playback engine.
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
