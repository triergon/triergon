//! What the DTS stream readers share: the checks each of them makes.
use triergon_core::{Error, Result, SourceFailure};

/// The error a reader keeps after its source reported `failure`.
pub(super) const fn source_error(failure: SourceFailure) -> Error {
    match failure {
        SourceFailure::Transport => Error::Transport,
        SourceFailure::Cancelled => Error::Cancelled,
    }
}

/// [`Error::Limit`] when `requested` exceeds `maximum`.
pub(super) fn limit(requested: usize, maximum: usize) -> Result<()> {
    if requested > maximum {
        Err(Error::Limit { requested, maximum })
    } else {
        Ok(())
    }
}
