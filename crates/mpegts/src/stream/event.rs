//! The events a transport stream returns.
use crate::compat::adts::UnboundedPes;
use crate::psi::{PatTable, ProgramMap, Section};
use crate::transport::carrier::TransportObservation;
use crate::{ContinuityOutcome, NullCarrierRun, Pes, SourceEpochs, TransportPacket};
use triergon_core::{Error, SourceLocation};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    Transport,
    Continuity,
    Psi,
    Pes,
    Program,
}
#[derive(Clone, Debug)]
pub struct TraceFault {
    pub kind: FaultKind,
    pub pid: Option<u16>,
    pub location: SourceLocation,
    pub error: Error,
    /// Original owned rejected carrier; absent for non-carrier/operational faults.
    pub rejected: Option<crate::RejectedTransport>,
}

/// A carrier or ordinary continuity event that can be inspected in place.
#[derive(Clone, Copy, Debug)]
pub enum CarrierObservation<'a> {
    /// The complete validated transport packet, including its original bytes.
    Transport(TransportObservation<'a>),
    /// The first packet on a route or its next continuous packet.
    Continuity {
        pid: u16,
        outcome: ContinuityOutcome,
    },
}

/// One result at a time. Every dynamic member owns shared charged storage; there
/// is no uncharged boxed variant or internal unbounded event queue.
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "one inline owned source event; heap storage inside variants is already charged"
)]
pub enum TraceEvent {
    Transport(TransportPacket),
    Continuity {
        pid: u16,
        outcome: ContinuityOutcome,
    },
    Section(Section),
    Pat(PatTable),
    ProgramMap {
        map: ProgramMap,
        active: bool,
        configuration_epoch: u64,
    },
    Pes(Pes),
    /// A rejected length-zero ADTS PES retained by the explicit compatibility policy.
    UnboundedAdtsPes(UnboundedPes),
    Timeline {
        program: u16,
        clock_epoch: u64,
    },
    Discontinuity {
        epochs: SourceEpochs,
    },
    Diagnostic(TraceFault),
    Finished,
}

impl TraceEvent {
    /// Header and span facts of a validated PES or a reported unbounded ADTS PES.
    /// The event variant identifies the source's wire validity.
    pub fn pes_source(&self) -> Option<&Pes> {
        match self {
            Self::Pes(pes) | Self::UnboundedAdtsPes(UnboundedPes(pes)) => Some(pes),
            _ => None,
        }
    }

    pub(super) fn from_assembled_pes(pes: Pes) -> Self {
        if pes.header.declared_packet_length == 0 && (0xc0..=0xdf).contains(&pes.header.stream_id) {
            Self::UnboundedAdtsPes(UnboundedPes(pes))
        } else {
            Self::Pes(pes)
        }
    }
}

/// Opt-in source view. A run expands to one transport event per carrier,
/// in physical order, with no intervening continuity or clock events.
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "keep one owned source event inline"
)]
pub enum CompactTraceEvent {
    Event(TraceEvent),
    NullRun(NullCarrierRun),
}
