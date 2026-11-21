//! The stream lifecycle the audio and video lanes share.
use crate::SessionState;
use crate::queue::FeedError;
use triergon_core::Error;

/// Where a loaded lane is in its stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Taking input.
    Open,
    /// After end of stream: no more input, and the decoder is not drained yet.
    Ending,
    /// After end of stream, with the decoder drained.
    Drained,
    /// The decoder refused input with this error; the lane decodes nothing more until it
    /// restarts.
    Failed(Error),
}

impl Phase {
    /// Whether a lane in this phase takes another access unit; `loaded` is whether it has
    /// a decoder.
    pub(crate) fn admit(self, loaded: bool) -> Result<(), FeedError> {
        match self {
            Phase::Open if loaded => Ok(()),
            Phase::Ending | Phase::Drained if loaded => Err(FeedError::Invalid),
            _ => Err(FeedError::Idle),
        }
    }

    /// What a lane in this phase reports; `pending` is whether it still holds input or
    /// output.
    pub(crate) fn state(self, loaded: bool, pending: bool) -> SessionState {
        match self {
            _ if !loaded => SessionState::Idle,
            Phase::Failed(error) => SessionState::Failed(error),
            _ if pending => SessionState::Running,
            Phase::Drained => SessionState::Ended,
            Phase::Open | Phase::Ending => SessionState::Starved,
        }
    }
}
