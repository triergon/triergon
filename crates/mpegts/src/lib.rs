#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used
)]
// Pending: `clippy::expect_used`.

pub mod clock;
pub mod compat;
pub mod mux;
pub mod pes;
pub mod psi;
pub mod reader;
#[cfg(feature = "reference-trace")]
pub mod reference_trace;
pub mod stream;
pub mod transport;

pub use reader::{AccessUnit, Packet, Reader, ReaderEvent, ReaderLimits, Track};
pub use transport::PacketFormat;

use triergon_core::{BudgetVec, Error, Result, SourceId};

/// Format bounds on sections, PES packets and tables, independent from the shared
/// live-capacity budget.
///
/// Input beyond a bound fails with `Limit`. Building a parser fails with `Invalid` when
/// `section_bytes` is outside 3..=4096, `pes_bytes` is below 6 or any count is zero.
#[derive(Clone, Copy, Debug)]
pub struct ParseLimits {
    /// Maximum bytes of one PSI section, from its table ID through its CRC; 3 to 4096,
    /// default 4096. PAT and PMT sections keep their normative 1024-byte limit even when
    /// this is larger.
    pub section_bytes: usize,
    /// Maximum bytes of one PES packet, from its start code through its payload; at least
    /// 6, default 16 MiB. Also bounds an unbounded video PES.
    pub pes_bytes: usize,
    /// Maximum source ranges of one section, and of one PES header or payload span list;
    /// default 4096.
    pub source_spans: usize,
    /// Maximum program entries in one PAT version, across its sections; default 64.
    pub programs: usize,
    /// Maximum elementary streams in one PMT; default 64.
    pub streams: usize,
    /// Maximum descriptors in one PMT section, program and stream loops together; default
    /// 256.
    pub descriptors: usize,
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self {
            section_bytes: 4096,
            pes_bytes: 16 * 1024 * 1024,
            source_spans: 4096,
            programs: 64,
            streams: 64,
            descriptors: 256,
        }
    }
}

impl ParseLimits {
    pub(crate) fn validate(self) -> Result<Self> {
        if !(3..=4096).contains(&self.section_bytes)
            || self.pes_bytes < 6
            || self.source_spans == 0
            || self.programs == 0
            || self.streams == 0
            || self.descriptors == 0
        {
            return Err(Error::Invalid("TS parser limits"));
        }
        Ok(self)
    }
}

/// Container-local provenance. Entity identity is not an I/O generation or epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceRange {
    /// The source entity the bytes were read from.
    pub entity: SourceId,
    /// Byte offset of the first byte in that entity.
    pub offset: u64,
    /// Length of the range in bytes.
    pub bytes: usize,
}

impl SourceRange {
    /// The offset one past the last byte. Fails with `Overflow` when that does not fit
    /// in a `u64`.
    pub fn end(self) -> Result<u64> {
        self.offset
            .checked_add(u64::try_from(self.bytes).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)
    }
}

/// Source facts supplied by the format coordinator; neither is an I/O token.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SourceEpochs {
    /// Advances at each accepted PCR discontinuity and each explicit stream
    /// discontinuity; 0 for the initial time base.
    pub clock: u64,
    /// Advances with each accepted program map, each PAT that changes the selected
    /// program's PMT PID or transport stream ID, and each explicit stream discontinuity;
    /// 0 before the first PAT.
    pub configuration: u64,
}

/// One parser poll produces at most one owned object and never performs source I/O.
#[derive(Debug)]
pub enum AssemblyStep<T> {
    /// A completed object.
    Output(T),
    /// The pushed payload is used up; push the next payload before polling again.
    NeedPayload,
    /// The work grant is spent; poll again with a new grant.
    Yield,
    /// The source has ended and every object has been returned.
    Finished,
}

pub(crate) const ELEMENTARY_PID_RANGE: std::ops::RangeInclusive<u16> = 0x0010..=0x1ffe;

pub(crate) fn checked_pid(pid: u16) -> Result<()> {
    if pid > 0x1ffe {
        Err(Error::Invalid("assembly PID"))
    } else {
        Ok(())
    }
}

pub(crate) fn table_storage<T>(table: &BudgetVec<T>) -> Result<usize> {
    table
        .capacity()
        .checked_mul(size_of::<T>())
        .ok_or(Error::Overflow)
}

/// Whether an operation that failed with `error` can be retried unchanged: the pool
/// was full, an allocation failed or the caller cancelled.
pub(crate) fn retryable(error: Error) -> bool {
    matches!(
        error,
        Error::Backpressure(..) | Error::Allocation | Error::Cancelled
    )
}

pub(crate) fn limit(requested: usize, maximum: usize) -> Error {
    Error::Limit { requested, maximum }
}

/// Reserve before either byte or extent state is committed. Adjacent physical
/// regions coalesce independently of the caller's work-yield schedule.
pub(crate) fn prepare_range(
    ranges: &mut BudgetVec<SourceRange>,
    range: SourceRange,
    maximum: usize,
) -> Result<bool> {
    range.end()?;
    let merge = ranges
        .as_slice()
        .last()
        .is_some_and(|last| last.entity == range.entity && last.end().ok() == Some(range.offset));
    if merge {
        ranges
            .as_slice()
            .last()
            .ok_or(Error::Invalid("missing source extent"))?
            .bytes
            .checked_add(range.bytes)
            .ok_or(Error::Overflow)?;
    }
    if !merge {
        if ranges.len() == maximum {
            return Err(limit(maximum.saturating_add(1), maximum));
        }
        ranges.try_reserve_exact(1)?;
    }
    Ok(merge)
}

pub(crate) fn commit_range(
    ranges: &mut BudgetVec<SourceRange>,
    range: SourceRange,
    merge: bool,
) -> Result<()> {
    if merge {
        let last = ranges
            .as_mut_slice()
            .last_mut()
            .ok_or(Error::Invalid("missing source extent"))?;
        last.bytes = last.bytes.checked_add(range.bytes).ok_or(Error::Overflow)?;
        Ok(())
    } else {
        ranges.try_push(range).map_err(|error| error.error)
    }
}
