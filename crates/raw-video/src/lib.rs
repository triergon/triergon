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

use std::sync::atomic::AtomicBool;
use triergon_core::{Error, Result, SourceFailure, SourceLocation, Step, WorkBudget};
use triergon_framing::nal::{RawAccessUnit, RawAnnexBReader};
use triergon_playback::{Source, SourceReply, io::SourceOwner};

mod config;
mod feed;
mod probe;
/// The HEVC parameter-set chain a feed starts from.
pub mod hevc_startup {
    pub use super::config::hevc::{
        StartupChain, StartupNal, parameter_set_payload, startup_chain, startup_nal,
    };
}
pub use config::{AvcVuiTiming, HevcVuiTiming, RawVideoConfigTracker, RawVideoConfiguration};
pub use feed::{RawFeedClock, RawVideoFeed, RawVideoFeedPacket};
pub use probe::{
    PlaybackInput, PlaybackInputKind, PlaybackProbe, PlaybackProbeStep, ProbeReplay, RawVideoProbe,
    RawVideoProbeLimits, RawVideoProbeStep,
};

#[allow(
    clippy::large_enum_variant,
    reason = "one inline raw parser avoids an uncharged allocation for the active source"
)]
#[derive(Debug)]
enum State<S: Source> {
    Active {
        reader: RawAnnexBReader,
        source: SourceOwner<S>,
    },
    End(SourceLocation),
    Failed(Error),
}

/// Owns one explicitly identified raw video source. Container timestamps are
/// absent. The caller may provide a presentation clock at the sink boundary.
#[derive(Debug)]
pub struct RawVideoSource<S: Source> {
    state: State<S>,
}

impl<S: Source> RawVideoSource<S> {
    /// Reads `source` through `reader`.
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
            State::Failed(error) => return Err(*error),
        };
        match &result {
            Ok(Step::End(at)) => self.state = State::End(*at),
            Err(error) if !matches!(error, Error::Backpressure { .. } | Error::Allocation) => {
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
                    SourceFailure::Transport => Error::Transport,
                    SourceFailure::Cancelled => Error::Cancelled,
                });
            }
        }
    }
}
