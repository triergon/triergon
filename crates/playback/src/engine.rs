//! The container engines a player drives, one per format.

pub(crate) mod matroska;
pub(crate) mod mp4;
pub(crate) mod ts;

pub(crate) use mp4::{Mp4RangeLanding, Mp4SyncLanding};

use crate::engine::matroska::MkvEngine;
use crate::engine::mp4::Mp4Engine;
use crate::engine::ts::{Delivery as TsDelivery, Entities, TsEngine};
use crate::event::EngineStep;
use crate::source::SourceReply;
use crate::{
    Format, Result, SeekIntent, TrackKey, TrackKind,
    mp4::plan::{PlanTrack, PreparedMp4},
};
use std::sync::atomic::AtomicBool;
use triergon_core::container::{CodecId, ColourDescription, MediaKind, TrackId, TrackInfo};
use triergon_core::{
    Budget, BudgetVec, Error, InputCursor, SourceCapabilities, SourceId, WorkBudget,
};
use triergon_isobmff::compat::mov::packets::PacketCursor;
use triergon_matroska::Reader;
use triergon_mpegts::psi::DescriptorNamespace;
use triergon_mpegts::stream::ProgramStream;
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

/// The format-neutral view of a track of an engine: a borrow of the format's own
/// track, never a copy.
#[derive(Clone, Copy, Debug)]
pub enum Track<'a> {
    /// A Matroska track.
    Matroska(&'a triergon_matroska::Track),
    /// A track of an MP4 plan.
    Mp4(&'a PlanTrack),
    /// An elementary stream of the transport stream's active program map.
    Ts {
        /// The program number of the map.
        program: u16,
        /// The stream's PID, type and descriptors.
        stream: ProgramStream<'a>,
    },
}

impl Track<'_> {
    /// The key this track's events carry.
    pub fn key(&self) -> TrackKey {
        match self {
            Self::Matroska(track) => TrackKey::Matroska(track.number),
            Self::Mp4(track) => TrackKey::Mp4(track.snapshot.track_id),
            Self::Ts { program, stream } => TrackKey::Ts {
                program: *program,
                pid: stream.pid(),
            },
        }
    }

    fn info(&self) -> &dyn TrackInfo {
        match self {
            Self::Matroska(track) => *track,
            Self::Mp4(track) => *track,
            Self::Ts { stream, .. } => stream,
        }
    }
}

impl TrackInfo for Track<'_> {
    fn id(&self) -> TrackId {
        self.info().id()
    }
    fn kind(&self) -> MediaKind {
        self.info().kind()
    }
    fn codec_tag(&self) -> &[u8] {
        self.info().codec_tag()
    }
    fn codec(&self) -> CodecId {
        self.info().codec()
    }
    fn configuration(&self) -> &[u8] {
        self.info().configuration()
    }
    fn language(&self) -> Option<&str> {
        self.info().language()
    }
    fn default_track(&self) -> Option<bool> {
        self.info().default_track()
    }
    fn forced(&self) -> Option<bool> {
        self.info().forced()
    }
    fn colour(&self) -> Option<ColourDescription> {
        self.info().colour()
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
        let engine = Mp4Engine::new(input, plan, maximum_packet)?;
        if engine.plan.demux_front {
            return Err(Error::Unsupported(
                "front demux plan requires progressive source completion",
            ));
        }
        if engine.plan.terminal_mdat.is_some() {
            return Err(Error::Unsupported(
                "open terminal MDAT needs progressive source completion",
            ));
        }
        if engine.plan.forward_regular_extents.is_some() {
            return Err(Error::Unsupported(
                "forward regular MDAT needs progressive source completion",
            ));
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
                "front demux plan must be opened with compat::mov::open_front",
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
            return Err(Error::Invalid(
                "expected a front demux plan and a nonseekable source",
            ));
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
        let mut engine = Mp4Engine::new(input, plan, maximum_packet)?;
        engine.progressive = Some(state);
        Ok(Self::Mp4(engine))
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
            entities: Entities::Sequence,
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
    pub(crate) fn continue_entity(
        &mut self,
        source: SourceId,
        caps: SourceCapabilities,
        discontinuity: bool,
    ) -> Result<()> {
        let Self::Ts(t) = self else {
            return Err(Error::Invalid("entity continuation requires TS source"));
        };
        let Entities::AtEnd(end) = &t.entities else {
            return Err(Error::Invalid("actual entity End required"));
        };
        if source == end.location().source {
            return Err(Error::SourceMismatch);
        }
        if discontinuity {
            t.stream.discontinuity()?;
        }
        t.input.continue_source(source, caps)?;
        end.validate_continuation(&t.input)?;
        t.entities = Entities::Sequence;
        Ok(())
    }
    pub(crate) fn finish_logical_source(&mut self) -> Result<()> {
        let Self::Ts(t) = self else {
            return Err(Error::Invalid("logical TS End required"));
        };
        if !matches!(t.entities, Entities::AtEnd(_)) {
            return Err(Error::Invalid("actual source entity End required"));
        }
        t.stream.end_of_source()?;
        t.entities = Entities::Sequence;
        t.start_draining();
        Ok(())
    }
    /// The Matroska reader of a Matroska engine.
    pub fn matroska_reader(&self) -> Option<&Reader> {
        match self {
            Self::Matroska(m) => Some(&m.reader),
            _ => None,
        }
    }
    /// The plan of an MP4 engine, which seeks are planned from.
    pub fn mp4_plan(&self) -> Option<&PreparedMp4> {
        match self {
            Self::Mp4(m) => Some(&m.plan),
            _ => None,
        }
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
            Mp4Engine::check_binding(&r.input, &r.plan)?;
        }
        Ok(())
    }
    /// The number of tracks known so far: the Matroska reader's, the MP4 plan's, or
    /// the streams of the transport stream's active program map, none before its PMT.
    pub fn track_count(&self) -> usize {
        match self {
            Self::Matroska(m) => m.reader.tracks().len(),
            Self::Mp4(m) => m.plan.tracks.len(),
            Self::Ts(t) => t
                .stream
                .active_program()
                .map_or(0, |program| program.streams().len()),
        }
    }
    /// The track at `index` of [`Self::track_count`]; `None` past the last.
    pub fn track(&self, index: usize) -> Option<Track<'_>> {
        match self {
            Self::Matroska(m) => m.reader.tracks().get(index).map(Track::Matroska),
            Self::Mp4(m) => m.plan.tracks.as_slice().get(index).map(Track::Mp4),
            Self::Ts(t) => {
                let program = t.stream.active_program()?;
                Some(Track::Ts {
                    program: program.program_number(),
                    stream: program.streams().nth(index)?,
                })
            }
        }
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
            Self::Matroska(r) => r.reader.generation(),
            Self::Mp4(r) => r.input.generation(),
            Self::Ts(r) => r.input.generation(),
        }
    }
    /// The budget the engine's storage is charged to.
    pub fn pool(&self) -> &Budget {
        match self {
            Self::Matroska(r) => r.reader.budget(),
            Self::Mp4(r) => r.input.budget(),
            Self::Ts(r) => r.input.budget(),
        }
    }
    /// Hands a source reply to the engine, and returns whether it made progress: `false`
    /// for `Pending` or empty data. A failure reply fails with `Transport` or
    /// `Cancelled`.
    pub fn provide(&mut self, reply: &SourceReply) -> Result<bool> {
        match self {
            Self::Mp4(r) => reply.provide_to(&mut r.input),
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
            _ => Err(Error::Invalid("demux range cursor requires MP4")),
        }
    }
    pub(crate) fn facts(&self) -> Result<BudgetVec<TrackFact>> {
        let mut out = BudgetVec::new(self.pool());
        match self {
            Self::Matroska(r) => {
                let tracks = r.reader.tracks();
                if tracks.len() > 64 {
                    return Err(Error::Limit {
                        requested: tracks.len(),
                        maximum: 64,
                    });
                }
                for t in tracks {
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
            Self::Matroska(r) => r.begin_seek(intent),
            Self::Mp4(r) => r.begin_seek(intent, mp4_landing),
            Self::Ts(r) => r.begin_seek(intent),
        }
    }
}
