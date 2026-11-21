//! Shared admission facts for owned and borrowed-range PES delivery.

use triergon_core::{Budget, BudgetVec, SourceLocation};
use triergon_mpegts::pes::PesHeader;
use triergon_mpegts::{
    SourceEpochs, SourceRange,
    compat::pes::{
        CompactDemuxPacket, CompactPayload, DemuxPacket, DemuxPayloadKind, DemuxProfile, DemuxSpan,
    },
};

/// What a reader checks of a PES before it takes the payload, the same for
/// owned and compact payloads.
#[derive(Debug)]
pub struct PesFacts<'a> {
    /// Program number.
    pub program: u16,
    /// Elementary stream PID.
    pub pid: u16,
    /// The observer profile that cut the PES.
    pub profile: DemuxProfile,
    /// Where the PES started in the source.
    pub source_origin: SourceLocation,
    /// The source's clock and configuration epochs at the PES.
    pub source_epochs: SourceEpochs,
    /// Demux packet flags.
    pub flags: u32,
    /// PES `stream_id`.
    pub stream_id: u8,
    /// How the payload is carried.
    pub payload_kind: DemuxPayloadKind,
    /// The PES header as the source carries it.
    pub source_header: &'a PesHeader,
    /// Whether the header's timestamps apply to this packet.
    pub time_header_applies: bool,
    /// Payload length in bytes.
    pub payload_len: usize,
    /// Whether the payload is empty.
    pub empty: bool,
    /// Budget of the payload storage.
    pub payload_budget: &'a Budget,
    /// Budget of the header range list.
    pub header_budget: &'a Budget,
}

impl PesFacts<'_> {
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
            profile: self.profile,
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
            profile: self.profile,
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
