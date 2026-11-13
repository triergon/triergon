//! Owned, charged playback plan produced by borrowing actual compact indexes.
//! The temporary IndexJob's lifetime is explicit; no self-reference or copied core.
use crate::{
    Result,
    mp4_open::{CachedMedia, Movie, RETAINED_MEDIA_BYTES},
    sample_sort::SampleSort,
};
use std::{
    alloc::Layout,
    ops::{Deref, Range},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use triergon_core::container::MediaKind;
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, Reservation, Rounding, Time, TimeBase, WorkBudget,
};
use triergon_isobmff::{
    self as iso, BoxView, EditList, InterleaveStatus, PhysicalInterleaveLimits,
    PhysicalSampleCursor, Sample, SampleCursor, SampleDescription, SampleStep, SourceTrack,
    compat::mov::{
        clock::{MediaClock, SourceTimeBase},
        edits::FragmentAacTrimJob,
        packets::{
            CursorStatus, DemuxProfile, DemuxTrack, PacketCursor, PacketLimits, PacketOccurrence,
            StreamView,
        },
    },
};
fn push<T>(v: &mut BudgetVec<T>, x: T) -> Result<()> {
    v.try_push(x).map_err(|e| e.error)
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
    fn native_packet_limits(self) -> PacketLimits {
        PacketLimits {
            max_tracks: self.tracks,
            max_media_extents: self.media_extents,
            max_source_samples: self.samples,
            max_occurrences: self.occurrences,
            ..PacketLimits::default()
        }
    }
}
/// Immutable track metadata shared by the plan and its emitted packets.
#[derive(Clone, Debug)]
pub struct TrackSnapshot(Arc<TrackMetadata>);

#[derive(Debug)]
pub struct TrackMetadata {
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
    pub native_catalogue: std::result::Result<StreamView, Error>,
    _charge: Reservation,
}

impl TrackSnapshot {
    fn reserve(pool: &Budget) -> Result<Reservation> {
        let (layout, _) = Layout::new::<[AtomicUsize; 2]>()
            .extend(Layout::new::<TrackMetadata>())
            .map_err(|_| Error::Overflow)?;
        pool.reserve(layout.pad_to_align().size())
    }
}

impl Deref for TrackSnapshot {
    type Target = TrackMetadata;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
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
    pub(crate) fragment_aac_trim: u32,
    pub snapshot: TrackSnapshot,
    pub descriptions: BudgetVec<SampleDescription>,
    pub edits: Option<EditList>,
}
impl PlanTrack {
    pub(crate) fn configuration(
        &self,
        description_index: u32,
        revision: u64,
    ) -> Result<Mp4Configuration> {
        let description = self
            .descriptions
            .as_slice()
            .iter()
            .find(|d| d.index == description_index)
            .ok_or(Error::Invalid("sample description absent"))?;
        Ok(Mp4Configuration {
            track: self.snapshot.clone(),
            description: description.clone(),
            revision,
        })
    }
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
    /// A single track's first sample declared at or beyond the end of the media
    /// read while indexing. Only the physical end of the source shows whether it
    /// lies outside the source.
    pub(crate) unavailable_first: Option<SourceRow>,
    pub(crate) source: triergon_core::SourceId,
    pub(crate) opening_generation: u64,
    pub(crate) source_length: Option<u64>,
    pub(crate) view: Mp4View,
    pub(crate) native_front: bool,
    pub(crate) replay_native_order: bool,
    pub(crate) native_front_boundary: Option<iso::compat::mov::front::FrontReadBoundary>,
    pub(crate) native_empty_duration: Option<i64>,
    pub(crate) front_tracks: BudgetVec<iso::compat::mov::front::FrontTrack>,
    pub(crate) source_rows: BudgetVec<SourceRow>,
    pub(crate) native_rows: BudgetVec<PacketOccurrence>,
    pub(crate) encoded_edits: Option<BudgetVec<Option<iso::compat::mov::edits::EncodedEditPlan>>>,
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
            .partition_point(|extent| extent.start() <= range.start)
            .checked_sub(1)
        else {
            return Ok(None);
        };
        let extent = &cache[index];
        let end = range
            .end
            .checked_sub(extent.start())
            .ok_or(Error::Overflow)?;
        if end > extent.bytes().len() as u64 {
            return Ok(None);
        }
        let start = (range.start - extent.start()) as usize;
        Ok(Some(extent.bytes().slice(start..end as usize)?))
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
    pub fn native_rows(&self) -> &[PacketOccurrence] {
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
    pub fn native_front_read_boundary_v1(
        &self,
    ) -> Option<iso::compat::mov::front::FrontReadBoundary> {
        self.native_front_boundary
    }
    pub(crate) fn next_native_empty_duration(
        &self,
        packet: &PacketOccurrence,
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
            .demux
            .dts?
            .checked_add(i64::from(packet.source.duration_ticks))?;
        Some(previous.max(end))
    }
    pub fn rows(&self) -> usize {
        if self.native_front && !self.replay_native_order && self.encoded_edits.is_none() {
            return self.source_rows.len();
        }
        match self.view {
            Mp4View::SourcePhysical => self.source_rows.len(),
            Mp4View::PinnedMov9NoParseNoFillin => self.native_rows.len(),
        }
    }
    pub fn row(&self, at: usize) -> Option<SourceRow> {
        if self.native_front && !self.replay_native_order && self.encoded_edits.is_none() {
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
            .ok_or(Error::Invalid("unknown plan track"))
    }
    pub fn native(&self, at: usize) -> Option<&PacketOccurrence> {
        if self.view == Mp4View::PinnedMov9NoParseNoFillin {
            self.native_rows.as_slice().get(at)
        } else {
            None
        }
    }
    pub fn config(&self, row: &SourceRow, revision: u64) -> Result<Mp4Configuration> {
        self.track(row.track_id)?
            .configuration(row.sample.description_index, revision)
    }
}
pub struct Mp4IndexJob<'a> {
    movie: &'a Movie,
    plan: Option<PreparedMp4>,
    source: Option<PhysicalSampleCursor<'a>>,
    native: Option<PacketCursor<'a>>,
    range_duration: Option<RangeDuration>,
    limits: PlanLimits,
    phase: u8,
    track: usize,
    description: usize,
    fragment_aac_trim: Option<FragmentAacTrimJob>,
    root: usize,
    extent: usize,
    ready: bool,
    pending_source: Option<SourceRow>,
    pending_native: Option<PacketOccurrence>,
    source_track_order: bool,
    source_sort: Option<SampleSort>,
    zero_scale_clock: Option<MediaClock>,
    first_sample_probe: Option<(SampleCursor<'a>, Error)>,
}

const OUTSIDE_MEDIA: &str = "sample outside media extent";
const BACKWARD_LAYOUT: &str = "physical merge needs nondecreasing per-track source offsets";
/// The strict view's refusal of a first sample that the source continues past.
pub(crate) const BACKWARD_LAYOUT_REFUSAL: Error = Error::Unsupported(BACKWARD_LAYOUT);

struct RangeDuration {
    micros: i64,
    scan: bool,
    media_tracks_only: bool,
}

pub(crate) fn include_media_track(movie: &Movie, ordinal: usize) -> bool {
    iso::media_kind(&movie.tracks()[ordinal].header().handler) != MediaKind::Data
}

impl<'a> Mp4IndexJob<'a> {
    /// Explicit native view of one from-start front-metadata source. Source
    /// clocks/extents are unchanged; the engine must retain actual EOF authority.
    /// This does not establish a complete-source fact for an isolated fragment.
    pub fn new_native_terminal_v1(movie: &'a Movie, limits: PlanLimits) -> Result<Self> {
        if !matches!(
            movie.read_scope(),
            crate::mp4_open::MetadataReadScope::FrontMetadataBeforeMdat(_)
        ) {
            return Err(Error::Invalid(
                "native front operation needs actual front metadata",
            ));
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
    pub(crate) fn for_forward_encoded_edits(
        movie: &'a Movie,
        plans: &'a [Option<iso::compat::mov::edits::EncodedEditPlan>],
        limits: PlanLimits,
    ) -> Result<Self> {
        if plans.len() != movie.tracks().len() {
            return Err(Error::Invalid("encoded edit track coverage"));
        }
        if movie.native_recovery()
            || !matches!(
                movie.read_scope(),
                crate::mp4_open::MetadataReadScope::FrontMetadataBeforeMdat(_)
            )
            || movie.media().len() != 1
            || movie.terminal_mdat().is_some()
            || !movie.trex().is_empty()
            || !plans.iter().any(Option::is_some)
        {
            return Err(Error::Unsupported(
                "forward encoded edits require regular front media",
            ));
        }
        Self::new_inner(
            movie,
            Mp4View::PinnedMov9NoParseNoFillin,
            limits,
            true,
            Some(plans),
            None,
        )
    }

    /// Build the native range view with explicitly validated encoded edits.
    /// The retained plans also govern seeks on this exact range index.
    pub fn new_native_encoded_range(
        movie: &'a Movie,
        plans: &'a [Option<iso::compat::mov::edits::EncodedEditPlan>],
        limits: PlanLimits,
    ) -> Result<Self> {
        if movie.native_recovery()
            || movie.read_scope() != crate::mp4_open::MetadataReadScope::CompleteEntity
        {
            return Err(Error::Unsupported(
                "encoded edits require a complete native range source",
            ));
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
        plans: Option<&'a [Option<iso::compat::mov::edits::EncodedEditPlan>]>,
        limits: PlanLimits,
        media_tracks_only: bool,
    ) -> Result<Self> {
        if movie.native_recovery()
            || movie.read_scope() != crate::mp4_open::MetadataReadScope::CompleteEntity
        {
            return Err(Error::Unsupported(
                "native range playback requires a complete source",
            ));
        }
        let header = movie.header();
        let ticks = header.declared_duration_ticks().unwrap_or(0);
        let duration = Time::from_ticks(i128::from(ticks), header.time_base()?)?;
        let micros =
            match duration.to_i64_ticks(TimeBase::new(1, 1_000_000)?, Rounding::NearestAway) {
                Ok(value) => value,
                Err(Error::Overflow) => 0,
                Err(error) => return Err(error),
            };
        Self::new_inner(
            movie,
            Mp4View::PinnedMov9NoParseNoFillin,
            limits,
            false,
            plans,
            Some(RangeDuration {
                micros,
                scan: micros == 0,
                media_tracks_only,
            }),
        )
    }

    fn new_inner(
        movie: &'a Movie,
        view: Mp4View,
        limits: PlanLimits,
        native_front: bool,
        encoded_edits: Option<&'a [Option<iso::compat::mov::edits::EncodedEditPlan>]>,
        range_duration: Option<RangeDuration>,
    ) -> Result<Self> {
        if movie.native_recovery() && view == Mp4View::SourcePhysical {
            return Err(Error::Invalid(
                "native recovery open requires native packet view",
            ));
        }
        if limits.tracks == 0 || limits.tracks > 64 || movie.tracks().len() > limits.tracks {
            return Err(Error::Limit {
                requested: movie.tracks().len(),
                maximum: limits.tracks.min(64),
            });
        }
        if movie.terminal_mdat().is_some() && view != Mp4View::SourcePhysical && !native_front {
            return Err(Error::Unsupported(
                "native terminal MDAT context is not established",
            ));
        }
        let media_count = movie
            .media()
            .len()
            .checked_add(usize::from(movie.terminal_mdat().is_some()))
            .ok_or(Error::Overflow)?;
        if media_count > limits.media_extents {
            return Err(Error::Limit {
                requested: media_count,
                maximum: limits.media_extents,
            });
        }
        let b = movie.pool();
        let mut sources = BudgetVec::new(b);
        let mut native = BudgetVec::new(b);
        let mut zero_scale_clock = None;
        for t in movie.tracks() {
            let media_base = if t.header().timescale == 0 && movie.native_recovery() {
                if !movie.trex().is_empty()
                    || !t.fragments().is_empty()
                    || t.edits().is_some()
                    || t.table().sample_count() == 0
                    || t.header().handler != *b"vide"
                    || !matches!(t.table().descriptions(), [entry] if entry.format == *b"raw ")
                {
                    return Err(Error::Unsupported("zero-scale native track context"));
                }
                let clock =
                    MediaClock::for_zero_scale(iso::compat::mov::recovery::RunRecoveryProfile)?;
                zero_scale_clock = Some(clock);
                clock.demux_base
            } else {
                t.header().time_base()?
            };
            if range_duration.is_none() {
                push(
                    &mut sources,
                    SourceTrack::new(t.header().track_id, Some(t.table()), t.fragments())?,
                )?;
            }
            push(
                &mut native,
                DemuxTrack {
                    track_id: t.header().track_id,
                    table: t.table(),
                    fragments: t.fragments(),
                    media_base,
                    edits: t
                        .edits()
                        .map(|e| Ok::<_, Error>((e, movie.header().time_base()?)))
                        .transpose()?,
                },
            )?;
        }
        let interleave_limits = PhysicalInterleaveLimits {
            max_tracks: limits.tracks,
            max_samples: limits.samples,
        };
        let timing_policy = if view == Mp4View::SourcePhysical {
            iso::TimingRunPolicy::Strict
        } else {
            iso::TimingRunPolicy::BoundedPrefix
        };
        let forward_regular = match movie.read_scope() {
            crate::mp4_open::MetadataReadScope::FrontMetadataBeforeMdat(h)
                if movie.terminal_mdat().is_none()
                    && movie.media().len() == 1
                    && (!movie.native_recovery()
                        || movie
                            .source_length()
                            .is_none_or(|length| movie.media()[0].end <= length))
                    && (view == Mp4View::SourcePhysical || native_front)
                    && movie.trex().is_empty()
                    && movie
                        .tracks()
                        .iter()
                        .all(|track| track.edits().is_none() && track.fragments().is_empty()) =>
            {
                Some(iso::ForwardRegularMdat::new(h, movie.source_length())?)
            }
            _ => None,
        };
        let source = if range_duration.is_some() {
            None
        } else if let Some(bounds) = forward_regular {
            Some(
                PhysicalSampleCursor::new_forward_regular_with_timing_policy(
                    b,
                    sources.as_slice(),
                    movie.media(),
                    bounds,
                    interleave_limits,
                    timing_policy,
                )?,
            )
        } else if movie.terminal_mdat().is_some() {
            // Metadata traversal preserves declared source coordinates. Each row
            // is checked against the terminal declaration, then read physically.
            Some(PhysicalSampleCursor::with_timing_policy(
                b,
                sources.as_slice(),
                None,
                interleave_limits,
                timing_policy,
            )?)
        } else {
            Some(PhysicalSampleCursor::with_timing_policy(
                b,
                sources.as_slice(),
                Some(movie.media()),
                interleave_limits,
                timing_policy,
            )?)
        };
        let native = if view == Mp4View::PinnedMov9NoParseNoFillin {
            let native_limits = limits.native_packet_limits();
            let media_tracks_only = range_duration
                .as_ref()
                .is_some_and(|range| range.media_tracks_only);
            Some(if media_tracks_only {
                PacketCursor::new_projected(
                    b,
                    native.as_slice(),
                    movie.media(),
                    native_limits,
                    encoded_edits,
                    |index| include_media_track(movie, index),
                )?
            } else if let Some(plans) = encoded_edits.filter(|_| native_front) {
                PacketCursor::new_front_with_encoded_edits(
                    b,
                    native.as_slice(),
                    movie.media(),
                    native_limits,
                    plans,
                )?
            } else if let Some(plans) = encoded_edits {
                PacketCursor::new_encoded_regular(
                    b,
                    native.as_slice(),
                    movie.media(),
                    native_limits,
                    plans,
                    iso::compat::mov::edits::EncodedEditProfile,
                )?
            } else if let Some(bounds) = forward_regular {
                PacketCursor::new_front_regular(
                    b,
                    native.as_slice(),
                    movie.media(),
                    bounds,
                    native_limits,
                    DemuxProfile::NoParseNoFillin,
                )?
            } else if native_front {
                PacketCursor::new_front_source(
                    b,
                    native.as_slice(),
                    movie.media(),
                    movie.terminal_mdat(),
                    native_limits,
                    DemuxProfile::NoParseNoFillin,
                )?
            } else if movie.read_scope() == crate::mp4_open::MetadataReadScope::CompleteEntity {
                PacketCursor::new_complete_source(
                    b,
                    native.as_slice(),
                    movie.media(),
                    native_limits,
                    DemuxProfile::NoParseNoFillin,
                )?
            } else {
                PacketCursor::new(
                    b,
                    native.as_slice(),
                    movie.media(),
                    native_limits,
                    DemuxProfile::NoParseNoFillin,
                )?
            })
        } else {
            None
        };
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
            && let [track] = movie.tracks()
            && track.edits().is_none()
            && track.table().sample_count() == 0
            && track.header().handler == *b"vide"
            && matches!(track.table().descriptions(), [entry] if entry.format == *b"raw ")
        {
            i64::try_from(track.header().duration).ok()
        } else {
            None
        };
        Ok(Self {
            movie,
            plan: Some(PreparedMp4 {
                read_scope: movie.read_scope(),
                native_recovery: movie.native_recovery(),
                unavailable_first: None,
                source: movie.source(),
                opening_generation: movie.generation(),
                source_length: movie.source_length(),
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
                terminal_mdat: movie.terminal_mdat(),
                raw_roots: BudgetVec::new(b),
                movie_base: movie.header().time_base()?,
                pool: b.clone(),
            }),
            source,
            native,
            range_duration,
            limits,
            phase: 0,
            track: 0,
            description: 0,
            fragment_aac_trim: None,
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
            return Err(Error::Cancelled);
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
                if let Some(t) = self.movie.tracks().get(self.track) {
                    if self.description == 0 {
                        if !w.try_charge(1) {
                            return Ok(false);
                        }
                        let h = t.header();
                        let fragment_aac_trim = if p.view == Mp4View::PinnedMov9NoParseNoFillin {
                            FragmentAacTrimJob::new(self.movie, self.track)?
                        } else {
                            None
                        };
                        let (base, native_catalogue) = if h.timescale == 0 {
                            (
                                SourceTimeBase::UNAVAILABLE,
                                StreamView::for_zero_scale_source(
                                    h,
                                    t.table(),
                                    self.movie.major_brand(),
                                    iso::compat::mov::recovery::RunRecoveryProfile,
                                ),
                            )
                        } else {
                            (
                                h.time_base()?.into(),
                                StreamView::for_source(
                                    h,
                                    t.table(),
                                    !t.fragments().is_empty(),
                                    self.movie.major_brand(),
                                ),
                            )
                        };
                        push(
                            &mut p.tracks,
                            PlanTrack {
                                trex: self
                                    .movie
                                    .trex()
                                    .iter()
                                    .find(|x| x.track_id == t.header().track_id)
                                    .copied(),
                                next_ordinal: t.next_ordinal(),
                                fragment_aac_trim: 0,
                                snapshot: TrackSnapshot(Arc::new(TrackMetadata {
                                    track_id: h.track_id,
                                    base,
                                    handler: h.handler,
                                    enabled: h.enabled,
                                    raw_tkhd: h.raw_tkhd.clone(),
                                    raw_mdhd: h.raw_mdhd.clone(),
                                    raw_hdlr: h.raw_hdlr.clone(),
                                    raw_table: t.table().raw().clone(),
                                    raw_edits: t.edits().map(|e| e.raw().clone()),
                                    raw_dref: t.raw_dref().clone(),
                                    native_catalogue,
                                    _charge: TrackSnapshot::reserve(&p.pool)?,
                                })),
                                descriptions: BudgetVec::new(&p.pool),
                                edits: t
                                    .edits()
                                    .map(|e| {
                                        EditList::parse(e.raw().clone(), iso::Limits::default())
                                    })
                                    .transpose()?,
                            },
                        )?;
                        self.fragment_aac_trim = fragment_aac_trim;
                        self.description = 1;
                        return Ok(false);
                    }
                    if let Some(job) = &mut self.fragment_aac_trim {
                        let Some(samples) = job.poll(w, c)? else {
                            return Ok(false);
                        };
                        p.tracks.as_mut_slice()[self.track].fragment_aac_trim = samples;
                        self.fragment_aac_trim = None;
                    }
                    if let Some(d) = t.table().descriptions().get(self.description - 1) {
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
                if let Some(raw) = self.movie.raw_roots().get(self.root) {
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
                if let Some(range) = self.movie.media().get(self.extent) {
                    if !w.try_charge(1) {
                        return Ok(false);
                    }
                    push(&mut p.media, range.clone())?;
                    self.extent += 1;
                } else if let Some(extent) = self.movie.media_cache().get(p.media_cache.len()) {
                    if !w.try_charge(1) {
                        return Ok(false);
                    }
                    push(&mut p.media_cache, extent.clone())?;
                } else {
                    self.phase = if let Some(duration) = &self.range_duration {
                        if let Some(cursor) = &mut self.native {
                            if !duration.scan {
                                cursor.check_projections_before_replay()?;
                            }
                            cursor.enable_range_seek_checkpoints();
                        }
                        4
                    } else {
                        3
                    };
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
                        _ => return Err(*refusal),
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
                        });
                    }
                    match p.source_rows.try_push(row) {
                        Ok(()) => {}
                        Err(e) => {
                            self.pending_source = Some(e.value);
                            return Err(e.error);
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
                    Err(error)
                        if match error {
                            Error::Invalid(OUTSIDE_MEDIA) => p.native_recovery,
                            // Without a source length the strict view cannot yet
                            // tell a first sample in a later mdat from one beyond
                            // the source end; the physical end decides.
                            Error::Unsupported(BACKWARD_LAYOUT) => {
                                p.native_recovery || p.source_length.is_none()
                            }
                            _ => false,
                        } && p.source_rows.is_empty()
                            && p.terminal_mdat.is_none()
                            && !p.media.is_empty()
                            && self.movie.trex().is_empty() =>
                    {
                        let [track] = self.movie.tracks() else {
                            return Err(error);
                        };
                        if !track.fragments().is_empty() || track.edits().is_some() {
                            return Err(error);
                        }
                        // Probe the first decode occurrence only after a layout refusal.
                        // The engine still needs the actual physical end before reporting it.
                        self.first_sample_probe = Some((track.table().cursor(), error));
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
                            CursorStatus::Yield => return Ok(false),
                            CursorStatus::Ready if !w.try_charge(1) => return Ok(false),
                            CursorStatus::Ready => {
                                let row = cursor.take_occurrence()?;
                                let Some(pts) = row.demux.pts else { continue };
                                let end =
                                    pts.checked_add(row.demux.duration).ok_or(Error::Overflow)?;
                                let micros = row.media_clock.demux_base.rescale(
                                    end,
                                    TimeBase::new(1, 1_000_000)?,
                                    Rounding::NearestAway,
                                )?;
                                duration.micros = duration.micros.max(micros);
                            }
                            CursorStatus::End => {
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
                    if p.native_front && p.encoded_edits.is_some() {
                        if p.native_rows.as_slice().last().is_some_and(|previous| {
                            previous.source.range.end > row.source.range.start
                        }) {
                            return Err(Error::Unsupported("forward encoded edits revisit media"));
                        }
                    } else if p.native_front && !replay_native_order {
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
                                && self.movie.trex().is_empty()
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
                                return Err(Error::Unsupported(reason));
                            }
                        }
                    }
                    if p.native_rows.len() as u64 >= self.limits.occurrences {
                        return Err(Error::Limit {
                            requested: p.native_rows.len() + 1,
                            maximum: self.limits.occurrences as usize,
                        });
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
                            return Err(e.error);
                        }
                    }
                    return Ok(false);
                }
                if let Some(native) = self.native.as_mut() {
                    match native.poll(w, c)? {
                        CursorStatus::Yield => {}
                        CursorStatus::Ready => {
                            self.pending_native = Some(native.take_occurrence()?);
                        }
                        CursorStatus::End => {
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
                    .front_track(self.track)?;
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
            return Err(Error::Invalid("playback plan not ready"));
        }
        self.plan.take().ok_or(Error::Invalid("plan consumed"))
    }

    pub(crate) fn finish_native_range(mut self) -> Result<(PreparedMp4, PacketCursor<'a>, i64)> {
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
        requested_movie_time: Time,
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
    Outside { requested_movie_time: Time },
}
