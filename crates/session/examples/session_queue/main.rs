//! Public session feed and drain on preloaded media access units.
#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use triergon::{Budget, CodecId, ContainerReader, PacketInfo, RandomAccess, open_sequential};
use triergon_harness_protocol::digest::{fold, fold_bytes, fold_framed};
use triergon_harness_runner::{Invocation, Mode, Result, Totals, measure_with};
use triergon_session::{
    AudioCodec, AudioSession, FeedError, PcmBlock, QueueLimits, SessionState, VideoCodec,
    VideoSession, audio_decoder, video_decoder,
};

#[derive(Clone, Copy)]
enum Codec {
    Ac3,
    Aac,
    Avc,
}

impl Codec {
    fn id(self) -> CodecId<'static> {
        match self {
            Self::Ac3 => CodecId::Ac3,
            Self::Aac => CodecId::Aac,
            Self::Avc => CodecId::H264,
        }
    }
}

struct Input {
    payloads: Vec<Vec<u8>>,
    codec: Codec,
    repeat_first: bool,
    limits: QueueLimits,
    distinct: usize,
    adjacent_equal: usize,
    payload_fold: u64,
}

impl Input {
    fn read(file: &[u8], case: &str) -> Result<Self> {
        let (codec, repeat_first) = match case {
            "ac3" => (Codec::Ac3, false),
            "aac" => (Codec::Aac, false),
            "avc" | "avc-runs" => (Codec::Avc, false),
            "avc-repeat" => (Codec::Avc, true),
            _ => return Err("expected ac3, aac, avc or avc-repeat".into()),
        };
        let pool = Budget::new(32 << 20)?;
        let mut reader = open_sequential(file, &pool)?;
        let mut payloads = Vec::new();
        while let Some(packet) = reader.next_packet()? {
            let catalogue = reader.reader();
            let selected = (0..catalogue.track_count())
                .filter_map(|i| catalogue.track(i))
                .any(|t| t.id() == packet.track() && t.codec() == codec.id());
            if selected {
                if repeat_first
                    && payloads.is_empty()
                    && packet.random_access() != RandomAccess::Sync
                {
                    return Err("repeated AVC control requires a random-access picture".into());
                }
                payloads.push(packet.payload().to_vec());
            }
        }
        if payloads.len() < 2 {
            return Err("fixture needs at least two access units".into());
        }
        let mut lengths = BTreeSet::new();
        let mut adjacent_equal = 0;
        let mut previous = None;
        let mut bytes = 0;
        let mut payload_fold = 0;
        for payload in &payloads {
            let data = if repeat_first { &payloads[0] } else { payload };
            lengths.insert(data.len());
            adjacent_equal += usize::from(previous == Some(data.len()));
            previous = Some(data.len());
            bytes += data.len();
            fold_framed(&mut payload_fold, data);
        }
        if (lengths.len() == 1) != matches!(case, "ac3" | "avc-repeat") {
            return Err("unexpected access-unit size pattern".into());
        }
        // One spare slot makes the extra feed fail on total bytes alone.
        let limits = QueueLimits {
            units: payloads.len() + 1,
            bytes,
            unit_bytes: *lengths.last().ok_or("empty pattern")?,
        };
        Ok(Self {
            payloads,
            codec,
            repeat_first,
            limits,
            distinct: lengths.len(),
            adjacent_equal,
            payload_fold,
        })
    }

    fn units(&self) -> impl Iterator<Item = &[u8]> {
        self.payloads.iter().map(|p| {
            if self.repeat_first {
                self.payloads[0].as_slice()
            } else {
                p
            }
        })
    }
}

// Keep both sessions inline so their wrapper adds no heap allocation to the observer.
#[allow(clippy::large_enum_variant)]
enum Session {
    Audio(AudioSession, PcmBlock),
    Video(VideoSession),
}

impl Session {
    fn new(codec: Codec, limits: QueueLimits) -> Result<Self> {
        match codec {
            Codec::Avc => {
                let mut video = VideoSession::new(limits);
                video.load(video_decoder(VideoCodec::H264));
                Ok(Self::Video(video))
            }
            Codec::Aac | Codec::Ac3 => {
                let kind = if matches!(codec, Codec::Aac) {
                    AudioCodec::Aac
                } else {
                    AudioCodec::Ac3
                };
                let mut audio = AudioSession::new(limits);
                audio.load(Some(audio_decoder(kind, &[]).map_err(|e| e.0)?));
                Ok(Self::Audio(audio, PcmBlock::default()))
            }
        }
    }

    fn feed(&mut self, bytes: &[u8], pts: i64) -> std::result::Result<(), FeedError> {
        match self {
            Self::Audio(s, _) => s.feed(bytes, Some(pts)),
            Self::Video(s) => s.feed(bytes, Some(pts)),
        }
    }

    fn state(&self) -> SessionState {
        match self {
            Self::Audio(s, _) => s.state(),
            Self::Video(s) => s.state(),
        }
    }

    fn end(&mut self) {
        match self {
            Self::Audio(s, _) => s.end_of_stream(),
            Self::Video(s) => s.end_of_stream(),
        }
    }

    fn drain(&mut self, out: &mut Totals) -> Result<()> {
        match self {
            Self::Audio(s, pcm) => {
                pcm.samples.clear();
                s.pull(None, 0, pcm);
                if !pcm.samples.is_empty() {
                    out.packets += 1;
                    out.bytes += (pcm.samples.len() * 2) as u64;
                    fold(&mut out.digest, u64::from(pcm.sample_rate));
                    fold(
                        &mut out.digest,
                        pcm.pts.ok_or("missing audio timestamp")? as u64,
                    );
                    for &sample in &pcm.samples {
                        fold(&mut out.digest, u64::from(sample as u16));
                    }
                }
            }
            Self::Video(s) => {
                while s.step(None) {
                    let picture = s.latest().ok_or("missing published picture")?;
                    out.packets += 1;
                    out.bytes += picture.rgba.len() as u64;
                    fold(
                        &mut out.digest,
                        picture.pts.ok_or("missing video timestamp")? as u64,
                    );
                    fold_bytes(&mut out.digest, &picture.rgba);
                }
            }
        }
        if let SessionState::Failed(reason) = self.state() {
            return Err(reason.into());
        }
        Ok(())
    }
}

fn run(input: &Input, stage: &str, held: &mut Option<Session>) -> Result<Totals> {
    let mut session = Session::new(input.codec, input.limits)?;
    let mut out = Totals {
        digest: input.payload_fold,
        ..Totals::default()
    };
    let n = input.payloads.len();
    let feed = |session: &mut Session, bytes, pts| {
        session.feed(bytes, pts).map_err(|e| format!("feed: {e:?}"))
    };
    let fill = |session: &mut Session, start: usize| -> Result<()> {
        for (i, unit) in input.units().enumerate() {
            feed(session, unit, (start + i) as i64)?;
        }
        if session.feed(&input.payloads[0], (start + n) as i64) != Err(FeedError::Backpressure) {
            return Err("queue admitted a payload beyond the total byte limit".into());
        }
        Ok(())
    };
    fill(&mut session, 0)?;
    session.drain(&mut out)?;
    // Retry the rejected unit first, then drain between borrowed feeds.
    for (i, unit) in input.units().enumerate() {
        feed(&mut session, unit, (n + i) as i64)?;
        session.drain(&mut out)?;
    }
    if session.state() != SessionState::Starved || out.bytes == 0 {
        return Err("active session did not decode and reach starvation".into());
    }
    if matches!(stage, "full" | "drain-full") {
        fill(&mut session, 2 * n)?;
    }
    if matches!(stage, "cycle" | "drain-full") {
        session.end();
        session.drain(&mut out)?;
        if session.state() != SessionState::Ended {
            return Err("incomplete session drain".into());
        }
    } else {
        // Keep the entire session alive through the observer's END marker.
        *held = Some(session);
    }
    Ok(out)
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let invocation = Invocation::parse(&args, "queue-", " CASE STAGE", 2..=2, true)?;
    let input = Input::read(&invocation.input(1 << 20)?, &invocation.arguments[0])?;
    let stage = invocation.arguments[1].as_str();
    if !matches!(stage, "cycle" | "starved" | "full" | "drain-full")
        || (stage != "cycle" && invocation.iterations != 1)
    {
        return Err(
            "expected cycle, starved, full or drain-full; retention takes one iteration".into(),
        );
    }
    if invocation.mode == Mode::Trace {
        let sizes: Vec<_> = input.units().map(<[u8]>::len).collect();
        println!("{{\"sizes\":{sizes:?}}}");
        return Ok(());
    }
    let fields = [
        ("units_per_pass", input.payloads.len() as u64),
        ("queue_bytes", input.limits.bytes as u64),
        ("queue_slots", input.limits.units as u64),
        ("max_unit_bytes", input.limits.unit_bytes as u64),
        ("distinct_sizes", input.distinct as u64),
        ("adjacent_equal_sizes", input.adjacent_equal as u64),
        ("input_digest", input.payload_fold),
    ];
    let mut held = None;
    measure_with(&invocation, "session-queue", None, &fields, || {
        run(&input, stage, &mut held)
    })
}
