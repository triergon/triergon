//! Typed Annex B input over the same source ownership protocol as playback.

use crate::{
    Error, Result,
    input::{Source, SourceReply},
    io::SourceOwner,
};
use std::sync::atomic::AtomicBool;
use triergon_core::{Error as CoreError, SourceFailure, SourceLocation, Step, WorkBudget};
use triergon_framing::{RawAccessUnit, RawAnnexBReader};

mod config;
mod feed;
mod probe;
pub(crate) use config::hevc as hevc_syntax;
pub use config::{AvcVuiTiming, HevcVuiTiming, RawVideoConfigTracker, RawVideoConfiguration};
pub use feed::{RawFeedClock, RawVideoFeed, RawVideoFeedPacket};
pub use probe::{
    PlaybackInput, PlaybackInputKind, PlaybackProbe, PlaybackProbeStep, ProbeReplay, RawVideoProbe,
    RawVideoProbeLimits, RawVideoProbeStep, RawVideoReplay,
};

#[allow(
    clippy::large_enum_variant,
    reason = "one inline raw parser avoids an uncharged allocation for the active source"
)]
enum State<S: Source> {
    Active {
        reader: RawAnnexBReader,
        source: SourceOwner<S>,
    },
    End(SourceLocation),
    Failed(CoreError),
}

/// Owns one explicitly identified raw video source. Container timestamps are
/// absent. The caller may provide a presentation clock at the sink boundary.
pub struct RawVideoSource<S: Source> {
    state: State<S>,
}

impl<S: Source> RawVideoSource<S> {
    pub fn new(reader: RawAnnexBReader, source: S) -> Self {
        Self {
            state: State::Active {
                reader,
                source: SourceOwner::new(source),
            },
        }
    }

    /// Wait for actual transport EOF before the last packet can be delivered.
    /// A pending or empty read leaves the same request available for retry.
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<RawAccessUnit>> {
        let result = match &mut self.state {
            State::Active { reader, source } => poll_source(reader, source, work, cancel),
            State::End(at) => return Ok(Step::End(*at)),
            State::Failed(error) => return Err((*error).into()),
        };
        match &result {
            Ok(Step::End(at)) => self.state = State::End(*at),
            Err(Error::Core(error))
                if !matches!(
                    error,
                    CoreError::Backpressure { .. } | CoreError::Allocation
                ) =>
            {
                self.state = State::Failed(*error);
            }
            _ => {}
        }
        result
    }
}

fn poll_source<S: Source>(
    reader: &mut RawAnnexBReader,
    source: &mut SourceOwner<S>,
    work: &mut WorkBudget,
    cancel: &AtomicBool,
) -> Result<Step<RawAccessUnit>> {
    loop {
        let request = match reader.step(work, cancel)? {
            Step::NeedInput(request) => request,
            step => return Ok(step),
        };
        match source.poll(&request, work, cancel)? {
            SourceReply::Pending => return Ok(Step::NeedInput(request)),
            SourceReply::Data(window) => {
                reader.provide(&window)?;
                if window.bytes().is_empty() {
                    return Ok(Step::NeedInput(request));
                }
            }
            SourceReply::End { source, token } => reader.finish(source, token)?,
            SourceReply::Failed {
                source,
                token,
                failure,
            } => {
                reader.fail(source, token, failure)?;
                return Err(match failure {
                    SourceFailure::Transport => CoreError::Transport,
                    SourceFailure::Cancelled => CoreError::Cancelled,
                }
                .into());
            }
        }
    }
}
