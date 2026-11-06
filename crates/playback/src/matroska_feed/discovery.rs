//! Bounded initial Matroska discovery over the player's original Source.
mod audio;
mod video;

use crate::{
    Engine, Output, Player, PlayerPolicy, Progress, RawEvent, Result, Selection, Source,
    SourceReply, State, io::SourceOwner, player::MatroskaReplayOutput,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{BudgetVec, Error, InputRequest, WorkBudget};
use triergon_matroska::typed::{Configuration, Event, Packet};
use triergon_subtitles::pgs::{Composition, Envelope, SegmentIter};

#[derive(Clone, Copy, Debug)]
pub struct MatroskaDiscoveryLimits {
    /// Count complete demuxed packet payloads, including the packet that
    /// reaches the threshold.
    pub packet_bytes: usize,
    /// Hard cap on new transport bytes supplied while discovering.
    pub source_bytes: usize,
    /// Includes configuration and other source events kept for replay.
    pub outputs: usize,
}

impl Default for MatroskaDiscoveryLimits {
    fn default() -> Self {
        Self {
            packet_bytes: 5_000_000,
            source_bytes: 8 * 1024 * 1024,
            outputs: 4096,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatroskaDiscoveryCutoff {
    StreamsReady,
    PacketBytes,
    SourceBytes,
    OutputCount,
    SourceEnd,
}

#[derive(Debug)]
pub enum MatroskaDiscoveryStep {
    Progress(Progress),
    Ready(MatroskaDiscoveryCutoff),
}

/// The same Source continues with unrestricted playback requests after the
/// discovery operation. Pending and failed replies keep their usual meanings.
pub struct MatroskaStartupSource<S> {
    source: S,
    remaining: Option<usize>,
}

impl<S: Source> Source for MatroskaStartupSource<S> {
    fn poll(
        &mut self,
        request: &InputRequest,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<SourceReply> {
        let Some(remaining) = self.remaining else {
            return self.source.poll(request, work, cancel);
        };
        if remaining == 0 && request.maximum != 0 {
            return Ok(SourceReply::Pending);
        }
        let mut bounded = request.clone();
        bounded.maximum = bounded.maximum.min(remaining);
        let reply = self.source.poll(&bounded, work, cancel)?;
        if let SourceReply::Data(window) = &reply {
            let bytes = window.bytes().len();
            if bytes > bounded.maximum {
                return Err(Error::Limit {
                    requested: bytes,
                    maximum: bounded.maximum,
                });
            }
            self.remaining = Some(remaining - bytes);
        }
        Ok(reply)
    }

    fn invalidate(&mut self) {
        self.source.invalidate();
    }

    fn abort(&mut self) {
        self.source.abort();
    }

    fn close(&mut self) {
        self.source.close();
    }
}

struct Readiness {
    configuration: Configuration,
    waiting: u64,
    pgs: u64,
    audio: audio::AudioProbe,
    video: video::VideoProbe,
}

impl Readiness {
    fn new(configuration: &Configuration, probe_subtitles: bool) -> Result<Self> {
        let tracks = configuration.tracks();
        if tracks.len() > 64 {
            return Err(Error::Limit {
                requested: tracks.len(),
                maximum: 64,
            });
        }
        let mut waiting = 0;
        let mut pgs = 0;
        for (index, track) in tracks.iter().enumerate() {
            let bit = 1_u64 << index;
            match (track.kind, track.codec.as_str()) {
                (17, _) if !probe_subtitles => {}
                (17, "S_HDMV/PGS") => {
                    pgs |= bit;
                    waiting |= bit;
                }
                (
                    17,
                    "S_VOBSUB" | "S_DVBSUB" | "S_TEXT/UTF8" | "S_TEXT/ASS" | "S_TEXT/SSA"
                    | "S_TEXT/WEBVTT",
                ) => {}
                _ => waiting |= bit,
            }
        }
        Ok(Self {
            configuration: configuration.clone(),
            waiting,
            pgs,
            audio: audio::AudioProbe::default(),
            video: video::VideoProbe::default(),
        })
    }

    fn observe(
        &mut self,
        output: &Output,
        packet: &Packet,
        cursor: &mut usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if packet.configuration.generation() != self.configuration.generation()
            || packet.configuration.revision() != self.configuration.revision()
            || !std::ptr::eq(packet.configuration.tracks(), self.configuration.tracks())
            || packet.packet.track >= self.configuration.tracks().len()
        {
            return Err(Error::Invalid("Matroska discovery packet configuration"));
        }
        let bit = 1_u64 << packet.packet.track;
        if self.waiting & bit == 0 {
            return Ok(true);
        }
        let track = &self.configuration.tracks()[packet.packet.track];
        let observed = match track.kind {
            1 => Some(self.video.poll(output, packet, track, work, cancel)?),
            2 => Some(self.audio.poll(output, packet, track, work, cancel)?),
            _ => None,
        };
        if let Some(observed) = observed {
            let Some(ready) = observed else {
                return Ok(false);
            };
            if ready {
                self.waiting &= !bit;
            }
            return Ok(true);
        }
        if self.pgs & bit == 0 {
            return Ok(true);
        }
        let bytes = packet.packet.data.as_ref();
        while *cursor < bytes.len() {
            if cancel.load(Ordering::Relaxed) || !work.try_charge(1) {
                return Ok(false);
            }
            let Some(Ok(segment)) = SegmentIter::new(&bytes[*cursor..], Envelope::Packet).next()
            else {
                *cursor = bytes.len();
                break;
            };
            *cursor += segment.raw.len();
            if segment.kind == 0x16
                && Composition::parse(segment)
                    .is_ok_and(|canvas| canvas.width != 0 && canvas.height != 0)
            {
                self.waiting &= !bit;
                break;
            }
        }
        Ok(true)
    }
}

/// Own the prefix until discovery completes. Outputs move from the player
/// into a budgeted replay buffer in their original order.
pub struct MatroskaDiscovery<S: Source> {
    player: Player<MatroskaStartupSource<S>>,
    limits: MatroskaDiscoveryLimits,
    outputs: ReplayBuffer,
    pending: Option<Output>,
    pending_segment: usize,
    pending_observed: bool,
    probe_subtitles: bool,
    packet_bytes: usize,
    readiness: Option<Readiness>,
    cutoff: Option<MatroskaDiscoveryCutoff>,
}

enum ReplayBuffer {
    Full(BudgetVec<Output>),
    Matroska(BudgetVec<MatroskaReplayOutput>),
}

impl ReplayBuffer {
    fn len(&self) -> usize {
        match self {
            Self::Full(outputs) => outputs.len(),
            Self::Matroska(outputs) => outputs.len(),
        }
    }

    fn clear(&mut self) {
        match self {
            Self::Full(outputs) => outputs.clear(),
            Self::Matroska(outputs) => outputs.clear(),
        }
    }
}

/// Startup replay stored using the typed Matroska event representation.
pub struct MatroskaReplayPrefix(BudgetVec<MatroskaReplayOutput>);

impl MatroskaReplayPrefix {
    pub fn events(&self) -> impl ExactSizeIterator<Item = &Event> {
        self.0.as_slice().iter().map(|output| &output.event)
    }

    /// Move each output in original order, preserving scope and delivery.
    pub fn into_outputs(self) -> impl ExactSizeIterator<Item = Output> {
        self.0.into_iter().map(MatroskaReplayOutput::into_output)
    }
}

/// The same discovery operation with a format-specific replay buffer.
pub struct MatroskaCompactDiscovery<S: Source>(MatroskaDiscovery<S>);

pub struct MatroskaCompactDiscovered<S: Source> {
    pub player: Player<MatroskaStartupSource<S>>,
    pub prefix: MatroskaReplayPrefix,
    pub cutoff: MatroskaDiscoveryCutoff,
}

impl<S: Source> MatroskaCompactDiscovery<S> {
    pub fn new(
        engine: Engine,
        source: SourceOwner<S>,
        selection: Selection,
        player_policy: PlayerPolicy,
        limits: MatroskaDiscoveryLimits,
    ) -> Result<Self> {
        Ok(Self(MatroskaDiscovery::with_storage(
            engine,
            source,
            selection,
            player_policy,
            limits,
            |budget| ReplayBuffer::Matroska(BudgetVec::new(budget)),
        )?))
    }

    /// Start an A/V feed as soon as its streams are ready. Subtitle packets
    /// remain selected and are replayed or read in source order. Their first
    /// display set may occur well after the start of playback.
    pub fn for_av_feed(
        engine: Engine,
        source: SourceOwner<S>,
        selection: Selection,
        player_policy: PlayerPolicy,
        limits: MatroskaDiscoveryLimits,
    ) -> Result<Self> {
        let mut discovery = Self::new(engine, source, selection, player_policy, limits)?;
        discovery.0.probe_subtitles = false;
        Ok(discovery)
    }

    pub fn buffered_events(&self) -> impl ExactSizeIterator<Item = &Event> {
        let ReplayBuffer::Matroska(outputs) = &self.0.outputs else {
            unreachable!("compact discovery always has a Matroska replay buffer");
        };
        outputs.as_slice().iter().map(|output| &output.event)
    }

    pub fn confirm_stream_ready(&mut self, track: u64, revision: u64) -> Result<()> {
        self.0.confirm_stream_ready(track, revision)
    }

    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<MatroskaDiscoveryStep> {
        self.0.poll(work, cancel)
    }

    pub fn finish(mut self) -> Result<MatroskaCompactDiscovered<S>> {
        let cutoff = self.0.release_source()?;
        let ReplayBuffer::Matroska(outputs) = self.0.outputs else {
            unreachable!("compact discovery always has a Matroska replay buffer");
        };
        Ok(MatroskaCompactDiscovered {
            player: self.0.player,
            prefix: MatroskaReplayPrefix(outputs),
            cutoff,
        })
    }
}

pub struct MatroskaDiscovered<S: Source> {
    pub player: Player<MatroskaStartupSource<S>>,
    pub prefix: BudgetVec<Output>,
    pub cutoff: MatroskaDiscoveryCutoff,
}

impl<S: Source> MatroskaDiscovery<S> {
    pub fn new(
        engine: Engine,
        source: SourceOwner<S>,
        selection: Selection,
        player_policy: PlayerPolicy,
        limits: MatroskaDiscoveryLimits,
    ) -> Result<Self> {
        Self::with_storage(engine, source, selection, player_policy, limits, |budget| {
            ReplayBuffer::Full(BudgetVec::new(budget))
        })
    }

    fn with_storage(
        engine: Engine,
        source: SourceOwner<S>,
        selection: Selection,
        player_policy: PlayerPolicy,
        limits: MatroskaDiscoveryLimits,
        buffer: impl FnOnce(&triergon_core::Budget) -> ReplayBuffer,
    ) -> Result<Self> {
        if !matches!(&engine, Engine::Matroska(reader) if reader.configuration().is_none())
            || limits.outputs == 0
            || limits.packet_bytes == 0
            || limits.source_bytes == 0
        {
            return Err(Error::Invalid(
                "Matroska discovery needs a fresh reader and positive limits",
            ));
        }
        let source = source.map(|source| MatroskaStartupSource {
            source,
            remaining: Some(limits.source_bytes),
        })?;
        let player = Player::new_direct(engine, source, selection, player_policy)?;
        let outputs = buffer(player.budget());
        Ok(Self {
            player,
            limits,
            outputs,
            pending: None,
            pending_segment: 0,
            pending_observed: false,
            probe_subtitles: true,
            packet_bytes: 0,
            readiness: None,
            cutoff: None,
        })
    }

    pub fn buffered(&self) -> &[Output] {
        let ReplayBuffer::Full(outputs) = &self.outputs else {
            unreachable!("ordinary discovery always has a full replay buffer");
        };
        outputs.as_slice()
    }

    /// Report codec discovery performed by an audio/video or other stream
    /// consumer. The revision is from the configuration that consumer used.
    /// PGS readiness comes from its actual presentation segment.
    pub fn confirm_stream_ready(&mut self, track: u64, revision: u64) -> Result<()> {
        let state = self
            .readiness
            .as_mut()
            .filter(|state| self.cutoff.is_none() && state.configuration.revision() == revision)
            .ok_or(Error::Invalid(
                "Matroska discovery configuration not current",
            ))?;
        let index = state
            .configuration
            .tracks()
            .iter()
            .position(|entry| entry.number == track)
            .ok_or(Error::Invalid("Matroska discovery track number"))?;
        let bit = 1_u64 << index;
        if state.pgs & bit != 0 {
            return Err(Error::Invalid(
                "Matroska PGS readiness comes from its packet",
            ));
        }
        state.waiting &= !bit;
        Ok(())
    }

    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<MatroskaDiscoveryStep> {
        if cancel.load(Ordering::Relaxed) {
            self.player.cancel();
            self.outputs.clear();
            self.pending = None;
            self.cutoff = None;
            return Err(Error::Cancelled);
        }
        if matches!(
            self.player.state(),
            State::Failed | State::Stopped | State::Cancelled
        ) {
            return Err(crate::Error::Closed);
        }
        if let Some(cutoff) = self.cutoff {
            return Ok(MatroskaDiscoveryStep::Ready(cutoff));
        }
        if work.remaining() == 0 {
            return Ok(MatroskaDiscoveryStep::Progress(Progress::Yield));
        }
        if self.pending.is_none() {
            if self
                .readiness
                .as_ref()
                .is_some_and(|state| state.waiting == 0)
            {
                return Ok(self.complete(MatroskaDiscoveryCutoff::StreamsReady));
            }
            if self.packet_bytes >= self.limits.packet_bytes {
                return Ok(self.complete(MatroskaDiscoveryCutoff::PacketBytes));
            }
            if self.outputs.len() == self.limits.outputs {
                return Ok(self.complete(MatroskaDiscoveryCutoff::OutputCount));
            }
            let progress = self.player.read_output(&mut self.pending, work, cancel)?;
            match progress {
                Progress::Queued => {}
                Progress::Ended => return Ok(self.complete(MatroskaDiscoveryCutoff::SourceEnd)),
                Progress::NeedInput(_)
                    if self
                        .player
                        .source_mut()
                        .is_some_and(|source| source.remaining == Some(0)) =>
                {
                    return Ok(self.complete(MatroskaDiscoveryCutoff::SourceBytes));
                }
                progress => return Ok(MatroskaDiscoveryStep::Progress(progress)),
            }
        }
        self.buffer_output(work, cancel)
    }

    fn buffer_output(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<MatroskaDiscoveryStep> {
        let output = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("Matroska discovery lost output"))?;
        if cancel.load(Ordering::Relaxed) || work.remaining() == 0 {
            return Ok(MatroskaDiscoveryStep::Progress(Progress::Yield));
        }
        let packet_bytes = match output.event() {
            RawEvent::Matroska(Event::Packet(packet)) => {
                if !self.pending_observed {
                    let ready = self.readiness.as_mut().ok_or(Error::Invalid(
                        "Matroska discovery packet before configuration",
                    ))?;
                    if !ready.observe(output, packet, &mut self.pending_segment, work, cancel)? {
                        return Ok(MatroskaDiscoveryStep::Progress(Progress::Yield));
                    }
                }
                packet.packet.data.len()
            }
            RawEvent::Matroska(Event::Configuration(configuration)) => {
                if !self.pending_observed {
                    self.readiness = Some(Readiness::new(configuration, self.probe_subtitles)?);
                }
                0
            }
            _ => 0,
        };
        self.pending_observed = true;
        if !work.try_charge(1) {
            return Ok(MatroskaDiscoveryStep::Progress(Progress::Yield));
        }
        let total = self
            .packet_bytes
            .checked_add(packet_bytes)
            .ok_or(Error::Overflow)?;
        let output = self
            .pending
            .take()
            .ok_or(Error::Invalid("Matroska discovery lost output"))?;
        match &mut self.outputs {
            ReplayBuffer::Full(outputs) => {
                if let Err(failure) = outputs.try_push(output) {
                    self.pending = Some(failure.value);
                    return Err(failure.error);
                }
            }
            ReplayBuffer::Matroska(outputs) => {
                let output = output.into_matroska_replay().ok_or(Error::Invalid(
                    "unexpected event in Matroska startup replay",
                ))?;
                if let Err(failure) = outputs.try_push(output) {
                    self.pending = Some(failure.value.into_output());
                    return Err(failure.error);
                }
            }
        }
        self.packet_bytes = total;
        self.pending_segment = 0;
        self.pending_observed = false;
        Ok(MatroskaDiscoveryStep::Progress(Progress::Queued))
    }

    fn complete(&mut self, cutoff: MatroskaDiscoveryCutoff) -> MatroskaDiscoveryStep {
        self.cutoff = Some(cutoff);
        MatroskaDiscoveryStep::Ready(cutoff)
    }

    fn release_source(&mut self) -> Result<MatroskaDiscoveryCutoff> {
        let cutoff = self
            .cutoff
            .ok_or(Error::Invalid("Matroska discovery still running"))?;
        self.player
            .source_mut()
            .ok_or(crate::Error::Closed)?
            .remaining = None;
        Ok(cutoff)
    }

    pub fn finish(mut self) -> Result<MatroskaDiscovered<S>> {
        let cutoff = self.release_source()?;
        let ReplayBuffer::Full(outputs) = self.outputs else {
            unreachable!("ordinary discovery always has a full replay buffer");
        };
        Ok(MatroskaDiscovered {
            player: self.player,
            prefix: outputs,
            cutoff,
        })
    }
}
