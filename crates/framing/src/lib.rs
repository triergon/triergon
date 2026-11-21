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

use triergon_core::{BudgetVec, Bytes, Error, InputCursor, InputRequest, Result, SourceId};

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

/// A reader's input: an [`InputCursor`] until the reader fails, then the request it
/// had open and the error it returns from then on.
pub(crate) enum ReaderInput {
    Open(InputCursor),
    Failed { request: InputRequest, error: Error },
}

impl ReaderInput {
    /// The open request; once failed, the request that was open then.
    pub(crate) fn request(&self) -> InputRequest {
        match self {
            Self::Open(cursor) => cursor.request(),
            Self::Failed { request, .. } => request.clone(),
        }
    }
    /// Source offset of the next byte the cursor yields.
    pub(crate) fn position(&self) -> u64 {
        match self {
            Self::Open(cursor) => cursor.position(),
            Self::Failed { request, .. } => request.offset,
        }
    }
    /// Generation of the cursor's token.
    pub(crate) fn generation(&self) -> u64 {
        match self {
            Self::Open(cursor) => cursor.generation(),
            Self::Failed { request, .. } => request.token.generation(),
        }
    }
    /// The source the reader reads.
    pub(crate) fn source(&self) -> SourceId {
        match self {
            Self::Open(cursor) => cursor.source(),
            Self::Failed { request, .. } => request.source,
        }
    }
    /// The error that ended the reader, if any.
    pub(crate) fn failure(&self) -> Option<Error> {
        match self {
            Self::Open(_) => None,
            Self::Failed { error, .. } => Some(*error),
        }
    }
    /// The cursor, or the error that ended the reader.
    pub(crate) fn cursor(&mut self) -> Result<&mut InputCursor> {
        match self {
            Self::Open(cursor) => Ok(cursor),
            Self::Failed { error, .. } => Err(*error),
        }
    }
    /// Ends the input with `error`, keeping the request that is open.
    pub(crate) fn fail_with(&mut self, error: Error) {
        let request = self.request();
        *self = Self::Failed { request, error };
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
