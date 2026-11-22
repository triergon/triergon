//! What the DTS stream readers share: the checks each of them makes.
use triergon_core::{Error, SourceFailure};

/// The error a reader keeps after its source reported `failure`.
pub(super) const fn source_error(failure: SourceFailure) -> Error {
    match failure {
        SourceFailure::Transport => Error::Transport,
        SourceFailure::Cancelled => Error::Cancelled,
    }
}
