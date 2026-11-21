#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]
// Pending: `missing_docs`, `missing_debug_implementations`, `unreachable_pub`.

pub mod aac;
pub mod ac3;
pub mod av1;
pub mod avc;
pub mod compat;
pub mod dts;
pub mod flac;
pub mod hevc;
pub mod mpeg_audio;
pub mod nal;
pub mod opus;
pub mod picture_output;
#[cfg(feature = "reference-trace")]
pub mod reference_trace;
pub mod vorbis;
pub mod vp9;

mod canonical;
mod inline_storage;
mod record_input;

/// One step of a work-bounded job that requests no input.
///
/// `Ready` occurs exactly once; later calls return `Complete` unless the job was
/// cancelled or failed.
#[derive(Debug)]
pub enum BuildStep<T> {
    /// The work budget is spent; call again with more.
    Yield,
    /// The job's output.
    Ready(T),
    /// The output was already returned.
    Complete,
}

/// Progress of a configuration parser publishing into a caller-provided slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigProgress {
    /// A complete configuration was placed in the previously empty slot.
    Ready,
    /// More work is needed; the slot remains empty.
    Yield,
    /// The configuration was already published; the slot remains empty.
    Complete,
}
