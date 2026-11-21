//! The container engines a player drives, one per format.

pub(crate) mod matroska;
pub(crate) mod mp4;
pub(crate) mod ts;

pub(crate) use mp4::{Mp4RangeLanding, Mp4SyncLanding};

use crate::engine::matroska::MkvEngine;
use crate::engine::mp4::Mp4Engine;
use crate::engine::ts::{Delivery as TsDelivery, TsEngine};
use crate::event::EngineStep;
use crate::input::SourceReply;
use crate::{Format, Result, SeekIntent, TrackKey, TrackKind, io::provide, mp4_plan::PreparedMp4};
use std::sync::atomic::AtomicBool;
use triergon_core::container::{MediaKind, TrackInfo};
use triergon_core::{
    Budget, BudgetVec, Error, InputCursor, SourceCapabilities, SourceId, WorkBudget,
};
use triergon_isobmff::compat::mov::packets::PacketCursor;
use triergon_matroska::Reader;
use triergon_mpegts::compat::pes::DemuxClock;
use triergon_mpegts::psi::DescriptorNamespace;
use triergon_mpegts::stream::TransportStream;
use triergon_mpegts::{self as mpegts, clock::SourceTimePolicy};
#[derive(Clone, Copy, Debug)]
pub(crate) struct TrackFact {
    pub key: TrackKey,
    pub kind: TrackKind,
    pub enabled: bool,
}

pub(crate) fn track_kind(kind: MediaKind) -> TrackKind {
    match kind {
        MediaKind::Video => TrackKind::Video,
        MediaKind::Audio => TrackKind::Audio,
        MediaKind::Subtitle => TrackKind::Subtitle,
        MediaKind::Data | MediaKind::Unknown => TrackKind::Other,
    }
}

/// The demuxer of one container input, as a [`Player`](crate::Player) drives it.
#[expect(
    clippy::large_enum_variant,
    reason = "bounded inline format state; no uncharged boxed parser"
)]
#[derive(Debug)]
pub enum Engine {
    /// A Matroska reader.
    Matroska(MkvEngine),
    /// An indexed or progressive MP4 plan.
    Mp4(Mp4Engine),
    /// A transport stream program.
    Ts(TsEngine),
}

impl Engine {
    /// Hands a consumed PES's lists back to the observer of `pid`, when this
    /// is a transport stream engine.
    #[inline]
    pub(crate) fn reuse_pes_lists(&mut self, pid: u16, lists: ts::feed::SpentLists) {
        if let Self::Ts(engine) = self {
            engine.reuse_pes_lists(pid, lists);
        }
    }
    /// Play an unstarted Matroska reader configured by the caller, for example
    /// with `Reader::new`, `Reader::new_with_payload_ownership`,
    /// `Reader::new_with_immutable_seek_index` or
    /// `compat::block_additions::new_reader`. A player refuses the engine if
    /// the reader has already read or repositioned its input.
    pub fn matroska(reader: Reader) -> Self {
        let source = reader.source();
        Self::Matroska(MkvEngine { reader, source })
    }
    /// Plays the indexed MP4 `plan` over `input`, with packets of at most `maximum_packet`
    /// bytes. Fails with `Unsupported` for a front demux plan, which
    /// [`compat::mov::open_front`](crate::compat::mov::open_front) plays.
    pub fn mp4(input: InputCursor, plan: PreparedMp4, maximum_packet: usize) -> Result<Self> {
        let engine = Self::mp4_unattached(input, plan, maximum_packet)?;
        if matches!(&engine, Self::Mp4(mp4) if mp4.plan.demux_front) {
            return Err(Error::Unsupported(
                "native front plan requires actual source completion",
            ));
        }
        if matches!(&engine, Self::Mp4(mp4) if mp4.plan.terminal_mdat.is_some()) {
            return Err(Error::Unsupported(
                "open terminal MDAT needs progressive source completion",
            ));
        }
        if matches!(&engine, Self::Mp4(mp4) if mp4.plan.forward_regular_extents.is_some()) {
            return Err(Error::Unsupported(
                "forward regular MDAT needs progressive source completion",
            ));
        }
        Ok(engine)
    }
    fn mp4_unattached(
        input: InputCursor,
        plan: PreparedMp4,
        maximum_packet: usize,
    ) -> Result<Self> {
        if !input.budget().same_pool(&plan.pool) {
            return Err(Error::ForeignPool);
        }
        if input.source() != plan.source {
            return Err(Error::SourceMismatch);
        }
        if input.generation() != plan.opening_generation {
            return Err(Error::StaleGeneration {
                expected: plan.opening_generation,
                received: input.generation(),
            });
        }
        if input.capabilities().length != plan.source_length {
            return Err(Error::Invalid("plan source extent changed"));
        }
        if maximum_packet == 0 {
            return Err(Error::Invalid("zero packet policy"));
        }
        let mut engine = Mp4Engine {
            progressive: None,
            media_cache_fill: None,
            front_boundary: None,
            empty_run: None,
            ended_short: false,
            input,
            plan,
            row: 0,
            skip_video_until: None,
            demux_seek: None,
            packet: None,
            configs: [None; 64],
            revision: 0,
            maximum_packet,
        };
        if engine.plan.demux_order_replay {
            engine.media_cache_fill = Some(mp4::progressive::prepare_media_replay(
                &engine.input,
                &mut engine.plan,
            )?);
        }
        Ok(Self::Mp4(engine))
    }
    /// As [`Self::mp4`], continuing through later `moof` and `mdat` pairs within
    /// `limits`. Fails with `Unsupported` for a front demux plan.
    pub fn mp4_progressive(
        input: InputCursor,
        plan: PreparedMp4,
        maximum_packet: usize,
        limits: mp4::progressive::ProgressiveLimits,
    ) -> Result<Self> {
        if plan.demux_front {
            return Err(Error::Unsupported(
                "native front plan needs explicit native attachment",
            ));
        }
        Self::mp4_with_progressive(input, plan, maximum_packet, limits)
    }
    /// Complete one actual front-metadata source using the explicit nonseekable
    /// demux view. A terminal MDAT finishes only on a matching Source End.
    pub(crate) fn mp4_front(
        input: InputCursor,
        plan: PreparedMp4,
        maximum_packet: usize,
        limits: mp4::progressive::ProgressiveLimits,
    ) -> Result<Self> {
        if !plan.demux_front || input.capabilities().random_access {
            return Err(Error::Invalid("native front attachment context"));
        }
        Self::mp4_with_progressive(input, plan, maximum_packet, limits)
    }
    fn mp4_with_progressive(
        input: InputCursor,
        plan: PreparedMp4,
        maximum_packet: usize,
        limits: mp4::progressive::ProgressiveLimits,
    ) -> Result<Self> {
        let state = mp4::progressive::ProgressiveState::new(&plan, limits)?;
        let mut engine = Self::mp4_unattached(input, plan, maximum_packet)?;
        if let Self::Mp4(e) = &mut engine {
            e.progressive = Some(state);
        }
        Ok(engine)
    }
    /// Source events of one program of a transport stream: carriers, tables,
    /// PES with their source clocks, and faults. The source's end ends the stream.
    pub fn ts(
        input: InputCursor,
        format: mpegts::PacketFormat,
        program: u16,
        limits: mpegts::ParseLimits,
        policy: SourceTimePolicy,
        namespace: DescriptorNamespace,
    ) -> Result<Self> {
        TsEngine::program(input, format, program, limits, policy, namespace).map(Self::Ts)
    }

    /// [`Self::ts`] over a sequence of entities: each entity's end is reported
    /// as [`Event::EntityEnd`](crate::Event::EntityEnd), and the caller continues with the next one or
    /// finishes the logical source.
    pub fn ts_entities(
        input: InputCursor,
        format: mpegts::PacketFormat,
        program: u16,
        limits: mpegts::ParseLimits,
        policy: SourceTimePolicy,
        namespace: DescriptorNamespace,
    ) -> Result<Self> {
        let engine = TsEngine::program(input, format, program, limits, policy, namespace)?;
        Ok(Self::Ts(TsEngine {
            entity_sequence: true,
            ..engine
        }))
    }

    /// Whether the engine delivers demux PES: a TS engine with selected PIDs, or one
    /// that finds its segment's tracks.
    #[cfg(feature = "reference-trace")]
    pub fn delivers_demux_pes(&self) -> bool {
        matches!(self, Self::Ts(t) if t.delivery.discovers_tracks() || !t.pes_tracks.as_slice().is_empty())
    }
    pub(crate) fn assembles_pid(&self, pid: u16) -> bool {
        matches!(self, Self::Ts(t) if t.pes_tracks.index(pid).is_some())
    }
    pub(crate) fn ts_hls_segment(
        input: InputCursor,
        limits: mpegts::ParseLimits,
        media: bool,
    ) -> Result<Self> {
        TsEngine::segment(
            input,
            limits,
            SourceTimePolicy::Unanchored,
            DescriptorNamespace::Unspecified,
            if media {
                TsDelivery::MediaFeed
            } else {
                TsDelivery::AvcAacFeed
            },
        )
        .map(Self::Ts)
    }

    pub(crate) fn continue_entity(
        &mut self,
        source: SourceId,
        caps: SourceCapabilities,
        discontinuity: bool,
    ) -> Result<()> {
        let Self::Ts(t) = self else {
            return Err(Error::Invalid("entity continuation requires TS source"));
        };
        let end = t
            .entity_end
            .as_ref()
            .ok_or(Error::Invalid("actual entity End required"))?;
        if source == end.location().source {
            return Err(Error::SourceMismatch);
        }
        if discontinuity {
            t.stream.discontinuity()?;
        }
        t.input.continue_source(source, caps)?;
        end.validate_continuation(&t.input)?;
        t.entity_end = None;
        Ok(())
    }
    pub(crate) fn finish_logical_source(&mut self) -> Result<()> {
        let Self::Ts(t) = self else {
            return Err(Error::Invalid("logical TS End required"));
        };
        if t.entity_end.is_none() {
            return Err(Error::Invalid("actual source entity End required"));
        }
        t.stream.end_of_source()?;
        t.entity_end = None;
        t.draining = true;
        Ok(())
    }
    /// The container format.
    pub fn format(&self) -> Format {
        match self {
            Self::Matroska(_) => Format::Matroska,
            Self::Mp4(_) => Format::Mp4,
            Self::Ts(t) => Format::Ts(t.format),
        }
    }
    pub(crate) fn validate_binding(&self) -> Result<()> {
        if let Self::Matroska(m) = self
            && !m.reader.is_unstarted()
        {
            return Err(Error::Invalid("Matroska engine needs an unstarted reader"));
        }
        if let Self::Mp4(r) = self {
            if !r.input.budget().same_pool(&r.plan.pool) {
                return Err(Error::ForeignPool);
            }
            if r.input.source() != r.plan.source {
                return Err(Error::SourceMismatch);
            }
            if r.input.generation() != r.plan.opening_generation {
                return Err(Error::StaleGeneration {
                    expected: r.plan.opening_generation,
                    received: r.input.generation(),
                });
            }
            if r.input.capabilities().length != r.plan.source_length {
                return Err(Error::Invalid("plan source extent changed"));
            }
        }
        Ok(())
    }
    /// The source the engine reads.
    pub fn source(&self) -> SourceId {
        match self {
            Self::Matroska(m) => m.source,
            Self::Mp4(m) => m.input.source(),
            Self::Ts(t) => t.input.source(),
        }
    }
    /// The input generation; a seek starts a new one.
    pub fn generation(&self) -> u64 {
        match self {
            Self::Matroska(r) => r.generation(),
            Self::Mp4(r) => r.input.generation(),
            Self::Ts(r) => r.input.generation(),
        }
    }
    /// The budget the engine's storage is charged to.
    pub fn pool(&self) -> &Budget {
        match self {
            Self::Matroska(r) => r.budget(),
            Self::Mp4(r) => r.input.budget(),
            Self::Ts(r) => r.input.budget(),
        }
    }
    /// Hands a source reply to the engine, and returns whether it made progress: `false`
    /// for `Pending` or empty data. A failure reply fails with `Transport` or
    /// `Cancelled`.
    pub fn provide(&mut self, reply: &SourceReply) -> Result<bool> {
        match self {
            Self::Mp4(r) => provide(&mut r.input, reply),
            Self::Ts(r) => r.provide(reply),
            Self::Matroska(r) => match reply {
                SourceReply::Pending => Ok(false),
                SourceReply::Data(w) => {
                    r.reader.provide(w)?;
                    Ok(!w.bytes().is_empty())
                }
                SourceReply::End { source, token } => {
                    r.reader.finish(*source, token.clone())?;
                    Ok(true)
                }
                SourceReply::Failed {
                    source,
                    token,
                    failure,
                } => {
                    r.reader.fail(*source, token.clone(), *failure)?;
                    Err(match failure {
                        triergon_core::SourceFailure::Transport => Error::Transport,
                        triergon_core::SourceFailure::Cancelled => Error::Cancelled,
                    })
                }
            },
        }
    }
    /// Demuxes within `w`. Fails with `Invalid` for a compact progressive TS engine,
    /// which only its reader polls.
    pub fn poll(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<EngineStep> {
        match self {
            Self::Matroska(r) => Ok(EngineStep::from_matroska(r.step(w, c)?)),
            Self::Mp4(r) => r.poll(w, c, None),
            Self::Ts(r) => {
                if r.delivery == TsDelivery::CompactProgressive {
                    return Err(Error::Invalid("compact TS payload polling required"));
                }
                r.poll::<EngineStep>(w, c)
            }
        }
    }

    pub(crate) fn take_suppressed_events(&mut self) -> u64 {
        match self {
            Self::Ts(engine) => engine.take_suppressed_events(),
            _ => 0,
        }
    }

    pub(crate) fn poll_mp4_range(
        &mut self,
        cursor: &mut PacketCursor<'_>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<EngineStep> {
        match self {
            Self::Mp4(mp4) => mp4.poll(work, cancel, Some(cursor)),
            _ => Err(Error::Invalid("native range cursor requires MP4")),
        }
    }
    pub(crate) fn facts(&self) -> Result<BudgetVec<TrackFact>> {
        let mut out = BudgetVec::new(self.pool());
        match self {
            Self::Matroska(r) => {
                if r.tracks().len() > 64 {
                    return Err(Error::Limit {
                        requested: r.tracks().len(),
                        maximum: 64,
                    });
                }
                for t in r.tracks() {
                    out.try_push(TrackFact {
                        key: TrackKey::Matroska(t.number),
                        kind: track_kind(t.kind()),
                        enabled: t.enabled,
                    })
                    .map_err(|e| e.error)?;
                }
            }
            Self::Mp4(r) => {
                for t in r.plan.tracks.as_slice() {
                    let h = &t.snapshot;
                    out.try_push(TrackFact {
                        key: TrackKey::Mp4(h.track_id),
                        kind: track_kind(triergon_isobmff::media_kind(&h.handler)),
                        enabled: h.enabled,
                    })
                    .map_err(|e| e.error)?;
                }
            }
            Self::Ts(r) => {
                out.try_extend_from_slice(r.facts.as_slice())?;
            }
        }
        Ok(out)
    }
    /// Matroska uses validated Cues, and MP4 can use a checked sync landing.
    /// Without an indexed landing, MP4 and TS replay preceding state.
    pub(crate) fn begin_seek(
        &mut self,
        intent: &SeekIntent,
        mp4_landing: Option<Mp4RangeLanding<'_>>,
    ) -> Result<()> {
        match self {
            Self::Matroska(r) => {
                let TrackKey::Matroska(number) = intent.track else {
                    return Err(Error::Invalid("seek track format"));
                };
                let at = r
                    .tracks()
                    .iter()
                    .position(|t| t.number == number && t.kind == 1)
                    .ok_or(Error::Invalid("seek needs source video track"))?;
                r.reader.seek_for_playback(
                    at,
                    intent
                        .target
                        .to_nanoseconds(triergon_core::Rounding::Floor)?,
                )?;
            }
            Self::Mp4(r) => {
                if r.progressive.is_some() {
                    return Err(Error::Unsupported(
                        "progressive replay requires retained source/index context",
                    ));
                }
                if !r.input.capabilities().random_access {
                    return Err(Error::Unsupported("MP4 replay needs source ranges"));
                }
                if r.plan.unavailable_first.is_some() {
                    return Err(Error::Unsupported("native first sample is unavailable"));
                }
                let TrackKey::Mp4(id) = intent.track else {
                    return Err(Error::Invalid("seek track format"));
                };
                let t = r.plan.track(id)?;
                if t.snapshot.handler != *b"vide" {
                    return Err(Error::Invalid("MP4 seek track is not video"));
                }
                let (row, offset, skip_video_until, demux_seek) = match mp4_landing {
                    Some(Mp4RangeLanding::Source(landing)) => {
                        let rows = r.plan.source_rows();
                        let first = rows
                            .get(landing.first_row)
                            .ok_or(Error::Invalid("MP4 seek first row"))?;
                        let sync = rows
                            .get(landing.sync_row)
                            .ok_or(Error::Invalid("MP4 seek sync row"))?;
                        if landing.first_row > landing.sync_row
                            || sync.track_id != id
                            || !sync.sample.is_container_sync()
                        {
                            return Err(Error::Invalid("MP4 seek landing"));
                        }
                        let skip_video = (landing.first_row < landing.sync_row)
                            .then_some((id, landing.sync_row));
                        (
                            landing.first_row,
                            first.sample.range.start,
                            skip_video,
                            None,
                        )
                    }
                    Some(Mp4RangeLanding::Demux(replay)) => {
                        let first = replay.first_row();
                        let occurrence = r
                            .plan
                            .demux_row(first)
                            .ok_or(Error::Invalid("MP4 native seek first row"))?;
                        (
                            first,
                            occurrence.source.range.start,
                            None,
                            Some(replay.clone()),
                        )
                    }
                    Some(Mp4RangeLanding::DemuxCursor(offset)) => (0, offset, None, None),
                    None => (0, 0, None, None),
                };
                r.input.reset(offset)?;
                r.row = row;
                r.skip_video_until = skip_video_until;
                r.demux_seek = demux_seek;
                r.packet = None;
                r.ended_short = false;
                r.configs = [None; 64];
            }
            Self::Ts(r) => {
                if r.delivery.discovers_tracks() {
                    return Err(Error::Unsupported("seek needs a fresh TS segment"));
                }
                if r.bound_context.is_some() {
                    return Err(Error::Unsupported(
                        "bound native startup needs a fresh observed context after seek",
                    ));
                }
                if !r.input.capabilities().random_access {
                    return Err(Error::Unsupported("TS replay needs source rewind"));
                }
                let TrackKey::Ts { program, pid: _ } = intent.track else {
                    return Err(Error::Invalid("seek track format"));
                };
                if program != r.program {
                    return Err(Error::Invalid("seek source program"));
                }
                if matches!(r.policy, SourceTimePolicy::Unanchored)
                    && intent.clock == crate::ClockChoice::Source
                {
                    return Err(Error::Unsupported("unanchored TS source time"));
                }
                r.input.reset(0)?;
                r.stream = TransportStream::new_with_namespace(
                    r.input.budget().clone(),
                    r.format,
                    r.limits,
                    r.program,
                    r.namespace,
                )?;
                r.source_pes_clock = Some(DemuxClock::new(r.program)?);
                r.tsid = None;
                r.pcr_pid = None;
                r.pending = None;
                r.ended = false;
                r.draining = false;
                for track in r.pes_tracks.as_mut_slice() {
                    track.observer = None;
                    track.update = None;
                }
                r.pes_active = None;
                r.pes_adopting = 0;
                r.pes_pending = None;
                r.pes_clock = Some(DemuxClock::new(r.program)?);
                r.event_ordinal = 0;
                r.pes_updates_from = None;
                r.entity_end = None;
            }
        }
        Ok(())
    }
}
