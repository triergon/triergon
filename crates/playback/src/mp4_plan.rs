//! Owned, charged playback plan produced by borrowing actual compact indexes.
//! The temporary IndexJob's lifetime is explicit; no self-reference or copied core.
use crate::{
    Result,
    mp4_open::{CachedMedia, Movie, RETAINED_MEDIA_BYTES},
    sample_sort::SampleSort,
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Rounding, Time, TimeBase, WorkBudget};
use triergon_isobmff::{
    self as iso, BoxView, EditList, InterleaveStatus, Mov9CursorStatus, Mov9MediaClock,
    Mov9PacketCursor, Mov9PacketLimits, Mov9PacketOccurrence, Mov9Profile, Mov9StreamView,
    Mov9Track, PhysicalInterleaveLimits, PhysicalSampleCursor, Sample, SampleCursor,
    SampleDescription, SampleStep, SourceTimeBase, SourceTrack,
};
fn push<T>(v: &mut BudgetVec<T>, x: T) -> Result<()> {
    v.try_push(x).map_err(|e| e.error.into())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mp4View {
    SourcePhysical,
    PinnedMov9NoParseNoFillin,
}
#[derive(Clone, Copy, Debug)]
pub struct PlanLimits {
    pub tracks: usize,
    pub samples: u64,
    pub occurrences: u64,
    pub media_extents: usize,
}
impl Default for PlanLimits {
    fn default() -> Self {
        Self {
            tracks: 64,
            samples: 1_000_000,
            occurrences: 1_000_000,
            media_extents: 4096,
        }
    }
}

impl PlanLimits {
    fn native_packet_limits(self) -> Mov9PacketLimits {
        Mov9PacketLimits {
            max_tracks: self.tracks,
            max_media_extents: self.media_extents,
            max_source_samples: self.samples,
            max_occurrences: self.occurrences,
            ..Mov9PacketLimits::default()
        }
    }
}
#[derive(Clone, Debug)]
pub struct TrackSnapshot {
    pub track_id: u32,
    pub base: SourceTimeBase,
    pub handler: [u8; 4],
    pub enabled: bool,
    pub raw_tkhd: BoxView,
    pub raw_mdhd: BoxView,
    pub raw_hdlr: BoxView,
    pub raw_table: BoxView,
    pub raw_edits: Option<BoxView>,
    pub raw_dref: BoxView,
    pub native_catalogue: std::result::Result<Mov9StreamView, Error>,
}
#[derive(Clone, Debug)]
pub struct Mp4Configuration {
    pub track: TrackSnapshot,
    pub description: SampleDescription,
    pub revision: u64,
}
pub struct PlanTrack {
    pub(crate) trex: Option<iso::Trex>,
    pub(crate) next_ordinal: u64,
    pub snapshot: TrackSnapshot,
    pub descriptions: BudgetVec<SampleDescription>,
    pub edits: Option<EditList>,
}
#[derive(Clone, Debug)]
pub struct SourceRow {
    pub track_id: u32,
    pub sample: Sample,
}
impl SourceRow {
    pub(crate) fn physical_key(&self) -> (u64, u32, u64) {
        (self.sample.range.start, self.track_id, self.sample.ordinal)
    }
}
/// Constructed only from a checked, source-bound index job.
/// ```compile_fail
/// fn corrupt(plan: &mut triergon_playback::mp4_plan::PreparedMp4) {
///     plan.source_rows.clear();
/// }
/// ```
pub struct PreparedMp4 {
    pub(crate) read_scope: crate::mp4_open::MetadataReadScope,
    pub(crate) native_recovery: bool,
    pub(crate) unavailable_first: Option<SourceRow>,
    pub(crate) source: triergon_core::SourceId,
    pub(crate) opening_generation: u64,
    pub(crate) source_length: Option<u64>,
    pub(crate) view: Mp4View,
    pub(crate) native_front: bool,
    pub(crate) replay_native_order: bool,
    pub(crate) native_front_boundary: Option<iso::Mov9FrontReadBoundaryV1>,
    pub(crate) native_empty_duration: Option<i64>,
    pub(crate) front_tracks: BudgetVec<iso::Mov9FrontTrackV1>,
    pub(crate) source_rows: BudgetVec<SourceRow>,
    pub(crate) native_rows: BudgetVec<Mov9PacketOccurrence>,
    pub(crate) encoded_edits: Option<BudgetVec<Option<iso::Mov9EncodedEditPlanV2>>>,
    pub(crate) tracks: BudgetVec<PlanTrack>,
    pub(crate) media: BudgetVec<Range<u64>>,
    pub(crate) media_cache: BudgetVec<CachedMedia>,
    pub(crate) forward_regular_extents: Option<usize>,
    pub(crate) terminal_mdat: Option<iso::TerminalMdat>,
    pub(crate) raw_roots: BudgetVec<BoxView>,
    pub(crate) movie_base: TimeBase,
    pub(crate) pool: Budget,
}
impl PreparedMp4 {
    pub(crate) fn cached_payload(&self, range: &Range<u64>) -> Result<Option<Bytes>> {
        let cache = self.media_cache.as_slice();
        let Some(index) = cache
            .partition_point(|extent| extent.start <= range.start)
            .checked_sub(1)
        else {
            return Ok(None);
        };
        let extent = &cache[index];
        let end = range.end.checked_sub(extent.start).ok_or(Error::Overflow)?;
        if end > extent.bytes.len() as u64 {
            return Ok(None);
        }
        let start = (range.start - extent.start) as usize;
        Ok(Some(extent.bytes.slice(start..end as usize)?))
    }

    pub fn read_scope(&self) -> crate::mp4_open::MetadataReadScope {
        self.read_scope
    }
    pub fn terminal_mdat(&self) -> Option<iso::TerminalMdat> {
        self.terminal_mdat
    }
    pub fn movie_base(&self) -> TimeBase {
        self.movie_base
    }
    /// Declared Source rows. A forward regular plan checks pending rows against
    /// the actual later MDAT before it can read or publish their payload.
    pub fn source_rows(&self) -> &[SourceRow] {
        self.source_rows.as_slice()
    }
    pub fn native_rows(&self) -> &[Mov9PacketOccurrence] {
        self.native_rows.as_slice()
    }
    pub fn tracks(&self) -> &[PlanTrack] {
        self.tracks.as_slice()
    }
    pub fn raw_roots(&self) -> &[BoxView] {
        self.raw_roots.as_slice()
    }

    /// A front-native operation continues all Source declarations even after a
    /// separately reported native read boundary. Only native occurrences stop.
    pub fn native_front_read_boundary_v1(&self) -> Option<iso::Mov9FrontReadBoundaryV1> {
        self.native_front_boundary
    }
    pub(crate) fn next_native_empty_duration(
        &self,
        packet: &Mov9PacketOccurrence,
        completed_run: bool,
    ) -> Option<i64> {
        let previous = self.native_empty_duration?;
        if packet.source.composition_offset.is_some() {
            return None;
        }
        if !completed_run {
            return Some(previous);
        }
        let end = packet
            .native
            .dts?
            .checked_add(i64::from(packet.source.duration_ticks))?;
        Some(previous.max(end))
    }
    pub fn rows(&self) -> usize {
        if self.native_front && !self.replay_native_order {
            return self.source_rows.len();
        }
        match self.view {
            Mp4View::SourcePhysical => self.source_rows.len(),
            Mp4View::PinnedMov9NoParseNoFillin => self.native_rows.len(),
        }
    }
    pub fn row(&self, at: usize) -> Option<SourceRow> {
        if self.native_front && !self.replay_native_order {
            return self.source_rows.as_slice().get(at).cloned();
        }
        match self.view {
            Mp4View::SourcePhysical => self.source_rows.as_slice().get(at).cloned(),
            Mp4View::PinnedMov9NoParseNoFillin => {
                self.native_rows.as_slice().get(at).map(|n| SourceRow {
                    track_id: n.track_id,
                    sample: n.source.clone(),
                })
            }
        }
    }
    pub fn track(&self, id: u32) -> Result<&PlanTrack> {
        self.tracks
            .as_slice()
            .iter()
            .find(|t| t.snapshot.track_id == id)
            .ok_or_else(|| Error::Invalid("unknown plan track").into())
    }
    pub fn native(&self, at: usize) -> Option<&Mov9PacketOccurrence> {
        if self.view == Mp4View::PinnedMov9NoParseNoFillin {
            self.native_rows.as_slice().get(at)
        } else {
            None
        }
    }
    pub fn config(&self, row: &SourceRow, revision: u64) -> Result<Mp4Configuration> {
        let t = self.track(row.track_id)?;
        let d = t
            .descriptions
            .as_slice()
            .iter()
            .find(|d| d.index == row.sample.description_index)
            .ok_or(Error::Invalid("sample description absent"))?;
        Ok(Mp4Configuration {
            track: t.snapshot.clone(),
            description: d.clone(),
            revision,
        })
    }
}
pub struct Mp4IndexJob<'a> {
    movie: &'a Movie,
    plan: Option<PreparedMp4>,
    source: Option<PhysicalSampleCursor<'a>>,
    native: Option<Mov9PacketCursor<'a>>,
    range_duration: Option<RangeDuration>,
    limits: PlanLimits,
    phase: u8,
    track: usize,
    description: usize,
    root: usize,
    extent: usize,
    ready: bool,
    pending_source: Option<SourceRow>,
    pending_native: Option<Mov9PacketOccurrence>,
    source_track_order: bool,
    source_sort: Option<SampleSort>,
    zero_scale_clock: Option<Mov9MediaClock>,
    first_sample_probe: Option<(SampleCursor<'a>, Error)>,
}

struct RangeDuration {
    micros: i64,
    scan: bool,
}

impl<'a> Mp4IndexJob<'a> {
    /// Explicit native view of one from-start front-metadata source. Source
    /// clocks/extents are unchanged; the engine must retain actual EOF authority.
    /// This does not establish a complete-source fact for an isolated fragment.
    pub fn new_native_terminal_v1(movie: &'a Movie, limits: PlanLimits) -> Result<Self> {
        if !matches!(
            movie.read_scope,
            crate::mp4_open::MetadataReadScope::FrontMetadataBeforeMdat(_)
        ) {
            return Err(
                Error::Invalid("native front operation needs actual front metadata").into(),
            );
        }
        Self::new_inner(
            movie,
            Mp4View::PinnedMov9NoParseNoFillin,
            limits,
            true,
            None,
            None,
        )
    }
    pub fn new(movie: &'a Movie, view: Mp4View, limits: PlanLimits) -> Result<Self> {
        Self::new_inner(movie, view, limits, false, None, None)
    }
    /// Build the native range view with explicitly validated encoded edits.
    /// The retained plans also govern seeks on this exact range index.
    pub fn new_native_encoded_range(
        movie: &'a Movie,
        plans: &'a [Option<iso::Mov9EncodedEditPlanV2>],
        limits: PlanLimits,
    ) -> Result<Self> {
        if movie.native_recovery
            || movie.read_scope != crate::mp4_open::MetadataReadScope::CompleteEntity
        {
            return Err(
                Error::Unsupported("encoded edits require a complete native range source").into(),
            );
        }
        Self::new_inner(
            movie,
            Mp4View::PinnedMov9NoParseNoFillin,
            limits,
            false,
            Some(plans),
            None,
        )
    }

    pub(crate) fn new_native_range(
        movie: &'a Movie,
        plans: Option<&'a [Option<iso::Mov9EncodedEditPlanV2>]>,
        limits: PlanLimits,
    ) -> Result<Self> {
        if movie.native_recovery
            || movie.read_scope != crate::mp4_open::MetadataReadScope::CompleteEntity
        {
            return Err(
                Error::Unsupported("native range playback requires a complete source").into(),
            );
        }
        let header = movie.header();
        let micros = Time::from_ticks(i128::from(header.duration), header.time_base()?)?
            .to_i64_ticks(TimeBase::new(1, 1_000_000)?, Rounding::NearestAway)?;
        Self::new_inner(
            movie,
            Mp4View::PinnedMov9NoParseNoFillin,
            limits,
            false,
            plans,
            Some(RangeDuration {
                micros,
                scan: header.duration == 0,
            }),
        )
    }

    fn new_inner(
        movie: &'a Movie,
        view: Mp4View,
        limits: PlanLimits,
        native_front: bool,
        encoded_edits: Option<&'a [Option<iso::Mov9EncodedEditPlanV2>]>,
        range_duration: Option<RangeDuration>,
    ) -> Result<Self> {
        if movie.native_recovery && view == Mp4View::SourcePhysical {
            return Err(Error::Invalid("native recovery open requires native packet view").into());
        }
        if limits.tracks == 0 || limits.tracks > 64 || movie.tracks.len() > limits.tracks {
            return Err(Error::Limit {
                requested: movie.tracks.len(),
                maximum: limits.tracks.min(64),
            }
            .into());
        }
        if movie.terminal_mdat.is_some() && view != Mp4View::SourcePhysical && !native_front {
            return Err(
                Error::Unsupported("native terminal MDAT context is not established").into(),
            );
        }
        let media_count = movie
            .media
            .len()
            .checked_add(usize::from(movie.terminal_mdat.is_some()))
            .ok_or(Error::Overflow)?;
        if media_count > limits.media_extents {
            return Err(Error::Limit {
                requested: media_count,
                maximum: limits.media_extents,
            }
            .into());
        }
        let b = &movie.pool;
        let mut sources = BudgetVec::new(b);
        let mut native = BudgetVec::new(b);
        let mut zero_scale_clock = None;
        for t in movie.tracks.as_slice() {
            let media_base = if t.header.timescale == 0 && movie.native_recovery {
                if !movie.trex.is_empty()
                    || !t.fragments.is_empty()
                    || t.edits.is_some()
                    || t.table.sample_count() == 0
                    || t.header.handler != *b"vide"
                    || !matches!(t.table.descriptions(), [entry] if entry.format == *b"raw ")
                {
                    return Err(Error::Unsupported("zero-scale native track context").into());
                }
                let clock =
                    Mov9MediaClock::for_pinned_zero_scale(iso::PinnedMovD32RegularRecoveryV1)?;
                zero_scale_clock = Some(clock);
                clock.native_base
            } else {
                t.header.time_base()?
            };
            if range_duration.is_none() {
                push(
                    &mut sources,
                    SourceTrack::new(t.header.track_id, Some(&t.table), t.fragments.as_slice())?,
                )?;
            }
            push(
                &mut native,
                Mov9Track {
                    track_id: t.header.track_id,
                    table: &t.table,
                    fragments: t.fragments.as_slice(),
                    media_base,
                    edits: t
                        .edits
                        .as_ref()
                        .map(|e| Ok::<_, Error>((e, movie.movie.time_base()?)))
                        .transpose()?,
                },
            )?;
        }
        let interleave_limits = PhysicalInterleaveLimits {
            max_tracks: limits.tracks,
            max_samples: limits.samples,
        };
        let forward_regular = match movie.read_scope {
            crate::mp4_open::MetadataReadScope::FrontMetadataBeforeMdat(h)
                if movie.terminal_mdat.is_none()
                    && movie.media.len() == 1
                    && (!movie.native_recovery
                        || movie
                            .source_length
                            .is_none_or(|length| movie.media.as_slice()[0].end <= length))
                    && (view == Mp4View::SourcePhysical || native_front)
                    && movie.trex.is_empty()
                    && movie
                        .tracks
                        .as_slice()
                        .iter()
                        .all(|track| track.edits.is_none() && track.fragments.is_empty()) =>
            {
                Some(iso::ForwardRegularMdatV1::new(h, movie.source_length)?)
            }
            _ => None,
        };
        let source = if range_duration.is_some() {
            None
        } else if let Some(bounds) = forward_regular {
            Some(PhysicalSampleCursor::new_forward_regular_v1(
                b,
                sources.as_slice(),
                movie.media.as_slice(),
                bounds,
                interleave_limits,
            )?)
        } else if movie.terminal_mdat.is_some() {
            // Metadata traversal preserves declared source coordinates. Each row
            // is checked against the terminal declaration, then read physically.
            Some(PhysicalSampleCursor::new(
                b,
                sources.as_slice(),
                interleave_limits,
            )?)
        } else {
            Some(PhysicalSampleCursor::new_with_media_extents(
                b,
                sources.as_slice(),
                movie.media.as_slice(),
                interleave_limits,
            )?)
        };
        let mut native = if view == Mp4View::PinnedMov9NoParseNoFillin {
            let native_limits = limits.native_packet_limits();
            Some(if let Some(plans) = encoded_edits {
                Mov9PacketCursor::new_encoded_regular_v2(
                    b,
                    native.as_slice(),
                    movie.media.as_slice(),
                    native_limits,
                    plans,
                    iso::PinnedMovD32EncodedEditsV2,
                )?
            } else if let Some(bounds) = forward_regular {
                Mov9PacketCursor::new_front_regular_v1(
                    b,
                    native.as_slice(),
                    movie.media.as_slice(),
                    bounds,
                    native_limits,
                    Mov9Profile::PinnedNoParseNoFillin,
                )?
            } else if native_front {
                Mov9PacketCursor::new_front_source_v1(
                    b,
                    native.as_slice(),
                    movie.media.as_slice(),
                    movie.terminal_mdat,
                    native_limits,
                    Mov9Profile::PinnedNoParseNoFillin,
                )?
            } else if movie.read_scope == crate::mp4_open::MetadataReadScope::CompleteEntity {
                Mov9PacketCursor::new_complete_source(
                    b,
                    native.as_slice(),
                    movie.media.as_slice(),
                    native_limits,
                    Mov9Profile::PinnedNoParseNoFillin,
                )?
            } else {
                Mov9PacketCursor::new(
                    b,
                    native.as_slice(),
                    movie.media.as_slice(),
                    native_limits,
                    Mov9Profile::PinnedNoParseNoFillin,
                )?
            })
        } else {
            None
        };
        if range_duration.is_some()
            && let Some(cursor) = &mut native
        {
            cursor.enable_range_seek_checkpoints();
        }
        let encoded_edits = if let Some(plans) = encoded_edits {
            let mut retained = BudgetVec::new(b);
            retained.try_reserve_exact(plans.len())?;
            for plan in plans {
                push(&mut retained, plan.clone())?;
            }
            Some(retained)
        } else {
            None
        };
        let native_empty_duration = if native_front
            && let [track] = movie.tracks.as_slice()
            && track.edits.is_none()
            && track.table.sample_count() == 0
            && track.header.handler == *b"vide"
            && matches!(track.table.descriptions(), [entry] if entry.format == *b"raw ")
        {
            i64::try_from(track.header.duration).ok()
        } else {
            None
        };
        Ok(Self {
            movie,
            plan: Some(PreparedMp4 {
                read_scope: movie.read_scope,
                native_recovery: movie.native_recovery,
                unavailable_first: None,
                source: movie.source,
                opening_generation: movie.generation,
                source_length: movie.source_length,
                view,
                native_front,
                replay_native_order: false,
                native_front_boundary: None,
                native_empty_duration,
                front_tracks: BudgetVec::new(b),
                source_rows: BudgetVec::new(b),
                native_rows: BudgetVec::new(b),
                encoded_edits,
                tracks: BudgetVec::new(b),
                media: BudgetVec::new(b),
                media_cache: BudgetVec::new(b),
                forward_regular_extents: None,
                terminal_mdat: movie.terminal_mdat,
                raw_roots: BudgetVec::new(b),
                movie_base: movie.movie.time_base()?,
                pool: b.clone(),
            }),
            source,
            native,
            range_duration,
            limits,
            phase: 0,
            track: 0,
            description: 0,
            root: 0,
            extent: 0,
            ready: false,
            pending_source: None,
            pending_native: None,
            source_track_order: false,
            source_sort: None,
            zero_scale_clock,
            first_sample_probe: None,
        })
    }
    pub fn poll(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<bool> {
        if c.load(Ordering::Relaxed) {
            return Err(Error::Cancelled.into());
        }
        if self.ready {
            return Ok(true);
        }
        if w.remaining() == 0 {
            return Ok(false);
        }
        let p = self.plan.as_mut().ok_or(Error::Invalid("plan consumed"))?;
        match self.phase {
            0 => {
                if let Some(t) = self.movie.tracks.as_slice().get(self.track) {
                    if self.description == 0 {
                        if !w.try_charge(1) {
                            return Ok(false);
                        }
                        let h = &t.header;
                        let (base, native_catalogue) = if h.timescale == 0 {
                            (
                                SourceTimeBase::UNAVAILABLE,
                                Mov9StreamView::for_pinned_zero_scale_source(
                                    h,
                                    &t.table,
                                    self.movie.major_brand,
                                    iso::PinnedMovD32RegularRecoveryV1,
                                ),
                            )
                        } else {
                            (
                                h.time_base()?.into(),
                                Mov9StreamView::for_source(
                                    h,
                                    &t.table,
                                    !t.fragments.is_empty(),
                                    self.movie.major_brand,
                                ),
                            )
                        };
                        push(
                            &mut p.tracks,
                            PlanTrack {
                                trex: self
                                    .movie
                                    .trex
                                    .as_slice()
                                    .iter()
                                    .find(|x| x.track_id == t.header.track_id)
                                    .copied(),
                                next_ordinal: t.next_ordinal,
                                snapshot: TrackSnapshot {
                                    track_id: h.track_id,
                                    base,
                                    handler: h.handler,
                                    enabled: h.enabled,
                                    raw_tkhd: h.raw_tkhd.clone(),
                                    raw_mdhd: h.raw_mdhd.clone(),
                                    raw_hdlr: h.raw_hdlr.clone(),
                                    raw_table: t.table.raw().clone(),
                                    raw_edits: t.edits.as_ref().map(|e| e.raw().clone()),
                                    raw_dref: t.raw_dref.clone(),
                                    native_catalogue,
                                },
                                descriptions: BudgetVec::new(&p.pool),
                                edits: t
                                    .edits
                                    .as_ref()
                                    .map(|e| {
                                        EditList::parse(e.raw().clone(), iso::Limits::default())
                                    })
                                    .transpose()?,
                            },
                        )?;
                        self.description = 1;
                        return Ok(false);
                    }
                    if let Some(d) = t.table.descriptions().get(self.description - 1) {
                        if !w.try_charge(1) {
                            return Ok(false);
                        }
                        push(
                            &mut p.tracks.as_mut_slice()[self.track].descriptions,
                            d.clone(),
                        )?;
                        self.description += 1;
                        return Ok(false);
                    }
                    self.track += 1;
                    self.description = 0;
                } else {
                    self.phase = 1;
                }
            }
            1 => {
                if let Some(raw) = self.movie.raw_roots.as_slice().get(self.root) {
                    if !w.try_charge(1) {
                        return Ok(false);
                    }
                    push(&mut p.raw_roots, raw.clone())?;
                    self.root += 1;
                } else {
                    self.phase = 2;
                }
            }
            2 => {
                if let Some(range) = self.movie.media.as_slice().get(self.extent) {
                    if !w.try_charge(1) {
                        return Ok(false);
                    }
                    push(&mut p.media, range.clone())?;
                    self.extent += 1;
                } else if let Some(extent) =
                    self.movie.media_cache.as_slice().get(p.media_cache.len())
                {
                    if !w.try_charge(1) {
                        return Ok(false);
                    }
                    push(&mut p.media_cache, extent.clone())?;
                } else {
                    self.phase = if self.range_duration.is_some() { 4 } else { 3 };
                }
            }
            3 => {
                let source = self
                    .source
                    .as_mut()
                    .ok_or(Error::Invalid("missing physical sample cursor"))?;
                if let Some((probe, refusal)) = self.first_sample_probe.as_mut() {
                    let media_end = p
                        .media
                        .as_slice()
                        .last()
                        .ok_or(Error::Invalid("first sample media extent missing"))?
                        .end;
                    match probe.poll(w)? {
                        SampleStep::Yield => return Ok(false),
                        SampleStep::Sample(sample)
                            if !sample.range.is_empty()
                                && sample.range.start >= p.source_length.unwrap_or(media_end) =>
                        {
                            p.unavailable_first = Some(SourceRow {
                                track_id: p.tracks.as_slice()[0].snapshot.track_id,
                                sample,
                            });
                            self.ready = true;
                            return Ok(true);
                        }
                        _ => return Err((*refusal).into()),
                    }
                }
                if let Some(sort) = self.source_sort.as_mut() {
                    if sort.poll(p.source_rows.as_mut_slice(), SourceRow::physical_key, w) {
                        self.phase = 4;
                    }
                    return Ok(false);
                }
                if self.pending_source.is_some() && !w.try_charge(1) {
                    return Ok(false);
                }
                if let Some(row) = self.pending_source.take() {
                    if let Some(terminal) = p.terminal_mdat {
                        terminal.require_sample(&row.sample)?;
                    }
                    if p.source_rows.len() as u64 >= self.limits.samples {
                        return Err(Error::Limit {
                            requested: p.source_rows.len() + 1,
                            maximum: self.limits.samples as usize,
                        }
                        .into());
                    }
                    match p.source_rows.try_push(row) {
                        Ok(()) => {}
                        Err(e) => {
                            self.pending_source = Some(e.value);
                            return Err(e.error.into());
                        }
                    }
                    return Ok(false);
                }
                let step = match source.poll(w, c) {
                    Err(_)
                        if p.read_scope == crate::mp4_open::MetadataReadScope::CompleteEntity
                            && source.resume_in_track_order() =>
                    {
                        self.source_track_order = true;
                        return Ok(false);
                    }
                    Err(
                        error @ (Error::Invalid("sample outside media extent")
                        | Error::Unsupported(
                            "physical merge needs nondecreasing per-track source offsets",
                        )),
                    ) if p.native_recovery
                        && p.source_rows.is_empty()
                        && p.terminal_mdat.is_none()
                        && !p.media.is_empty()
                        && self.movie.trex.is_empty() =>
                    {
                        let [track] = self.movie.tracks.as_slice() else {
                            return Err(error.into());
                        };
                        if !track.fragments.is_empty() || track.edits.is_some() {
                            return Err(error.into());
                        }
                        // Probe the first decode occurrence only after a layout refusal.
                        // The engine still needs the actual physical end before reporting it.
                        self.first_sample_probe = Some((track.table.cursor(), error));
                        return Ok(false);
                    }
                    result => result?,
                };
                match step {
                    InterleaveStatus::Yield => {}
                    InterleaveStatus::Ready => {
                        let r = source.take_sample()?;
                        self.pending_source = Some(SourceRow {
                            track_id: r.track_id,
                            sample: r.sample,
                        });
                    }
                    InterleaveStatus::End => {
                        p.forward_regular_extents =
                            source.deferred_media().then_some(self.limits.media_extents);
                        if self.source_track_order {
                            self.source_sort = Some(SampleSort::new(p.source_rows.len()));
                        } else {
                            self.phase = 4;
                        }
                    }
                }
            }
            4 => {
                if let Some(duration) = &mut self.range_duration {
                    let cursor = self
                        .native
                        .as_mut()
                        .ok_or(Error::Invalid("missing native range cursor"))?;
                    loop {
                        match cursor.poll(w, c)? {
                            Mov9CursorStatus::Yield => return Ok(false),
                            Mov9CursorStatus::Ready if !w.try_charge(1) => return Ok(false),
                            Mov9CursorStatus::Ready => {
                                let row = cursor.take_occurrence()?;
                                if !duration.scan {
                                    continue;
                                }
                                let Some(pts) = row.native.pts else { continue };
                                let end = pts
                                    .checked_add(row.native.duration)
                                    .ok_or(Error::Overflow)?;
                                let micros = row.media_clock.native_base.rescale(
                                    end,
                                    TimeBase::new(1, 1_000_000)?,
                                    Rounding::NearestAway,
                                )?;
                                duration.micros = duration.micros.max(micros);
                            }
                            Mov9CursorStatus::End => {
                                cursor.rewind_at_end()?;
                                self.ready = true;
                                return Ok(true);
                            }
                        }
                    }
                }
                if self.pending_native.is_some() && !w.try_charge(1) {
                    return Ok(false);
                }
                if let Some(mut row) = self.pending_native.take() {
                    if let Some(clock) = self.zero_scale_clock
                        && p.track(row.track_id)?.snapshot.base.get().is_none()
                    {
                        row.media_clock = clock;
                    }
                    let mut replay_native_order = p.replay_native_order;
                    if p.native_front && !replay_native_order {
                        let source = p.source_rows.as_slice().get(p.native_rows.len());
                        if source
                            .is_none_or(|s| s.track_id != row.track_id || s.sample != row.source)
                        {
                            // Only a complete retained extent can serve a repeated occurrence.
                            replay_native_order = matches!(
                                p.media.as_slice(),
                                [extent] if (1..=RETAINED_MEDIA_BYTES as u64)
                                    .contains(&(extent.end - extent.start))
                            ) && p.terminal_mdat.is_none()
                                && self.movie.trex.is_empty()
                                && p.tracks
                                    .as_slice()
                                    .iter()
                                    .any(|track| track.edits.is_some());
                            if !replay_native_order {
                                let reason = if source.is_none() {
                                    "native front repeated occurrence needs seekable context"
                                } else {
                                    "native front source occurrence order is not physical"
                                };
                                return Err(Error::Unsupported(reason).into());
                            }
                        }
                    }
                    if p.native_rows.len() as u64 >= self.limits.occurrences {
                        return Err(Error::Limit {
                            requested: p.native_rows.len() + 1,
                            maximum: self.limits.occurrences as usize,
                        }
                        .into());
                    }
                    let duration = if row.source.range.is_empty() {
                        None
                    } else {
                        p.next_native_empty_duration(&row, true)
                    };
                    match p.native_rows.try_push(row) {
                        Ok(()) => {
                            p.replay_native_order = replay_native_order;
                            p.native_empty_duration = duration;
                        }
                        Err(e) => {
                            self.pending_native = Some(e.value);
                            return Err(e.error.into());
                        }
                    }
                    return Ok(false);
                }
                if let Some(native) = self.native.as_mut() {
                    match native.poll(w, c)? {
                        Mov9CursorStatus::Yield => {}
                        Mov9CursorStatus::Ready => {
                            self.pending_native = Some(native.take_occurrence()?)
                        }
                        Mov9CursorStatus::End => {
                            self.track = 0;
                            self.phase = 5;
                        }
                    }
                } else {
                    self.phase = 5;
                }
            }
            5 if p.native_front && self.track < p.tracks.len() => {
                if !w.try_charge(1) {
                    return Ok(false);
                }
                let state = self
                    .native
                    .as_ref()
                    .ok_or(Error::Invalid("front native cursor"))?
                    .front_track_v1(self.track)?;
                push(&mut p.front_tracks, state)?;
                self.track += 1;
            }
            5 if p.native_front => {
                self.ready = true;
                return Ok(true);
            }
            _ => {
                self.ready = true;
                return Ok(true);
            }
        }
        Ok(false)
    }
    pub fn finish(mut self) -> Result<PreparedMp4> {
        if !self.ready {
            return Err(Error::Invalid("playback plan not ready").into());
        }
        self.plan
            .take()
            .ok_or_else(|| Error::Invalid("plan consumed").into())
    }

    pub(crate) fn finish_native_range(
        mut self,
    ) -> Result<(PreparedMp4, Mov9PacketCursor<'a>, i64)> {
        let duration = self
            .range_duration
            .take()
            .ok_or(Error::Invalid("missing range duration"))?;
        let cursor = self
            .native
            .take()
            .ok_or(Error::Invalid("missing native range cursor"))?;
        Ok((self.finish()?, cursor, duration.micros))
    }
}
/// Source movie-time mapping uses the existing format job. The command binds
/// the actual model/pool/entity, so callers cannot invent a source mapping.
pub struct MovieSeekJob<'a> {
    locator: iso::MovieEditLocator<'a>,
    source: triergon_core::SourceId,
    generation: u64,
    pool: Budget,
    track: u32,
    raw_media_clock: BoxView,
    raw_edits: Option<BoxView>,
}
pub struct MovieSeekCommand {
    pub(crate) point: iso::MoviePoint,
    pub(crate) source: triergon_core::SourceId,
    pub(crate) generation: u64,
    pub(crate) pool: Budget,
    pub(crate) track: u32,
    raw_media_clock: BoxView,
    raw_edits: Option<BoxView>,
}
impl MovieSeekCommand {
    pub fn point(&self) -> &iso::MoviePoint {
        &self.point
    }
    pub fn raw_media_clock(&self) -> &BoxView {
        &self.raw_media_clock
    }
    pub fn raw_edits(&self) -> Option<&BoxView> {
        self.raw_edits.as_ref()
    }
}
impl PreparedMp4 {
    pub fn movie_seek(
        &self,
        track: u32,
        requested_movie_time: triergon_core::Time,
        maximum_edits: u32,
    ) -> Result<MovieSeekJob<'_>> {
        let t = self.track(track)?;
        let locator = iso::MovieEditLocator::new(
            t.edits.as_ref(),
            iso::MovieSeekClocks {
                movie_base: self.movie_base,
                media_base: t.snapshot.base.require()?,
                media_duration: None,
            },
            requested_movie_time,
            maximum_edits,
        )?;
        Ok(MovieSeekJob {
            locator,
            source: self.source,
            generation: self.opening_generation,
            pool: self.pool.clone(),
            track,
            raw_media_clock: t.snapshot.raw_mdhd.clone(),
            raw_edits: t.snapshot.raw_edits.clone(),
        })
    }
}
impl MovieSeekJob<'_> {
    pub fn poll(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<bool> {
        Ok(self.locator.poll(w, c)? == iso::BuildStatus::Ready)
    }
    pub fn finish(self) -> Result<MovieSeekCommand> {
        Ok(MovieSeekCommand {
            point: self.locator.finish()?,
            source: self.source,
            generation: self.generation,
            pool: self.pool,
            track: self.track,
            raw_media_clock: self.raw_media_clock,
            raw_edits: self.raw_edits,
        })
    }
}
#[derive(Clone, Debug)]
pub enum MovieSeekOutcome {
    Applied(iso::MovieSourcePoint),
    Yield,
    Empty(iso::MovieEmptyEdit),
    Outside {
        requested_movie_time: triergon_core::Time,
    },
}
