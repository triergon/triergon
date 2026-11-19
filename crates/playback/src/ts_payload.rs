//! Shared admission facts for owned and borrowed-range PES delivery.

use triergon_core::{Budget, BudgetVec, SourceLocation};
use triergon_mpegts::{
    PesHeader, SourceEpochs, SourceRange,
    compat::pes::{
        CompactDemuxPacket, CompactPayload, DemuxPacket, DemuxPayloadKind, DemuxProfile, DemuxSpan,
    },
};

pub(crate) struct PesFacts<'a> {
    pub program: u16,
    pub pid: u16,
    pub profile: DemuxProfile,
    pub source_origin: SourceLocation,
    pub source_epochs: SourceEpochs,
    pub flags: u32,
    pub stream_id: u8,
    pub payload_kind: DemuxPayloadKind,
    pub source_header: &'a PesHeader,
    pub time_header_applies: bool,
    pub payload_len: usize,
    pub empty: bool,
    pub payload_budget: &'a Budget,
    pub header_budget: &'a Budget,
}

impl PesFacts<'_> {
    pub fn time_header(&self) -> Option<&PesHeader> {
        self.time_header_applies.then_some(self.source_header)
    }
}

pub(crate) trait PesInput {
    fn facts(&self) -> PesFacts<'_>;
    /// The payload, and the header source ranges a reader does not read.
    fn into_parts(self) -> (CompactPayload, BudgetVec<SourceRange>);
}

/// The lists of the PES a reader consumed last, kept until the reader's owner
/// hands them back to the observer that emitted them.
#[derive(Debug, Default)]
pub(crate) struct SpentLists {
    pub spans: Option<BudgetVec<DemuxSpan>>,
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
