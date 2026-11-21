//! H.222.0 2.4.3.6-8: bounded PES assembly with original timestamp/header facts.
//! This module deliberately does not group codec access units or infer DTS.
mod assembler;
mod header;

pub use assembler::PesAssembler;
pub(crate) use assembler::UNBOUNDED_NON_VIDEO;
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
    fn inline(bytes: &[u8]) -> Self {
        let mut data = [0; INLINE_HEADER_BYTES];
        data[..bytes.len()].copy_from_slice(bytes);
        Self::Inline {
            data,
            length: u8::try_from(bytes.len()).expect("inline PES header length"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PesHeader {
    pub stream_id: u8,
    pub declared_packet_length: u16,
    pub pts_raw33: Option<u64>,
    pub dts_raw33: Option<u64>,
    pub data_alignment_indicator: bool,
    pub priority: bool,
    raw: HeaderBytes,
}
impl PesHeader {
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
    pub fn raw_dts_time(&self) -> Result<Option<Time>> {
        self.dts_raw33
            .map(|ticks| Time::new(i128::from(ticks), 90_000))
            .transpose()
    }
}

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

    pub fn source_range(&self) -> SourceRange {
        self.source
    }
    pub fn backing_bytes(&self) -> &Bytes {
        &self.bytes
    }
}

#[derive(Debug)]
pub struct Pes {
    pub pid: u16,
    pub header: PesHeader,
    pub epochs: SourceEpochs,
    pub elementary_offset: u64,
    /// Identifies an uninterrupted elementary byte run; unrelated to I/O or time.
    pub byte_run_epoch: u64,
    pub payload_len: usize,
    pub header_source_ranges: BudgetVec<SourceRange>,
    spans: BudgetVec<PesSpan>,
}
impl Pes {
    pub fn spans(&self) -> &[PesSpan] {
        self.spans.as_slice()
    }
    pub(crate) fn table_storage(&self) -> Result<usize> {
        crate::table_storage(&self.spans)?
            .checked_add(crate::table_storage(&self.header_source_ranges)?)
            .and_then(|bytes| bytes.checked_add(self.header.retained_storage()))
            .ok_or(Error::Overflow)
    }

    pub fn payload(&self) -> impl Iterator<Item = &[u8]> {
        self.spans.as_slice().iter().map(AsRef::as_ref)
    }
    pub fn source_ranges(&self) -> impl Iterator<Item = SourceRange> + '_ {
        self.spans.as_slice().iter().map(|span| span.source)
    }
}
