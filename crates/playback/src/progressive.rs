//! Sequential MP4 continuation through real source box/fragment/sample jobs.
//! A drained current MDAT is a batch boundary, never an invented media EOF.
use crate::{
    BackendStep, Mp4NativeEmptyPacket, RawEvent, Result,
    mp4_open::{PendingMedia, RETAINED_MEDIA_BYTES, box_reader, word},
    mp4_plan::{Mp4View, PreparedMp4, SourceRow},
    sample_sort::SampleSort,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{BudgetVec, Error, InputCursor, Step, WorkBudget};
use triergon_isobmff::{
    self as iso, BoxChildren, BoxEvent, BoxReader, BoxStep, BoxView, BuildStatus, FragmentBuilder,
    FragmentContext, ImplicitBase, OwnedFragmentCursor, SampleStep,
};

#[derive(Clone, Copy, Debug)]
pub struct ProgressiveLimits {
    pub metadata_bytes: usize,
    pub roots: u64,
    pub fragments_per_batch: usize,
    pub samples_per_batch: usize,
    pub format: iso::Limits,
}
impl Default for ProgressiveLimits {
    fn default() -> Self {
        Self {
            metadata_bytes: 16 << 20,
            roots: 1_000_000,
            fragments_per_batch: 64,
            samples_per_batch: 65_536,
            format: iso::Limits::default(),
        }
    }
}
enum SourceCursor {
    Physical(OwnedFragmentCursor),
    Native(iso::compat::mov::front::FrontFragment),
}
impl SourceCursor {
    fn index(&self) -> &iso::FragmentIndex {
        match self {
            Self::Physical(c) => c.index(),
            Self::Native(c) => c.index(),
        }
    }
}
struct Cursor {
    cursor: SourceCursor,
    head: Option<SourceRow>,
    native: Option<iso::compat::mov::front::FrontSample>,
}
struct PendingRow {
    source: SourceRow,
    native: Option<iso::compat::mov::front::FrontSample>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Drain,
    TerminalDrain,
    Roots,
    Traf,
    Merge,
    PrepareReplay,
    CopySource,
    SortSource,
    Admit,
}
/// Borrow the MP4 fields needed to continue reading the source.
pub(crate) struct Continuation<'a> {
    pub(crate) input: &'a mut InputCursor,
    pub(crate) plan: &'a mut PreparedMp4,
    pub(crate) row: &'a mut usize,
    pub(crate) replay_media: &'a mut Option<PendingMedia>,
    pub(crate) native_empty_run: &'a mut Option<(Mp4NativeEmptyPacket, u32)>,
}

pub(crate) fn prepare_media_replay(
    input: &InputCursor,
    plan: &mut PreparedMp4,
) -> Result<PendingMedia> {
    let [extent] = plan.media.as_slice() else {
        return Err(Error::Unsupported(
            "native replay needs a finite media extent",
        ));
    };
    if input.position() != extent.start || input.capabilities().random_access {
        return Err(Error::Invalid("native replay source position"));
    }
    let length = extent.end - extent.start;
    if !(1..=RETAINED_MEDIA_BYTES as u64).contains(&length) {
        return Err(Error::Unsupported(
            "native replay exceeds media cache bound",
        ));
    }
    let pending = PendingMedia::new(extent.start, length as usize, input.budget())?;
    plan.media_cache.try_reserve_exact(1)?;
    plan.replay_native_order = true;
    Ok(pending)
}

pub(crate) struct ProgressiveState {
    limits: ProgressiveLimits,
    phase: Phase,
    next_root: Option<u64>,
    roots: u64,
    regular_media_seen: usize,
    reader: Option<BoxReader>,
    header: Option<iso::BoxHeader>,
    moof: Option<BoxView>,
    children: Option<BoxChildren>,
    traf: Option<BoxView>,
    scan: Option<BoxChildren>,
    builder: Option<FragmentBuilder>,
    track: usize,
    previous_traf: Option<u64>,
    cursors: BudgetVec<Option<Cursor>>,
    retained: usize,
    pending_cursor: Option<Cursor>,
    fill: usize,
    select: usize,
    best: Option<usize>,
    native_next: [u64; 64],
    cursor_tracks: [u8; 64],
    blocked_in_extent: bool,
    needs_reorder: bool,
    sort: Option<SampleSort>,
    rows: BudgetVec<PendingRow>,
    row: usize,
    extent: Option<std::ops::Range<u64>>,
    terminal: Option<iso::TerminalMdat>,
    finished: bool,
}
impl ProgressiveState {
    pub(crate) fn new(plan: &PreparedMp4, limits: ProgressiveLimits) -> Result<Self> {
        if plan.view != Mp4View::SourcePhysical && !plan.native_front {
            return Err(Error::Unsupported(
                "progressive native MOV context is not established",
            ));
        }
        if limits.metadata_bytes < 8
            || limits.roots == 0
            || limits.fragments_per_batch == 0
            || limits.fragments_per_batch > 64
            || limits.samples_per_batch == 0
        {
            return Err(Error::Invalid("progressive MP4 policy"));
        }
        let crate::mp4_open::MetadataReadScope::FrontMetadataBeforeMdat(h) = plan.read_scope else {
            return Err(Error::Invalid(
                "progressive attachment needs actual front-MDAT boundary",
            ));
        };
        let next_root = h.end()?;
        let terminal = if next_root.is_none() {
            Some(iso::TerminalMdat::new(h)?)
        } else {
            None
        };
        // A policy maximum is not retained media. Slots become charged only
        // when a real source cursor needs one; completed holes are reused.
        let cursors = BudgetVec::new(&plan.pool);
        Ok(Self {
            limits,
            phase: if terminal.is_some() {
                Phase::TerminalDrain
            } else {
                Phase::Drain
            },
            next_root,
            roots: 0,
            regular_media_seen: 1,
            reader: None,
            header: None,
            moof: None,
            children: None,
            traf: None,
            scan: None,
            builder: None,
            track: 0,
            previous_traf: None,
            cursors,
            retained: 0,
            pending_cursor: None,
            fill: 0,
            select: 0,
            best: None,
            native_next: [0; 64],
            cursor_tracks: [0; 64],
            blocked_in_extent: false,
            needs_reorder: false,
            sort: None,
            rows: BudgetVec::new(&plan.pool),
            row: 0,
            extent: None,
            terminal,
            finished: false,
        })
    }
    pub(crate) fn poll(
        &mut self,
        engine: &mut Continuation<'_>,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<BackendStep> {
        if c.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if w.remaining() == 0 {
            return Ok(BackendStep::Yield);
        }
        if self.finished {
            return Ok(BackendStep::End);
        }
        match self.phase {
            Phase::Drain => {
                let next_root = self
                    .next_root
                    .ok_or(Error::Invalid("terminal MDAT has no following root"))?;
                if engine.input.position() > next_root {
                    return Err(Error::Invalid("sample crossed following root"));
                }
                if engine.input.position() < next_root {
                    let n = usize::try_from(
                        (next_root - engine.input.position()).min(usize::MAX as u64),
                    )
                    .map_err(|_| Error::Overflow)?;
                    return Ok(match engine.input.read_discard(n, w, c)? {
                        Step::NeedInput(r) => BackendStep::NeedInput(r),
                        Step::Ready(_) | Step::Yield => BackendStep::Yield,
                        Step::End(_) => return Err(Error::Truncated),
                    });
                }
                if !w.try_charge(1) {
                    return Ok(BackendStep::Yield);
                }
                self.reader = Some(box_reader(
                    engine.input,
                    self.limits.metadata_bytes,
                    engine.plan.native_recovery,
                )?);
                self.phase = Phase::Roots;
            }
            Phase::TerminalDrain => {
                if self.terminal.is_none() || self.retained != 0 || self.row != self.rows.len() {
                    return Err(Error::Invalid(
                        "terminal media has unadmitted source samples",
                    ));
                }
                // A bounded discard request, never a fabricated final range or
                // a search for box signatures inside opaque media payload.
                match engine.input.read_discard(64 * 1024, w, c)? {
                    Step::Ready(_) | Step::Yield => {}
                    Step::NeedInput(request) => return Ok(BackendStep::NeedInput(request)),
                    Step::End(_) => {
                        self.finished = true;
                        return Ok(BackendStep::End);
                    }
                }
            }
            Phase::Roots => {
                let reader = self
                    .reader
                    .as_mut()
                    .ok_or(Error::Invalid("progressive root reader"))?;
                if let Some(mut h) = self.header {
                    if !w.try_charge(1) {
                        return Ok(BackendStep::Yield);
                    }
                    if h.kind == *b"mdat" {
                        if engine.plan.native_recovery && reader.announced_open_mdat() {
                            h.size = None;
                        }
                        if let Some(maximum) = engine.plan.forward_regular_extents
                            && *engine.row < engine.plan.rows()
                        {
                            let next_root = h.end()?;
                            let seen = self
                                .regular_media_seen
                                .checked_add(1)
                                .ok_or(Error::Overflow)?;
                            if seen > maximum {
                                return Err(Error::Limit {
                                    requested: seen,
                                    maximum,
                                });
                            }
                            if next_root.is_some() {
                                let [current] = engine.plan.media.as_mut_slice() else {
                                    return Err(Error::Invalid(
                                        "forward regular live media extent",
                                    ));
                                };
                                *current = h.payload_range()?;
                                self.phase = Phase::Drain;
                            } else {
                                let terminal = iso::TerminalMdat::new(h)?;
                                let row = engine
                                    .plan
                                    .row(*engine.row)
                                    .ok_or(Error::Invalid("pending forward regular sample"))?;
                                terminal.require_sample(&row.sample)?;
                                // The plan keeps all deferred rows. SampleReader
                                // now verifies their actual bytes against EOF.
                                engine.plan.media.clear();
                                engine.plan.forward_regular_extents = None;
                                engine.plan.terminal_mdat = Some(terminal);
                                self.terminal = Some(terminal);
                                self.phase = Phase::TerminalDrain;
                            }
                            self.next_root = next_root;
                            self.regular_media_seen = seen;
                            engine.plan.read_scope =
                                crate::mp4_open::MetadataReadScope::FrontMetadataBeforeMdat(h);
                            self.reader = None;
                            self.header = None;
                            return Ok(BackendStep::Yield);
                        }
                        engine.plan.forward_regular_extents = None;
                        self.next_root = h.end()?;
                        self.terminal = if self.next_root.is_none() {
                            Some(iso::TerminalMdat::new(h)?)
                        } else {
                            None
                        };
                        self.extent = if self.terminal.is_some() {
                            None
                        } else {
                            Some(h.payload_range()?)
                        };
                        engine.plan.source_rows.clear();
                        engine.plan.native_rows.clear();
                        engine.plan.media_cache.clear();
                        engine.plan.replay_native_order = false;
                        engine.plan.media.clear();
                        if let Some(extent) = self.extent.clone() {
                            engine.plan.media.try_push(extent).map_err(|e| e.error)?;
                        }
                        engine.plan.terminal_mdat = self.terminal;
                        engine.plan.read_scope =
                            crate::mp4_open::MetadataReadScope::FrontMetadataBeforeMdat(h);
                        *engine.row = 0;
                        self.rows.clear();
                        self.row = 0;
                        self.fill = 0;
                        self.select = 0;
                        self.best = None;
                        self.blocked_in_extent = false;
                        self.needs_reorder = false;
                        self.native_next.fill(0);
                        if engine.plan.native_front && engine.plan.native_front_boundary.is_none() {
                            for (next, track) in self
                                .native_next
                                .iter_mut()
                                .zip(engine.plan.front_tracks.as_slice())
                            {
                                *next = track.next_source_ordinal()?;
                            }
                        }
                        self.phase = Phase::Merge;
                    } else {
                        if let Some(row) = engine.plan.row(*engine.row) {
                            if h.end()?.is_some_and(|end| row.sample.range.start < end) {
                                return Err(Error::Invalid("sample outside media extent"));
                            }
                            if h.kind == *b"moof" {
                                return Err(Error::Unsupported(
                                    "fragment before forward regular samples finish",
                                ));
                            }
                        }
                        // Keep the announced header until allocation succeeds.
                        reader.read_metadata(engine.input)?;
                    }
                    self.header = None;
                    return Ok(BackendStep::Yield);
                }
                match reader.poll(engine.input, w, c)? {
                    Step::Yield => {}
                    Step::NeedInput(r) => return Ok(BackendStep::NeedInput(r)),
                    Step::End(_) => {
                        if *engine.row != engine.plan.rows()
                            || self.row != self.rows.len()
                            || self.retained != 0
                        {
                            return Err(Error::Truncated);
                        }
                        self.finished = true;
                        return Ok(BackendStep::End);
                    }
                    Step::Ready(BoxEvent::Header(h)) => {
                        self.roots = self.roots.checked_add(1).ok_or(Error::Overflow)?;
                        if self.roots > self.limits.roots {
                            return Err(Error::Limit {
                                requested: usize::try_from(self.roots)
                                    .map_err(|_| Error::Overflow)?,
                                maximum: usize::try_from(self.limits.roots)
                                    .map_err(|_| Error::Overflow)?,
                            });
                        }
                        self.header = Some(h);
                    }
                    Step::Ready(BoxEvent::Metadata(raw)) => {
                        if raw.header().kind == *b"moof" {
                            // Retain source cursors across later moofs; merge only at actual media.
                            self.previous_traf = None;
                            self.children = Some(raw.children(0, self.limits.format)?);
                            self.moof = Some(raw.clone());
                            self.phase = Phase::Traf;
                        } else if raw.header().kind == *b"moov" {
                            return Err(Error::Unsupported(
                                "new movie catalogue requires explicit source discontinuity",
                            ));
                        }
                        return Ok(BackendStep::Event(RawEvent::Mp4Root(raw)));
                    }
                    Step::Ready(BoxEvent::Skipped(_)) => {}
                }
            }
            Phase::Traf => {
                if let Some(cursor) = self.pending_cursor.take() {
                    if !w.try_charge(1) {
                        self.pending_cursor = Some(cursor);
                        return Ok(BackendStep::Yield);
                    }
                    let end = cursor.cursor.index().implicit_data_end()?;
                    let next = engine.plan.tracks.as_slice()[self.track]
                        .next_ordinal
                        .checked_add(cursor.cursor.index().sample_count())
                        .ok_or(Error::Overflow)?;
                    let slot = if let Some(at) =
                        self.cursors.as_slice().iter().position(Option::is_none)
                    {
                        self.cursors.as_mut_slice()[at] = Some(cursor);
                        at
                    } else {
                        if self.cursors.len() >= self.limits.fragments_per_batch {
                            self.pending_cursor = Some(cursor);
                            return Err(Error::Limit {
                                requested: self.retained + 1,
                                maximum: self.limits.fragments_per_batch,
                            });
                        }
                        // Reserve before publishing either the cursor or its
                        // track ordinal/end. Growth refusal keeps the exact
                        // completed index available for a retry, without parsing
                        // the source again or skipping any declared sample.
                        if let Err(error) = self.cursors.try_reserve_exact(1) {
                            self.pending_cursor = Some(cursor);
                            return Err(error);
                        }
                        if let Err(failure) = self.cursors.try_push(Some(cursor)) {
                            self.pending_cursor = failure.value;
                            return Err(failure.error);
                        }
                        self.cursors.len() - 1
                    };
                    self.cursor_tracks[slot] = self.track as u8;
                    self.retained += 1;
                    self.previous_traf = Some(end);
                    engine.plan.tracks.as_mut_slice()[self.track].next_ordinal = next;
                    self.scan = None;
                    self.traf = None;
                    return Ok(BackendStep::Yield);
                }
                if let Some(job) = self.builder.as_mut() {
                    if job.poll(w)? == BuildStatus::Ready {
                        let index = self
                            .builder
                            .take()
                            .ok_or(Error::Invalid("fragment job"))?
                            .finish()?;
                        self.pending_cursor = Some(Cursor {
                            cursor: if engine.plan.native_front {
                                SourceCursor::Native(iso::compat::mov::front::FrontFragment::new(
                                    index,
                                ))
                            } else {
                                SourceCursor::Physical(index.into_cursor())
                            },
                            head: None,
                            native: None,
                        });
                    }
                    return Ok(BackendStep::Yield);
                }
                if let Some(scan) = self.scan.as_mut() {
                    match scan.poll(w)? {
                        BoxStep::Yield => {}
                        BoxStep::End => {
                            return Err(Error::Invalid("progressive traf missing tfhd"));
                        }
                        BoxStep::Box(raw) => {
                            if raw.header().kind == *b"tfhd" {
                                let id = word(raw.payload(), 4)?;
                                let at = engine
                                    .plan
                                    .tracks
                                    .as_slice()
                                    .iter()
                                    .position(|t| t.snapshot.track_id == id)
                                    .ok_or(Error::Invalid(
                                        "progressive fragment unknown source track",
                                    ))?;
                                let track = &engine.plan.tracks.as_slice()[at];
                                let trex = track
                                    .trex
                                    .ok_or(Error::Invalid("progressive fragment missing trex"))?;
                                let raw = self
                                    .traf
                                    .as_ref()
                                    .ok_or(Error::Invalid("progressive traf"))?
                                    .clone();
                                let context = FragmentContext {
                                    moof_start: self
                                        .moof
                                        .as_ref()
                                        .ok_or(Error::Invalid("moof"))?
                                        .header()
                                        .start,
                                    implicit_base: self.previous_traf.map_or(
                                        ImplicitBase::FirstTraf,
                                        ImplicitBase::PreviousTraf,
                                    ),
                                    decode_origin: None,
                                    first_ordinal: track.next_ordinal,
                                    description_count: u32::try_from(track.descriptions.len())
                                        .map_err(|_| Error::Overflow)?,
                                };
                                self.track = at;
                                self.builder = Some(FragmentBuilder::new(
                                    raw,
                                    trex,
                                    context,
                                    engine.input.budget().clone(),
                                    self.limits.format,
                                )?);
                            }
                        }
                    }
                    return Ok(BackendStep::Yield);
                }
                match self
                    .children
                    .as_mut()
                    .ok_or(Error::Invalid("moof children"))?
                    .poll(w)?
                {
                    BoxStep::Yield => {}
                    BoxStep::Box(raw) => {
                        if raw.header().kind == *b"traf" {
                            if self.retained >= self.limits.fragments_per_batch {
                                return Err(Error::Limit {
                                    requested: self.retained + 1,
                                    maximum: self.limits.fragments_per_batch,
                                });
                            }
                            self.scan = Some(raw.children(0, self.limits.format)?);
                            self.traf = Some(raw);
                        }
                    }
                    BoxStep::End => {
                        self.children = None;
                        self.moof = None;
                        self.phase = Phase::Roots;
                    }
                }
            }
            Phase::Merge => {
                if self.fill < self.cursors.len() {
                    let Some(current) = self.cursors.as_mut_slice()[self.fill].as_mut() else {
                        self.fill += 1;
                        return Ok(BackendStep::Yield);
                    };
                    if current.head.is_some() {
                        self.fill += 1;
                        return Ok(BackendStep::Yield);
                    }
                    let step = match &mut current.cursor {
                        SourceCursor::Physical(cursor) => cursor.poll(w)?,
                        SourceCursor::Native(cursor) => match cursor.poll(w, c)? {
                            iso::compat::mov::packets::CursorStatus::Yield => SampleStep::Yield,
                            iso::compat::mov::packets::CursorStatus::End => SampleStep::End,
                            iso::compat::mov::packets::CursorStatus::Ready => {
                                let head = cursor.take_sample()?;
                                let source = head.source().clone();
                                current.native = Some(head);
                                SampleStep::Sample(source)
                            }
                        },
                    };
                    match step {
                        SampleStep::Yield => {}
                        SampleStep::End => {
                            self.cursors.as_mut_slice()[self.fill] = None;
                            self.retained -= 1;
                            self.fill += 1;
                        }
                        SampleStep::Sample(sample) => {
                            current.head = Some(SourceRow {
                                track_id: current.cursor.index().track_id(),
                                sample,
                            });
                            self.fill += 1;
                        }
                    }
                    return Ok(BackendStep::Yield);
                }
                if self.select < self.cursors.len() {
                    if !w.try_charge(1) {
                        return Ok(BackendStep::Yield);
                    }
                    let at = self.select;
                    self.select += 1;
                    if let Some(cursor) = self.cursors.as_slice()[at].as_ref()
                        && let Some(head) = cursor.head.as_ref()
                    {
                        let expected = self.native_next[usize::from(self.cursor_tracks[at])];
                        if expected != 0 && head.sample.ordinal != expected {
                            self.blocked_in_extent |= self.extent.as_ref().is_none_or(|extent| {
                                head.sample.range.start < extent.end
                                    || (head.sample.range.start == extent.end
                                        && head.sample.range.is_empty())
                            });
                        } else if self.best.is_none_or(|old| {
                            self.cursors.as_slice()[old]
                                .as_ref()
                                .and_then(|cursor| cursor.head.as_ref())
                                .is_some_and(|r| head.physical_key() < r.physical_key())
                        }) {
                            self.best = Some(at);
                        }
                    }
                    return Ok(BackendStep::Yield);
                }
                if !w.try_charge(1) {
                    return Ok(BackendStep::Yield);
                }
                if let Some(at) = self.best.take() {
                    let sample = &self.cursors.as_slice()[at]
                        .as_ref()
                        .and_then(|cursor| cursor.head.as_ref())
                        .ok_or(Error::Invalid("pending merge head"))?
                        .sample;
                    if let Some(extent) = self.extent.as_ref() {
                        if sample.range.start > extent.end
                            || (sample.range.start == extent.end && !sample.range.is_empty())
                        {
                            self.finish_merge(engine.plan)?;
                            return Ok(BackendStep::Yield);
                        }
                        sample.require_within(extent.clone())?;
                    } else if let Some(terminal) = self.terminal {
                        terminal.require_sample(sample)?;
                    } else {
                        return Err(Error::Invalid("pending media extent"));
                    }
                    if self.rows.len() >= self.limits.samples_per_batch {
                        return Err(Error::Limit {
                            requested: self.rows.len() + 1,
                            maximum: self.limits.samples_per_batch,
                        });
                    }
                    let cursor = self.cursors.as_mut_slice()[at]
                        .as_mut()
                        .ok_or(Error::Invalid("pending cursor"))?;
                    let row = PendingRow {
                        source: cursor.head.take().ok_or(Error::Invalid("merge head"))?,
                        native: cursor.native.take(),
                    };
                    let track = usize::from(self.cursor_tracks[at]);
                    let next = if self.native_next[track] == 0 {
                        0
                    } else {
                        row.source
                            .sample
                            .ordinal
                            .checked_add(1)
                            .ok_or(Error::Overflow)?
                    };
                    let backwards = self.rows.as_slice().last().is_some_and(|last| {
                        let before = &last.source.sample.range;
                        let after = &row.source.sample.range;
                        after.start
                            < if self.native_next[track] == 0 {
                                before.start
                            } else {
                                before.end
                            }
                    });
                    if let Err(error) = self.rows.try_push(row) {
                        let cursor = self.cursors.as_mut_slice()[at]
                            .as_mut()
                            .ok_or(Error::Invalid("pending cursor"))?;
                        cursor.head = Some(error.value.source);
                        cursor.native = error.value.native;
                        self.best = Some(at);
                        return Err(error.error);
                    }
                    self.native_next[track] = next;
                    self.needs_reorder |= backwards;
                    self.blocked_in_extent = false;
                    self.fill = at;
                    self.select = 0;
                } else {
                    self.moof = None;
                    self.finish_merge(engine.plan)?;
                }
            }
            Phase::PrepareReplay => {
                *engine.replay_media = Some(prepare_media_replay(engine.input, engine.plan)?);
                self.phase = Phase::CopySource;
            }
            Phase::CopySource => {
                if let Some(row) = self.rows.as_slice().get(self.row) {
                    if w.try_charge(1) {
                        engine
                            .plan
                            .source_rows
                            .try_push(row.source.clone())
                            .map_err(|e| e.error)?;
                        self.row += 1;
                    }
                } else {
                    self.row = 0;
                    self.sort = Some(SampleSort::new(engine.plan.source_rows.len()));
                    self.phase = Phase::SortSource;
                }
            }
            Phase::SortSource => {
                let sort = self
                    .sort
                    .as_mut()
                    .ok_or(Error::Invalid("media order sort missing"))?;
                let done = if engine.plan.replay_native_order {
                    sort.poll(
                        engine.plan.source_rows.as_mut_slice(),
                        SourceRow::physical_key,
                        w,
                    )
                } else {
                    sort.poll(self.rows.as_mut_slice(), |row| row.source.physical_key(), w)
                };
                if done {
                    self.sort = None;
                    self.phase = Phase::Admit;
                }
            }
            Phase::Admit => {
                if !w.try_charge(1) {
                    return Ok(BackendStep::Yield);
                }
                if let Some(row) = self.rows.as_slice().get(self.row) {
                    if let Some(extent) = self.extent.as_ref() {
                        row.source.sample.require_within(extent.clone())?;
                    } else if let Some(terminal) = self.terminal {
                        terminal.require_sample(&row.source.sample)?;
                    } else {
                        return Err(Error::Invalid("live media extent"));
                    }
                    // Reserve both owners before committing either a source row
                    // or the separate native clock. Retry keeps the exact head.
                    if !engine.plan.replay_native_order {
                        engine.plan.source_rows.try_reserve_exact(1)?;
                    }
                    if engine.plan.native_front && engine.plan.native_front_boundary.is_none() {
                        let at = engine
                            .plan
                            .tracks
                            .as_slice()
                            .iter()
                            .position(|t| t.snapshot.track_id == row.source.track_id)
                            .ok_or(Error::Invalid("front continuation track"))?;
                        let head = row
                            .native
                            .as_ref()
                            .ok_or(Error::Invalid("front native head"))?;
                        let duration = engine
                            .plan
                            .native_empty_duration
                            .filter(|_| head.source().composition_offset.is_none());
                        let boundary = if duration.is_some() {
                            head.zero_read_boundary()
                        } else {
                            head.read_boundary_in_media(self.row + 1 < self.rows.len())?
                        };
                        let (next, native) =
                            engine.plan.front_tracks.as_slice()[at].project(head)?;
                        let next_duration = engine.plan.next_native_empty_duration(
                            &native,
                            boundary.is_none() && head.remaining_in_run() == 0,
                        );
                        let base = native.media_clock.demux_base;
                        engine.plan.native_rows.try_reserve_exact(1)?;
                        engine
                            .plan
                            .native_rows
                            .try_push(native)
                            .map_err(|e| e.error)?;
                        engine.plan.front_tracks.as_mut_slice()[at] = next;
                        engine.plan.native_front_boundary = boundary;
                        engine.plan.native_empty_duration = next_duration;
                        if let (Some(boundary), Some(duration)) = (boundary, duration)
                            && head.remaining_in_run() != 0
                        {
                            *engine.native_empty_run = Some((
                                Mp4NativeEmptyPacket {
                                    boundary,
                                    time_base: base,
                                    handler: *b"vide",
                                    pts: 0,
                                    dts: 0,
                                    duration,
                                    position: 0,
                                    flags: 0,
                                },
                                head.remaining_in_run(),
                            ));
                        }
                    }
                    if !engine.plan.replay_native_order {
                        engine
                            .plan
                            .source_rows
                            .try_push(row.source.clone())
                            .map_err(|e| e.error)?;
                    }
                    self.row += 1;
                    return Ok(BackendStep::Yield);
                }
                self.reader = None;
                self.extent = None;
                self.phase = if self.terminal.is_some() {
                    Phase::TerminalDrain
                } else {
                    Phase::Drain
                };
            }
        }
        Ok(BackendStep::Yield)
    }

    fn finish_merge(&mut self, plan: &PreparedMp4) -> Result<()> {
        if self.blocked_in_extent {
            return Err(Error::Unsupported(
                "native continuation source ordinal gap or reorder",
            ));
        }
        self.phase = if !self.needs_reorder {
            Phase::Admit
        } else if plan.native_front && plan.native_front_boundary.is_none() {
            Phase::PrepareReplay
        } else {
            self.sort = Some(SampleSort::new(self.rows.len()));
            Phase::SortSource
        };
        Ok(())
    }
}
