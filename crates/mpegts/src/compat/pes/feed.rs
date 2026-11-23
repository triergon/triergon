//! Payload ownership and packet admission for a transport PES feed.

use super::observer::{
    CompactDemuxPacket, CompactPayload, DemuxPacket, DemuxPayloadKind, DemuxSpan, PES_CORRUPT,
    PrivateAudioCarriage, RegisteredCarriage,
};
use crate::pes::PesHeader;
use crate::{SourceEpochs, SourceRange};
use triergon_core::{Budget, BudgetVec, SourceLocation};

/// What a reader checks of a PES before it takes the payload, the same for
/// owned and compact payloads.
#[derive(Debug)]
pub struct PesFacts<'a> {
    /// The program number of the PMT that declared the stream.
    pub program: u16,
    /// The PID the PES was carried on.
    pub pid: u16,
    /// Where the PES started in the source.
    pub source_origin: SourceLocation,
    /// The source's clock and configuration epochs at the PES.
    pub source_epochs: SourceEpochs,
    /// Demux packet flags.
    pub flags: u32,
    /// The PES header's `stream_id` byte, such as 0xe0 for video.
    pub stream_id: u8,
    /// How the payload is carried.
    pub payload_kind: DemuxPayloadKind,
    /// The PES header as the source carries it.
    pub source_header: &'a PesHeader,
    time_header_applies: bool,
    /// Payload length in bytes.
    pub payload_len: usize,
    /// Whether the payload is empty.
    pub empty: bool,
    /// The pool the payload's storage is charged to.
    pub payload_budget: &'a Budget,
    /// The pool the header range list is charged to.
    pub header_budget: &'a Budget,
}

impl PesFacts<'_> {
    /// Whether the demuxer marks the payload damaged or incomplete.
    #[inline]
    pub const fn is_corrupt(&self) -> bool {
        self.flags & PES_CORRUPT != 0
    }

    /// The header whose timestamps apply, if any.
    pub fn time_header(&self) -> Option<&PesHeader> {
        self.time_header_applies.then_some(self.source_header)
    }
}

/// A completed PES as a reader takes it: owned spans or a compact payload.
pub trait PesInput {
    /// What the reader checks before taking the payload.
    fn facts(&self) -> PesFacts<'_>;
    /// The payload, and the header source ranges a reader does not read.
    fn into_parts(self) -> (CompactPayload, BudgetVec<SourceRange>);
}

/// The lists of the PES a reader consumed last, kept until the reader's owner
/// hands them back to the observer that emitted them.
#[derive(Debug, Default)]
pub struct SpentLists {
    /// The payload span list.
    pub spans: Option<BudgetVec<DemuxSpan>>,
    /// The header source range list.
    pub header: Option<BudgetVec<SourceRange>>,
}

impl PesInput for DemuxPacket {
    fn facts(&self) -> PesFacts<'_> {
        PesFacts {
            program: self.program,
            pid: self.pid,
            source_origin: self.source_origin,
            source_epochs: self.source_epochs,
            flags: self.flags,
            stream_id: self.stream_id,
            payload_kind: self.payload_kind,
            source_header: &self.source_header,
            time_header_applies: self.time_header_applies,
            payload_len: self.payload_len,
            empty: self.spans.is_empty(),
            payload_budget: self.spans.budget(),
            header_budget: self.header_source_ranges.budget(),
        }
    }
    fn into_parts(self) -> (CompactPayload, BudgetVec<SourceRange>) {
        (
            CompactPayload::from_spans(self.spans),
            self.header_source_ranges,
        )
    }
}

impl PesInput for CompactDemuxPacket {
    fn facts(&self) -> PesFacts<'_> {
        PesFacts {
            program: self.program,
            pid: self.pid,
            source_origin: self.source_origin,
            source_epochs: self.source_epochs,
            flags: self.flags,
            stream_id: self.stream_id,
            payload_kind: self.payload_kind,
            source_header: &self.source_header,
            time_header_applies: self.time_header_applies,
            payload_len: self.payload_len,
            empty: self.payload.is_empty(),
            payload_budget: self.payload.budget(),
            header_budget: self.header_source_ranges.budget(),
        }
    }
    fn into_parts(self) -> (CompactPayload, BudgetVec<SourceRange>) {
        (self.payload, self.header_source_ranges)
    }
}

/// A completed PES of a feed, with its payload as the observer delivers it.
#[derive(Debug)]
pub enum FeedPacket {
    /// Payload spans of the shared input windows.
    Owned(DemuxPacket),
    /// One compact payload per carrier run.
    Compact(CompactDemuxPacket),
}

/// Explicit packet interpretation for one selected PID of a TS program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Carriage {
    /// A registered stream type, such as AVC, HEVC or ADTS AAC.
    Registered(RegisteredCarriage),
    /// AC-3, E-AC-3 or DVB audio in private-stream PES.
    PrivateAudio(PrivateAudioCarriage),
    /// DVB subtitles.
    DvbSubtitles,
    /// Resolve the established AAC and private-audio profiles from each active PMT.
    AudioFromPmt,
}
