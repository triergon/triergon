//! Independent source-bound lifecycle.
use super::*;
use crate::psi::{PatTable, ProgramMap, TableStep};
use triergon_core::SourceId;

/// Scalar acknowledgement of a borrowed complete source PAT. Raw sections,
/// ordered entries and storage remain in that source table; nothing is flattened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PatRoute {
    entity: SourceId,
    tsid: u16,
    program: u16,
    pmt_pid: u16,
    version: u8,
    context: SourceContext,
    programs: usize,
}
impl PatRoute {
    /// The source entity the PAT was read from.
    pub fn entity(self) -> SourceId {
        self.entity
    }
    /// The 16-bit transport stream ID of the PAT.
    pub fn tsid(self) -> u16 {
        self.tsid
    }
    /// The selected program number.
    pub fn program(self) -> u16 {
        self.program
    }
    /// The 13-bit PID of the selected program's PMT.
    pub fn pmt_pid(self) -> u16 {
        self.pmt_pid
    }
    /// The 5-bit PAT version.
    pub fn version(self) -> u8 {
        self.version
    }
    /// The source context the PAT was issued with.
    pub fn context(self) -> SourceContext {
        self.context
    }
    /// Number of programs in the PAT, network entries excluded.
    pub fn program_count(self) -> usize {
        self.programs
    }
}

#[derive(Debug)]
enum PatStep {
    Header,
    Sections {
        index: usize,
        bytes: usize,
    },
    Ranges {
        index: usize,
        section_bytes: usize,
        range_bytes: usize,
        entity: Option<SourceId>,
    },
    Programs {
        index: usize,
        entity: SourceId,
        selected: Option<u16>,
        programs: usize,
    },
    Finish {
        entity: SourceId,
        selected: Option<u16>,
        programs: usize,
    },
    Done,
}

/// One actual source storage/range/entry per work unit. Borrows the table; no heap
/// workspace, reparsing, source mutation or queue is involved.
#[derive(Debug)]
pub struct PatSelection<'a> {
    table: &'a PatTable,
    budget: &'a Budget,
    requested: u16,
    context: SourceContext,
    step: PatStep,
}
impl<'a> PatSelection<'a> {
    /// A selection of program `requested` from the complete PAT `table`.
    ///
    /// Fails with `Invalid` for program 0.
    pub fn new(
        table: &'a PatTable,
        requested: u16,
        budget: &'a Budget,
        context: SourceContext,
    ) -> Result<Self> {
        if requested == 0 {
            return Err(Error::Invalid(
                "selected program zero is a network association",
            ));
        }
        Ok(Self {
            table,
            budget,
            requested,
            context,
            step: PatStep::Header,
        })
    }
    /// Checks the table within `work` and returns the route to the requested program once (`None`
    /// when the PAT does not list it), then `Done`.
    ///
    /// Fails with `Invalid` for an incomplete or non-current table, inconsistent byte accounting or
    /// a duplicate program, with `Unsupported` for sections from several entities, with
    /// `ForeignPool` for another byte pool, and with `Cancelled` when `cancel` is set.
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<TableStep<Option<PatRoute>>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if matches!(self.step, PatStep::Done) {
                return Ok(TableStep::Done);
            }
            if work.remaining() == 0 {
                return Ok(TableStep::Yield);
            }
            // End-of-array control transitions do not debit pretend syntax work.
            match self.step {
                PatStep::Sections { index, bytes } if index == self.table.source_sections.len() => {
                    self.step = PatStep::Ranges {
                        index: 0,
                        section_bytes: bytes,
                        range_bytes: 0,
                        entity: None,
                    };
                    continue;
                }
                PatStep::Programs {
                    index,
                    entity,
                    selected,
                    programs,
                } if index == self.table.programs.len() => {
                    self.step = PatStep::Finish {
                        entity,
                        selected,
                        programs,
                    };
                    continue;
                }
                _ => {}
            }
            if !work.try_charge(1) {
                return Ok(TableStep::Yield);
            }
            match &mut self.step {
                PatStep::Header => {
                    let h = self.table.header;
                    if h.table_id != 0
                        || !h.current_next
                        || h.version > 31
                        || self.context.byte_run_epoch.is_some()
                        || self.table.source_sections.as_slice().len()
                            != usize::from(h.last_section_number) + 1
                        || self.table.source_ranges.as_slice().is_empty()
                    {
                        return Err(Error::Invalid(
                            "selected PAT must be a complete current source table",
                        ));
                    }
                    self.step = PatStep::Sections { index: 0, bytes: 0 };
                }
                PatStep::Sections { index, bytes } => {
                    if let Some(source) = self.table.source_sections.as_slice().get(*index) {
                        if !source.belongs_to(self.budget) {
                            return Err(Error::ForeignPool);
                        }
                        *bytes = bytes.checked_add(source.len()).ok_or(Error::Overflow)?;
                        *index += 1;
                    } else {
                        return Err(Error::Invalid("selected PAT owner cursor"));
                    }
                }
                PatStep::Ranges {
                    index,
                    section_bytes,
                    range_bytes,
                    entity,
                } => {
                    if let Some(item) = self.table.source_ranges.as_slice().get(*index) {
                        let r = item.range;
                        r.end()?;
                        if r.bytes == 0
                            || item.section_number > self.table.header.last_section_number
                            || entity.is_some_and(|s| s != r.entity)
                        {
                            return Err(Error::Unsupported(
                                "selected PAT entity/range context unobserved",
                            ));
                        }
                        *entity = Some(r.entity);
                        *range_bytes = range_bytes.checked_add(r.bytes).ok_or(Error::Overflow)?;
                        *index += 1;
                    } else {
                        if *range_bytes != *section_bytes {
                            return Err(Error::Invalid("selected PAT physical byte accounting"));
                        }
                        let Some(entity) = *entity else {
                            return Err(Error::Invalid(
                                "selected PAT must be a complete current source table",
                            ));
                        };
                        self.step = PatStep::Programs {
                            index: 0,
                            entity,
                            selected: None,
                            programs: 0,
                        };
                    }
                }
                PatStep::Programs {
                    index,
                    selected,
                    programs,
                    ..
                } => {
                    if let Some(entry) = self.table.programs.as_slice().get(*index) {
                        if !entry.is_network_pid() {
                            *programs = programs.checked_add(1).ok_or(Error::Overflow)?;
                            if entry.program_number == self.requested {
                                crate::checked_pid(entry.pid)?;
                                if selected.replace(entry.pid).is_some() {
                                    return Err(Error::Invalid("selected PAT duplicate program"));
                                }
                            }
                        }
                        *index += 1;
                    } else {
                        return Err(Error::Invalid("selected PAT entry cursor"));
                    }
                }
                PatStep::Finish {
                    entity,
                    selected,
                    programs,
                } => {
                    let route = selected.map(|pmt_pid| PatRoute {
                        entity: *entity,
                        tsid: self.table.header.extension,
                        program: self.requested,
                        pmt_pid,
                        version: self.table.header.version,
                        context: self.context,
                        programs: *programs,
                    });
                    if cancel.load(Ordering::Relaxed) {
                        return Err(Error::Cancelled);
                    }
                    self.step = PatStep::Done;
                    return Ok(TableStep::Output(route));
                }
                PatStep::Done => return Ok(TableStep::Done),
            }
        }
    }
}

/// Acknowledgement of a source map. This scalar value does not claim to
/// own descriptor bytes or a core generation token. Keep the real source Map
/// whenever its raw sections/descriptors are needed after the borrowed job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PmtBinding {
    /// The PAT route the PMT was bound through.
    pub route: PatRoute,
    /// The 5-bit PMT version.
    pub version: u8,
    /// The 13-bit PCR PID of the PMT.
    pub pcr_pid: u16,
    /// The observer scope bound to the PMT's stream.
    pub scope: DemuxScope,
    /// The source context the PMT was issued with.
    pub context: SourceContext,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Initial,
    Rebind,
}

#[derive(Debug)]
enum BindStep {
    Header,
    Context,
    Ranges {
        target: DemuxScope,
        index: usize,
        bytes: usize,
        previous_end: Option<u64>,
    },
    Commit {
        target: DemuxScope,
    },
    Done,
}

/// Exclusive observer + immutable actual source Map, held only for this bounded
/// transaction. Dropping before commit aborts; no pending input is fabricated.
#[derive(Debug)]
pub struct PmtBindingJob<'a> {
    observer: &'a mut RecoveryObserver,
    map: &'a ProgramMap,
    route: PatRoute,
    context: SourceContext,
    mode: Mode,
    step: BindStep,
}
impl RecoveryObserver {
    /// The current PMT binding, once one has been made.
    pub fn pmt_binding(&self) -> Option<PmtBinding> {
        self.pmt_binding
    }
    pub(super) fn after_pmt_binding(&self, c: SourceContext) -> bool {
        self.pmt_binding
            .is_none_or(|b| c.source_event_ordinal > b.context.source_event_ordinal)
    }
    /// Starts binding the observer to the first PMT `map` of the selected program.
    ///
    /// Fails with `Unsupported` while the observer is not drained, and with `Invalid` when the
    /// observer is already bound or has received input.
    pub fn bind_selected_pmt<'a>(
        &'a mut self,
        route: PatRoute,
        map: &'a ProgramMap,
        context: SourceContext,
    ) -> Result<PmtBindingJob<'a>> {
        PmtBindingJob::begin(self, route, map, context, Mode::Initial)
    }
    /// Starts rebinding a drained, bound observer to a later PMT `map` of the selected program.
    ///
    /// Fails with `Unsupported` while the observer is not drained, and with `Invalid` when it is
    /// not
    /// bound yet.
    pub fn rebind_drained_selected_pmt<'a>(
        &'a mut self,
        route: PatRoute,
        map: &'a ProgramMap,
        context: SourceContext,
    ) -> Result<PmtBindingJob<'a>> {
        PmtBindingJob::begin(self, route, map, context, Mode::Rebind)
    }
}
impl<'a> PmtBindingJob<'a> {
    fn begin(
        observer: &'a mut RecoveryObserver,
        route: PatRoute,
        map: &'a ProgramMap,
        context: SourceContext,
        mode: Mode,
    ) -> Result<Self> {
        if observer.pending.is_some()
            || observer.pending_timeline.is_some()
            || observer.fault.is_some()
            || !observer.inner.recovery_idle()
        {
            return Err(Error::Unsupported(
                "selected PMT binding requires drained observer",
            ));
        }
        if (mode == Mode::Initial && (observer.pmt_binding.is_some() || observer.last.is_some()))
            || (mode == Mode::Rebind && observer.pmt_binding.is_none())
        {
            return Err(Error::Invalid("selected PMT binding state"));
        }
        Ok(Self {
            observer,
            map,
            route,
            context,
            mode,
            step: BindStep::Header,
        })
    }
    /// The observer's scope before the binding completes.
    pub fn current_scope(&self) -> DemuxScope {
        self.observer.scope()
    }
    /// Checks the PMT within `work` and returns the binding once, then `Done`.
    ///
    /// Fails with `Invalid` or `Unsupported` when the PMT does not match the route, the scope or
    /// the
    /// previous binding, with `ForeignPool` for another byte pool, and with `Cancelled` when
    /// `cancel` is set.
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<TableStep<PmtBinding>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if matches!(self.step, BindStep::Done) {
                return Ok(TableStep::Done);
            }
            if !work.try_charge(1) {
                return Ok(TableStep::Yield);
            }
            match &mut self.step {
                BindStep::Header => {
                    if !self.map.source.belongs_to(&self.observer.budget) {
                        return Err(Error::ForeignPool);
                    }
                    let h = self.map.header;
                    if h.table_id != 2
                        || !h.current_next
                        || h.section_number != 0
                        || h.last_section_number != 0
                        || h.version > 31
                        || h.extension != self.route.program
                        || self.map.streams.as_slice().len() != 1
                    {
                        return Err(Error::Invalid(
                            "selected PMT must be current single-stream source Map",
                        ));
                    }
                    if self.map.source_ranges.as_slice().is_empty()
                        || self.map.source_ranges.as_slice().len()
                            > self.observer.inner.limits().source_spans
                    {
                        return Err(Error::Invalid("selected PMT source range count"));
                    }
                    self.step = BindStep::Context;
                }
                BindStep::Context => {
                    let current = self.observer.scope();
                    let stream = &self.map.streams.as_slice()[0];
                    if self.route.entity != current.entity
                        || self.route.program != current.program
                        || self.context.byte_run_epoch.is_some()
                        || self.context.source_event_ordinal
                            <= self.route.context.source_event_ordinal
                        || self
                            .observer
                            .last
                            .is_some_and(|(n, _)| self.context.source_event_ordinal <= n)
                    {
                        return Err(Error::Invalid("selected PMT source event context"));
                    }
                    if !self
                        .map
                        .program_descriptors
                        .belongs_to(&self.observer.budget)
                        || !stream.descriptor_bytes.belongs_to(&self.observer.budget)
                        || !self
                            .map
                            .program_descriptors
                            .shares_storage_with(&self.map.source)
                        || !stream
                            .descriptor_bytes
                            .shares_storage_with(&self.map.source)
                    {
                        return Err(Error::ForeignPool);
                    }
                    if stream.pid != current.pid
                        || !matches!(stream.stream_type, 0x1b | 0x24)
                        || current.stream_id != 0xe0
                    {
                        return Err(Error::Unsupported(
                            "selected PMT PID/carriage transition unobserved",
                        ));
                    }
                    let mut target = current;
                    if self.mode == Mode::Initial {
                        if stream.stream_type != current.stream_type
                            || self.context.epochs != current.epochs
                        {
                            return Err(Error::Invalid("selected initial PMT scope mismatch"));
                        }
                    } else {
                        let old = self
                            .observer
                            .pmt_binding
                            .ok_or(Error::Invalid("selected old PMT binding missing"))?;
                        if self.route.tsid != old.route.tsid
                            || self.route.pmt_pid != old.route.pmt_pid
                            || self.map.pcr_pid != old.pcr_pid
                            || current.stream_type != 0x1b
                            || stream.stream_type != 0x24
                        {
                            return Err(Error::Unsupported(
                                "selected PMT routing/retype context unobserved",
                            ));
                        }
                        if self.map.header.version != ((old.version + 1) & 31)
                            || self.context.epochs.clock != current.epochs.clock
                            || self.context.epochs.configuration
                                != current
                                    .epochs
                                    .configuration
                                    .checked_add(1)
                                    .ok_or(Error::Overflow)?
                            || self.context.source_event_ordinal <= old.context.source_event_ordinal
                        {
                            return Err(Error::Invalid("selected PMT version/epoch sequence"));
                        }
                    }
                    target.stream_type = stream.stream_type;
                    target.epochs = self.context.epochs;
                    self.step = BindStep::Ranges {
                        target,
                        index: 0,
                        bytes: 0,
                        previous_end: None,
                    };
                }
                BindStep::Ranges {
                    target,
                    index,
                    bytes,
                    previous_end,
                } => {
                    if let Some(r) = self.map.source_ranges.as_slice().get(*index) {
                        let end = r.end()?;
                        if r.entity != self.route.entity
                            || r.bytes == 0
                            || previous_end.is_some_and(|p| r.offset < p)
                        {
                            return Err(Error::Unsupported(
                                "selected PMT entity/physical ranges unobserved",
                            ));
                        }
                        *bytes = bytes.checked_add(r.bytes).ok_or(Error::Overflow)?;
                        *previous_end = Some(end);
                        *index += 1;
                    } else {
                        if *bytes != self.map.source.len() {
                            return Err(Error::Invalid("selected PMT physical byte accounting"));
                        }
                        self.step = BindStep::Commit { target: *target };
                    }
                }
                BindStep::Commit { target } => {
                    let scope = *target;
                    let binding = PmtBinding {
                        route: self.route,
                        version: self.map.header.version,
                        pcr_pid: self.map.pcr_pid,
                        scope,
                        context: self.context,
                    };
                    if cancel.load(Ordering::Relaxed) {
                        return Err(Error::Cancelled);
                    }
                    if self.mode == Mode::Rebind {
                        self.observer.inner.commit_pmt_scope(scope);
                        self.observer.selected_faults = false;
                    }
                    self.observer.pmt_binding = Some(binding);
                    self.step = BindStep::Done;
                    return Ok(TableStep::Output(binding));
                }
                BindStep::Done => return Ok(TableStep::Done),
            }
        }
    }
}
