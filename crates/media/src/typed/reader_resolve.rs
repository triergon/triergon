use super::*;
use crate::matroska::typed::syntax::Required;
use crate::matroska::typed::{range::RangeLeafJob, syntax::check};

#[derive(Clone, Copy, Debug)]
pub(super) enum ResolvePurpose {
    Opening,
    Seek,
}
#[derive(Clone, Copy, Debug)]
enum Phase {
    Next,
    Cache,
    Seen,
    WaitSeen,
    Reserve,
    WaitReserve,
    Catalogue,
    Begin,
    Read,
    Metadata,
    Finish,
}
#[derive(Debug)]
pub(super) struct Resolver {
    purpose: ResolvePurpose,
    phase: Phase,
    at: usize,
    lookup: usize,
    seen_at: usize,
    seen: CopyVec<u64>,
    target: Option<(u64, u64)>,
    header_only: bool,
    maximum: usize,
    prepare: Option<CataloguePrepareJob>,
    range: Option<RangeLeafJob>,
    scanned: u64,
    done: bool,
}
impl Resolver {
    pub(super) fn needs_header_input(&self) -> bool {
        self.range
            .as_ref()
            .is_some_and(RangeLeafJob::needs_header_input)
    }

    pub(super) fn new(purpose: ResolvePurpose, pool: &Budget) -> Self {
        Self {
            purpose,
            phase: Phase::Next,
            at: 0,
            lookup: 0,
            seen_at: 0,
            seen: CopyVec::new(pool),
            target: None,
            header_only: false,
            maximum: 0,
            prepare: None,
            range: None,
            scanned: 0,
            done: false,
        }
    }
}
#[derive(Debug)]
pub(super) struct Opening {
    at: usize,
    loaded: usize,
    checking: bool,
}
impl Opening {
    pub(super) fn new() -> Self {
        Self {
            at: 0,
            loaded: 0,
            checking: false,
        }
    }
}

impl Reader {
    pub(super) fn step_opening(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Event>> {
        loop {
            check(cancel)?;
            if self.tracks().is_empty() {
                if !self.capabilities().random_access {
                    return Err(Error::Unsupported(
                        "Tracks must precede first Cluster on sequential input",
                    ));
                }
                self.opening = None;
                self.resolver = Some(Resolver::new(ResolvePurpose::Opening, &self.pool));
                return Ok(ParseStep::Yield);
            }
            let opening = self
                .opening
                .as_mut()
                .required("missing opening transaction")?;
            if !self.envelope.input_capabilities().random_access
                || opening.at == self.seek_targets.len()
            {
                self.opening = None;
                return Ok(ParseStep::Yield);
            }
            if !work.try_charge(1) {
                return Ok(ParseStep::Yield);
            }
            let (id, position) = self.seek_targets.as_slice()[opening.at];
            if !matches!(
                id,
                crate::metadata::ATTACHMENTS | crate::metadata::CHAPTERS | crate::metadata::TAGS
            ) {
                opening.at += 1;
                continue;
            }
            if !opening.checking {
                opening.loaded = 0;
                opening.checking = true;
            }
            if opening.loaded == self.loaded.len() {
                self.opening = None;
                self.resolver = Some(Resolver::new(ResolvePurpose::Opening, &self.pool));
                return Ok(ParseStep::Yield);
            }
            if self.loaded.as_slice()[opening.loaded].0 == position {
                opening.at += 1;
                opening.checking = false;
            } else {
                opening.loaded += 1;
            }
        }
    }
    pub(super) fn step_resolver(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Event>> {
        let mut resolver = self
            .resolver
            .take()
            .required("missing metadata range resolver")?;
        let result = self.advance_resolver(&mut resolver, work, cancel);
        if !resolver.done {
            self.resolver = Some(resolver);
        }
        result
    }
    fn advance_resolver(
        &mut self,
        resolver: &mut Resolver,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Event>> {
        loop {
            check(cancel)?;
            match resolver.phase {
                Phase::Next => {
                    if resolver.at == self.seek_targets.len() {
                        resolver.phase = Phase::Finish;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    resolver.target = Some(self.seek_targets.as_slice()[resolver.at]);
                    resolver.lookup = 0;
                    resolver.seen_at = 0;
                    resolver.phase = Phase::Cache;
                }
                Phase::Cache => {
                    let (id, position) = resolver.target.required("missing metadata target")?;
                    if resolver.lookup == self.loaded.len() {
                        resolver.phase = Phase::Seen;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let (old_position, old_id) = self.loaded.as_slice()[resolver.lookup];
                    resolver.lookup += 1;
                    if old_position == position {
                        if old_id != id {
                            return Err(Error::Invalid("SeekID disagrees with cached target"));
                        }
                        resolver.at += 1;
                        resolver.phase = Phase::Next;
                    }
                }
                Phase::Seen => {
                    let (_, position) = resolver.target.required("missing metadata target")?;
                    if resolver.seen_at == resolver.seen.len() {
                        resolver.seen.push(position)?;
                        resolver.phase = Phase::WaitSeen;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let old = resolver.seen.as_slice()[resolver.seen_at];
                    resolver.seen_at += 1;
                    if old == position {
                        resolver.at += 1;
                        resolver.phase = Phase::Next;
                    }
                }
                Phase::WaitSeen => match resolver.seen.step(work, cancel)? {
                    ParseStep::Event(()) => {
                        let (id, position) = resolver.target.required("missing metadata target")?;
                        if self.segment_end.is_some_and(|end| position >= end) {
                            return Err(Error::Invalid("metadata seek outside Segment"));
                        }
                        resolver.header_only = id == TRACKS && !self.tracks().is_empty();
                        resolver.phase = if resolver.header_only {
                            Phase::Begin
                        } else {
                            Phase::Reserve
                        };
                    }
                    other => return crate::matroska::typed::pending(other),
                },
                Phase::Reserve => {
                    self.loaded.reserve(1)?;
                    resolver.phase = Phase::WaitReserve;
                }
                Phase::WaitReserve => match self.loaded.step(work, cancel)? {
                    ParseStep::Event(()) => {
                        let (id, _) = resolver.target.required("missing metadata target")?;
                        let left = self
                            .limits
                            .metadata_bytes
                            .checked_sub(self.metadata_used)
                            .ok_or(Error::Limit("metadata"))?;
                        resolver.maximum = if id == crate::metadata::ATTACHMENTS {
                            left.checked_add(
                                self.limits
                                    .attachment_bytes
                                    .checked_sub(self.catalogue.attachment_bytes())
                                    .ok_or(Error::Limit("attachment payload bytes"))?,
                            )
                            .ok_or(Error::Overflow)?
                        } else {
                            left
                        };
                        if document(id) {
                            resolver.prepare = Some(CataloguePrepareJob::new());
                            resolver.phase = Phase::Catalogue;
                        } else {
                            resolver.phase = Phase::Begin;
                        }
                    }
                    other => return crate::matroska::typed::pending(other),
                },
                Phase::Catalogue => match resolver
                    .prepare
                    .as_mut()
                    .required("missing ranged catalogue admission")?
                    .step(&mut self.catalogue, self.limits, work, cancel)?
                {
                    ParseStep::Event(available) => {
                        resolver.maximum = resolver.maximum.min(available);
                        resolver.prepare = None;
                        resolver.phase = Phase::Begin;
                    }
                    other => return crate::matroska::typed::pending(other),
                },
                Phase::Begin => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let (id, position) = resolver.target.required("missing metadata target")?;
                    resolver.range = Some(if resolver.header_only {
                        RangeLeafJob::begin_header_only(
                            self.envelope.range_input(),
                            position,
                            id,
                            self.segment_end,
                        )?
                    } else {
                        RangeLeafJob::begin(
                            self.envelope.range_input(),
                            position,
                            id,
                            self.segment_end,
                            resolver.maximum,
                        )?
                    });
                    resolver.phase = Phase::Read;
                }
                Phase::Read => {
                    let remaining = self
                        .limits
                        .scan_bytes
                        .checked_sub(resolver.scanned)
                        .ok_or(Error::Limit("operation read bytes"))?;
                    let range = resolver
                        .range
                        .as_mut()
                        .required("missing metadata range job")?;
                    range.set_read_limit(remaining);
                    let before = self.position();
                    let step = range.step(self.envelope.range_input(), work, cancel);
                    let bytes = self.position().checked_sub(before).ok_or(Error::Overflow)?;
                    resolver.scanned =
                        resolver.scanned.checked_add(bytes).ok_or(Error::Overflow)?;
                    self.record_demux_read(before)?;
                    match step? {
                        ParseStep::Event(leaf) => {
                            resolver.range = None;
                            if resolver.header_only {
                                resolver.at += 1;
                                resolver.phase = Phase::Next;
                            } else {
                                self.start_leaf(leaf.header, leaf.data)?;
                                resolver.phase = Phase::Metadata;
                            }
                        }
                        other => return crate::matroska::typed::pending(other),
                    }
                }
                Phase::Metadata => {
                    if matches!(self.state, State::Envelope) {
                        resolver.at += 1;
                        resolver.phase = Phase::Next;
                        continue;
                    }
                    let step = self.step_demux(work, cancel, None)?;
                    if matches!(self.state, State::Envelope) {
                        resolver.at += 1;
                        resolver.phase = Phase::Next;
                    }
                    return Ok(step);
                }
                Phase::Finish => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    match resolver.purpose {
                        ResolvePurpose::Opening => {
                            if self.tracks().is_empty() {
                                return Err(Error::Invalid("missing Tracks"));
                            }
                            let first = self
                                .first_cluster
                                .as_ref()
                                .required("missing first Cluster")?;
                            self.envelope
                                .begin_internal_cluster_range(first, first.offset())?;
                            self.state = State::Envelope;
                            self.cluster_time = None;
                            self.last_location = None;
                            self.opening = None;
                        }
                        ResolvePurpose::Seek => {
                            self.seek_metadata_bytes = resolver.scanned;
                            self.start_seek_plan()?;
                        }
                    }
                    resolver.done = true;
                    return Ok(ParseStep::Yield);
                }
            }
        }
    }
}

crate::matroska::typed::usage::storage!(Resolver, |s| [s.seen, s.prepare, s.range]);
