//! Sequential TS188 feed for AVC and ADTS AAC.

use crate::{
    Engine, Error, Player, PlayerPolicy, Progress, RawEvent, Result, Selection, Source,
    SourceReply, TrackKey,
    hls_aac::{HlsAac, HlsAacFrame},
    hls_avc::{AvcTsAccessUnit, AvcTsLimits, AvcTsReader, AvcTsStep},
    io::SourceOwner,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{InputCursor, InputRequest, Time, WorkBudget};
use triergon_mpegts::{
    AssemblyStep, ContinuityOutcome, FaultKind, ParseLimits, TraceEvent,
    psi::{PatTable, ProgramMap},
};

const TS_CLOCK_RATE: u32 = 90_000;

#[derive(Clone, Copy, Debug, Default)]
pub struct TsProgressiveLimits {
    pub transport: ParseLimits,
    pub avc: AvcTsLimits,
}

#[derive(Debug)]
pub enum TsProgressiveUnit {
    /// Annex B in decode order. The clock retains raw source timestamps and
    /// their native 90 kHz view, which supplies a decode tick for PTS-only PES.
    Video(AvcTsAccessUnit),
    Audio {
        frame: HlsAacFrame,
        /// Exact presentation time on this body's native PES clock. Missing anchors
        /// advance from the preceding frame by its ADTS sample duration.
        /// The frame retains the original PTS/DTS presence independently.
        presentation: Time,
    },
}

#[derive(Debug)]
pub enum TsProgressiveStep {
    Yield,
    NeedInput(InputRequest),
    Backpressure { requested: usize, available: usize },
    Unit(TsProgressiveUnit),
    End,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Read,
    Video,
    Audio,
    DrainVideo,
    DrainAudio,
    End,
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PatBinding {
    tsid: u16,
    program: u16,
    pmt_pid: u16,
}

struct Tracks {
    video: TrackKey,
    audio: TrackKey,
    pcr_pid: u16,
    epoch: u64,
}

#[derive(Clone, Copy)]
enum NextAudioTime {
    Ticks90(i64),
    Exact(Time),
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
pub struct TsProgressiveReader<S: Source> {
    player: Option<Player<S>>,
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

    pub fn new(input: InputCursor, source: S, limits: TsProgressiveLimits) -> Result<Self> {
        let source = SourceOwner::new(source);
        if input.position() != 0 || input.capabilities().random_access {
            return Err(Error::Invalid("progressive TS requires a sequential body"));
        }
        let engine = Engine::ts_progressive(input, limits.transport)?;
        // The feed applies the raw PES clock to access units. It never uses
        // the player's packet presentation classification.
        let player = Player::new_direct(
            engine,
            source,
            Selection::All,
            PlayerPolicy {
                slots: 1,
                queued_payload_bytes: limits.transport.pes_bytes,
                maximum_events: u64::MAX,
                ..PlayerPolicy::default()
            },
        )?;
        Ok(Self {
            player: Some(player),
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
        let result = self.player.as_mut().ok_or(Error::Closed)?.provide(reply);
        if matches!(
            result,
            Err(Error::Transport | Error::Truncated | Error::Cancelled)
        ) {
            self.fail();
        }
        result
    }

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
                Err(Error::Backpressure {
                    requested,
                    available,
                }) => {
                    return Ok(TsProgressiveStep::Backpressure {
                        requested,
                        available,
                    });
                }
                // The frame assemblers retain state across allocation failure.
                // A native track refusal while reading is terminal.
                Err(Error::Allocation) if self.phase != Phase::Read => {
                    return Err(Error::Allocation);
                }
                Err(error) => {
                    self.fail();
                    return Err(error);
                }
                result => return result,
            }
        }
        Ok(TsProgressiveStep::Yield)
    }

    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<TsProgressiveStep> {
        match self.phase {
            Phase::Read => {
                let player = self.player.as_mut().ok_or(Error::Closed)?;
                let mut output = None;
                match player.read_output(&mut output, work, cancel)? {
                    Progress::Queued => {
                        let event = output.ok_or(Error::Invalid("missing TS player output"))?;
                        self.on_event(event.into_event())?;
                    }
                    Progress::NeedInput(request) => {
                        return Ok(TsProgressiveStep::NeedInput(request));
                    }
                    Progress::Backpressure {
                        requested,
                        available,
                    } => {
                        return Ok(TsProgressiveStep::Backpressure {
                            requested,
                            available,
                        });
                    }
                    Progress::Ended => {
                        self.player = None;
                        self.video.as_mut().ok_or(Error::Truncated)?.finish();
                        self.audio.as_mut().ok_or(Error::Truncated)?.finish();
                        self.phase = Phase::DrainVideo;
                    }
                    Progress::Yield => {}
                    _ => return Err(Error::Invalid("unexpected TS player state")),
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
                        self.phase = Phase::Read;
                    }
                    AvcTsStep::End if self.phase == Phase::DrainVideo => {
                        self.video = None;
                        self.phase = Phase::DrainAudio;
                    }
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
                        self.phase = Phase::Read;
                    }
                    AssemblyStep::Finished if self.phase == Phase::DrainAudio => {
                        if !self.saw_video || self.audio_times.is_none() {
                            return Err(Error::Invalid("progressive TS lacks a complete A/V feed"));
                        }
                        self.audio = None;
                        self.phase = Phase::End;
                        return Ok(TsProgressiveStep::End);
                    }
                    _ => return Err(Error::Invalid("unexpected progressive AAC state")),
                }
            }
            Phase::End => return Ok(TsProgressiveStep::End),
            Phase::Failed => return Err(Error::Closed),
        }
        Ok(TsProgressiveStep::Yield)
    }

    fn on_event(&mut self, event: RawEvent) -> Result<()> {
        match event {
            RawEvent::Ts(event) => self.on_transport(event.source)?,
            RawEvent::NativeTsConfiguration(config) => {
                let tracks = self
                    .tracks
                    .as_ref()
                    .ok_or(Error::Invalid("missing TS PMT"))?;
                if config.configuration_epoch != tracks.epoch {
                    return Err(Error::Invalid("progressive TS configuration epoch"));
                }
                let pool = self.player.as_ref().ok_or(Error::Closed)?.budget();
                if config.track == tracks.video {
                    self.video = Some(AvcTsReader::new_progressive(pool, config, self.limits.avc)?);
                } else if config.track == tracks.audio {
                    self.audio = Some(HlsAac::new(pool, config, 0x1fff)?);
                }
            }
            RawEvent::NativeTs { packet, clock } => {
                let tracks = self
                    .tracks
                    .as_ref()
                    .ok_or(Error::Invalid("missing TS PMT"))?;
                let track = TrackKey::Ts {
                    program: packet.program,
                    pid: packet.pid,
                };
                let mut mailbox = Some(packet);
                if track == tracks.video {
                    let video = self
                        .video
                        .as_mut()
                        .ok_or(Error::Invalid("missing AVC configuration"))?;
                    if !video.push(&mut mailbox, clock)? {
                        return Err(Error::Invalid("progressive AVC mailbox is busy"));
                    }
                    self.phase = Phase::Video;
                } else if track == tracks.audio {
                    let audio = self
                        .audio
                        .as_mut()
                        .ok_or(Error::Invalid("missing AAC configuration"))?;
                    if !audio.push(&mut mailbox, clock)? {
                        return Err(Error::Invalid("progressive AAC mailbox is busy"));
                    }
                    self.phase = Phase::Audio;
                }
            }
            RawEvent::NativeRefusal { track, error, .. }
                if self
                    .tracks
                    .as_ref()
                    .is_some_and(|t| track == t.video || track == t.audio) =>
            {
                return Err(error);
            }
            _ => {}
        }
        Ok(())
    }

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
                    video: select_track(&map, 0x1b)?,
                    audio: select_track(&map, 0x0f)?,
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

    fn required_pid(&self, pid: u16) -> bool {
        pid == 0
            || self.pat.is_some_and(|pat| pat.pmt_pid == pid)
            || self.tracks.as_ref().is_some_and(|tracks| {
                tracks.pcr_pid == pid
                    || [tracks.video, tracks.audio]
                        .iter()
                        .any(|track| matches!(*track, TrackKey::Ts { pid: selected, .. } if selected == pid))
            })
    }

    fn fail(&mut self) {
        self.phase = Phase::Failed;
        self.player = None;
        self.video = None;
        self.audio = None;
    }
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
                Time::new(i128::from(ticks), u128::from(TS_CLOCK_RATE))?,
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
    if !TS_CLOCK_RATE.is_multiple_of(denominator) {
        return None;
    }
    i64::try_from(time.numerator())
        .ok()?
        .checked_mul(i64::from(TS_CLOCK_RATE / denominator))
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

fn select_track(map: &ProgramMap, stream_type: u8) -> Result<TrackKey> {
    let mut streams = map
        .streams
        .as_slice()
        .iter()
        .filter(|stream| stream.stream_type == stream_type);
    let stream = streams.next().ok_or(Error::Unsupported(
        "progressive TS requires AVC and ADTS AAC",
    ))?;
    if streams.next().is_some() {
        return Err(Error::Unsupported("ambiguous progressive TS track"));
    }
    Ok(TrackKey::Ts {
        program: map.header.extension,
        pid: stream.pid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
