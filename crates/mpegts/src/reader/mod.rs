//! Source access units composed from the transport reader and existing framers.
mod advance;
mod lane;
mod limits;
mod model;
mod seek;
use crate::compat::adts;
use crate::stream::{CarrierObservation, TransportStream};
use advance::{InFlight, LaneWork};
use lane::{Lane, StorageScan};
pub use limits::{AudioEofPolicy, DvbSubtitleTailPolicy, ReaderLimits};
use model::Shared;
pub use model::{
    Ac3AccessUnit, AccessUnit, DtsAudioFrame, DtsExtensionFrame, DvbSubtitleUnit, IncompleteVideo,
    LoasFrame, MpegAudioFrame, Packet, PacketClock, PayloadSource, ReaderEvent, SourceEvent, Track,
    UnframedAudio,
};
pub use seek::SeekEntity;
use std::sync::atomic::AtomicBool;
use triergon_core::container::{ContainerReader, Event, ReaderStep, SeekRequest, TrackId};
use triergon_core::{
    Budget, BudgetVec, Charged, Error, InputCursor, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
};

/// Interpretation of the next physical entity in a transport stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityTransition {
    /// Continue PSI, PES, clock and access-unit state across the boundary.
    Contiguous,
    /// Start a new configuration and clock epoch, reporting interrupted units.
    Discontinuity,
}

#[derive(Clone, Copy)]
struct NextEntity {
    source: SourceId,
    capabilities: SourceCapabilities,
    transition: EntityTransition,
}

/// How the current entity ends.
#[derive(Clone, Copy)]
enum Ending {
    /// No end has been declared.
    Open,
    /// The current entity is complete; the reader continues with this one.
    Continues(NextEntity),
    /// The transport reached the logical end here and drains its assemblers.
    Ended(SourceLocation),
    /// `Finished` has been published.
    Finished(SourceLocation),
}

enum PacketTracks {
    All,
    Selected(BudgetVec<u16>),
    Closed,
}

/// The input and transport state the reader reads from.
#[allow(
    clippy::large_enum_variant,
    reason = "the open transport is the common case and stays inline"
)]
enum Transport {
    /// Transport packets are being read.
    Open {
        input: InputCursor,
        stream: TransportStream,
    },
    /// The transport finished and released its storage; lane fragments remain.
    Released { input: InputCursor },
    /// Closed or failed: every call reports this error.
    Stopped(Error),
}

/// Incremental TS188/M2TS192 reader delivering complete source AUs, separately
/// from every original transport event. This performs no I/O or decoder-feed
/// injection. Unsupported carriages retain source facts and report a typed gap.
pub struct Reader {
    pool: Budget,
    source: SourceId,
    caps: SourceCapabilities,
    window: usize,
    limits: ReaderLimits,
    packet_tracks: PacketTracks,
    transport: Transport,
    lanes: BudgetVec<Lane>,
    tracks: BudgetVec<Track>,
    staged_tracks: BudgetVec<Track>,
    map: Option<SourceEvent>,
    flight: InFlight,
    lane_work: LaneWork,
    tsid: Option<u16>,
    boundary: Option<SourceEvent>,
    ending: Ending,
    storage: Option<StorageScan>,
    seek: BudgetVec<seek::Seeking>,
    seek_index: Option<Shared<seek::SeekIndex>>,
    seek_entities: Option<Charged<Box<seek::EntityPlan>>>,
    continued_entity: bool,
}

impl std::fmt::Debug for Reader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader")
            .field("source", &self.source)
            .field("limits", &self.limits)
            .field("tracks", &self.lanes.len())
            .field("finished", &matches!(self.ending, Ending::Finished(_)))
            .field("failure", &self.healthy().err())
            .finish_non_exhaustive()
    }
}

impl Reader {
    /// A reader of the transport stream in `source`, charging its state to `pool`, with an input
    /// window of `input_window` bytes.
    ///
    /// Fails with `Invalid` when a count or byte limit in `limits` is zero, `program` is 0 or the
    /// unlabelled DTS core selection names program 0 or a reserved PID, and when the transport
    /// limits or the input window are invalid.
    pub fn new(
        pool: &Budget,
        source: SourceId,
        caps: SourceCapabilities,
        limits: ReaderLimits,
        input_window: usize,
    ) -> Result<Self> {
        if let Some(selection) = limits.unlabelled_dts_core
            && (selection.program == 0 || !crate::ELEMENTARY_PID_RANGE.contains(&selection.pid))
        {
            return Err(Error::Invalid("unlabelled DTS core selection"));
        }
        if limits.maximum_audio_frame_bytes == 0
            || limits.maximum_dts_hd_pes_bytes == 0
            || limits.maximum_access_units == 0
            || limits.maximum_pes_per_access_unit == 0
            || limits.elementary_input_window == 0
            || limits.program == Some(0)
        {
            return Err(Error::Invalid("MPEG-TS access-unit reader limits"));
        }
        let mut stream = TransportStream::new_with_program(
            pool.clone(),
            limits.format,
            limits.transport,
            limits.program,
            limits.descriptor_namespace,
        )?;
        stream.share_input();
        stream.set_pcr_policy(limits.pcr_policy);
        if limits.adts_pes_policy == adts::PesPolicy::ReportUnbounded {
            stream.allow_unbounded_adts();
        }
        let input = InputCursor::new(pool, source, caps, input_window)?;
        Ok(Self {
            pool: pool.clone(),
            source,
            caps,
            window: input_window,
            limits,
            packet_tracks: PacketTracks::All,
            transport: Transport::Open { input, stream },
            lanes: BudgetVec::new(pool),
            tracks: BudgetVec::new(pool),
            staged_tracks: BudgetVec::new(pool),
            map: None,
            flight: InFlight::Idle,
            lane_work: LaneWork::Idle,
            tsid: None,
            boundary: None,
            ending: Ending::Open,
            storage: None,
            seek: BudgetVec::new(pool),
            seek_index: None,
            seek_entities: None,
            continued_entity: false,
        })
    }
    /// Frame access units only for the supplied elementary PIDs.
    ///
    /// All track metadata and original transport events remain available,
    /// including diagnostics and PES on unselected PIDs. An empty selection
    /// reads source events only. Reset keeps the selection until the reader closes.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid or duplicate PID, invalid reader
    /// limits, or insufficient storage for the bounded selection.
    pub fn new_with_packet_tracks(
        pool: &Budget,
        source: SourceId,
        caps: SourceCapabilities,
        limits: ReaderLimits,
        input_window: usize,
        tracks: &[TrackId],
    ) -> Result<Self> {
        let mut reader = Self::new(pool, source, caps, limits, input_window)?;
        if tracks.len() > limits.transport.streams {
            return Err(crate::limit(tracks.len(), limits.transport.streams));
        }
        let mut selection = BudgetVec::new(pool);
        selection.try_reserve_exact(tracks.len())?;
        for track in tracks {
            let pid = u16::try_from(track.0)
                .ok()
                .filter(|pid| crate::ELEMENTARY_PID_RANGE.contains(pid))
                .ok_or(Error::Invalid("MPEG-TS packet track selection"))?;
            selection.try_push(pid).map_err(|rejected| rejected.error)?;
        }
        selection.as_mut_slice().sort_unstable();
        if selection
            .as_slice()
            .windows(2)
            .any(|pair| pair[0] == pair[1])
        {
            return Err(Error::Invalid("MPEG-TS packet track selection"));
        }
        reader.packet_tracks = PacketTracks::Selected(selection);
        Ok(reader)
    }

    fn selected_track(&self, pid: u16) -> bool {
        match &self.packet_tracks {
            PacketTracks::All => true,
            PacketTracks::Selected(selection) => selection.as_slice().binary_search(&pid).is_ok(),
            PacketTracks::Closed => false,
        }
    }

    fn frames_track(&self, pid: u16) -> bool {
        if let Some(track) = self
            .seek
            .as_slice()
            .first()
            .and_then(seek::Seeking::scan_track)
        {
            return track == TrackId(pid.into());
        }
        self.selected_track(pid)
    }

    pub(crate) fn continuity_source(&self) -> Option<(&InputCursor, Option<&TransportStream>)> {
        match &self.transport {
            Transport::Open { input, stream } => Some((input, Some(stream))),
            Transport::Released { input } => Some((input, None)),
            Transport::Stopped(_) => None,
        }
    }

    fn input(&self) -> Result<&InputCursor> {
        match &self.transport {
            Transport::Open { input, .. } | Transport::Released { input } => Ok(input),
            Transport::Stopped(error) => Err(*error),
        }
    }

    fn input_mut(&mut self) -> Result<&mut InputCursor> {
        match &mut self.transport {
            Transport::Open { input, .. } | Transport::Released { input } => Ok(input),
            Transport::Stopped(error) => Err(*error),
        }
    }

    fn stream(&self) -> Option<&TransportStream> {
        match &self.transport {
            Transport::Open { stream, .. } => Some(stream),
            Transport::Released { .. } | Transport::Stopped(_) => None,
        }
    }

    /// Drops the finished transport's storage and keeps the input.
    fn release_transport(&mut self) {
        let placeholder = Transport::Stopped(Error::Closed);
        self.transport = match std::mem::replace(&mut self.transport, placeholder) {
            Transport::Open { input, stream } => {
                drop(stream);
                Transport::Released { input }
            }
            other => other,
        };
    }
    /// The tracks of the selected program announced so far.
    pub fn tracks(&self) -> &[Track] {
        self.tracks.as_slice()
    }
    /// The input the reader needs next.
    ///
    /// Fails with the reader's stopping error once it has failed or been closed.
    pub fn request(&self) -> Result<InputRequest> {
        Ok(self.input()?.request())
    }
    /// Supplies input for the current request.
    ///
    /// Fails with the stopping error once the reader has stopped, and when the window does not
    /// answer
    /// the current request.
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.input_mut()?.provide(window)?;
        if self.storage.is_some() && !window.bytes().is_empty() {
            // An early reply can add backing storage while a growth audit yields.
            self.storage = None;
        }
        Ok(())
    }
    /// Declares definitive end of the current entity's input. During a seek over a planned sequence
    /// of entities, this moves on to the next entity instead.
    ///
    /// Fails with the stopping error once the reader has stopped, and when `source` and `token` do
    /// not
    /// match the current request; a `Truncated` result also stops the reader.
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.healthy()?;
        if let Some(next) = self.seek_entities.as_deref().and_then(|plan| plan.next()) {
            self.queue_entity(
                source,
                token,
                next.source,
                next.capabilities(),
                EntityTransition::Contiguous,
            )?;
            if let Some(plan) = self.seek_entities.as_deref_mut() {
                plan.advance();
            }
            return Ok(());
        }
        self.finish_input(source, token)
    }

    fn finish_input(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        let input = self.input_mut()?;
        let result = input.finish(source, token);
        self.caps = input.capabilities();
        if let Err(error) = result
            && error == Error::Truncated
        {
            self.stop(error);
        }
        result
    }
    /// Complete one entity and queue its successor. The reader consumes the
    /// final supplied bytes before requesting offset zero of the next entity.
    /// The generation is preserved and the request number advances.
    /// Use [Self::finish] for the last entity of the logical stream.
    pub fn finish_entity(
        &mut self,
        source: SourceId,
        token: InputToken,
        next: SourceId,
        capabilities: SourceCapabilities,
        transition: EntityTransition,
    ) -> Result<()> {
        self.healthy()?;
        if self.seek_entities.is_some() {
            return Err(Error::Invalid(
                "MPEG-TS entity continuation is managed by seek",
            ));
        }
        if !self.seek.is_empty() {
            return Err(Error::Unsupported("MPEG-TS seek across entities"));
        }
        self.queue_entity(source, token, next, capabilities, transition)
    }

    fn queue_entity(
        &mut self,
        source: SourceId,
        token: InputToken,
        next: SourceId,
        capabilities: SourceCapabilities,
        transition: EntityTransition,
    ) -> Result<()> {
        if !matches!(self.ending, Ending::Open) {
            return Err(Error::Invalid(
                "MPEG-TS entity completion already specified",
            ));
        }
        if self.source == next {
            return Err(Error::SourceMismatch);
        }
        token.request().checked_add(1).ok_or(Error::Overflow)?;
        self.finish_input(source, token)?;
        self.ending = Ending::Continues(NextEntity {
            source: next,
            capabilities,
            transition,
        });
        self.continued_entity = true;
        if self.seek_entities.is_none() {
            self.seek_index = None;
        }
        Ok(())
    }
    /// Reports that the current request failed; the reader stops with `Transport` or `Cancelled`.
    ///
    /// Fails with the stopping error once the reader has stopped, and when `source` and `token` do
    /// not
    /// match the current request.
    pub fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        self.input_mut()?.fail(source, token, failure)?;
        self.stop(match failure {
            SourceFailure::Transport => Error::Transport,
            SourceFailure::Cancelled => Error::Cancelled,
        });
        Ok(())
    }
    /// Release reader-owned state, including the packet selection. Previously
    /// returned packets and events remain valid.
    pub fn close(&mut self) {
        self.stop(Error::Closed);
        if matches!(self.packet_tracks, PacketTracks::Selected(_)) {
            self.packet_tracks = PacketTracks::Closed;
        }
    }
    /// Restart the current entity with fresh transport state and a new generation.
    /// A closed reader with an explicit packet selection cannot be reset.
    pub fn reset(&mut self) -> Result<InputRequest> {
        if matches!(self.packet_tracks, PacketTracks::Closed) {
            return Err(Error::Closed);
        }
        let mut next = Self::new(&self.pool, self.source, self.caps, self.limits, self.window)?;
        let request = next.request()?;
        std::mem::swap(&mut next.packet_tracks, &mut self.packet_tracks);
        *self = next;
        Ok(request)
    }
    fn healthy(&self) -> Result<()> {
        match self.transport {
            Transport::Stopped(error) => Err(error),
            Transport::Open { .. } | Transport::Released { .. } => Ok(()),
        }
    }
    fn stop(&mut self, error: Error) {
        self.transport = Transport::Stopped(error);
        self.lanes = BudgetVec::new(&self.pool);
        self.tracks = BudgetVec::new(&self.pool);
        self.staged_tracks = BudgetVec::new(&self.pool);
        self.flight = InFlight::Idle;
        self.map = None;
        self.lane_work = LaneWork::Idle;
        self.boundary = None;
        if let Ending::Continues(_) = self.ending {
            self.ending = Ending::Open;
        }
        self.storage = None;
        self.seek = BudgetVec::new(&self.pool);
        self.seek_index = None;
        self.seek_entities = None;
    }
    /// Advances within `work` to the next packet, other source event or progress step.
    ///
    /// Backpressure is returned as a step. Any other error except `Allocation` stops the reader and
    /// is
    /// returned by every later call.
    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ReaderStep<Self>> {
        self.step_observing::<false>(work, cancel, |_| ())
    }

    /// Inspect carriers and ordinary continuity events in source order, then
    /// return the next packet, other source event or progress step.
    ///
    /// The callback borrows each carrier within the caller's work grant. The
    /// reader preserves the input, yield and backpressure protocol of [Self::step].
    /// During a seek, the callback resumes after the landing packet.
    pub fn step_with_carriers(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        mut observe: impl FnMut(CarrierObservation<'_>),
    ) -> Result<ReaderStep<Self>> {
        loop {
            let step = self.step_observing::<true>(work, cancel, &mut observe)?;
            if let Step::Ready(Event::Other(event)) = &step
                && let Some(event) = event.carrier_observation()
            {
                observe(event);
            } else {
                return Ok(step);
            }
        }
    }
}

impl ContainerReader for Reader {
    type Packet = Packet;
    type Track = Track;
    type Other = ReaderEvent;
    fn provide(&mut self, window: &InputWindow) -> Result<()> {
        Self::provide(self, window)
    }
    fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        Self::finish(self, source, token)
    }
    fn fail(&mut self, source: SourceId, token: InputToken, failure: SourceFailure) -> Result<()> {
        Self::fail(self, source, token, failure)
    }
    fn reset(&mut self) -> Result<InputRequest> {
        Self::reset(self)
    }
    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ReaderStep<Self>> {
        Self::step(self, work, cancel)
    }
    fn request_seek(&mut self, request: SeekRequest) -> Result<InputRequest> {
        Self::request_seek(self, request)
    }
    fn track_count(&self) -> usize {
        self.tracks.len()
    }
    fn track(&self, index: usize) -> Option<&Track> {
        self.tracks.as_slice().get(index)
    }
    fn capabilities(&self) -> SourceCapabilities {
        self.caps
    }
}
