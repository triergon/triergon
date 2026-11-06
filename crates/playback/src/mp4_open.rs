//! Original bounded composition of the public ISO box/table/fragment readers.
use crate::input::Source;
use crate::{
    Format, Result,
    io::{SelectedInput, SourceOwner, provide},
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, BudgetVec, Bytes, Error, InputCursor, Step, WorkBudget};
use triergon_isobmff::{
    self as iso, BoxChildren, BoxEvent, BoxReader, BoxStep, BoxView, BuildStatus, EditList,
    FragmentBuilder, FragmentContext, FragmentIndex, ImplicitBase, MovieHeader, SampleTable,
    SampleTableBuilder, TimingRunPolicy, TrackHeader, Trex,
};
fn push<T>(v: &mut BudgetVec<T>, x: T) -> Result<()> {
    v.try_push(x).map_err(|e| e.error)
}
pub(crate) fn box_reader(
    input: &InputCursor,
    metadata_bytes: usize,
    native_recovery: bool,
) -> Result<BoxReader> {
    if native_recovery {
        BoxReader::new_pinned_mov_recovery(
            input,
            metadata_bytes,
            iso::PinnedMovD32RegularRecoveryV1,
        )
    } else {
        BoxReader::new(input, metadata_bytes)
    }
}
fn required<T>(v: Option<T>, s: &'static str) -> Result<T> {
    v.ok_or(Error::Invalid(s))
}
fn unique(slot: &mut Option<BoxView>, raw: BoxView) -> Result<()> {
    if slot.is_some() {
        return Err(Error::Invalid("duplicate structural movie box"));
    }
    *slot = Some(raw);
    Ok(())
}
fn validate_quicktime_data_handler(raw: &BoxView) -> Result<()> {
    // QTFF Handler reference atom with a counted component name.
    let (fields, name) = raw.payload().split_at_checked(24).ok_or(Error::Truncated)?;
    let (&name_length, name) = name.split_first().ok_or(Error::Truncated)?;
    if fields[..4] != [0; 4]
        || fields[4..8] != *b"dhlr"
        || fields[12..24] != [0; 12]
        || name.len() != usize::from(name_length)
    {
        return Err(Error::Invalid("QuickTime data handler"));
    }
    // Only self-contained URL data references are supported by this opener.
    if fields[8..12] != *b"url " {
        return Err(Error::Unsupported("QuickTime data handler reference type"));
    }
    Ok(())
}
pub(crate) fn word(b: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        b.get(at..at.checked_add(4).ok_or(Error::Overflow)?)
            .ok_or(Error::Truncated)?
            .try_into()
            .map_err(|_| Error::Truncated)?,
    ))
}
#[derive(Clone, Copy, Debug)]
pub struct OpenLimits {
    pub metadata_bytes: usize,
    pub roots: usize,
    pub tracks: usize,
    pub depth: usize,
    pub fragments: usize,
    pub format: iso::Limits,
}
impl Default for OpenLimits {
    fn default() -> Self {
        Self {
            metadata_bytes: 16 * 1024 * 1024,
            roots: 4096,
            tracks: 64,
            depth: 12,
            fragments: 4096,
            format: iso::Limits::default(),
        }
    }
}
#[derive(Default)]
struct Parts {
    tkhd: Option<BoxView>,
    mdhd: Option<BoxView>,
    hdlr: Option<BoxView>,
    stbl: Option<BoxView>,
    elst: Option<BoxView>,
    dref: Option<BoxView>,
    data_handler_seen: bool,
}
struct Frame {
    children: BoxChildren,
    track: Option<usize>,
    kind: [u8; 4],
    data_info_seen: bool,
}
struct Walk {
    frames: BudgetVec<Option<Frame>>,
    depth: usize,
    quicktime: bool,
    parts: BudgetVec<Parts>,
    movie: Option<MovieHeader>,
    trex: BudgetVec<Trex>,
    seen: u32,
    pending: Option<(BoxView, Option<usize>)>,
}
impl Walk {
    fn new(
        root: &BoxView,
        b: &Budget,
        l: OpenLimits,
        major_brand: Option<[u8; 4]>,
    ) -> Result<Self> {
        let mut frames = BudgetVec::new(b);
        frames.try_reserve_exact(l.depth)?;
        for _ in 0..l.depth {
            push(&mut frames, None)?;
        }
        frames.as_mut_slice()[0] = Some(Frame {
            children: root.children(0, l.format)?,
            track: None,
            kind: root.header().kind,
            data_info_seen: false,
        });
        Ok(Self {
            frames,
            depth: 1,
            quicktime: major_brand == Some(*b"qt  "),
            parts: BudgetVec::new(b),
            movie: None,
            trex: BudgetVec::new(b),
            seen: 0,
            pending: None,
        })
    }
    fn poll(&mut self, w: &mut WorkBudget, c: &AtomicBool, l: OpenLimits) -> Result<bool> {
        if c.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.depth == 0 {
            return Ok(true);
        }
        if let Some((raw, track)) = self.pending.as_ref() {
            if !w.try_charge(1) {
                return Ok(false);
            }
            // Keep the exact consumed box until all fallible work has succeeded.
            let (raw, track) = (raw.clone(), *track);
            self.accept(raw, track, l)?;
            self.pending = None;
            return Ok(false);
        }
        let at = self.depth - 1;
        let track = self.frames.as_slice()[at]
            .as_ref()
            .ok_or(Error::Invalid("frame missing"))?
            .track;
        match self.frames.as_mut_slice()[at]
            .as_mut()
            .ok_or(Error::Invalid("frame missing"))?
            .children
            .poll(w)?
        {
            BoxStep::Yield => Ok(false),
            BoxStep::End => {
                self.frames.as_mut_slice()[at] = None;
                self.depth -= 1;
                Ok(self.depth == 0)
            }
            BoxStep::Box(raw) => {
                self.seen = self.seen.checked_add(1).ok_or(Error::Overflow)?;
                if self.seen > l.format.max_boxes {
                    return Err(Error::Limit {
                        requested: self.seen as usize,
                        maximum: l.format.max_boxes as usize,
                    });
                }
                self.pending = Some((raw, track));
                Ok(false)
            }
        }
    }
    fn accept(&mut self, raw: BoxView, track: Option<usize>, l: OpenLimits) -> Result<()> {
        let kind = raw.header().kind;
        if kind == *b"trak" {
            if self.parts.len() == l.tracks {
                return Err(Error::Limit {
                    requested: self.parts.len() + 1,
                    maximum: l.tracks,
                });
            }
            let index = self.parts.len();
            self.parts.try_reserve_exact(1)?;
            // Descend validates the depth/child header before the part is published.
            self.descend(raw, Some(index), l)?;
            push(&mut self.parts, Parts::default())?;
        } else if [*b"mdia", *b"minf", *b"dinf", *b"edts", *b"mvex"].contains(&kind) {
            let parent = self.depth - 1;
            self.descend(raw, track, l)?;
            if self.quicktime
                && kind == *b"dinf"
                && let Some(frame) = &mut self.frames.as_mut_slice()[parent]
            {
                frame.data_info_seen = true;
            }
        } else if kind == *b"mvhd" {
            if self.movie.is_some() {
                return Err(Error::Invalid("duplicate movie header"));
            }
            self.movie = Some(MovieHeader::parse(&raw)?);
        } else if kind == *b"trex" {
            if self.trex.len() == l.tracks {
                return Err(Error::Limit {
                    requested: self.trex.len() + 1,
                    maximum: l.tracks,
                });
            }
            push(&mut self.trex, Trex::parse(&raw)?)?;
        } else if let Some(t) = track {
            let frames = &self.frames.as_slice()[..self.depth];
            let parent = frames[self.depth - 1]
                .as_ref()
                .ok_or(Error::Invalid("frame missing"))?;
            let p = &mut self.parts.as_mut_slice()[t];
            match &kind {
                b"tkhd" => unique(&mut p.tkhd, raw)?,
                b"mdhd" => unique(&mut p.mdhd, raw)?,
                b"hdlr" if parent.kind == *b"mdia" => unique(&mut p.hdlr, raw)?,
                b"hdlr"
                    if self.quicktime
                        && matches!(
                            frames,
                            [_, Some(trak), Some(mdia), Some(minf)]
                                if [trak.kind, mdia.kind, minf.kind] == [*b"trak", *b"mdia", *b"minf"]
                        ) =>
                {
                    if p.data_handler_seen {
                        return Err(Error::Invalid("duplicate QuickTime data handler"));
                    }
                    if parent.data_info_seen
                        || p.hdlr
                            .as_ref()
                            .is_none_or(|handler| handler.payload().get(4..8) != Some(b"mhlr"))
                    {
                        return Err(Error::Invalid("QuickTime data handler placement"));
                    }
                    validate_quicktime_data_handler(&raw)?;
                    p.data_handler_seen = true;
                }
                b"hdlr" => return Err(Error::Invalid("unexpected track handler box")),
                b"stbl" => unique(&mut p.stbl, raw)?,
                b"elst" => unique(&mut p.elst, raw)?,
                b"dref" => unique(&mut p.dref, raw)?,
                _ => {}
            }
        }
        Ok(())
    }
    fn descend(&mut self, raw: BoxView, track: Option<usize>, l: OpenLimits) -> Result<()> {
        if self.depth >= l.depth {
            return Err(Error::Limit {
                requested: self.depth + 1,
                maximum: l.depth,
            });
        }
        self.frames.as_mut_slice()[self.depth] = Some(Frame {
            children: raw.children(0, l.format)?,
            track,
            kind: raw.header().kind,
            data_info_seen: false,
        });
        self.depth += 1;
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetadataReadScope {
    CompleteEntity,
    FrontMetadataBeforeMdat(iso::BoxHeader),
}
pub struct MovieTrack {
    pub(crate) header: TrackHeader,
    pub(crate) table: SampleTable,
    pub(crate) edits: Option<EditList>,
    pub(crate) fragments: BudgetVec<FragmentIndex>,
    pub(crate) raw_dref: BoxView,
    pub(crate) next_ordinal: u64,
}

// Bound the payload retained for indexed backward reads on a pipe.
pub(crate) const RETAINED_MEDIA_BYTES: usize = 32 * 1024;

#[derive(Clone)]
pub(crate) struct CachedMedia {
    pub(crate) start: u64,
    pub(crate) bytes: Bytes,
}

pub(crate) struct PendingMedia {
    start: u64,
    length: usize,
    buffer: BudgetVec<u8>,
}

impl PendingMedia {
    pub(crate) fn new(start: u64, length: usize, pool: &Budget) -> Result<Self> {
        let mut buffer = BudgetVec::new(pool);
        buffer.try_reserve_exact(length)?;
        Ok(Self {
            start,
            length,
            buffer,
        })
    }

    pub(crate) fn poll(
        &mut self,
        input: &mut InputCursor,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<Step<CachedMedia>> {
        let remaining = self.length - self.buffer.len();
        if remaining != 0 {
            return match input.read_append(&mut self.buffer, remaining, w, c)? {
                Step::Ready(_) | Step::Yield => Ok(Step::Yield),
                Step::NeedInput(r) => Ok(Step::NeedInput(r)),
                Step::End(at) => Ok(Step::End(at)),
            };
        }
        input.read_into(&mut [], w, c)?;
        if !w.try_charge(1) {
            return Ok(Step::Yield);
        }
        let buffer = std::mem::replace(&mut self.buffer, BudgetVec::new(input.budget()));
        match buffer.freeze() {
            Ok(bytes) => Ok(Step::Ready(CachedMedia {
                start: self.start,
                bytes,
            })),
            Err(failure) => {
                self.buffer = failure.buffer;
                Err(failure.error)
            }
        }
    }
}

pub struct Movie {
    pub(crate) read_scope: MetadataReadScope,
    pub(crate) native_recovery: bool,
    pub(crate) source: triergon_core::SourceId,
    pub(crate) generation: u64,
    pub(crate) source_length: Option<u64>,
    pub(crate) movie: MovieHeader,
    pub(crate) trex: BudgetVec<Trex>,
    pub(crate) tracks: BudgetVec<MovieTrack>,
    pub(crate) media: BudgetVec<Range<u64>>,
    pub(crate) media_cache: BudgetVec<CachedMedia>,
    pub(crate) terminal_mdat: Option<iso::TerminalMdat>,
    pub(crate) raw_roots: BudgetVec<BoxView>,
    pub(crate) major_brand: Option<[u8; 4]>,
    pub(crate) pool: Budget,
}
impl Movie {
    pub fn major_brand(&self) -> Option<[u8; 4]> {
        self.major_brand
    }
    pub fn read_scope(&self) -> MetadataReadScope {
        self.read_scope
    }
    pub fn terminal_mdat(&self) -> Option<iso::TerminalMdat> {
        self.terminal_mdat
    }
    pub fn tracks(&self) -> &[MovieTrack] {
        self.tracks.as_slice()
    }
    pub fn raw_roots(&self) -> &[BoxView] {
        self.raw_roots.as_slice()
    }
    pub fn header(&self) -> &MovieHeader {
        &self.movie
    }
    pub fn source(&self) -> triergon_core::SourceId {
        self.source
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
}
impl MovieTrack {
    pub fn edits(&self) -> Option<&EditList> {
        self.edits.as_ref()
    }
    pub fn header(&self) -> &TrackHeader {
        &self.header
    }
    pub fn table(&self) -> &SampleTable {
        &self.table
    }
    pub fn fragments(&self) -> &[FragmentIndex] {
        self.fragments.as_slice()
    }
}
pub struct OpenedMp4<S: Source> {
    pub movie: Movie,
    pub input: InputCursor,
    pub source: SourceOwner<S>,
}
#[derive(Debug)]
pub enum OpenProgress {
    Yield,
    NeedInput(triergon_core::InputRequest),
    Ready,
}
pub struct Mp4Open<S: Source> {
    input: Option<InputCursor>,
    source: Option<SourceOwner<S>>,
    reader: BoxReader,
    native_recovery: bool,
    timing_policy: TimingRunPolicy,
    prefer_front: bool,
    limits: OpenLimits,
    pool: Budget,
    roots: BudgetVec<BoxView>,
    media: BudgetVec<Range<u64>>,
    media_cache: BudgetVec<CachedMedia>,
    pending_media: Option<PendingMedia>,
    remaining_cache_bytes: usize,
    terminal_mdat: Option<iso::TerminalMdat>,
    pending_header: Option<iso::BoxHeader>,
    pending_metadata: Option<BoxView>,
    moofs: BudgetVec<BoxView>,
    moov: Option<BoxView>,
    brand: Option<[u8; 4]>,
    request: Option<triergon_core::InputRequest>,
    scan_done: bool,
    read_scope: MetadataReadScope,
    top_count: usize,
    walk: Option<Walk>,
    tracks: BudgetVec<MovieTrack>,
    track_at: usize,
    builder: Option<SampleTableBuilder>,
    refs: Option<BoxChildren>,
    references: BudgetVec<bool>,
    pending_reference: Option<bool>,
    description_at: usize,
    built_table: Option<SampleTable>,
    fragment_moof: usize,
    fragment_children: Option<BoxChildren>,
    fragment_raw: Option<BoxView>,
    fragment_scan: Option<BoxChildren>,
    fragment_track: Option<usize>,
    fragment_job: Option<FragmentBuilder>,
    pending_fragment: Option<FragmentIndex>,
    previous_traf: Option<u64>,
    failed: bool,
    ready: bool,
}
impl<S: Source> Mp4Open<S> {
    pub fn new(selected: SelectedInput<S>, limits: OpenLimits) -> Result<Self> {
        Self::new_inner(selected, limits, false, false)
    }

    /// Choose how the opener validates surplus sample timing.
    /// Inspect each track's sample table for the declared surplus counts.
    pub fn with_timing_policy(
        selected: SelectedInput<S>,
        limits: OpenLimits,
        policy: TimingRunPolicy,
    ) -> Result<Self> {
        let mut opener = Self::new(selected, limits)?;
        opener.timing_policy = policy;
        Ok(opener)
    }

    /// Stop at the first media header after a front movie, even when the
    /// transport offers ranges. The progressive engine parses later fragments.
    pub fn new_progressive(selected: SelectedInput<S>, limits: OpenLimits) -> Result<Self> {
        Self::new_inner(selected, limits, false, true)
    }

    /// Admit terminal media declarations for the explicit pinned native
    /// recovery view. SourcePhysical uses the strict opener.
    pub fn new_pinned_mov_recovery(selected: SelectedInput<S>, limits: OpenLimits) -> Result<Self> {
        Self::new_inner(selected, limits, true, false)
    }

    fn new_inner(
        selected: SelectedInput<S>,
        limits: OpenLimits,
        native_recovery: bool,
        prefer_front: bool,
    ) -> Result<Self> {
        if selected.format != Format::Mp4 {
            return Err(Error::Invalid("not selected MP4"));
        }
        if limits.tracks == 0
            || limits.tracks > 64
            || limits.depth == 0
            || limits.depth > 32
            || limits.roots == 0
        {
            return Err(Error::Invalid("MP4 adapter open limits"));
        }
        let pool = selected.input.budget().clone();
        let reader = box_reader(&selected.input, limits.metadata_bytes, native_recovery)?;
        Ok(Self {
            input: Some(selected.input),
            source: Some(selected.source),
            reader,
            native_recovery,
            timing_policy: TimingRunPolicy::Strict,
            prefer_front,
            limits,
            pool: pool.clone(),
            roots: BudgetVec::new(&pool),
            media: BudgetVec::new(&pool),
            media_cache: BudgetVec::new(&pool),
            pending_media: None,
            remaining_cache_bytes: RETAINED_MEDIA_BYTES,
            terminal_mdat: None,
            pending_header: None,
            pending_metadata: None,
            moofs: BudgetVec::new(&pool),
            moov: None,
            brand: None,
            request: None,
            scan_done: false,
            read_scope: MetadataReadScope::CompleteEntity,
            top_count: 0,
            walk: None,
            tracks: BudgetVec::new(&pool),
            track_at: 0,
            builder: None,
            refs: None,
            references: BudgetVec::new(&pool),
            pending_reference: None,
            description_at: 0,
            built_table: None,
            fragment_moof: 0,
            fragment_children: None,
            fragment_raw: None,
            fragment_scan: None,
            fragment_track: None,
            fragment_job: None,
            pending_fragment: None,
            previous_traf: None,
            failed: false,
            ready: false,
        })
    }
    pub fn advance(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<OpenProgress> {
        if self.failed {
            return Err(Error::Closed);
        }
        let result = self.advance_inner(w, c);
        if result.is_err() && !matches!(&result, Err(Error::Backpressure { .. })) {
            self.failed = true;
            if let Some(source) = self.source.as_mut() {
                source.close();
            }
        }
        result
    }
    fn advance_inner(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<OpenProgress> {
        if c.load(Ordering::Relaxed) {
            self.source
                .as_mut()
                .ok_or(Error::Invalid("opener closed"))?
                .close();
            return Err(Error::Cancelled);
        }
        if self.ready {
            return Ok(OpenProgress::Ready);
        }
        if w.remaining() == 0 {
            return Ok(OpenProgress::Yield);
        }
        if let Some(r) = self.request.clone() {
            let reply = self
                .source
                .as_mut()
                .ok_or(Error::Invalid("source missing"))?
                .poll(&r, w, c)?;
            if provide(
                self.input.as_mut().ok_or(Error::Invalid("input missing"))?,
                &reply,
            )? {
                self.request = None;
            }
            return Ok(OpenProgress::Yield);
        }
        if self.pending_header.is_some() {
            return self.announced_header(w);
        }
        if self.pending_metadata.is_some() {
            return self.metadata(w);
        }
        if let Some(pending) = self.pending_media.as_mut() {
            let input = self.input.as_mut().ok_or(Error::Invalid("input missing"))?;
            return match pending.poll(input, w, c)? {
                Step::Ready(cached) => {
                    self.remaining_cache_bytes -= cached.bytes.len();
                    push(&mut self.media_cache, cached)?;
                    self.pending_media = None;
                    // The cursor is at the next top-level header after the retained MDAT.
                    self.reader =
                        box_reader(input, self.limits.metadata_bytes, self.native_recovery)?;
                    Ok(OpenProgress::Yield)
                }
                Step::NeedInput(r) => {
                    self.request = Some(r.clone());
                    Ok(OpenProgress::NeedInput(r))
                }
                Step::Yield => Ok(OpenProgress::Yield),
                Step::End(_) => Err(Error::Truncated),
            };
        }
        if !self.scan_done {
            let input = self.input.as_mut().ok_or(Error::Invalid("input missing"))?;
            return match self.reader.poll(input, w, c)? {
                Step::NeedInput(r) => {
                    self.request = Some(r.clone());
                    Ok(OpenProgress::NeedInput(r))
                }
                Step::Yield => Ok(OpenProgress::Yield),
                Step::End(_) => {
                    let walk = Walk::new(
                        self.moov
                            .as_ref()
                            .ok_or(Error::Invalid("MP4 missing moov"))?,
                        &self.pool,
                        self.limits,
                        self.brand,
                    )?;
                    self.walk = Some(walk);
                    self.scan_done = true;
                    Ok(OpenProgress::Yield)
                }
                Step::Ready(BoxEvent::Header(h)) => {
                    self.top_count = self.top_count.checked_add(1).ok_or(Error::Overflow)?;
                    if self.top_count > self.limits.roots {
                        return Err(Error::Limit {
                            requested: self.top_count,
                            maximum: self.limits.roots,
                        });
                    }
                    self.pending_header = Some(h);
                    Ok(OpenProgress::Yield)
                }
                Step::Ready(BoxEvent::Metadata(raw)) => {
                    self.pending_metadata = Some(raw);
                    Ok(OpenProgress::Yield)
                }
                Step::Ready(BoxEvent::Skipped(_)) => Ok(OpenProgress::Yield),
            };
        }
        if self
            .walk
            .as_ref()
            .ok_or(Error::Invalid("walk missing"))?
            .depth
            != 0
        {
            self.walk
                .as_mut()
                .ok_or(Error::Invalid("walk missing"))?
                .poll(w, c, self.limits)?;
            return Ok(OpenProgress::Yield);
        }
        if self.track_at
            < self
                .walk
                .as_ref()
                .ok_or(Error::Invalid("walk missing"))?
                .parts
                .len()
        {
            return self.track(w);
        }
        if self.fragment_moof < self.moofs.len() {
            return self.fragment(w);
        }
        self.ready = true;
        Ok(OpenProgress::Ready)
    }
    fn track(&mut self, w: &mut WorkBudget) -> Result<OpenProgress> {
        let p = &mut self
            .walk
            .as_mut()
            .ok_or(Error::Invalid("walk missing"))?
            .parts
            .as_mut_slice()[self.track_at];
        if self.builder.is_none() && self.built_table.is_none() {
            if !w.try_charge(1) {
                return Ok(OpenProgress::Yield);
            }
            let raw = required(p.stbl.clone(), "missing sample table")?;
            let builder = SampleTableBuilder::with_timing_policy(
                raw,
                self.pool.clone(),
                self.limits.format,
                self.timing_policy,
            )?;
            let dref = p
                .dref
                .as_ref()
                .ok_or(Error::Invalid("missing source data references"))?;
            if word(dref.payload(), 0)? != 0 {
                return Err(Error::Unsupported("data reference flags"));
            }
            self.refs = Some(dref.children(8, self.limits.format)?);
            self.builder = Some(builder);
            p.stbl = None;
        }
        if let Some(value) = self.pending_reference {
            if !w.try_charge(1) {
                return Ok(OpenProgress::Yield);
            }
            push(&mut self.references, value)?;
            self.pending_reference = None;
            return Ok(OpenProgress::Yield);
        }
        if let Some(refs) = self.refs.as_mut() {
            match refs.poll(w)? {
                BoxStep::Yield => return Ok(OpenProgress::Yield),
                BoxStep::Box(raw) => {
                    if self.references.len() >= self.limits.format.max_descriptions as usize {
                        return Err(Error::Limit {
                            requested: self.references.len() + 1,
                            maximum: self.limits.format.max_descriptions as usize,
                        });
                    }
                    self.pending_reference =
                        Some(raw.header().kind == *b"url " && raw.payload() == [0, 0, 0, 1]);
                    return Ok(OpenProgress::Yield);
                }
                BoxStep::End => {
                    let dref = p.dref.as_ref().ok_or(Error::Invalid("missing dref"))?;
                    if word(dref.payload(), 4)? as usize != self.references.len() {
                        return Err(Error::Invalid("data reference count"));
                    }
                    self.refs = None;
                }
            }
        }
        if let Some(builder) = self.builder.as_mut() {
            if builder.poll(w)? == BuildStatus::Yield {
                return Ok(OpenProgress::Yield);
            }
            self.built_table = Some(
                self.builder
                    .take()
                    .ok_or(Error::Invalid("table builder missing"))?
                    .finish()?,
            );
        }
        let table = self
            .built_table
            .as_ref()
            .ok_or(Error::Invalid("table missing"))?;
        if let Some(desc) = table.descriptions().get(self.description_at) {
            if !w.try_charge(1) {
                return Ok(OpenProgress::Yield);
            }
            let at = usize::from(desc.data_reference_index)
                .checked_sub(1)
                .ok_or(Error::Invalid("zero reference"))?;
            match self.references.as_slice().get(at) {
                Some(true) => {}
                Some(false) => {
                    return Err(Error::Unsupported("external source data reference"));
                }
                None => return Err(Error::Invalid("dangling source data reference")),
            }
            self.description_at += 1;
            return Ok(OpenProgress::Yield);
        }
        if !w.try_charge(1) {
            return Ok(OpenProgress::Yield);
        }
        let tkhd = p.tkhd.as_ref().ok_or(Error::Invalid("missing tkhd"))?;
        let mdhd = p.mdhd.as_ref().ok_or(Error::Invalid("missing mdhd"))?;
        let hdlr = p.hdlr.as_ref().ok_or(Error::Invalid("missing hdlr"))?;
        let header = if self.native_recovery {
            iso::Mov9RecoveryTrackHeader::parse(
                tkhd,
                mdhd,
                hdlr,
                iso::PinnedMovD32RegularRecoveryV1,
            )?
            .into_source()
        } else {
            TrackHeader::parse(tkhd, mdhd, hdlr)?
        };
        if self
            .tracks
            .as_slice()
            .iter()
            .any(|t| t.header.track_id == header.track_id)
        {
            return Err(Error::Invalid("duplicate source track ID"));
        }
        let edits = p
            .elst
            .clone()
            .map(|b| EditList::parse(b, self.limits.format))
            .transpose()?;
        self.tracks.try_reserve_exact(1)?;
        push(
            &mut self.tracks,
            MovieTrack {
                header,
                next_ordinal: table.sample_count().checked_add(1).ok_or(Error::Overflow)?,
                table: self
                    .built_table
                    .take()
                    .ok_or(Error::Invalid("table missing"))?,
                edits,
                fragments: BudgetVec::new(&self.pool),
                raw_dref: required(p.dref.take(), "missing dref")?,
            },
        )?;
        p.elst = None;
        self.description_at = 0;
        self.references.clear();
        self.track_at += 1;
        Ok(OpenProgress::Yield)
    }
    fn metadata(&mut self, w: &mut WorkBudget) -> Result<OpenProgress> {
        if !w.try_charge(1) {
            return Ok(OpenProgress::Yield);
        }
        let raw = self
            .pending_metadata
            .as_ref()
            .ok_or(Error::Invalid("metadata missing"))?;
        if self.roots.len() >= self.limits.roots {
            return Err(Error::Limit {
                requested: self.roots.len() + 1,
                maximum: self.limits.roots,
            });
        }
        let brand = match &raw.header().kind {
            b"moov" if self.moov.is_some() => {
                return Err(Error::Invalid("duplicate structural movie box"));
            }
            b"moof" => {
                if self.moofs.len() >= self.limits.fragments {
                    return Err(Error::Limit {
                        requested: self.moofs.len() + 1,
                        maximum: self.limits.fragments,
                    });
                }
                self.moofs.try_reserve_exact(1)?;
                None
            }
            b"ftyp" => Some(
                raw.payload()
                    .get(..4)
                    .ok_or(Error::Truncated)?
                    .try_into()
                    .map_err(|_| Error::Truncated)?,
            ),
            _ => None,
        };
        self.roots.try_reserve_exact(1)?;
        // Both destination capacities are present before changing source facts.
        match &raw.header().kind {
            b"moov" => self.moov = Some(raw.clone()),
            b"moof" => push(&mut self.moofs, raw.clone())?,
            b"ftyp" => self.brand = brand,
            _ => {}
        }
        push(&mut self.roots, raw.clone())?;
        self.pending_metadata = None;
        Ok(OpenProgress::Yield)
    }
    fn announced_header(&mut self, w: &mut WorkBudget) -> Result<OpenProgress> {
        if !w.try_charge(1) {
            return Ok(OpenProgress::Yield);
        }
        let h = self
            .pending_header
            .ok_or(Error::Invalid("pending MP4 header missing"))?;
        let input = self.input.as_mut().ok_or(Error::Invalid("input missing"))?;
        if h.kind == *b"mdat" {
            if self.media.len() >= self.limits.roots {
                return Err(Error::Limit {
                    requested: self.media.len() + 1,
                    maximum: self.limits.roots,
                });
            }
            if self.prefer_front && self.moov.is_none() {
                return Err(Error::Unsupported("progressive MP4 needs front metadata"));
            }
            let front =
                (self.prefer_front || !input.capabilities().random_access) && self.moov.is_some();
            let file_end = if self.native_recovery
                && input.capabilities().random_access
                && self.moov.is_some()
            {
                h.end()?
                    .zip(input.capabilities().length)
                    .and_then(|(declared, physical)| (declared > physical).then_some(physical))
            } else {
                None
            };
            let terminal = if h.size.is_none() {
                if !front || !self.media.is_empty() {
                    return Err(Error::Unsupported(
                        "open MDAT startup needs front movie before media",
                    ));
                }
                Some(iso::TerminalMdat::new(h)?)
            } else {
                None
            };
            // Reserve fallible metadata before committing the MDAT/header state.
            let walk = if front {
                Some(Walk::new(
                    self.moov.as_ref().ok_or(Error::Invalid("moov missing"))?,
                    &self.pool,
                    self.limits,
                    self.brand,
                )?)
            } else {
                None
            };
            let mut pending_media = None;
            if terminal.is_none() {
                let range = h.payload_range()?;
                let length = range.end - range.start;
                if !input.capabilities().random_access
                    && !front
                    && (1..=self.remaining_cache_bytes as u64).contains(&length)
                {
                    let length = length as usize; // bounded by remaining_cache_bytes
                    pending_media = Some(PendingMedia::new(range.start, length, &self.pool)?);
                    self.media_cache.try_reserve_exact(1)?;
                }
                push(&mut self.media, range)?;
            }
            if front {
                self.terminal_mdat = terminal;
                self.walk = walk;
                self.scan_done = true;
                self.read_scope = MetadataReadScope::FrontMetadataBeforeMdat(h);
            } else if pending_media.is_some() {
                self.pending_media = pending_media;
            } else if let Some(end) = file_end {
                // The checked declaration extends past the physical entity.
                // Still obtain a matching End before completing the open.
                input.request_range(end)?;
                self.reader = box_reader(input, self.limits.metadata_bytes, self.native_recovery)?;
            } else {
                self.reader.skip(input)?;
            }
        } else if [*b"moov", *b"moof", *b"ftyp"].contains(&h.kind) {
            self.reader.read_metadata(input)?;
        } else {
            self.reader.skip(input)?;
        }
        self.pending_header = None;
        Ok(OpenProgress::Yield)
    }
    fn fragment(&mut self, w: &mut WorkBudget) -> Result<OpenProgress> {
        let moof = &self.moofs.as_slice()[self.fragment_moof];
        if let Some(index) = self.pending_fragment.as_ref() {
            if !w.try_charge(1) {
                return Ok(OpenProgress::Yield);
            }
            let end = index.implicit_data_end()?;
            let at = self
                .fragment_track
                .ok_or(Error::Invalid("fragment track missing"))?;
            let track = &mut self.tracks.as_mut_slice()[at];
            let next = track
                .next_ordinal
                .checked_add(index.sample_count())
                .ok_or(Error::Overflow)?;
            track.fragments.try_reserve_exact(1)?;
            if let Some(index) = self.pending_fragment.take() {
                push(&mut track.fragments, index)?;
            }
            track.next_ordinal = next;
            self.previous_traf = Some(end);
            self.fragment_track = None;
            self.fragment_raw = None;
            return Ok(OpenProgress::Yield);
        }
        if let Some(job) = self.fragment_job.as_mut() {
            if job.poll(w)? == BuildStatus::Yield {
                return Ok(OpenProgress::Yield);
            }
            let index = self
                .fragment_job
                .take()
                .ok_or(Error::Invalid("fragment job missing"))?
                .finish()?;
            self.pending_fragment = Some(index);
            return Ok(OpenProgress::Yield);
        }
        if let Some(scan) = self.fragment_scan.as_mut() {
            match scan.poll(w)? {
                BoxStep::Yield => return Ok(OpenProgress::Yield),
                BoxStep::Box(raw) if raw.header().kind == *b"tfhd" => {
                    let id = word(raw.payload(), 4)?;
                    let at = self
                        .tracks
                        .as_slice()
                        .iter()
                        .position(|t| t.header.track_id == id)
                        .ok_or(Error::Invalid("fragment unknown source track"))?;
                    let t = &self.tracks.as_slice()[at];
                    let defaults = *self
                        .walk
                        .as_ref()
                        .ok_or(Error::Invalid("walk missing"))?
                        .trex
                        .as_slice()
                        .iter()
                        .find(|r| r.track_id == id)
                        .ok_or(Error::Invalid("fragment missing trex"))?;
                    let first = t.next_ordinal;
                    let context = FragmentContext {
                        moof_start: moof.header().start,
                        implicit_base: self
                            .previous_traf
                            .map_or(ImplicitBase::FirstTraf, ImplicitBase::PreviousTraf),
                        decode_origin: None,
                        first_ordinal: first,
                        description_count: u32::try_from(t.table.descriptions().len())
                            .map_err(|_| Error::Overflow)?,
                    };
                    let raw = self
                        .fragment_raw
                        .as_ref()
                        .ok_or(Error::Invalid("traf missing"))?
                        .clone();
                    let build = if self.native_recovery {
                        FragmentBuilder::new_pinned_mov_header
                    } else {
                        FragmentBuilder::new
                    };
                    self.fragment_job = Some(build(
                        raw,
                        defaults,
                        context,
                        self.pool.clone(),
                        self.limits.format,
                    )?);
                    self.fragment_track = Some(at);
                    self.fragment_scan = None;
                    return Ok(OpenProgress::Yield);
                }
                BoxStep::Box(_) => return Ok(OpenProgress::Yield),
                BoxStep::End => return Err(Error::Invalid("traf missing tfhd")),
            }
        }
        if self.fragment_children.is_none() {
            self.fragment_children = Some(moof.children(0, self.limits.format)?);
            self.previous_traf = None;
        }
        match self
            .fragment_children
            .as_mut()
            .ok_or(Error::Invalid("fragment children missing"))?
            .poll(w)?
        {
            BoxStep::Yield => {}
            BoxStep::Box(raw) if raw.header().kind == *b"traf" => {
                self.fragment_scan = Some(raw.children(0, self.limits.format)?);
                self.fragment_raw = Some(raw);
            }
            BoxStep::Box(_) => {}
            BoxStep::End => {
                self.fragment_children = None;
                self.fragment_moof += 1;
            }
        }
        Ok(OpenProgress::Yield)
    }
    pub fn finish(mut self) -> Result<OpenedMp4<S>> {
        if self.failed {
            return Err(Error::Closed);
        }
        if !self.ready {
            return Err(Error::Invalid("movie not ready"));
        }
        let walk = self.walk.take().ok_or(Error::Invalid("walk missing"))?;
        Ok(OpenedMp4 {
            movie: Movie {
                read_scope: self.read_scope,
                native_recovery: self.native_recovery,
                source: self
                    .input
                    .as_ref()
                    .ok_or(Error::Invalid("input missing"))?
                    .source(),
                generation: self
                    .input
                    .as_ref()
                    .ok_or(Error::Invalid("input missing"))?
                    .generation(),
                source_length: self
                    .input
                    .as_ref()
                    .ok_or(Error::Invalid("input missing"))?
                    .capabilities()
                    .length,
                movie: required(walk.movie, "missing movie header")?,
                trex: walk.trex,
                tracks: std::mem::replace(&mut self.tracks, BudgetVec::new(&self.pool)),
                media: std::mem::replace(&mut self.media, BudgetVec::new(&self.pool)),
                media_cache: std::mem::replace(&mut self.media_cache, BudgetVec::new(&self.pool)),
                terminal_mdat: self.terminal_mdat,
                raw_roots: std::mem::replace(&mut self.roots, BudgetVec::new(&self.pool)),
                major_brand: self.brand,
                pool: self.pool.clone(),
            },
            input: self.input.take().ok_or(Error::Invalid("input missing"))?,
            source: self.source.take().ok_or(Error::Invalid("source missing"))?,
        })
    }
}
