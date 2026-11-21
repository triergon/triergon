//! The events a transport stream returns.
use crate::SourceEpochs;
use crate::compat::adts::UnboundedPes;
use crate::pes::Pes;
use crate::psi::{PatTable, ProgramMap, Section};
use crate::transport::carrier::TransportObservation;
use crate::transport::{ContinuityOutcome, NullCarrierRun, TransportPacket};
use triergon_core::{Error, SourceLocation};

/// The layer a [`TraceFault`] belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    /// A carrier the framer rejected, a set transport error indicator, a scrambled
    /// payload, or PCR signalling that quarantines the program clock.
    Transport,
    /// A continuity counter loss or an excess duplicate carrier.
    Continuity,
    /// A PSI section or PAT/PMT table that failed to assemble or parse, including one cut
    /// off by the end of the source.
    Psi,
    /// A PES packet that failed to assemble, including one cut off by the end of the
    /// source.
    Pes,
    /// A program-level conflict: a PAT that cannot be collected, a PMT for another
    /// program or a conflicting PMT repeat, or a PCR PID change while a clock
    /// discontinuity is pending.
    Program,
}
/// A recoverable source fault. The stream discards the state the fault affects and keeps
/// parsing.
#[derive(Clone, Debug)]
pub struct TraceFault {
    /// The layer the fault belongs to.
    pub kind: FaultKind,
    /// The PID the fault concerns, when there is one; `None` for a carrier the framer
    /// rejected.
    pub pid: Option<u16>,
    /// Where the fault was found: the start of the carrier concerned, or the entity end
    /// when no carrier is in hand.
    pub location: SourceLocation,
    /// What was wrong.
    pub error: Error,
    /// Original owned rejected carrier; absent for non-carrier/operational faults.
    pub rejected: Option<crate::transport::RejectedTransport>,
}

/// A carrier or ordinary continuity event that can be inspected in place.
#[derive(Clone, Copy, Debug)]
pub enum CarrierObservation<'a> {
    /// The complete validated transport packet, including its original bytes.
    Transport(TransportObservation<'a>),
    /// The first packet on a route or its next continuous packet.
    Continuity {
        /// The 13-bit PID of the route.
        pid: u16,
        /// `First` or `Continuous`.
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
    /// An accepted transport packet, published before its continuity and payload are
    /// processed.
    Transport(TransportPacket),
    /// The continuity outcome of the preceding packet on a PAT, PMT, PCR or elementary
    /// route.
    Continuity {
        /// The 13-bit PID of the route.
        pid: u16,
        /// What the packet's continuity counter showed.
        outcome: ContinuityOutcome,
    },
    /// A complete PSI section on the PAT or PMT PID, repeats included.
    Section(Section),
    /// A complete current PAT. It selects the program in single-program discovery and
    /// binds the selected program's PMT PID.
    Pat(PatTable),
    /// A parsed PMT of the selected program.
    ProgramMap {
        /// The parsed map, with its original section bytes.
        map: ProgramMap,
        /// True when the map is current and now binds the elementary routes; false for a
        /// map whose `current_next_indicator` is 0, which changes nothing.
        active: bool,
        /// The configuration epoch the map opens when active, or the epoch in force when
        /// not.
        configuration_epoch: u64,
    },
    /// A validated PES packet of the selected program.
    Pes(Pes),
    /// A rejected length-zero ADTS PES retained by the explicit compatibility policy.
    UnboundedAdtsPes(UnboundedPes),
    /// A new time base: the first PCR after a signalled discontinuity on the PCR PID.
    /// Partial PES on every route is discarded.
    Timeline {
        /// The selected program number.
        program: u16,
        /// The clock epoch the new time base opens.
        clock_epoch: u64,
    },
    /// The boundary after
    /// [`TransportStream::discontinuity`](super::TransportStream::discontinuity): tables,
    /// routes and clock state start again.
    Discontinuity {
        /// The epochs after the boundary; both have advanced.
        epochs: SourceEpochs,
    },
    /// A recoverable source fault; the stream keeps parsing.
    Diagnostic(TraceFault),
    /// Every assembler has drained after
    /// [`TransportStream::end_of_source`](super::TransportStream::end_of_source). A later
    /// poll fails with `Invalid`.
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
    /// An event of the full trace.
    Event(TraceEvent),
    /// Consecutive null packets (PID 0x1FFF) that the full trace reports as one
    /// transport event each.
    NullRun(NullCarrierRun),
}
