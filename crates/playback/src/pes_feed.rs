//! The PES feed of a single-program TS188 source, which the progressive and
//! segment readers of `triergon-hls` assemble access units from.
//!
//! A feed consumes the validated carriers of its tracks directly and delivers
//! completed PES packets with their demux clock; it delivers only the rare
//! table, clock and fault events of the source whole.

use crate::engine::ts::{Delivery, TsEngine};
use crate::{Engine, Result};
use std::sync::atomic::AtomicBool;
use triergon_core::source::SourceReply;
use triergon_core::{InputCursor, WorkBudget};
use triergon_mpegts::{ParseLimits, clock::SourceTimePolicy, psi::DescriptorNamespace};

pub use crate::engine::ts::feed::{FeedPacket, FeedStep, PesFacts, PesInput, SpentLists};

/// The progressive PES feed of a single-program TS188 source: the PES of the tracks
/// found in its PMT, those of a registered carriage (AVC, HEVC, AAC) as compact
/// payloads, for a reader that polls it itself rather than through a
/// [`Player`](crate::Player).
#[derive(Debug)]
pub struct ProgressiveFeed(pub(crate) TsEngine);

impl ProgressiveFeed {
    /// A feed of the source `input` reads.
    pub fn new(input: InputCursor, limits: ParseLimits) -> Result<Self> {
        TsEngine::progressive(input, limits)
    }

    /// The input cursor the feed reads.
    #[inline]
    pub fn input(&self) -> &InputCursor {
        &self.0.input
    }

    /// Hands a source reply to the input; true when it was accepted.
    #[inline]
    pub fn provide(&mut self, reply: &SourceReply) -> Result<bool> {
        self.0.provide(reply)
    }

    /// The next step of the feed.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<FeedStep> {
        self.0.poll::<FeedStep>(work, cancel)
    }

    /// Source events the feed consumed without delivering since the last call.
    #[inline]
    pub fn take_suppressed_events(&mut self) -> u64 {
        self.0.take_suppressed_events()
    }

    /// Hands a consumed PES's lists back to the observer of `pid` for its next PES.
    #[inline]
    pub fn reuse_pes_lists(&mut self, pid: u16, lists: SpentLists) {
        self.0.reuse_pes_lists(pid, lists);
    }
}

/// Which PMT carriages a TS188 segment feed admits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentProfile {
    /// AVC video and ADTS AAC audio.
    AvcAac,
    /// AVC or HEVC video and ADTS AAC, AC-3 or E-AC-3 audio.
    Media,
}

/// A player engine over one independent TS188 segment with an in-band PAT and
/// PMT, delivering demux PES of the tracks allowed by `profile`.
pub fn open_segment_feed(
    input: InputCursor,
    limits: ParseLimits,
    profile: SegmentProfile,
) -> Result<Engine> {
    TsEngine::segment(
        input,
        limits,
        SourceTimePolicy::Unanchored,
        DescriptorNamespace::Unspecified,
        match profile {
            SegmentProfile::AvcAac => Delivery::AvcAacFeed,
            SegmentProfile::Media => Delivery::MediaFeed,
        },
    )
    .map(Engine::Ts)
}
