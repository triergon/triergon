// The format interface is sealed: its items are public to the trait and unreachable
// outside the crate.
#![allow(unreachable_pub)]
//! What each format supplies to the shared frame-stream engine in [`crate::reader`]:
//! how to read a header, how long its frame is, which headers belong to the same
//! stream, and the format's own opening, timing and seeking facts.
use crate::stream::Limits;
use triergon_core::container::{CodecId, SeekMethod, Trim};
use triergon_core::{Budget, BudgetVec, Bytes, Result};

/// The answer of [`Syntax::probe`] at one position.
#[derive(Clone, Copy, Debug)]
pub enum Probe<H> {
    /// More bytes would decide.
    Short,
    /// No header starts here.
    Invalid,
    Header(H),
}

/// How a frame's end is found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Length {
    /// The header states the length.
    Known(usize),
    /// The frame runs to the next header that [`Syntax::verdict`] accepts, at least
    /// `minimum` bytes on.
    Search { minimum: usize },
}

/// Whether a candidate header ends the frame before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    No,
    /// Consistent, but the frame's own check failed: accepted for a damaged frame.
    Plausible,
    /// The frame's own check passed.
    Confirmed,
}

/// Where bytes between two frames go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gap {
    /// Into the packet of the frame that follows them.
    PrependToNext,
    /// Into the packet of the frame before them.
    AppendToPrevious,
    /// Nowhere.
    Drop,
}

/// What a format's opening wants next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prelude {
    /// At least this many bytes of view.
    Need(usize),
    /// Consume bytes the format has read.
    Skip(u64),
    /// Consume bytes and hand them to [`Syntax::keep`], if the retention limit allows.
    Keep(u64),
    /// Frames follow.
    Done,
}

/// Where the audio is, for planning a seek.
#[derive(Clone, Copy, Debug)]
pub struct SeekContext {
    /// Offset of the first audio frame.
    pub audio_start: u64,
    /// End of the audio when the source length is known.
    pub audio_end: Option<u64>,
}

/// How a seek proceeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Search the byte range by frame timestamps: `low` starts at or before the target
    /// (offset, first sample), `high` ends after it.
    Bisect {
        low: (u64, u64),
        high: u64,
        method: SeekMethod,
    },
    /// Jump to `offset` and take the timestamp the format estimates there.
    Estimate { offset: u64, basis: SeekBasis },
    /// Restart at the first frame and count.
    Scan,
}

/// What a landing timestamp rests on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SeekBasis {
    /// Frame headers or a count of frames from the start: exact.
    Exact,
    /// The Xing table of contents: an estimate.
    Toc,
    /// The stream's average bit rate: an estimate.
    Bitrate,
}

/// Facts a format states about its track when it adopts the first frame.
#[derive(Debug)]
pub struct TrackFacts {
    pub(crate) codec_tag: &'static [u8],
    pub(crate) codec: CodecId<'static>,
    pub(crate) sample_rate: u32,
    pub(crate) channels: Option<u8>,
    pub(crate) configuration: BudgetVec<u8>,
    /// Samples in the stream when the stream states it.
    pub(crate) duration: Option<u64>,
}

pub(crate) mod sealed {
    use super::*;

    /// The per-format part of a reader.
    pub trait Syntax: Sized + std::fmt::Debug {
        type Header: Copy + std::fmt::Debug;
        /// Most bytes [`Syntax::probe`] reads.
        const PROBE: usize;
        /// First byte of every header.
        const SYNC: u8;
        const GAP: Gap;
        /// Frames that must follow a sync candidate before it is trusted.
        const CONFIRM: u8;
        /// Frames delivered before a seek target for decoder warm-up.
        const PREROLL: u32;

        /// A format with no stream adopted; `budget` charges what it retains.
        fn new(budget: &Budget, limits: &Limits) -> Self;
        fn probe(&self, bytes: &[u8]) -> Probe<Self::Header>;
        fn length(&self, header: &Self::Header) -> Length;
        /// Whether a frame with header `next` may follow one with `reference` in the same
        /// stream.
        fn continues(&self, reference: &Self::Header, next: &Self::Header) -> bool;
        /// Whether `next` joins the access unit `unit` starts.
        fn joins(&self, _unit: &Self::Header, _next: &Self::Header) -> bool {
            false
        }
        /// Whether a frame `frame` with header `header` ends where `next` starts (`None`:
        /// the end of the audio).
        fn verdict(
            &self,
            _frame: &[u8],
            _header: &Self::Header,
            _next: Option<&Self::Header>,
        ) -> Verdict {
            Verdict::Confirmed
        }
        /// Samples one access unit starting with `header` decodes to.
        fn samples(&self, header: &Self::Header) -> u32;
        /// The first sample of a frame when its header states it.
        fn first_sample(&self, _header: &Self::Header) -> Option<u64> {
            None
        }

        /// The opening after leading tags.
        fn prelude(&mut self, _view: &[u8], _at_end: bool) -> Result<Prelude> {
            Ok(Prelude::Done)
        }
        /// Bytes a [`Prelude::Keep`] retained.
        fn keep(&mut self, _bytes: Bytes) -> Result<()> {
            Ok(())
        }
        /// A [`Prelude::Keep`] the retention limit refused, by length.
        fn omit(&mut self, _len: u64) -> Result<()> {
            Ok(())
        }
        /// Adopt the first confirmed header as the stream's.
        fn adopt(&mut self, header: &Self::Header, budget: &Budget) -> Result<TrackFacts>;
        /// Length of a first frame worth inspecting with [`Syntax::describes`].
        fn inspect_first(&self, _header: &Self::Header) -> Option<usize> {
            None
        }
        /// Whether the first frame describes the stream instead of carrying audio.
        fn describes(
            &mut self,
            _frame: &[u8],
            _header: &Self::Header,
            _offset: u64,
            _source_len: Option<u64>,
        ) -> bool {
            false
        }
        /// Samples in the stream once the first frame has been inspected, when the
        /// stream states them.
        fn duration(&self) -> Option<u64> {
            None
        }
        /// Samples to discard from the unit starting at `pts`.
        fn trim(&self, _pts: i64, _header: &Self::Header) -> Trim {
            Trim::default()
        }
        fn plan_seek(&self, _target: u64, _context: SeekContext) -> Plan {
            Plan::Scan
        }
        /// The first sample at a frame found at `offset` after an estimated jump.
        fn estimate(&self, _offset: u64, _context: SeekContext, _basis: SeekBasis) -> u64 {
            0
        }
        /// Forget the stream: the reader starts again.
        fn reset(&mut self);
    }
}

pub(crate) use sealed::Syntax;
