//! The session clock, discard and drain rules with caller-provided PCM.

use std::{ffi::OsString, io::Write};

use triergon::{Error, PacketInfo, RandomAccess, TimeBase, TrackId, Trim};
use triergon_codec::{AudioDecoder, AudioFrame, ChannelLayout, DownmixLevels, SampleFormat};
use triergon_harness_protocol::digest::signed;
use triergon_session::{
    AudioCodec, AudioSession, Discard, FeedError, OnRefusal, PcmBlock, Planar, Planes, QueueLimits,
    SessionState, StreamSelection, VideoCodec, VideoSession,
};

use triergon_harness_runner::{Operation, Totals};

const RATE: u32 = 1000;
const MS: i64 = 1_000_000;
const REFUSED: Error = Error::Invalid("caller PCM refusal");

struct Packet<'a> {
    raw: &'a [u8],
    base: TimeBase,
    trim: Trim,
}

impl PacketInfo for Packet<'_> {
    fn track(&self) -> TrackId {
        TrackId(1)
    }
    fn payload(&self) -> &[u8] {
        self.raw
    }
    fn time_base(&self) -> TimeBase {
        self.base
    }
    fn pts(&self) -> Option<i64> {
        None
    }
    fn dts(&self) -> Option<i64> {
        None
    }
    fn duration(&self) -> Option<i64> {
        None
    }
    fn random_access(&self) -> RandomAccess {
        RandomAccess::Sync
    }
    fn trim(&self) -> Trim {
        self.trim
    }
}

struct CallerPcm {
    delay: usize,
    flush_error: bool,
    had_input: bool,
}

impl CallerPcm {
    fn frame(out: &mut AudioFrame, values: &[u8], rate: u32) -> Result<(), Error> {
        *out = AudioFrame {
            sample_rate: rate,
            layout: ChannelLayout::MONO,
            sample_format: SampleFormat::Float,
            samples_per_channel: values.len(),
            planes: vec![
                values
                    .iter()
                    .map(|&sample| f32::from(sample) / 256.0)
                    .collect(),
            ],
        };
        Ok(())
    }
}

impl AudioDecoder for CallerPcm {
    type Error = Error;

    fn decode(
        &mut self,
        unit: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, Error> {
        let ([scale @ (1 | 2)], data) = unit.split_at_checked(1).ok_or(REFUSED)? else {
            return Err(REFUSED);
        };
        Self::frame(out, data, u32::from(*scale) * RATE)?;
        self.had_input = true;
        Ok(None)
    }

    fn delay(&self) -> usize {
        self.delay
    }

    fn flush(&mut self, out: &mut AudioFrame) -> Result<(), Error> {
        if self.flush_error {
            return Err(REFUSED);
        }
        if std::mem::take(&mut self.had_input) {
            Self::frame(out, &[251, 252], 2 * RATE)?;
        } else {
            out.clear();
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.had_input = false;
    }
}

fn record(
    work: &mut Totals,
    trace: &mut Option<&mut dyn Write>,
    kind: u64,
    values: &[u64],
    bytes: &[u8],
) -> Result<(), String> {
    work.record(kind, values, bytes);
    if let Some(out) = trace {
        write!(out, "{kind}")
            .and_then(|()| {
                for value in values {
                    write!(out, " {value}")?;
                }
                if !bytes.is_empty() {
                    write!(out, " {}", triergon_harness_protocol::hex(bytes))?;
                }
                writeln!(out)
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn block(
    work: &mut Totals,
    trace: &mut Option<&mut dyn Write>,
    kind: u64,
    consumed: usize,
    pcm: &PcmBlock<Planes>,
) -> Result<(), String> {
    let [samples] = pcm.samples.planes.as_slice() else {
        return Err("expected mono PCM".into());
    };
    let bytes: Vec<_> = samples
        .iter()
        .map(|sample| (*sample * 256.0) as u8)
        .collect();
    if samples
        .iter()
        .zip(&bytes)
        .any(|(sample, byte)| sample.to_bits() != (f32::from(*byte) / 256.0).to_bits())
    {
        return Err("session changed a planar sample".into());
    }
    record(
        work,
        trace,
        kind,
        &[
            consumed as u64,
            u64::from(pcm.sample_rate),
            pcm.pts.map_or(u64::MAX, signed),
            pcm.samples.samples_per_channel() as u64,
        ],
        &bytes,
    )
}

fn lifecycle(
    input: &[u8],
    work: &mut Totals,
    mut trace: Option<&mut dyn Write>,
) -> Result<(), String> {
    let mut session = AudioSession::with_form(
        QueueLimits {
            units: 2,
            bytes: 10,
            unit_bytes: 5,
        },
        Planar,
    );
    if session.feed(&input[..5], None) != Err(FeedError::Idle) {
        return Err("an unloaded audio lane accepts a unit".into());
    }
    session.load(Some(
        CallerPcm {
            delay: 6,
            flush_error: false,
            had_input: false,
        }
        .boxed(),
    ));
    let base = TimeBase::new(1, u64::from(RATE)).map_err(|e| e.to_string())?;
    let packet = |raw, start, end| Packet {
        raw,
        base,
        trim: Trim { start, end },
    };
    let first = packet(&input[..5], Some(6), None);
    let next = packet(&input[5..10], None, Some(1));
    let after = &input[10..];
    session
        .feed_discarding(first.payload(), Some(0), Discard::of(&first))
        .map_err(|e| e.to_string())?;
    session
        .feed_discarding(next.payload(), Some(20 * MS), Discard::of(&next))
        .map_err(|e| e.to_string())?;
    if session.feed(after, None) != Err(FeedError::Backpressure) {
        return Err("an occupied audio queue accepted another unit".into());
    }
    let mut pcm = PcmBlock::default();
    if session.pull(Some(17 * MS - 1), 3 * MS, &mut pcm) != 1
        || pcm.samples.samples_per_channel() != 0
    {
        return Err("audio ignored lead or leaked completely discarded samples".into());
    }
    session.feed(after, None).map_err(|e| e.to_string())?;
    let consumed = session.pull(Some(17 * MS), 3 * MS, &mut pcm);
    block(work, &mut trace, 1, consumed, &pcm)?;
    if session.pull(None, 0, &mut pcm) != 0 {
        return Err("audio lost the carried block while the output was occupied".into());
    }
    let mut later = PcmBlock::default();
    let consumed = session.pull(None, 0, &mut later);
    block(work, &mut trace, 2, consumed, &later)?;
    session.end_of_stream();
    if session.feed(after, None) != Err(FeedError::Invalid)
        || session.pull(None, 0, &mut later) != 0
        || later.samples.samples_per_channel() != 4
    {
        return Err("the drain displaced occupied output or admitted later input".into());
    }
    let mut drained = PcmBlock::default();
    let consumed = session.pull(None, 0, &mut drained);
    block(work, &mut trace, 3, consumed, &drained)?;
    if session.state() != SessionState::Ended || session.refusals() != 0 {
        return Err("a drained block did not end the lane".into());
    }
    session.flush();
    if session.state() != SessionState::Starved {
        return Err("stop removed the audio decoder".into());
    }
    session.end_of_stream();
    let mut empty = PcmBlock::default();
    if session.pull(None, 0, &mut empty) != 0 || empty.samples.samples_per_channel() != 0 {
        return Err("stop retained prior drain state".into());
    }
    session.load(None);
    if session.state() != SessionState::Idle || session.feed(after, None) != Err(FeedError::Idle) {
        return Err("unload retained queued audio".into());
    }
    record(work, &mut trace, 4, &[session.generation()], &[])
}

fn refusals(
    input: &[u8],
    work: &mut Totals,
    mut trace: Option<&mut dyn Write>,
) -> Result<(), String> {
    let limits = QueueLimits::AUDIO;
    for (mode, policy) in [(0, OnRefusal::Fail), (1, OnRefusal::Skip)] {
        let mut lane = AudioSession::with_form(limits, Planar).on_refusal(policy);
        lane.load(Some(
            CallerPcm {
                delay: 0,
                flush_error: true,
                had_input: false,
            }
            .boxed(),
        ));
        lane.feed(&[0, 0], Some(0)).map_err(|e| e.to_string())?;
        lane.feed(&input[..5], Some(10 * MS))
            .map_err(|e| e.to_string())?;
        lane.end_of_stream();
        let mut pcm = PcmBlock::default();
        let consumed = lane.pull(None, 0, &mut pcm);
        if policy == OnRefusal::Fail {
            if lane.state() != SessionState::Failed(REFUSED)
                || pcm.samples.samples_per_channel() != 0
            {
                return Err("fatal decoder refusal published later audio".into());
            }
        } else {
            if lane.state() != SessionState::Ended {
                return Err("skip policy refused the whole lane".into());
            }
            block(work, &mut trace, 6, consumed, &pcm)?;
        }
        if lane.last_refusal() != Some(REFUSED) {
            return Err("the session lost its last typed refusal".into());
        }
        record(
            work,
            &mut trace,
            5,
            &[mode, consumed as u64, lane.refusals()],
            &[],
        )?;
        lane.flush();
        if lane.refusals() != 0 || lane.last_refusal().is_some() {
            return Err("stop kept diagnostics from the old generation".into());
        }
        lane.end_of_stream();
        let expected = if policy == OnRefusal::Fail {
            SessionState::Failed(REFUSED)
        } else {
            SessionState::Ended
        };
        if lane.state() != expected || lane.refusals() != 1 || lane.last_refusal() != Some(REFUSED)
        {
            return Err("the same policy did not govern a drain-only error".into());
        }
        record(
            work,
            &mut trace,
            7,
            &[mode, lane.refusals(), lane.generation()],
            &[],
        )?;
    }

    let mut audio = AudioSession::new(limits);
    let mut video = VideoSession::new(QueueLimits::VIDEO);
    let before = (audio.generation(), video.generation());
    let refused = StreamSelection {
        video: VideoCodec::H264,
        audio: Some(AudioCodec::Opus),
    };
    if refused.load(&mut video, &mut audio, &[]).is_ok()
        || (audio.generation(), video.generation()) != before
    {
        return Err("a refused audio configuration changed the stream".into());
    }
    let selection = StreamSelection {
        video: VideoCodec::H264,
        audio: Some(AudioCodec::Aac),
    };
    selection
        .load(&mut video, &mut audio, &[])
        .map_err(|e| e.to_string())?;
    if audio.state() != SessionState::Starved || video.state() != SessionState::Starved {
        return Err("the admitted stream did not load both sessions".into());
    }
    record(
        work,
        &mut trace,
        8,
        &[audio.generation(), video.generation()],
        &[],
    )
}

pub(super) fn operation<'a>(
    _prefix: &str,
    input: &'a [u8],
    args: &[OsString],
) -> Result<Operation<'a>, String> {
    if input.len() != 15 || input[0] != 1 || input[5] != 1 || input[10] != 2 {
        return Err("the lifecycle control needs three four-sample units".into());
    }
    let action = match args {
        [mode] if mode == "lifecycle" => lifecycle,
        [mode] if mode == "refusals" => refusals,
        _ => return Err("expected lifecycle or refusals".into()),
    };
    Ok(Operation {
        name: "audio-session-lifecycle",
        run: Box::new(move |work, trace| action(input, work, trace)),
    })
}
