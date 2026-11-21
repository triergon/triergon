#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]

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

use triergon_core::{BudgetVec, Bytes, Error, Result};

/// Whether a job may be stepped again after `error`: the pool refused storage it
/// can grant once the caller frees some.
#[inline]
pub(crate) const fn retryable(error: Error) -> bool {
    matches!(error, Error::Backpressure { .. } | Error::Allocation)
}

/// Freezes `output`, leaving an empty vector on its pool; a refused freeze leaves the
/// charged bytes in `output` for retry.
pub(crate) fn freeze_whole(output: &mut BudgetVec<u8>) -> Result<Bytes> {
    let empty = BudgetVec::new(output.budget());
    match std::mem::replace(output, empty).freeze() {
        Ok(bytes) => Ok(bytes),
        Err(refused) => {
            *output = refused.value;
            Err(refused.error)
        }
    }
}

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

impl ConfigProgress {
    /// The [`BuildStep`] of a parser that published into `ready`.
    pub(crate) fn into_build_step<T>(self, ready: Option<T>) -> Result<BuildStep<T>> {
        match self {
            Self::Ready => ready
                .map(BuildStep::Ready)
                .ok_or(Error::Invalid("missing published configuration")),
            Self::Yield => Ok(BuildStep::Yield),
            Self::Complete => Ok(BuildStep::Complete),
        }
    }
}
