//! What the DTS stream readers share: their input, and the checks each of them
//! makes.
use triergon_core::{Error, InputCursor, InputRequest, Result, SourceFailure, SourceId};

/// A reader's input: an [`InputCursor`] until the reader fails, then the request it
/// had open and the error it returns from then on.
pub(super) enum ReaderInput {
    Open(InputCursor),
    Failed { request: InputRequest, error: Error },
}

impl ReaderInput {
    /// The open request; once failed, the request that was open then.
    pub(super) fn request(&self) -> InputRequest {
        match self {
            Self::Open(cursor) => cursor.request(),
            Self::Failed { request, .. } => request.clone(),
        }
    }
    /// Source offset of the next byte the cursor yields.
    pub(super) fn position(&self) -> u64 {
        match self {
            Self::Open(cursor) => cursor.position(),
            Self::Failed { request, .. } => request.offset,
        }
    }
    /// Generation of the cursor's token.
    pub(super) fn generation(&self) -> u64 {
        match self {
            Self::Open(cursor) => cursor.generation(),
            Self::Failed { request, .. } => request.token.generation(),
        }
    }
    /// The source the reader reads.
    pub(super) fn source(&self) -> SourceId {
        match self {
            Self::Open(cursor) => cursor.source(),
            Self::Failed { request, .. } => request.source,
        }
    }
    /// The error that ended the reader, if any.
    pub(super) fn failure(&self) -> Option<Error> {
        match self {
            Self::Open(_) => None,
            Self::Failed { error, .. } => Some(*error),
        }
    }
    /// The cursor, or the error that ended the reader.
    pub(super) fn cursor(&mut self) -> Result<&mut InputCursor> {
        match self {
            Self::Open(cursor) => Ok(cursor),
            Self::Failed { error, .. } => Err(*error),
        }
    }
    /// Ends the input with `error`, keeping the request that is open.
    pub(super) fn fail_with(&mut self, error: Error) {
        let request = self.request();
        *self = Self::Failed { request, error };
    }
}

/// The error a reader keeps after its source reported `failure`.
pub(super) const fn source_error(failure: SourceFailure) -> Error {
    match failure {
        SourceFailure::Transport => Error::Transport,
        SourceFailure::Cancelled => Error::Cancelled,
    }
}

/// Whether a reader may be stepped again after `error`: the pool refused storage
/// it can grant once the caller frees some.
pub(super) const fn retryable(error: Error) -> bool {
    matches!(error, Error::Backpressure { .. } | Error::Allocation)
}

/// [`Error::Limit`] when `requested` exceeds `maximum`.
pub(super) fn limit(requested: usize, maximum: usize) -> Result<()> {
    if requested > maximum {
        Err(Error::Limit { requested, maximum })
    } else {
        Ok(())
    }
}
