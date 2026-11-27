//! What follows the first Cluster's start: which metadata the SeekHead points at is
//! still unread, and reading it by range.
use super::range::RangeLeafJob;
use super::*;
use crate::step::check;

/// A level-one element the SeekHead points at.
#[derive(Clone, Copy, Debug)]
struct Target {
    id: u64,
    /// Source offset of the element's ID.
    position: u64,
}
/// Where the resolver is with seek target `at`.
#[expect(
    clippy::large_enum_variant,
    reason = "the one range job is stepped in place; the other phases are a few words"
)]
#[derive(Debug)]
enum Phase {
    /// Taking the next target.
    Next,
    /// Comparing the target with loaded element `lookup`; a loaded one is passed over.
    Cache(Target),
    /// Comparing it with the positions already visited; a repeated one is passed over.
    Seen(Target),
    WaitSeen(Target),
    /// Reserving the slot that records the element as loaded.
    Reserve(Target),
    WaitReserve(Target),
    /// Making room in the catalogue for a document of at most `maximum` payload bytes.
    Catalogue {
        target: Target,
        maximum: usize,
    },
    /// Requesting the range. `maximum` bounds the payload; `None` reads the header
    /// alone, of a Tracks element when the tracks are already known.
    Begin {
        target: Target,
        maximum: Option<usize>,
    },
    Read {
        range: RangeLeafJob,
        header_only: bool,
    },
    /// The stream's transaction holds the element's job.
    Metadata,
    Finish,
}
/// Reads the metadata elements the SeekHead points at that sequential reading has
/// not reached: Tracks behind the Clusters, Cues, Attachments, Chapters and Tags.
#[derive(Debug)]
pub(super) struct Resolver {
    phase: Phase,
    at: usize,
    lookup: usize,
    seen_at: usize,
    seen: CopyVec<u64>,
    // The job the Catalogue phase steps; new whenever that phase is not running.
    prepare: CataloguePrepareJob,
    scanned: u64,
}
impl Resolver {
    pub(super) fn needs_header_input(&self) -> bool {
        matches!(&self.phase, Phase::Read { range, .. } if range.needs_header_input())
    }

    pub(super) fn new(pool: &Budget) -> Self {
        Self {
            phase: Phase::Next,
            at: 0,
            lookup: 0,
            seen_at: 0,
            seen: CopyVec::new(pool),
            prepare: CataloguePrepareJob::new(),
            scanned: 0,
        }
    }
    /// Source bytes read for the targets so far.
    pub(super) fn scanned(&self) -> u64 {
        self.scanned
    }
    fn range(&self) -> Option<&RangeLeafJob> {
        match &self.phase {
            Phase::Read { range, .. } => Some(range),
            _ => None,
        }
    }
    /// One step of reading the targets; `None` once every target is read.
    pub(super) fn step(
        &mut self,
        stream: &mut Stream,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Step<Event, Backpressure>>> {
        loop {
            check(cancel)?;
            match &mut self.phase {
                Phase::Next => {
                    if self.at == stream.seek_targets.len() {
                        self.phase = Phase::Finish;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(Some(Step::Yield));
                    }
                    let (id, position) = stream.seek_targets.as_slice()[self.at];
                    self.lookup = 0;
                    self.seen_at = 0;
                    self.phase = Phase::Cache(Target { id, position });
                }
                Phase::Cache(target) => {
                    let target = *target;
                    if self.lookup == stream.loaded.len() {
                        self.phase = Phase::Seen(target);
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(Some(Step::Yield));
                    }
                    let (old_position, old_id) = stream.loaded.as_slice()[self.lookup];
                    self.lookup += 1;
                    if old_position == target.position {
                        if old_id != target.id {
                            return Err(Error::Invalid("SeekID disagrees with cached target"));
                        }
                        self.at += 1;
                        self.phase = Phase::Next;
                    }
                }
                Phase::Seen(target) => {
                    let target = *target;
                    if self.seen_at == self.seen.len() {
                        self.seen.push(target.position)?;
                        self.phase = Phase::WaitSeen(target);
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(Some(Step::Yield));
                    }
                    let old = self.seen.as_slice()[self.seen_at];
                    self.seen_at += 1;
                    if old == target.position {
                        self.at += 1;
                        self.phase = Phase::Next;
                    }
                }
                Phase::WaitSeen(target) => {
                    let target = *target;
                    match self.seen.step(work, cancel)? {
                        ParseStep::Event(()) => {
                            if stream.segment_end.is_some_and(|end| target.position >= end) {
                                return Err(Error::Invalid("metadata seek outside Segment"));
                            }
                            self.phase = if target.id == TRACKS && !stream.tracks().is_empty() {
                                Phase::Begin {
                                    target,
                                    maximum: None,
                                }
                            } else {
                                Phase::Reserve(target)
                            };
                        }
                        other => return other.waiting().map(Some),
                    }
                }
                Phase::Reserve(target) => {
                    let target = *target;
                    stream.loaded.reserve(1)?;
                    self.phase = Phase::WaitReserve(target);
                }
                Phase::WaitReserve(target) => {
                    let target = *target;
                    match stream.loaded.step(work, cancel)? {
                        ParseStep::Event(()) => {
                            let left = stream
                                .limits
                                .metadata_bytes
                                .checked_sub(stream.metadata_used)
                                .ok_or(Error::Policy("metadata"))?;
                            let maximum = if target.id == crate::metadata::ATTACHMENTS {
                                left.checked_add(
                                    stream
                                        .limits
                                        .attachment_bytes
                                        .checked_sub(stream.catalogue.attachment_bytes())
                                        .ok_or(Error::Policy("attachment payload bytes"))?,
                                )
                                .ok_or(Error::Overflow)?
                            } else {
                                left
                            };
                            self.phase = if document(target.id) {
                                Phase::Catalogue { target, maximum }
                            } else {
                                Phase::Begin {
                                    target,
                                    maximum: Some(maximum),
                                }
                            };
                        }
                        other => return other.waiting().map(Some),
                    }
                }
                Phase::Catalogue { target, maximum } => {
                    match self
                        .prepare
                        .step(&mut stream.catalogue, stream.limits, work, cancel)?
                    {
                        ParseStep::Event(available) => {
                            let (target, maximum) = (*target, (*maximum).min(available));
                            self.prepare = CataloguePrepareJob::new();
                            self.phase = Phase::Begin {
                                target,
                                maximum: Some(maximum),
                            };
                        }
                        other => return other.waiting().map(Some),
                    }
                }
                Phase::Begin { target, maximum } => {
                    if !work.try_charge(1) {
                        return Ok(Some(Step::Yield));
                    }
                    let (target, maximum) = (*target, *maximum);
                    let range = match maximum {
                        None => RangeLeafJob::begin_header_only(
                            stream.envelope.range_input(),
                            target.position,
                            target.id,
                            stream.segment_end,
                        )?,
                        Some(maximum) => RangeLeafJob::begin(
                            stream.envelope.range_input(),
                            target.position,
                            target.id,
                            stream.segment_end,
                            maximum,
                        )?,
                    };
                    self.phase = Phase::Read {
                        range,
                        header_only: maximum.is_none(),
                    };
                }
                Phase::Read { range, header_only } => {
                    let remaining = stream
                        .limits
                        .scan_bytes
                        .checked_sub(self.scanned)
                        .ok_or(Error::Policy("operation read bytes"))?;
                    range.set_read_limit(remaining);
                    let before = stream.position();
                    let step = range.step(stream.envelope.range_input(), work, cancel);
                    let bytes = stream
                        .position()
                        .checked_sub(before)
                        .ok_or(Error::Overflow)?;
                    self.scanned = self.scanned.checked_add(bytes).ok_or(Error::Overflow)?;
                    stream.record_demux_read(before)?;
                    match step? {
                        ParseStep::Event(leaf) => {
                            // The range job goes before the element's own job starts.
                            if *header_only {
                                self.at += 1;
                                self.phase = Phase::Next;
                            } else {
                                self.phase = Phase::Metadata;
                                stream.start_leaf(leaf.header, leaf.data)?;
                            }
                        }
                        other => return other.waiting().map(Some),
                    }
                }
                Phase::Metadata => {
                    if matches!(stream.state, State::Envelope) {
                        self.at += 1;
                        self.phase = Phase::Next;
                        continue;
                    }
                    let step = stream.step_demux(work, cancel, None)?;
                    if matches!(stream.state, State::Envelope) {
                        self.at += 1;
                        self.phase = Phase::Next;
                    }
                    return Ok(Some(step));
                }
                Phase::Finish => {
                    if !work.try_charge(1) {
                        return Ok(Some(Step::Yield));
                    }
                    return Ok(None);
                }
            }
        }
    }
}

/// Which SeekHead targets the first Cluster's start leaves unread.
#[derive(Debug)]
pub(super) struct Opening {
    at: usize,
    loaded: usize,
    checking: bool,
}
/// What a step of the opening check found.
pub(super) enum Opened {
    /// The work budget ran out.
    Pending,
    /// Nothing is left to read: blocks follow.
    Blocks,
    /// Tracks, or a document the SeekHead points at, is still to read by range.
    Unread,
}
impl Opening {
    pub(super) fn new() -> Self {
        Self {
            at: 0,
            loaded: 0,
            checking: false,
        }
    }
    pub(super) fn step(
        &mut self,
        stream: &Stream,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Opened> {
        loop {
            check(cancel)?;
            let random_access = stream.envelope.input_capabilities().random_access;
            if stream.tracks().is_empty() {
                if !random_access {
                    return Err(Error::Unsupported(
                        "Tracks must precede first Cluster on sequential input",
                    ));
                }
                return Ok(Opened::Unread);
            }
            if !random_access || self.at == stream.seek_targets.len() {
                return Ok(Opened::Blocks);
            }
            if !work.try_charge(1) {
                return Ok(Opened::Pending);
            }
            let (id, position) = stream.seek_targets.as_slice()[self.at];
            if !matches!(
                id,
                crate::metadata::ATTACHMENTS | crate::metadata::CHAPTERS | crate::metadata::TAGS
            ) {
                self.at += 1;
                continue;
            }
            if !self.checking {
                self.loaded = 0;
                self.checking = true;
            }
            if self.loaded == stream.loaded.len() {
                return Ok(Opened::Unread);
            }
            if stream.loaded.as_slice()[self.loaded].0 == position {
                self.at += 1;
                self.checking = false;
            } else {
                self.loaded += 1;
            }
        }
    }
}

/// Returns the stream to its first Cluster once the opening metadata is read.
pub(super) fn begin_blocks(stream: &mut Stream, first: &ClusterCheckpoint) -> Result<()> {
    if stream.tracks().is_empty() {
        return Err(Error::Invalid("missing Tracks"));
    }
    stream
        .envelope
        .begin_internal_cluster_range(first, first.offset())?;
    stream.state = State::Envelope;
    stream.cluster_time = None;
    stream.last_location = None;
    Ok(())
}

impl Storage for Resolver {
    fn storage_slots(&self) -> Result<usize> {
        crate::storage::usage::slots(&[&self.seen, &self.prepare, &self.range()])
    }
    fn storage_owner(&self, index: usize) -> Result<Option<crate::storage::usage::Owner>> {
        crate::storage::usage::owner(&[&self.seen, &self.prepare, &self.range()], index)
    }
}
