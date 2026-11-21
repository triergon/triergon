//! Composed selected-program TS -> PSI -> PES source operation. No codec AU work.
mod clock;
mod end;
pub(crate) mod entity;
mod event;
mod feed;
mod held;
mod poll;
mod storage;
mod tables;

pub use entity::{MediaEntityEnd, ObservedTrace};
pub use event::{CarrierObservation, CompactTraceEvent, FaultKind, TraceEvent, TraceFault};
pub use feed::{FeedCarrier, FeedContinuation, FeedItem, FeedPayloadRun};
use held::{Held, Stage};
use tables::PsiWork;

pub(crate) use storage::{StorageItem, TransportStorageCursor};

use crate::pes::PesAssembler;
use crate::psi::SectionAssembler;
use crate::psi::{DescriptorNamespace, PatCollector};
use crate::transport::{Continuity, TransportFramer};
use crate::{PacketFormat, ParseLimits, SourceEpochs};
use std::sync::atomic::AtomicUsize;
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, SourceId, SourceLocation};

#[derive(Debug)]
struct PsiRoute {
    pid: u16,
    continuity: Continuity,
    assembler: SectionAssembler,
}
impl PsiRoute {
    fn new(pid: u16, budget: &Budget, limits: ParseLimits) -> Result<Self> {
        Ok(Self {
            pid,
            continuity: Continuity::new(pid)?,
            assembler: SectionAssembler::new(pid, budget.clone(), limits)?,
        })
    }
}
#[derive(Debug)]
struct PesRoute {
    pid: u16,
    stream_type: u8,
    descriptors: Bytes,
    epochs: SourceEpochs,
    continuity: Option<Continuity>,
    assembler: Option<PesAssembler>,
    reuse: Option<usize>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Pat,
    Pmt,
    Pes(usize),
    Clock,
    Ignore,
}

/// The caller selects the program explicitly or requests single-program
/// discovery. All PAT associations and raw transport/section facts remain
/// visible.
#[derive(Debug)]
pub struct TransportStream {
    budget: Budget,
    format: PacketFormat,
    limits: ParseLimits,
    program: u16,
    discover_program: bool,
    feed_omissions: Option<u64>,
    feed_validated_carriers: bool,
    unbounded_adts: bool,
    deferred_feed_events: u64,
    descriptor_namespace: DescriptorNamespace,
    framer: TransportFramer,
    pat: PsiRoute,
    pmt: Option<PsiRoute>,
    collector: PatCollector,
    routes: BudgetVec<PesRoute>,
    pmt_source: Option<Bytes>,
    pmt_version: Option<u8>,
    tsid: Option<u16>,
    pcr_pid: Option<u16>,
    clock_continuity: Option<Continuity>,
    clock_pending: bool,
    clock_signal: bool,
    // None until the first explicitly accepted new-clock boundary; then saturate
    // at the normative minimum of two samples before another boundary.
    clock_samples: Option<u8>,
    clock_quarantined: bool,
    clock_epoch: u64,
    config_epoch: u64,
    held: Option<Held>,
    route: Route,
    psi: PsiWork,
    entity_end: Option<SourceLocation>,
    logical_end: bool,
    eof_route: usize,
    finished: bool,
    boundary_event: bool,
    trace_issuer: AtomicUsize,
    trace_generation: Option<u64>,
}

impl TransportStream {
    /// A stream of `format` packets that demultiplexes `program`, with private
    /// descriptors left uninterpreted (`DescriptorNamespace::Unspecified`).
    ///
    /// Fails with `Invalid` for program 0, which names the network PID, and for `limits`
    /// out of range.
    pub fn new(
        budget: Budget,
        format: PacketFormat,
        limits: ParseLimits,
        program: u16,
    ) -> Result<Self> {
        Self::new_with_namespace(
            budget,
            format,
            limits,
            program,
            DescriptorNamespace::Unspecified,
        )
    }
    /// An explicit, immutable caller contract supplies descriptor namespace evidence.
    /// Raw source facts and clock/configuration epochs are unaffected by this choice.
    pub fn new_with_namespace(
        budget: Budget,
        format: PacketFormat,
        limits: ParseLimits,
        program: u16,
        descriptor_namespace: DescriptorNamespace,
    ) -> Result<Self> {
        if program == 0 {
            return Err(Error::Invalid("network PID is not a selected program"));
        }
        Self::new_with_program(budget, format, limits, Some(program), descriptor_namespace)
    }

    /// Read one TS188 segment with an in-band PAT and PMT. The complete PAT
    /// must identify exactly one media program. Program zero is a network PID.
    pub fn new_single_program_segment(
        budget: Budget,
        limits: ParseLimits,
        descriptor_namespace: DescriptorNamespace,
    ) -> Result<Self> {
        Self::new_with_program(
            budget,
            PacketFormat::Ts188,
            limits,
            None,
            descriptor_namespace,
        )
    }

    /// Read one independent segment, sharing complete carriers with the
    /// input window and validating source PES without retaining it. Feed
    /// callers still receive transport packets, program tables and faults.
    pub fn new_single_program_feed(
        budget: Budget,
        limits: ParseLimits,
        descriptor_namespace: DescriptorNamespace,
    ) -> Result<Self> {
        let mut stream = Self::new_single_program_segment(budget, limits, descriptor_namespace)?;
        stream.framer.share_input();
        stream.feed_omissions = Some(0);
        Ok(stream)
    }

    /// A sequential feed publishes PES carriers after continuity validation.
    /// PAT, PMT, continuity breaks, clock transitions and faults remain visible.
    pub fn new_single_program_progressive_feed(
        budget: Budget,
        limits: ParseLimits,
        descriptor_namespace: DescriptorNamespace,
    ) -> Result<Self> {
        let mut stream = Self::new_single_program_segment(budget, limits, descriptor_namespace)?;
        stream.framer.compact_progressive_input();
        stream.feed_omissions = Some(0);
        stream.feed_validated_carriers = true;
        Ok(stream)
    }

    /// Count source events suppressed since the last call, for consumers
    /// that track positions in the full source trace.
    pub fn take_feed_omissions(&mut self) -> u64 {
        self.feed_omissions.as_mut().map_or(0, std::mem::take)
    }

    fn omit_feed_events(&mut self, count: u64) -> Result<()> {
        let omissions = self
            .feed_omissions
            .as_mut()
            .ok_or(Error::Invalid("feed event omitted from source trace"))?;
        *omissions = omissions.checked_add(count).ok_or(Error::Overflow)?;
        Ok(())
    }

    pub(crate) fn new_with_program(
        budget: Budget,
        format: PacketFormat,
        limits: ParseLimits,
        program: Option<u16>,
        descriptor_namespace: DescriptorNamespace,
    ) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            framer: TransportFramer::new(budget.clone(), format),
            pat: PsiRoute::new(0, &budget, limits)?,
            collector: PatCollector::new(budget.clone(), limits)?,
            routes: BudgetVec::new(&budget),
            budget,
            format,
            limits,
            program: program.unwrap_or_default(),
            discover_program: program.is_none(),
            feed_omissions: None,
            feed_validated_carriers: false,
            unbounded_adts: false,
            deferred_feed_events: 0,
            descriptor_namespace,
            pmt: None,
            pmt_source: None,
            pmt_version: None,
            tsid: None,
            pcr_pid: None,
            clock_continuity: None,
            clock_pending: false,
            clock_signal: false,
            clock_samples: None,
            clock_quarantined: false,
            clock_epoch: 0,
            config_epoch: 0,
            held: None,
            route: Route::Ignore,
            psi: PsiWork::Idle,
            entity_end: None,
            logical_end: false,
            eof_route: 0,
            finished: false,
            boundary_event: false,
            trace_issuer: AtomicUsize::new(0),
            trace_generation: None,
        })
    }
    /// Zero until the first complete PAT in single-program discovery mode.
    pub fn selected_program(&self) -> u16 {
        self.program
    }
    /// The clock and configuration epochs in force at the current position.
    pub fn source_epochs(&self) -> SourceEpochs {
        SourceEpochs {
            clock: self.clock_epoch,
            configuration: self.config_epoch,
        }
    }
    /// Invalid signalling keeps its raw facts/diagnostics but cannot supply a
    /// trusted clock interpretation until an explicit format discontinuity/reset.
    pub fn clock_is_quarantined(&self) -> bool {
        self.clock_quarantined
    }
    pub(crate) fn has_active_map(&self) -> bool {
        self.pmt_source.is_some()
    }

    /// A fresh in-band map re-establishes this profile's transport state. Only
    /// its separately proved concatenated elementary coordinate needs restoring.
    pub(crate) fn resume_elementary_offset(&mut self, pid: u16, offset: u64) -> Result<()> {
        let [route] = self.routes.as_mut_slice() else {
            return Err(Error::SourceMismatch);
        };
        if route.pid != pid
            || !matches!(route.stream_type, 0x03 | 0x04 | 0x1b | 0x24)
            || route.epochs
                != (SourceEpochs {
                    configuration: 2,
                    clock: 0,
                })
            || self.clock_pending
            || self.clock_quarantined
            || self.clock_samples.is_some()
        {
            return Err(Error::SourceMismatch);
        }
        route
            .assembler
            .as_mut()
            .ok_or(Error::SourceMismatch)?
            .resume_elementary_offset(offset)
    }
    pub(crate) fn route_state(
        &self,
        index: usize,
        pid: u16,
    ) -> Option<(SourceEpochs, Option<SourceLocation>)> {
        self.routes
            .as_slice()
            .get(index)
            .filter(|route| route.pid == pid)
            .map(|route| {
                let pending = route
                    .assembler
                    .as_ref()
                    .and_then(PesAssembler::pending_header_location);
                (route.epochs, pending)
            })
    }
    fn location(&self) -> SourceLocation {
        self.held.as_ref().map_or(
            self.entity_end.unwrap_or(SourceLocation {
                source: SourceId(0),
                offset: 0,
            }),
            |held| SourceLocation {
                source: held.packet.source_id.unwrap_or(SourceId(0)),
                offset: held.packet.source_offset,
            },
        )
    }
    fn diagnostic(&self, kind: FaultKind, pid: Option<u16>, error: Error) -> TraceEvent {
        TraceEvent::Diagnostic(TraceFault {
            kind,
            pid,
            error,
            rejected: None,
            location: self.location(),
        })
    }
    pub(crate) fn allow_unbounded_adts(&mut self) {
        self.unbounded_adts = true;
    }

    pub(crate) fn set_pcr_policy(&mut self, policy: crate::compat::pes::PcrPolicy) {
        self.framer.set_pcr_policy(policy);
    }

    pub(crate) fn share_input(&mut self) {
        self.framer.share_input();
    }
}

#[cfg(test)]
mod tests;
