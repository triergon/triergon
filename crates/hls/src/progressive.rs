//! Sequential TS188 feed for AVC and ADTS AAC.

use crate::{
    aac::{HlsAac, HlsAacFrame, MAXIMUM_ADTS_BYTES},
    avc::{AvcTsAccessUnit, AvcTsLimits, AvcTsReader, AvcTsStep},
    packet::UnitPacket,
    pes::PES_CLOCK_HZ,
    pmt::select_pid,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, InputCursor, InputRequest, Result, Time, WorkBudget};
use triergon_mpegts::psi::{PatTable, ProgramMap, stream_type};
use triergon_mpegts::stream::{FaultKind, TraceEvent};
use triergon_mpegts::transport::ContinuityOutcome;
use triergon_mpegts::{AssemblyStep, ParseLimits};
use triergon_playback::compat::pes::AdmittedCarriage;
use triergon_playback::{
    Source, SourceOwner, SourceReply, TrackKey,
    pes_feed::{FeedPacket, FeedStep, PesInput, ProgressiveFeed, SpentLists},
};

/// Bounds of a [`TsProgressiveReader`].
#[derive(Clone, Copy, Debug, Default)]
pub struct TsProgressiveLimits {
    /// Transport stream parse bounds.
    pub transport: ParseLimits,
    /// AVC access unit bounds.
    pub avc: AvcTsLimits,
}

/// One unit of a progressive stream.
#[derive(Debug)]
pub enum TsProgressiveUnit {
    /// Annex B in decode order. The clock retains raw source timestamps and
    /// their demux 90 kHz view, which supplies a decode tick for PTS-only PES.
    Video(AvcTsAccessUnit),
    /// One ADTS AAC frame.
    Audio {
        /// The frame, with the timestamps of its PES.
        frame: HlsAacFrame,
        /// Exact presentation time on this body's demux PES clock. Missing anchors
        /// advance from the preceding frame by its ADTS sample duration.
        /// The frame retains the original PTS/DTS presence independently.
        presentation: Time,
    },
}

/// One step of a [`TsProgressiveReader`].
#[derive(Debug)]
pub enum TsProgressiveStep {
    /// The work budget ran out: advance again with more work.
    Yield,
    /// The source is pending on this request: provide a reply or advance again.
    NeedInput(InputRequest),
    /// The pool is full of output the caller still holds: release some, then
    /// advance again.
    Backpressure {
        /// Bytes the step needed.
        requested: usize,
        /// Bytes the pool had free.
        available: usize,
    },
    /// A completed unit.
    Unit(TsProgressiveUnit),
    /// The source ended and both assemblers drained.
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Read,
    Video,
    Audio,
    DrainVideo,
    DrainAudio,
    End,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PatBinding {
    tsid: u16,
    program: u16,
    pmt_pid: u16,
}

/// The AVC and AAC tracks of the active PMT, on PIDs of `program`.
#[derive(Debug)]
struct Tracks {
    program: u16,
    video: u16,
    audio: u16,
    pcr_pid: u16,
    epoch: u64,
}

impl Tracks {
    /// Whether `track` is the AVC track.
    fn is_video(&self, track: TrackKey) -> bool {
        track
            == (TrackKey::Ts {
                program: self.program,
                pid: self.video,
            })
    }

    /// Whether `track` is the AAC track.
    fn is_audio(&self, track: TrackKey) -> bool {
        track
            == (TrackKey::Ts {
                program: self.program,
                pid: self.audio,
            })
    }
}

#[derive(Clone, Copy, Debug)]
enum NextAudioTime {
    Ticks90(i64),
    Exact(Time),
}

#[derive(Debug)]
struct Transport<S: Source> {
    engine: ProgressiveFeed,
    request: Option<InputRequest>,
    pending: Option<FeedStep>,
    source: SourceOwner<S>,
}

// Prefer an end on the TS188 grid without changing cursor identity or its
// hard window limit. Arbitrarily short replies still use the normal carry path.
#[inline]
fn carrier_aligned_request(mut request: InputRequest) -> InputRequest {
    let stride = triergon_mpegts::PacketFormat::Ts188.bytes();
    // Both remainders are below one carrier, including on 32-bit targets.
    let residual = ((request.offset % stride as u64) as usize + request.maximum % stride) % stride;
    if request.maximum > residual {
        request.maximum -= residual;
    }
    request
}

impl<S: Source> Transport<S> {
    fn provide(&mut self, reply: &SourceReply) -> Result<bool> {
        let request = self
            .request
            .as_ref()
            .ok_or(Error::Invalid("unsolicited completion"))?;
        if let SourceReply::Data(window) = reply
            && window.bytes().len() > request.maximum
        {
            // Recover the original cursor bound only on the oversize path.
            // Preserve its Limit-before-token-check ordering for all replies.
            let hard_maximum = self.engine.input().request().maximum;
            let maximum = if window.bytes().len() > hard_maximum {
                Some(hard_maximum)
            } else if window.source == request.source
                && window.token == request.token
                && window.offset == request.offset
                && window.bytes().belongs_to(self.engine.input().budget())
            {
                Some(request.maximum)
            } else {
                None
            };
            if let Some(maximum) = maximum {
                return Err(Error::Limit {
                    requested: window.bytes().len(),
                    maximum,
                });
            }
        }
        let accepted = self.engine.provide(reply)?;
        if accepted {
            self.request = None;
        }
        Ok(accepted)
    }

    // Out of line, the reader's loop keeps its state in registers: 0.4 to 0.5% fewer
    // instructions per progressive stream than with this loop inlined into it.
    #[inline(never)]
    fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        maximum_payload_bytes: usize,
    ) -> Result<FeedStep> {
        while work.remaining() != 0 {
            if let Some(event) = self.pending.take() {
                work.try_charge(1);
                return Ok(event);
            }
            if let Some(request) = &self.request {
                let reply = self.source.poll(request, work, cancel)?;
                if !self.provide(&reply)? {
                    if work.remaining() == 0 {
                        return Ok(FeedStep::Yield);
                    }
                    let request = self
                        .request
                        .clone()
                        .ok_or(Error::Invalid("missing TS input request"))?;
                    return Ok(FeedStep::NeedInput(request));
                }
                continue;
            }
            let before = work.remaining();
            let step = self.engine.poll(work, cancel);
            self.engine.take_suppressed_events();
            let step = step?;
            if let Some(bytes) = step.payload_bytes() {
                if bytes > maximum_payload_bytes {
                    return Err(Error::Limit {
                        requested: bytes,
                        maximum: maximum_payload_bytes,
                    });
                }
                if work.try_charge(1) {
                    return Ok(step);
                }
                self.pending = Some(step);
                continue;
            }
            match step {
                FeedStep::NeedInput(request) => {
                    let request = carrier_aligned_request(request);
                    if work.remaining() == 0 {
                        self.request = Some(request.clone());
                        return Ok(FeedStep::NeedInput(request));
                    }
                    self.request = Some(request);
                }
                FeedStep::Yield if work.remaining() < before => {}
                other => return Ok(other),
            }
        }
        Ok(FeedStep::Yield)
    }
}

impl<S: Source> Drop for Transport<S> {
    fn drop(&mut self) {
        // Quiesce the transport before dropping its retained windows.
        self.source.abort();
    }
}

/// One sequential stream with a stable PAT/PMT and clock, one unambiguous
/// AVC track and one unambiguous ADTS AAC track. AVC must start with a
/// delimited IDR and pictures need PTS. One leading SEI NAL in the first
/// delimiter's PES is retained for compatibility with muxed startup packets.
/// The first AAC frame needs a PES time anchor.
///
/// Each completed unit is delivered in assembler output order, including
/// before source EOF. The reader keeps no queue of completed units. The
/// caller owns delivered units and any startup buffering. A later failure
/// retires the reader. Transport must report an interrupted body as failure.
/// Only a matching source End drains the assemblers and can produce End.
///
/// Open a fresh input and reader to start at another media offset.
/// The input starts at byte zero and clocks stay on the source timeline.
#[derive(Debug)]
pub struct TsProgressiveReader<S: Source> {
    transport: Option<Transport<S>>,
    limits: TsProgressiveLimits,
    phase: Phase,
    pat: Option<PatBinding>,
    tracks: Option<Tracks>,
    video: Option<AvcTsReader>,
    audio: Option<HlsAac>,
    saw_video: bool,
    audio_times: Option<(Time, NextAudioTime)>,
}

impl<S: Source> TsProgressiveReader<S> {
    const POLLS_PER_ADVANCE: usize = 8;

    /// A reader of `source` from byte zero. Fails with `Invalid` unless `input` is at
    /// offset zero of a sequential source.
    pub fn new(input: InputCursor, source: S, limits: TsProgressiveLimits) -> Result<Self> {
        let source = SourceOwner::new(source);
        if input.position() != 0 || input.capabilities().random_access {
            return Err(Error::Invalid("progressive TS requires a sequential body"));
        }
        Ok(Self {
            transport: Some(Transport {
                engine: ProgressiveFeed::new(input, limits.transport)?,
                request: None,
                pending: None,
                source,
            }),
            limits,
            phase: Phase::Read,
            pat: None,
            tracks: None,
            video: None,
            audio: None,
            saw_video: false,
            audio_times: None,
        })
    }

    /// Supply a reply for the outstanding request. Alternatively the injected
    /// source can supply it when advanced. Stale replies leave the reader live.
    pub fn provide(&mut self, reply: &SourceReply) -> Result<bool> {
        let result = self.transport.as_mut().ok_or(Error::Closed)?.provide(reply);
        if matches!(
            result,
            Err(Error::Transport | Error::Truncated | Error::Cancelled)
        ) {
            self.fail();
        }
        result
    }

    /// Reads, demuxes and assembles within `work`. A failure other than backpressure, or
    /// than an allocation failure after reading, retires the reader.
    pub fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<TsProgressiveStep> {
        for _ in 0..Self::POLLS_PER_ADVANCE {
            if cancel.load(Ordering::Relaxed) {
                self.fail();
                return Err(Error::Cancelled);
            }
            let remaining = work.remaining();
            let phase = self.phase;
            match self.step(work, cancel) {
                Ok(TsProgressiveStep::Yield)
                    if work.remaining() != 0
                        && (work.remaining() < remaining || self.phase != phase) =>
                {
                    continue;
                }
                Err(error) => return self.step_failure(error),
                result => return result,
            }
        }
        Ok(TsProgressiveStep::Yield)
    }

    /// What a failed step gives the caller: backpressure is a step of its own,
    /// and any other failure retires the reader, except that the frame assemblers
    /// keep their state across a failed allocation after reading.
    #[cold]
    fn step_failure(&mut self, error: Error) -> Result<TsProgressiveStep> {
        match error {
            Error::Backpressure(triergon_core::Backpressure {
                requested,
                available,
            }) => Ok(TsProgressiveStep::Backpressure {
                requested,
                available,
            }),
            // A refused track while reading is terminal.
            Error::Allocation if self.phase != Phase::Read => Err(Error::Allocation),
            error => {
                self.fail();
                Err(error)
            }
        }
    }

    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<TsProgressiveStep> {
        match self.phase {
            Phase::Read => {
                let transport = self.transport.as_mut().ok_or(Error::Closed)?;
                match transport.poll(work, cancel, self.limits.transport.pes_bytes)? {
                    event @ (FeedStep::Event(_)
                    | FeedStep::Configuration(_)
                    | FeedStep::Refusal { .. }
                    | FeedStep::Packet { .. }
                    | FeedStep::EntityEnd(_)) => self.on_event(event)?,
                    FeedStep::NeedInput(request) => {
                        return Ok(TsProgressiveStep::NeedInput(request));
                    }
                    FeedStep::End => self.end_transport()?,
                    FeedStep::Yield => {}
                    FeedStep::AwaitEntity => {
                        return Err(Error::Invalid("unexpected TS entity boundary"));
                    }
                }
            }
            Phase::Video | Phase::DrainVideo => {
                let video = self.video.as_mut().ok_or(Error::Closed)?;
                match video.step(work, cancel)? {
                    AvcTsStep::Ready(unit) => {
                        if unit.clock.and_then(|clock| clock.pts90).is_none() {
                            return Err(Error::Unsupported("progressive AVC picture without PTS"));
                        }
                        self.saw_video = true;
                        return Ok(TsProgressiveStep::Unit(TsProgressiveUnit::Video(unit)));
                    }
                    AvcTsStep::Yield => {}
                    AvcTsStep::NeedInput if self.phase == Phase::Video => {
                        let lists = video.take_spent();
                        self.reuse_pes_lists(|tracks| tracks.video, lists);
                        self.phase = Phase::Read;
                    }
                    AvcTsStep::End if self.phase == Phase::DrainVideo => self.end_video(),
                    _ => return Err(Error::Invalid("unexpected progressive AVC state")),
                }
            }
            Phase::Audio | Phase::DrainAudio => {
                let audio = self.audio.as_mut().ok_or(Error::Closed)?;
                match audio.poll(work, cancel)? {
                    AssemblyStep::Output(frame) => {
                        let presentation = audio_presentation(
                            &mut self.audio_times,
                            frame.pts.or(frame.dts),
                            frame.duration,
                        )?;
                        return Ok(TsProgressiveStep::Unit(TsProgressiveUnit::Audio {
                            frame,
                            presentation,
                        }));
                    }
                    AssemblyStep::Yield => {}
                    AssemblyStep::NeedPayload if self.phase == Phase::Audio => {
                        let lists = audio.take_spent();
                        self.reuse_pes_lists(|tracks| tracks.audio, lists);
                        self.phase = Phase::Read;
                    }
                    AssemblyStep::Finished if self.phase == Phase::DrainAudio => {
                        return self.end_audio();
                    }
                    _ => return Err(Error::Invalid("unexpected progressive AAC state")),
                }
            }
            Phase::End => return Ok(TsProgressiveStep::End),
            Phase::Failed => return Err(Error::Closed),
        }
        Ok(TsProgressiveStep::Yield)
    }

    fn on_event(&mut self, event: FeedStep) -> Result<()> {
        match event {
            FeedStep::Event(event) => self.on_transport(event.source)?,
            FeedStep::Configuration(config) => self.configure(config)?,
            FeedStep::Packet { packet, clock } => match packet {
                FeedPacket::Owned(packet) => self.on_owned_packet(packet, &clock)?,
                FeedPacket::Compact(packet) => self.on_packet(packet, &clock)?,
            },
            FeedStep::Refusal { track, error, .. }
                if self
                    .tracks
                    .as_ref()
                    .is_some_and(|tracks| tracks.is_video(track) || tracks.is_audio(track)) =>
            {
                return Err(error);
            }
            _ => {}
        }
        Ok(())
    }

    /// Makes the reader of a configured track, once per track.
    #[cold]
    fn configure(&mut self, config: AdmittedCarriage) -> Result<()> {
        let tracks = self
            .tracks
            .as_ref()
            .ok_or(Error::Invalid("missing TS PMT"))?;
        if config.configuration_epoch != tracks.epoch {
            return Err(Error::Invalid("progressive TS configuration epoch"));
        }
        let pool = self
            .transport
            .as_ref()
            .ok_or(Error::Closed)?
            .engine
            .input()
            .budget();
        if tracks.is_video(config.track) {
            self.video = Some(AvcTsReader::new_progressive(pool, config, self.limits.avc)?);
        } else if tracks.is_audio(config.track) {
            self.audio = Some(HlsAac::new(pool, config, MAXIMUM_ADTS_BYTES)?);
        }
        Ok(())
    }

    /// Owned payloads come from tracks of a carriage without compact payloads, such
    /// as private audio.
    #[cold]
    fn on_owned_packet(
        &mut self,
        packet: triergon_mpegts::compat::pes::DemuxPacket,
        clock: &triergon_mpegts::compat::pes::DemuxTimeView,
    ) -> Result<()> {
        self.on_packet(packet, clock)
    }

    fn on_packet<P: PesInput>(
        &mut self,
        packet: P,
        clock: &triergon_mpegts::compat::pes::DemuxTimeView,
    ) -> Result<()> {
        let mut mailbox = Some(packet);
        let result = self.hand_to_reader(&mut mailbox, clock);
        // A reader takes the packets of its track; any other is dropped out of line.
        if let Some(packet) = mailbox.take() {
            discard(packet);
        }
        result
    }

    fn hand_to_reader<P: PesInput>(
        &mut self,
        mailbox: &mut Option<P>,
        clock: &triergon_mpegts::compat::pes::DemuxTimeView,
    ) -> Result<()> {
        let tracks = self
            .tracks
            .as_ref()
            .ok_or(Error::Invalid("missing TS PMT"))?;
        let facts = mailbox
            .as_ref()
            .ok_or(Error::Invalid("missing TS packet"))?
            .facts();
        let track = TrackKey::Ts {
            program: facts.program,
            pid: facts.pid,
        };
        if tracks.is_video(track) {
            let video = self
                .video
                .as_mut()
                .ok_or(Error::Invalid("missing AVC configuration"))?;
            if !video.push_packet(mailbox, clock)? {
                return Err(Error::Invalid("progressive AVC mailbox is busy"));
            }
            self.phase = Phase::Video;
        } else if tracks.is_audio(track) {
            let audio = self
                .audio
                .as_mut()
                .ok_or(Error::Invalid("missing AAC configuration"))?;
            if !audio.push_packet(mailbox, clock)? {
                return Err(Error::Invalid("progressive AAC mailbox is busy"));
            }
            self.phase = Phase::Audio;
        }
        Ok(())
    }

    /// Tables, clock and fault events, which arrive rarely beside the payload.
    #[cold]
    fn on_transport(&mut self, event: TraceEvent) -> Result<()> {
        match event {
            TraceEvent::Pat(table) => {
                let binding = pat_binding(&table)?;
                if self.pat.is_some_and(|previous| binding != previous) {
                    return Err(Error::Unsupported("progressive TS changes PAT binding"));
                }
                self.pat = Some(binding);
            }
            TraceEvent::ProgramMap {
                map,
                active: true,
                configuration_epoch,
            } => {
                if self.tracks.is_some() {
                    return Err(Error::Unsupported("progressive TS changes active PMT"));
                }
                self.tracks = Some(Tracks {
                    program: map.header.extension,
                    video: select_track(&map, stream_type::H264)?,
                    audio: select_track(&map, stream_type::AAC_ADTS)?,
                    pcr_pid: map.pcr_pid,
                    epoch: configuration_epoch,
                });
            }
            TraceEvent::Diagnostic(fault)
                if matches!(
                    fault.kind,
                    FaultKind::Transport | FaultKind::Psi | FaultKind::Program
                ) || fault.pid.is_some_and(|pid| self.required_pid(pid)) =>
            {
                return Err(fault.error);
            }
            TraceEvent::Continuity {
                pid,
                outcome: ContinuityOutcome::SignalledDiscontinuity,
            } if self.required_pid(pid) => {
                return Err(Error::Unsupported("progressive TS continuity break"));
            }
            TraceEvent::Timeline { .. } => {
                return Err(Error::Unsupported("progressive TS clock discontinuity"));
            }
            _ => {}
        }
        Ok(())
    }

    /// Borrows this reader's unit as a packet, using the active PMT's PID and the
    /// presentation clock for audio. Fails with `Invalid` before tracks are known
    /// or `Overflow` when a timestamp cannot fit its packet clock.
    pub fn packet<'a>(&self, unit: &'a TsProgressiveUnit) -> Result<UnitPacket<'a>> {
        let tracks = self
            .tracks
            .as_ref()
            .ok_or(Error::Invalid("missing TS PMT"))?;
        match unit {
            TsProgressiveUnit::Video(unit) => UnitPacket::video(tracks.video, unit),
            TsProgressiveUnit::Audio {
                frame,
                presentation,
            } => UnitPacket::audio(tracks.audio, frame, *presentation, frame.dts),
        }
    }

    /// Takes a finished video unit back when the next one arrives so its unshared
    /// buffer can be reused. An audio unit releases its storage when dropped.
    pub fn reuse_unit(&mut self, unit: TsProgressiveUnit) {
        if let (TsProgressiveUnit::Video(unit), Some(video)) = (unit, self.video.as_mut()) {
            video.reuse_unit(unit);
        }
    }

    /// The observer of a track allocates nothing for its next PES when it gets
    /// back the lists of the one a reader has just consumed.
    fn reuse_pes_lists(&mut self, pid: impl Fn(&Tracks) -> u16, lists: SpentLists) {
        #[cfg(test)]
        if !tests::RETURN_PES_LISTS.get() {
            return;
        }
        if let (Some(transport), Some(tracks)) = (self.transport.as_mut(), self.tracks.as_ref()) {
            transport.engine.reuse_pes_lists(pid(tracks), lists);
        }
    }

    fn required_pid(&self, pid: u16) -> bool {
        pid == 0
            || self.pat.is_some_and(|pat| pat.pmt_pid == pid)
            || self.tracks.as_ref().is_some_and(|tracks| {
                tracks.pcr_pid == pid || tracks.video == pid || tracks.audio == pid
            })
    }

    /// Retires the transport at the source's end and starts draining both readers.
    #[cold]
    fn end_transport(&mut self) -> Result<()> {
        self.transport = None;
        self.video.as_mut().ok_or(Error::Truncated)?.finish();
        self.audio.as_mut().ok_or(Error::Truncated)?.finish();
        self.phase = Phase::DrainVideo;
        Ok(())
    }

    /// Retires the drained video reader.
    #[cold]
    fn end_video(&mut self) {
        self.video = None;
        self.phase = Phase::DrainAudio;
    }

    /// Retires the drained audio reader, which ends a complete feed.
    #[cold]
    fn end_audio(&mut self) -> Result<TsProgressiveStep> {
        if !self.saw_video || self.audio_times.is_none() {
            return Err(Error::Invalid("progressive TS lacks a complete A/V feed"));
        }
        self.audio = None;
        self.phase = Phase::End;
        Ok(TsProgressiveStep::End)
    }

    /// Retires the reader after a failure.
    #[cold]
    fn fail(&mut self) {
        self.phase = Phase::Failed;
        self.transport = None;
        self.video = None;
        self.audio = None;
    }
}

/// Drops a packet no reader took.
#[cold]
fn discard<P>(packet: P) {
    drop(packet);
}

fn audio_presentation(
    times: &mut Option<(Time, NextAudioTime)>,
    anchor: Option<Time>,
    duration: Time,
) -> Result<Time> {
    let (presentation, ticks) = if let Some(anchor) = anchor {
        if times.is_some_and(|(previous, _)| anchor < previous) {
            return Err(Error::Invalid("progressive AAC timestamps move backwards"));
        }
        (anchor, exact_ticks90(anchor))
    } else {
        // The positive ADTS duration already makes the carried time monotone.
        match times.map(|(_, next)| next) {
            Some(NextAudioTime::Ticks90(ticks)) => (
                Time::new(i128::from(ticks), u128::from(PES_CLOCK_HZ))?,
                Some(ticks),
            ),
            Some(NextAudioTime::Exact(next)) => (next, None),
            None => {
                return Err(Error::Unsupported(
                    "progressive AAC starts without PES time",
                ));
            }
        }
    };
    let next = match ticks.and_then(|ticks| ticks.checked_add(exact_ticks90(duration)?)) {
        Some(next) => NextAudioTime::Ticks90(next),
        None => NextAudioTime::Exact(presentation.checked_add(duration)?),
    };
    *times = Some((presentation, next));
    Ok(presentation)
}

fn exact_ticks90(time: Time) -> Option<i64> {
    let denominator = u32::try_from(time.denominator()).ok()?;
    if !PES_CLOCK_HZ.is_multiple_of(denominator) {
        return None;
    }
    i64::try_from(time.numerator())
        .ok()?
        .checked_mul(i64::from(PES_CLOCK_HZ / denominator))
}

fn pat_binding(table: &PatTable) -> Result<PatBinding> {
    // The transport engine has already enforced one media program.
    let entry = table
        .programs
        .as_slice()
        .iter()
        .find(|entry| !entry.is_network_pid())
        .ok_or(Error::Invalid("progressive TS PAT without program"))?;
    Ok(PatBinding {
        tsid: table.header.extension,
        program: entry.program_number,
        pmt_pid: entry.pid,
    })
}

fn select_track(map: &ProgramMap, kind: u8) -> Result<u16> {
    select_pid(
        map,
        None,
        |stream| Ok(stream.stream_type == kind),
        "ambiguous progressive TS track",
    )?
    .ok_or(Error::Unsupported(
        "progressive TS requires AVC and ADTS AAC",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;
    use triergon_core::{Budget, Bytes, SourceCapabilities, SourceId};

    thread_local! {
        /// Whether readers on this thread hand consumed PES lists back to their
        /// observers. A test clears it to read without that reuse.
        pub(super) static RETURN_PES_LISTS: Cell<bool> = const { Cell::new(true) };
    }

    #[test]
    fn refusal_delivery_waits_for_work_and_keeps_its_original_fields() {
        let pool = Budget::new(256 * 1024).unwrap();
        let id = SourceId(73);
        let caps = SourceCapabilities {
            length: None,
            random_access: false,
        };
        let input = InputCursor::new(&pool, id, caps, 4096).unwrap();
        let source = triergon_playback::ByteSource::new(
            id,
            Bytes::copy_from_slice(&pool, &[]).unwrap(),
            188,
        )
        .unwrap();
        let track = TrackKey::Ts {
            program: 7,
            pid: 257,
        };
        let error = Error::Unsupported("test carriage");
        let mut transport = Transport {
            engine: ProgressiveFeed::new(input, ParseLimits::default()).unwrap(),
            request: None,
            pending: Some(FeedStep::Refusal {
                track,
                error,
                source_event_ordinal: 109,
            }),
            source: SourceOwner::new(source),
        };
        let cancel = AtomicBool::new(false);
        let mut work = WorkBudget::new(0);
        assert!(matches!(
            transport.poll(&mut work, &cancel, 0),
            Ok(FeedStep::Yield)
        ));
        assert!(transport.pending.is_some());
        assert_eq!(work.spent(), 0);

        let mut work = WorkBudget::new(1);
        assert!(
            matches!(transport.poll(&mut work, &cancel, 0), Ok(FeedStep::Refusal {
            track: actual_track, error: actual_error, source_event_ordinal: 109,
        }) if actual_track == track && actual_error == error)
        );
        assert_eq!(work.spent(), 1);
        assert!(transport.pending.is_none());
    }

    #[test]
    fn aligned_requests_keep_identity_and_never_round_positive_limits_to_zero() {
        let pool = Budget::new(64 * 1024).unwrap();
        let input = InputCursor::new(
            &pool,
            SourceId(71),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            4096,
        )
        .unwrap();
        for offset in [0, 1, 187, 188, u64::MAX - 187, u64::MAX] {
            for maximum in [0, 1, 17, 187, 188, 189, 4096, usize::MAX] {
                let mut original = input.request();
                original.offset = offset;
                original.maximum = maximum;
                let request = carrier_aligned_request(original.clone());
                assert_eq!(request.source, original.source);
                assert_eq!(request.token, original.token);
                assert_eq!(request.offset, original.offset);
                assert!(request.maximum <= maximum);
                assert_eq!(request.maximum == 0, maximum == 0);
                // Search all possible trims independently of the production formula.
                let expected = (0..maximum.min(188))
                    .find_map(|trim| {
                        let count = maximum - trim;
                        (offset % 188 + (count % 188) as u64)
                            .is_multiple_of(188)
                            .then_some(count)
                    })
                    .unwrap_or(maximum);
                assert_eq!(
                    request.maximum, expected,
                    "offset {offset}, maximum {maximum}"
                );
            }
        }
    }

    #[test]
    fn capped_requests_preserve_short_replies_and_stale_token_errors() {
        use std::{cell::Cell, rc::Rc};
        use triergon_core::InputWindow;

        struct Waiting(Rc<Cell<usize>>);
        impl Source for Waiting {
            fn poll(
                &mut self,
                request: &InputRequest,
                _: &mut WorkBudget,
                _: &AtomicBool,
            ) -> Result<SourceReply> {
                self.0.set(request.maximum);
                Ok(SourceReply::Pending)
            }
            fn invalidate(&mut self) {
                panic!("no random access");
            }
            fn abort(&mut self) {}
            fn close(&mut self) {}
        }
        fn request(transport: &mut Transport<Waiting>, cancel: &AtomicBool) -> InputRequest {
            for _ in 0..128 {
                match transport
                    .poll(&mut WorkBudget::new(65536), cancel, 64 * 1024)
                    .unwrap()
                {
                    FeedStep::NeedInput(request) => return request,
                    FeedStep::Yield => {}
                    _ => panic!("expected an input request"),
                }
            }
            panic!("request stalled");
        }

        let pool = Budget::new(256 * 1024).unwrap();
        let seen = Rc::new(Cell::new(0));
        let open = || {
            let input = InputCursor::new(
                &pool,
                SourceId(72),
                SourceCapabilities {
                    length: None,
                    random_access: false,
                },
                4096,
            )
            .unwrap();
            Transport {
                engine: ProgressiveFeed::new(input, ParseLimits::default()).unwrap(),
                request: None,
                pending: None,
                source: SourceOwner::new(Waiting(seen.clone())),
            }
        };
        let cancel = AtomicBool::new(false);
        let mut old = open();
        let stale = request(&mut old, &cancel);
        let mut current = open();
        let first = request(&mut current, &cancel);
        assert_eq!(first.offset, 0);
        assert_eq!(first.maximum, 3948);
        assert_eq!(seen.get(), first.maximum);

        let beyond_hard = Bytes::copy_from_slice(&pool, &[0x47; 5000]).unwrap();
        for token in [stale.token.clone(), first.token.clone()] {
            let reply = SourceReply::Data(
                InputWindow::new(first.source, token, 0, beyond_hard.clone()).unwrap(),
            );
            assert_eq!(
                current.provide(&reply),
                Err(Error::Limit {
                    requested: 5000,
                    maximum: 4096
                })
            );
            assert_eq!(current.request.as_ref().unwrap().token, first.token);
        }
        let bytes = Bytes::copy_from_slice(&pool, &[0x47; 4096]).unwrap();
        let stale = SourceReply::Data(
            InputWindow::new(stale.source, stale.token, 0, bytes.clone()).unwrap(),
        );
        assert!(matches!(
            current.provide(&stale),
            Err(Error::StaleGeneration { .. })
        ));
        assert_eq!(current.request.as_ref().unwrap().token, first.token);
        let foreign = Budget::new(16 * 1024).unwrap();
        let foreign = SourceReply::Data(
            InputWindow::new(
                first.source,
                first.token.clone(),
                first.offset,
                Bytes::copy_from_slice(&foreign, &[0x47; 4096]).unwrap(),
            )
            .unwrap(),
        );
        assert_eq!(current.provide(&foreign), Err(Error::ForeignPool));
        assert_eq!(current.request.as_ref().unwrap().token, first.token);
        let wrong_source = SourceReply::Data(
            InputWindow::new(
                SourceId(73),
                first.token.clone(),
                first.offset,
                bytes.clone(),
            )
            .unwrap(),
        );
        assert_eq!(current.provide(&wrong_source), Err(Error::SourceMismatch));
        let wrong_offset = SourceReply::Data(
            InputWindow::new(
                first.source,
                first.token.clone(),
                first.offset + 1,
                bytes.clone(),
            )
            .unwrap(),
        );
        assert_eq!(
            current.provide(&wrong_offset),
            Err(Error::Invalid("input window is not currently requested"))
        );
        assert_eq!(current.request.as_ref().unwrap().token, first.token);
        let oversized = SourceReply::Data(
            InputWindow::new(first.source, first.token.clone(), first.offset, bytes).unwrap(),
        );
        assert_eq!(
            current.provide(&oversized),
            Err(Error::Limit {
                requested: 4096,
                maximum: 3948
            })
        );
        assert_eq!(current.request.as_ref().unwrap().token, first.token);

        let short = SourceReply::Data(
            InputWindow::new(
                first.source,
                first.token.clone(),
                first.offset,
                Bytes::copy_from_slice(&pool, &[0x47]).unwrap(),
            )
            .unwrap(),
        );
        assert_eq!(current.provide(&short), Ok(true));
        let next = request(&mut current, &cancel);
        assert_eq!(next.source, first.source);
        assert_eq!(next.token, first.token);
        assert_eq!(next.offset, 1);
        assert_eq!(next.maximum, 3947);
        assert_eq!(seen.get(), next.maximum);
        assert_eq!(
            current.provide(&SourceReply::End {
                source: next.source,
                token: next.token
            }),
            Ok(true)
        );
        let mut truncated = false;
        for _ in 0..128 {
            match current
                .poll(&mut WorkBudget::new(65536), &cancel, 64 * 1024)
                .unwrap()
            {
                FeedStep::Yield => {}
                FeedStep::Event(event) => {
                    assert!(matches!(event.source, TraceEvent::Diagnostic(fault)
                        if fault.kind == FaultKind::Transport && fault.error == Error::Truncated));
                    truncated = true;
                    break;
                }
                _ => panic!("expected the truncated carrier diagnostic"),
            }
        }
        assert!(truncated);
    }

    #[test]
    fn every_retained_charge_returns_once_the_reader_and_its_units_drop() {
        fn read(quantum: u64, window: usize, private_audio: bool) {
            let pool = Budget::new(8 << 20).unwrap();
            let mut wire =
                include_bytes!("../../playback/tests/fixtures/hls/baseline-stereo.ts").to_vec();
            if private_audio {
                // Keep ordinary private-audio discovery and its eventual
                // progressive track refusal on the original observer path.
                let pmt = &mut wire[2 * 188 + 5..2 * 188 + 5 + 26];
                pmt[17] = 0x81;
                let crc = triergon_mpegts::psi::PsiCrc32::of(&pmt[..22]).to_be_bytes();
                pmt[22..].copy_from_slice(&crc);
            }
            let id = SourceId(31);
            let caps = SourceCapabilities {
                length: Some(wire.len() as u64),
                random_access: false,
            };
            let source = triergon_playback::ByteSource::new(
                id,
                Bytes::copy_from_slice(&pool, &wire).unwrap(),
                window,
            )
            .unwrap();
            let input = InputCursor::new(&pool, id, caps, window).unwrap();
            let limits = TsProgressiveLimits::default();
            let mut reader = TsProgressiveReader::new(input, source, limits).unwrap();
            let cancel = AtomicBool::new(false);
            let mut retained = Vec::new();
            for _ in 0..2_000_000 {
                let mut work = WorkBudget::new(quantum);
                let ended = match reader.advance(&mut work, &cancel) {
                    Ok(TsProgressiveStep::Unit(unit)) => {
                        retained.push(unit);
                        false
                    }
                    Ok(TsProgressiveStep::Yield) => false,
                    Ok(TsProgressiveStep::End) => {
                        assert!(!private_audio);
                        true
                    }
                    Ok(other) => panic!("unexpected {other:?}"),
                    Err(_) => {
                        assert!(private_audio);
                        true
                    }
                };
                if ended {
                    drop(reader);
                    drop(retained);
                    assert_eq!(pool.used(), Budget::control_storage_bytes());
                    return;
                }
            }
            panic!("progressive input did not complete");
        }
        for quantum in [1, 31, 4096] {
            for window in [188, 4096] {
                for private_audio in [false, true] {
                    read(quantum, window, private_audio);
                }
            }
        }
    }

    #[test]
    fn reused_storage_keeps_every_output_and_work_boundary() {
        // Like the progressive worker, hold two units per track and hand the
        // oldest back when a third arrives. Reused storage is at most twice
        // the item it serves, so the peak may rise by at most one unit.
        fn read(
            wire: &[u8],
            reuse: bool,
            quantum: u64,
            window: usize,
        ) -> (Vec<(String, u64)>, usize, usize) {
            let pool = Budget::new(8 << 20).unwrap();
            let id = SourceId(37);
            let caps = SourceCapabilities {
                length: Some(wire.len() as u64),
                random_access: false,
            };
            let source = triergon_playback::ByteSource::new(
                id,
                Bytes::copy_from_slice(&pool, wire).unwrap(),
                window,
            )
            .unwrap();
            let input = InputCursor::new(&pool, id, caps, window).unwrap();
            let mut reader =
                TsProgressiveReader::new(input, source, TsProgressiveLimits::default()).unwrap();
            RETURN_PES_LISTS.set(reuse);
            let cancel = AtomicBool::new(false);
            let mut trace = Vec::new();
            let mut held: [VecDeque<TsProgressiveUnit>; 2] = Default::default();
            let mut peak = 0;
            let mut largest = 0;
            loop {
                let mut work = WorkBudget::new(quantum);
                let step = reader.advance(&mut work, &cancel).unwrap();
                let tag = match &step {
                    TsProgressiveStep::Unit(TsProgressiveUnit::Video(video)) => format!(
                        "video {:?} {} {:?}",
                        video.clock,
                        video.idr,
                        video.feed_spans().flatten().collect::<Vec<_>>()
                    ),
                    TsProgressiveStep::Unit(TsProgressiveUnit::Audio {
                        frame,
                        presentation,
                    }) => format!("audio {presentation:?} {:?}", frame.bytes.as_ref()),
                    other => format!("{other:?}"),
                };
                trace.push((tag, work.spent()));
                peak = peak.max(pool.used());
                match step {
                    TsProgressiveStep::Unit(unit) => {
                        if let TsProgressiveUnit::Video(video) = &unit {
                            largest = largest.max(video.source_bytes().len());
                        }
                        let track =
                            &mut held[usize::from(matches!(unit, TsProgressiveUnit::Audio { .. }))];
                        track.push_back(unit);
                        if track.len() > 2
                            && let Some(oldest) = track.pop_front()
                            && reuse
                        {
                            reader.reuse_unit(oldest);
                        }
                    }
                    TsProgressiveStep::End => {
                        RETURN_PES_LISTS.set(true);
                        drop((reader, held));
                        assert_eq!(pool.used(), Budget::control_storage_bytes());
                        return (trace, peak, largest);
                    }
                    _ => {}
                }
            }
        }
        for wire in [
            include_bytes!("../../playback/tests/fixtures/hls/baseline-stereo.ts").as_slice(),
            include_bytes!("../../playback/tests/fixtures/hls/bframes-multislice.ts").as_slice(),
        ] {
            for quantum in [1, 31, 4096] {
                for window in [188, 4096] {
                    let (reused, reused_peak, largest) = read(wire, true, quantum, window);
                    let (fresh, fresh_peak, _) = read(wire, false, quantum, window);
                    assert_eq!(reused, fresh, "work {quantum}, window {window}");
                    assert!(
                        reused_peak <= fresh_peak + largest,
                        "peak {reused_peak} above {fresh_peak} and a {largest}-byte unit: \
                         work {quantum}, window {window}"
                    );
                }
            }
        }
    }

    #[test]
    fn integral_audio_ticks_match_exact_time_and_accept_a_fresh_anchor() {
        let start = Time::new(-7, 90_000).unwrap();
        let duration = Time::new(1024, 48_000).unwrap();
        let mut times = None;
        let mut expected = start;
        for frame in 0..128 {
            let anchor = (frame == 0).then_some(start);
            assert_eq!(
                audio_presentation(&mut times, anchor, duration),
                Ok(expected)
            );
            expected = expected.checked_add(duration).unwrap();
            assert!(matches!(times, Some((_, NextAudioTime::Ticks90(_)))));
        }

        let previous = times.unwrap().0;
        assert_eq!(
            audio_presentation(&mut times, Some(previous), duration),
            Ok(previous)
        );
        assert_eq!(
            audio_presentation(&mut times, Some(start), duration),
            Err(Error::Invalid("progressive AAC timestamps move backwards"))
        );
    }

    #[test]
    fn fractional_durations_and_tick_overflow_keep_exact_time() {
        let start = Time::new(13, 90_000).unwrap();
        let integral = Time::new(1024, 48_000).unwrap();
        let fractional = Time::new(1024, 44_100).unwrap();
        let mut times = None;
        assert_eq!(
            audio_presentation(&mut times, Some(start), integral),
            Ok(start)
        );
        let next = start.checked_add(integral).unwrap();
        assert_eq!(audio_presentation(&mut times, None, fractional), Ok(next));
        let expected = next.checked_add(fractional).unwrap();
        assert!(matches!(times, Some((_, NextAudioTime::Exact(next))) if next == expected));
        assert_eq!(audio_presentation(&mut times, None, integral), Ok(expected));

        let start = Time::new(i128::from(i64::MAX), 90_000).unwrap();
        assert_eq!(
            audio_presentation(&mut times, Some(start), integral),
            Ok(start)
        );
        let expected = start.checked_add(integral).unwrap();
        assert!(matches!(times, Some((_, NextAudioTime::Exact(next))) if next == expected));
        assert_eq!(audio_presentation(&mut times, None, integral), Ok(expected));
    }
}
