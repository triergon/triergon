//! H.222.0 2.4.3.6-8: bounded PES assembly with original timestamp/header facts.
//! This module deliberately does not group codec access units or infer DTS.
mod assembler;
mod header;

pub use assembler::PesAssembler;
pub(crate) use assembler::UNBOUNDED_NON_VIDEO;
pub(crate) use header::{FORBIDDEN_TIME_FLAGS, TIMESTAMP_MARKERS};
pub(crate) use header::{parse_header, parse_timed_header, timestamp_bits};

use crate::{SourceEpochs, SourceRange};
use triergon_core::{BudgetVec, Bytes, Error, Result, Time};

pub(crate) const HEADER_MAX: usize = 264; // 9 fixed bytes plus 8-bit optional-header length
const INLINE_HEADER_BYTES: usize = 19; // Fixed header plus PTS and DTS.

#[derive(Clone, Debug)]
enum HeaderBytes {
    Inline {
        data: [u8; INLINE_HEADER_BYTES],
        length: u8,
    },
    Extended(Bytes),
}

impl HeaderBytes {
    /// `bytes` held inline; `None` when they are longer than `INLINE_HEADER_BYTES`.
    fn inline(bytes: &[u8]) -> Option<Self> {
        let mut data = [0; INLINE_HEADER_BYTES];
        data.get_mut(..bytes.len())?.copy_from_slice(bytes);
        Some(Self::Inline {
            data,
            length: bytes.len() as u8,
        })
    }
}

/// The parsed fixed and optional header of one PES packet, with its original bytes.
#[derive(Clone, Debug)]
pub struct PesHeader {
    /// The 8-bit stream ID after the start code prefix.
    pub stream_id: u8,
    /// The 16-bit PES packet length field; 0 declares an unbounded PES.
    pub declared_packet_length: u16,
    /// The raw 33-bit PTS in 90 kHz ticks, when the header carries one.
    pub pts_raw33: Option<u64>,
    /// The raw 33-bit DTS in 90 kHz ticks, when the header carries one. Never inferred from the
    /// PTS.
    pub dts_raw33: Option<u64>,
    /// The data alignment indicator.
    pub data_alignment_indicator: bool,
    /// The PES priority bit.
    pub priority: bool,
    raw: HeaderBytes,
}
impl PesHeader {
    /// The original header bytes, from the start code prefix through the optional header fields and
    /// stuffing.
    pub fn as_bytes(&self) -> &[u8] {
        match &self.raw {
            HeaderBytes::Inline { data, length } => &data[..usize::from(*length)],
            HeaderBytes::Extended(bytes) => bytes.as_ref(),
        }
    }

    pub(crate) fn retained_storage(&self) -> usize {
        match &self.raw {
            HeaderBytes::Inline { .. } => 0,
            HeaderBytes::Extended(bytes) => bytes.retained_storage_bytes(),
        }
    }
    /// Raw period-zero view only. Clock extension/epoch selection remains explicit.
    pub fn raw_pts_time(&self) -> Result<Option<Time>> {
        self.pts_raw33
            .map(|ticks| Time::new(i128::from(ticks), 90_000))
            .transpose()
    }
    /// The raw DTS as an exact 90 kHz time, with no clock extension or epoch applied; never fails,
    /// as the rate is fixed and nonzero.
    pub fn raw_dts_time(&self) -> Result<Option<Time>> {
        self.dts_raw33
            .map(|ticks| Time::new(i128::from(ticks), 90_000))
            .transpose()
    }
}

/// One contiguous run of PES payload bytes from a single transport packet payload.
#[derive(Debug)]
pub struct PesSpan {
    bytes: Bytes,
    start: usize,
    source: SourceRange,
}
impl AsRef<[u8]> for PesSpan {
    fn as_ref(&self) -> &[u8] {
        &self.bytes.as_ref()[self.start..self.start + self.source.bytes]
    }
}
impl PesSpan {
    /// An exact charged alias of this elementary payload span.
    pub fn payload_bytes(&self) -> Result<Bytes> {
        self.bytes.slice(self.start..self.start + self.source.bytes)
    }

    /// Where these payload bytes lie in the source entity.
    pub fn source_range(&self) -> SourceRange {
        self.source
    }
    /// The transport payload bytes this span aliases; the span is a subrange of them.
    pub fn backing_bytes(&self) -> &Bytes {
        &self.bytes
    }
}

/// One assembled PES packet: header, source facts and payload spans that alias the transport
/// payloads.
#[derive(Debug)]
pub struct Pes {
    /// The 13-bit PID that carried the packet.
    pub pid: u16,
    /// The parsed PES header.
    pub header: PesHeader,
    /// The clock and configuration epochs in force when the packet was assembled.
    pub epochs: SourceEpochs,
    /// Byte offset of the first payload byte in this PID's elementary byte stream.
    pub elementary_offset: u64,
    /// Identifies an uninterrupted elementary byte run; unrelated to I/O or time.
    pub byte_run_epoch: u64,
    /// Total payload bytes after the header, across all spans.
    pub payload_len: usize,
    /// Where the header bytes lie in the source, one range per contiguous source run.
    pub header_source_ranges: BudgetVec<SourceRange>,
    spans: BudgetVec<PesSpan>,
}
impl Pes {
    /// The payload spans in stream order.
    pub fn spans(&self) -> &[PesSpan] {
        self.spans.as_slice()
    }
    pub(crate) fn table_storage(&self) -> Result<usize> {
        crate::table_storage(&self.spans)?
            .checked_add(crate::table_storage(&self.header_source_ranges)?)
            .and_then(|bytes| bytes.checked_add(self.header.retained_storage()))
            .ok_or(Error::Overflow)
    }

    /// The payload bytes, one slice per span, in stream order.
    pub fn payload(&self) -> impl Iterator<Item = &[u8]> {
        self.spans.as_slice().iter().map(AsRef::as_ref)
    }
    /// Where each payload span lies in the source, in stream order.
    pub fn source_ranges(&self) -> impl Iterator<Item = SourceRange> + '_ {
        self.spans.as_slice().iter().map(|span| span.source)
    }
}
