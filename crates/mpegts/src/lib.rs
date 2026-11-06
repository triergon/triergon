//! Independently authored, bounded MPEG-TS syntax and assembly.
//!
//! Transport packets, PSI sections and PES byte runs are distinct outputs. A PES
//! is NOT a codec access unit. Access-unit framing, source I/O tokens, exact rational
//! time and seeking belong to other crates.
#![forbid(unsafe_code)]
#![warn(
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used
)]
// Pending: `missing_docs`, `missing_debug_implementations`, `clippy::expect_used`.

pub mod carrier;
pub mod clock;
mod continuity;
mod framer;
mod media_entity;
pub mod mux;
pub mod native_catalogue;
pub mod native_input;
/// Packet-completion trace projection, used only by the reference trace tooling.
#[cfg(feature = "reference-trace")]
pub mod native_packet_completion;
pub mod native_pes;
pub mod native_recovery;
pub mod pes;
pub mod psi;
mod stream;
mod transport;

pub use carrier::{RejectedTransport, TransportInspection, TransportInspector};
pub use continuity::{Continuity, ContinuityOutcome};
pub use framer::{FrameState, FrameStep, NullCarrierRun, TransportFramer};
pub use media_entity::MediaEntityEndV1;
pub use pes::{Pes, PesAssembler, PesHeader, PesSpan};
pub use psi::SectionAssembler;
pub use stream::{
    CompactTraceEvent, FaultKind, FeedContinuation, TraceEvent, TraceFault, TransportStream,
};
pub use transport::{Adaptation, PacketFormat, Pcr, TransportPacket, parse_transport};

use triergon_core::{BudgetVec, Error, Result, SourceId};

/// Format policy, independent from the shared live-capacity Budget.
#[derive(Clone, Copy, Debug)]
pub struct ParseLimits {
    /// PAT/PMT still have a normative 1024-byte total, even under this private limit.
    pub section_bytes: usize,
    pub pes_bytes: usize,
    pub source_spans: usize,
    pub programs: usize,
    pub streams: usize,
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
    pub entity: SourceId,
    pub offset: u64,
    pub bytes: usize,
}

impl SourceRange {
    pub fn end(self) -> Result<u64> {
        self.offset
            .checked_add(u64::try_from(self.bytes).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)
    }
}

/// Source facts supplied by the format coordinator; neither is an I/O token.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SourceEpochs {
    pub clock: u64,
    pub configuration: u64,
}

/// One parser poll produces at most one owned object and never performs source I/O.
#[derive(Debug)]
pub enum AssemblyStep<T> {
    Output(T),
    NeedPayload,
    Yield,
    Finished,
}

pub(crate) fn checked_pid(pid: u16) -> Result<()> {
    if pid > 0x1ffe {
        Err(Error::Invalid("assembly PID"))
    } else {
        Ok(())
    }
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
