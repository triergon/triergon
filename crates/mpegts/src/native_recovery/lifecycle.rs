//! Independent source-bound lifecycle draft. Activate only after the parent
//! observes the frozen intended runtime RED; no execution is claimed here.
use super::*;
use crate::psi::{PatTable, ProgramMap, TableStep};
use triergon_core::SourceId;

/// Scalar acknowledgement of a borrowed complete source PAT. Raw sections,
/// ordered entries and owners remain in that source table; nothing is flattened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativePatRoute {
    entity: SourceId,
    tsid: u16,
    program: u16,
    pmt_pid: u16,
    version: u8,
    context: NativeSourceContext,
    programs: usize,
}
impl NativePatRoute {
    pub fn entity(self) -> SourceId {
        self.entity
    }
    pub fn tsid(self) -> u16 {
        self.tsid
    }
    pub fn program(self) -> u16 {
        self.program
    }
    pub fn pmt_pid(self) -> u16 {
        self.pmt_pid
    }
    pub fn version(self) -> u8 {
        self.version
    }
    pub fn context(self) -> NativeSourceContext {
        self.context
    }
    pub fn program_count(self) -> usize {
        self.programs
    }
}

/// One actual source owner/range/entry per work unit. Borrows the table; no heap
/// workspace, reparsing, source mutation or queue is involved.
#[derive(Debug)]
pub struct NativePatSelection<'a> {
    table: &'a PatTable,
    budget: &'a Budget,
    requested: u16,
    context: NativeSourceContext,
    phase: u8,
    index: usize,
    entity: Option<SourceId>,
    source_bytes: usize,
    range_bytes: usize,
    selected: Option<u16>,
    programs: usize,
    done: bool,
}
impl<'a> NativePatSelection<'a> {
    pub fn new(
        table: &'a PatTable,
        requested: u16,
        budget: &'a Budget,
        context: NativeSourceContext,
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
            phase: 0,
            index: 0,
            entity: None,
            source_bytes: 0,
            range_bytes: 0,
            selected: None,
            programs: 0,
            done: false,
        })
    }
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<TableStep<Option<NativePatRoute>>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.done {
                return Ok(TableStep::Done);
            }
            if work.remaining() == 0 {
                return Ok(TableStep::Yield);
            }
            // End-of-array control transitions do not debit pretend syntax work.
            if self.phase == 1 && self.index == self.table.source_sections.as_slice().len() {
                self.index = 0;
                self.phase = 2;
                continue;
            }
            if self.phase == 3 && self.index == self.table.programs.as_slice().len() {
                self.phase = 4;
                continue;
            }
            if !work.try_charge(1) {
                return Ok(TableStep::Yield);
            }
            match self.phase {
                0 => {
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
                    self.phase = 1;
                }
                1 => {
                    if let Some(source) = self.table.source_sections.as_slice().get(self.index) {
                        if !source.belongs_to(self.budget) {
                            return Err(Error::ForeignPool);
                        }
                        self.source_bytes = self
                            .source_bytes
                            .checked_add(source.len())
                            .ok_or(Error::Overflow)?;
                        self.index += 1;
                    } else {
                        return Err(Error::Invalid("selected PAT owner cursor"));
                    }
                }
                2 => {
                    if let Some(item) = self.table.source_ranges.as_slice().get(self.index) {
                        let r = item.range;
                        r.end()?;
                        if r.bytes == 0
                            || item.section_number > self.table.header.last_section_number
                            || self.entity.is_some_and(|s| s != r.entity)
                        {
                            return Err(Error::Unsupported(
                                "selected PAT entity/range context unobserved",
                            ));
                        }
                        self.entity = Some(r.entity);
                        self.range_bytes = self
                            .range_bytes
                            .checked_add(r.bytes)
                            .ok_or(Error::Overflow)?;
                        self.index += 1;
                    } else {
                        if self.range_bytes != self.source_bytes {
                            return Err(Error::Invalid("selected PAT physical byte accounting"));
                        }
                        self.index = 0;
                        self.phase = 3;
                    }
                }
                3 => {
                    if let Some(entry) = self.table.programs.as_slice().get(self.index) {
                        if !entry.is_network_pid() {
                            self.programs = self.programs.checked_add(1).ok_or(Error::Overflow)?;
                            if entry.program_number == self.requested {
                                crate::checked_pid(entry.pid)?;
                                if self.selected.replace(entry.pid).is_some() {
                                    return Err(Error::Invalid("selected PAT duplicate program"));
                                }
                            }
                        }
                        self.index += 1;
                    } else {
                        return Err(Error::Invalid("selected PAT entry cursor"));
                    }
                }
                4 => {
                    let route = self.selected.map(|pmt_pid| NativePatRoute {
                        entity: self.entity.expect("validated PAT range"),
                        tsid: self.table.header.extension,
                        program: self.requested,
                        pmt_pid,
                        version: self.table.header.version,
                        context: self.context,
                        programs: self.programs,
                    });
                    if cancel.load(Ordering::Relaxed) {
                        return Err(Error::Cancelled);
                    }
                    self.done = true;
                    return Ok(TableStep::Output(route));
                }
                _ => return Err(Error::Invalid("selected PAT phase")),
            }
        }
    }
}

/// Historical source-map acknowledgement. This scalar value does not claim to
/// own descriptor bytes or a core generation token. Keep the real source Map
/// whenever its raw sections/descriptors are needed after the borrowed job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativePmtBinding {
    pub route: NativePatRoute,
    pub version: u8,
    pub pcr_pid: u16,
    pub scope: NativePesScope,
    pub context: NativeSourceContext,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Initial,
    Rebind,
}

/// Exclusive observer + immutable actual source Map, held only for this bounded
/// transaction. Dropping before commit aborts; no pending input is fabricated.
#[derive(Debug)]
pub struct NativePmtBindingJob<'a> {
    observer: &'a mut NativePesRecoveryObserver,
    map: &'a ProgramMap,
    route: NativePatRoute,
    context: NativeSourceContext,
    mode: Mode,
    phase: u8,
    range: usize,
    bytes: usize,
    previous_end: Option<u64>,
    target: Option<NativePesScope>,
    done: bool,
}
impl NativePesRecoveryObserver {
    pub fn pmt_binding_v1(&self) -> Option<NativePmtBinding> {
        self.pmt_binding
    }
    pub(super) fn after_pmt_binding_v1(&self, c: NativeSourceContext) -> bool {
        self.pmt_binding
            .is_none_or(|b| c.source_event_ordinal > b.context.source_event_ordinal)
    }
    pub fn bind_selected_pmt_v1<'a>(
        &'a mut self,
        route: NativePatRoute,
        map: &'a ProgramMap,
        context: NativeSourceContext,
    ) -> Result<NativePmtBindingJob<'a>> {
        NativePmtBindingJob::begin(self, route, map, context, Mode::Initial)
    }
    pub fn rebind_drained_selected_pmt_v1<'a>(
        &'a mut self,
        route: NativePatRoute,
        map: &'a ProgramMap,
        context: NativeSourceContext,
    ) -> Result<NativePmtBindingJob<'a>> {
        NativePmtBindingJob::begin(self, route, map, context, Mode::Rebind)
    }
}
impl<'a> NativePmtBindingJob<'a> {
    fn begin(
        observer: &'a mut NativePesRecoveryObserver,
        route: NativePatRoute,
        map: &'a ProgramMap,
        context: NativeSourceContext,
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
            phase: 0,
            range: 0,
            bytes: 0,
            previous_end: None,
            target: None,
            done: false,
        })
    }
    pub fn current_scope(&self) -> NativePesScope {
        self.observer.scope()
    }
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<TableStep<NativePmtBinding>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.done {
                return Ok(TableStep::Done);
            }
            if !work.try_charge(1) {
                return Ok(TableStep::Yield);
            }
            match self.phase {
                0 => {
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
                    self.phase = 1;
                }
                1 => {
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
                    self.target = Some(target);
                    self.phase = 2;
                }
                2 => {
                    if let Some(r) = self.map.source_ranges.as_slice().get(self.range) {
                        let end = r.end()?;
                        if r.entity != self.route.entity
                            || r.bytes == 0
                            || self.previous_end.is_some_and(|p| r.offset < p)
                        {
                            return Err(Error::Unsupported(
                                "selected PMT entity/physical ranges unobserved",
                            ));
                        }
                        self.bytes = self.bytes.checked_add(r.bytes).ok_or(Error::Overflow)?;
                        self.previous_end = Some(end);
                        self.range += 1;
                    } else {
                        if self.bytes != self.map.source.len() {
                            return Err(Error::Invalid("selected PMT physical byte accounting"));
                        }
                        self.phase = 3;
                    }
                }
                3 => {
                    let scope = self
                        .target
                        .ok_or(Error::Invalid("selected PMT target missing"))?;
                    let binding = NativePmtBinding {
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
                        self.observer.inner.commit_pmt_scope_v1(scope);
                        self.observer.selected_faults = false;
                    }
                    self.observer.pmt_binding = Some(binding);
                    self.done = true;
                    return Ok(TableStep::Output(binding));
                }
                _ => return Err(Error::Invalid("selected PMT binding phase")),
            }
        }
    }
}
