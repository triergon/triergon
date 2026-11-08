//! Matroska video startup and seek through the playback engine: open a preloaded
//! seekable input with the immutable seek index, discover the streams, optionally
//! drain to the end and seek the video track, bind it to its AVC or HEVC feed and
//! fold every delivered video packet with its native clock.
//!
//! The input is a capsule: an 8-byte magic, the operation kind (startup, seek, or
//! seek after draining) and the seek target in milliseconds, then the Matroska file.

use crate::matroska::{AVC_AAC, HEVC_DOLBY, WORK, ready};
use crate::runner::{Invocation, Result, Totals, measure};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, SourceId, Time, WorkBudget};
use triergon_harness_protocol::digest::{fold, signed};
use triergon_matroska::typed::{Event, Reader};
use triergon_playback::{
    ByteSource, Engine, Output, PlayerPolicy, Progress, RawEvent, Selection, TrackKey,
    io::SourceOwner,
    matroska_feed::{
        MatroskaCompactDiscovery, MatroskaDiscoveryLimits, MatroskaDiscoveryStep,
        MatroskaFeedSetup, MatroskaHevcDolbySetup, MatroskaPacketTiming, MatroskaVideoSeekGate,
    },
};

pub const PREFIX: &str = "feed-";
const OPERATION: &str = "matroska-video-session";
const QUANTUM: usize = 4096;
const HEADER: usize = 17;

#[derive(Clone, Copy)]
enum Operation {
    Startup,
    Seek { milliseconds: i64, drain: bool },
}

impl Operation {
    /// The operation recorded in a capsule header.
    fn from_header(header: &[u8]) -> Option<Self> {
        if header.len() < HEADER || !header.starts_with(b"SMVCL001") {
            return None;
        }
        let milliseconds = i64::from_be_bytes(header[9..HEADER].try_into().ok()?);
        match header[8] {
            0 => Some(Self::Startup),
            kind @ (1 | 2) => Some(Self::Seek {
                milliseconds,
                drain: kind == 2,
            }),
            _ => None,
        }
    }
}

/// Packets, payload bytes and the digest of one session: PTS, DTS, duration and
/// size, then the payload as little-endian words, the last one zero-padded.
#[derive(Default)]
struct Work {
    packets: u64,
    bytes: u64,
    digest: u64,
}

impl Work {
    fn packet(&mut self, timing: MatroskaPacketTiming, data: &[u8]) {
        self.packets += 1;
        self.bytes += data.len() as u64;
        fold(&mut self.digest, signed(timing.pts_ns.unwrap_or(i64::MIN)));
        fold(&mut self.digest, signed(timing.dts_ns.unwrap_or(i64::MIN)));
        fold(&mut self.digest, timing.duration_ns.unwrap_or(0));
        fold(&mut self.digest, data.len() as u64);
        let chunks = data.chunks_exact(8);
        let tail = chunks.remainder();
        for chunk in chunks {
            fold(
                &mut self.digest,
                u64::from_le_bytes(chunk.try_into().expect("an 8-byte chunk")),
            );
        }
        if !tail.is_empty() {
            let mut chunk = [0; 8];
            chunk[..tail.len()].copy_from_slice(tail);
            fold(&mut self.digest, u64::from_le_bytes(chunk));
        }
    }

    fn totals(self) -> Totals {
        Totals {
            packets: self.packets,
            bytes: self.bytes,
            digest: self.digest,
        }
    }
}

/// One session. A failure inside it is a broken invariant of the corpus and panics.
fn session(pool: &Budget, raw: &Bytes, operation: Operation) -> Work {
    let cancel = AtomicBool::new(false);
    let id = SourceId(31);
    let reader = Reader::new_with_immutable_seek_index(
        pool,
        id,
        raw.clone(),
        triergon_matroska::Limits::default(),
        QUANTUM,
    )
    .unwrap();
    let engine = Engine::matroska_reader(reader);
    let source = ByteSource::new(id, raw.clone(), QUANTUM).unwrap();
    let mut discovery = MatroskaCompactDiscovery::new(
        engine,
        SourceOwner::new(source),
        Selection::All,
        PlayerPolicy::default(),
        MatroskaDiscoveryLimits::default(),
    )
    .unwrap();
    loop {
        if let MatroskaDiscoveryStep::Ready(_) =
            discovery.poll(&mut WorkBudget::new(WORK), &cancel).unwrap()
        {
            break;
        }
    }
    let finished = discovery.finish().unwrap();
    let mut player = finished.player;
    let prefix = finished.prefix;
    let mut configuration = prefix
        .events()
        .find_map(|event| match event {
            Event::Configuration(configuration) => Some(configuration),
            _ => None,
        })
        .unwrap()
        .clone();
    let (track, descriptor) = configuration
        .tracks()
        .iter()
        .enumerate()
        .find(|(_, track)| track.kind == 1)
        .unwrap();
    let key = TrackKey::Matroska(descriptor.number);
    let mut pending = None;
    let mut first = None;
    let mut gate = None;
    let prefix = match operation {
        Operation::Startup => Some(prefix),
        Operation::Seek {
            milliseconds,
            drain,
        } => {
            drop(prefix);
            if drain {
                loop {
                    let progress = player
                        .read_output(&mut pending, &mut WorkBudget::new(WORK), &cancel)
                        .unwrap();
                    pending = None;
                    if matches!(progress, Progress::Ended) {
                        break;
                    }
                }
            }
            assert!(
                player
                    .seek_and_select(
                        key,
                        Time::new(i128::from(milliseconds), 1000).unwrap(),
                        Selection::All,
                        &mut WorkBudget::new(WORK),
                        &cancel,
                    )
                    .unwrap()
            );
            let mut intent = None;
            loop {
                let progress = player
                    .read_output(&mut pending, &mut WorkBudget::new(WORK), &cancel)
                    .unwrap();
                if let Some(output) = pending.take() {
                    match output.event() {
                        RawEvent::Seek(_) => intent = Some(output),
                        RawEvent::Matroska(Event::Seek(_)) => {
                            gate = Some(
                                MatroskaVideoSeekGate::new(intent.as_ref().unwrap(), &output)
                                    .unwrap(),
                            );
                        }
                        RawEvent::Matroska(Event::Packet(packet))
                            if packet.packet.track == track
                                && gate.as_mut().unwrap().admits(&output).unwrap() =>
                        {
                            configuration = packet.configuration.clone();
                            first = Some(output);
                            break;
                        }
                        _ => {}
                    }
                }
                assert!(!matches!(progress, Progress::Ended));
            }
            None
        }
    };
    let (avc, hevc) = match configuration.tracks()[track].codec.as_str() {
        "V_MPEG4/ISO/AVC" => {
            let mut setup =
                MatroskaFeedSetup::new(pool, id, &configuration, track, AVC_AAC).unwrap();
            (Some(ready(|work| setup.step(work, &cancel)).unwrap()), None)
        }
        "V_MPEGH/ISO/HEVC" => {
            let mut setup =
                MatroskaHevcDolbySetup::new(pool, id, &configuration, track, HEVC_DOLBY).unwrap();
            (None, Some(ready(|work| setup.step(work, &cancel)).unwrap()))
        }
        _ => panic!("unknown video codec"),
    };
    let mut avc_clock = avc
        .as_ref()
        .map(|feed| feed.native_avc_clock().unwrap().unwrap());
    let mut hevc_clock = hevc
        .as_ref()
        .map(|feed| feed.native_hevc_clock().unwrap().unwrap());
    let mut work = Work::default();
    let mut output = |event: &Output| {
        let RawEvent::Matroska(Event::Packet(packet)) = event.event() else {
            return;
        };
        if packet.packet.track != track
            || gate
                .as_mut()
                .is_some_and(|gate| !gate.admits(event).unwrap())
        {
            return;
        }
        let timing = if let (Some(feed), Some(clock)) = (&avc, &mut avc_clock) {
            let mut job = feed.packet(event).unwrap().unwrap();
            clock
                .observe(&ready(|work| job.step(work, &cancel)).unwrap())
                .unwrap()
        } else {
            let mut job = hevc.as_ref().unwrap().packet(event).unwrap().unwrap();
            hevc_clock
                .as_mut()
                .unwrap()
                .observe(&ready(|work| job.step(work, &cancel)).unwrap())
                .unwrap()
        };
        work.packet(timing, packet.packet.data.as_ref());
    };
    if let Some(prefix) = prefix {
        for event in prefix.into_outputs() {
            output(&event);
        }
    }
    if let Some(first) = first {
        output(&first);
    }
    loop {
        let progress = player
            .read_output(&mut pending, &mut WorkBudget::new(WORK), &cancel)
            .unwrap();
        if let Some(event) = pending.take() {
            output(&event);
        }
        if matches!(progress, Progress::Ended) {
            break;
        }
    }
    work
}

/// `feed-fold CAPSULE`, `feed-bench CAPSULE ITERATIONS` or `feed-memory CAPSULE`.
pub fn run(args: &[String]) -> Result<()> {
    let invocation = Invocation::parse(args, PREFIX, "", 0..=0, false)?;
    let pool = Budget::new(64 << 20)?;
    let capsule = std::fs::read(invocation.path)?;
    let operation = Operation::from_header(&capsule).ok_or("not a playback capsule")?;
    let raw = Bytes::copy_from_slice(&pool, &capsule[HEADER..])?;
    drop(capsule);
    measure(&invocation, OPERATION, Some(&pool), || {
        Ok(session(&pool, &raw, operation).totals())
    })
}
