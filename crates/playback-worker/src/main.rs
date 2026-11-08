//! Playback measurement worker; serialization and startup are outside the timed loop.
#![forbid(unsafe_code)]
mod hls;
mod matroska_audio;
mod mp4_forward;
mod mp4_range;
mod packet_fold;
mod pgs;
mod progressive;
mod raw_video;
mod runner;
mod subtitle_text;
mod ts_program;
use std::{
    io::{self, Write},
    process::ExitCode,
    sync::atomic::AtomicBool,
    time::Instant,
};
use triergon::typed::Event;
use triergon_core::{Budget, Bytes, SourceId, Time, WorkBudget};
use triergon_framing::{
    AacConfigLimits, AvcAnnexBLimits, AvcConfigLimits, BuildStep, HvcConfigLimits,
};
use triergon_harness_protocol::digest::{fold, signed};
use triergon_matroska as triergon;
use triergon_playback::{
    ByteSource, Engine, Output, PlayerPolicy, Progress, RawEvent, Selection, TrackKey,
    io::SourceOwner,
    matroska_feed::{
        MatroskaCompactDiscovery, MatroskaDiscoveryLimits, MatroskaDiscoveryStep,
        MatroskaDolbyLimits, MatroskaFeedLimits, MatroskaFeedSetup, MatroskaHevcDolbyLimits,
        MatroskaHevcDolbySetup, MatroskaPacketTiming, MatroskaVideoSeekGate,
    },
};

const VIDEO: AvcAnnexBLimits = AvcAnnexBLimits {
    maximum_packet_bytes: 32768,
    maximum_output_bytes: 49152,
    maximum_nals: 64,
};
const AVC: MatroskaFeedLimits = MatroskaFeedLimits {
    avc_config: AvcConfigLimits {
        maximum_record_bytes: 8192,
        maximum_nals: 64,
        maximum_nal_bytes: 4096,
    },
    avc_packet: VIDEO,
    aac_config: AacConfigLimits {
        maximum_record_bytes: 128,
        maximum_pce_elements: 32,
        maximum_comment_bytes: 64,
    },
    aac_output_bytes: 8191,
};
const HEVC: MatroskaHevcDolbyLimits = MatroskaHevcDolbyLimits {
    hevc_config: HvcConfigLimits {
        maximum_record_bytes: 8192,
        maximum_arrays: 16,
        maximum_nals: 64,
        maximum_nal_bytes: 4096,
    },
    hevc_packet: VIDEO,
    dolby_packet: MatroskaDolbyLimits {
        maximum_packet_bytes: 16384,
        maximum_syncframes: 8,
    },
};

fn ready<T>(mut step: impl FnMut(&mut WorkBudget) -> triergon_playback::Result<BuildStep<T>>) -> T {
    for _ in 0..4096 {
        match step(&mut WorkBudget::new(4096)).unwrap() {
            BuildStep::Ready(value) => return value,
            BuildStep::Yield => {}
            BuildStep::Complete => break,
        }
    }
    panic!("selected feed did not finish");
}

const OPERATION: &str = "matroska-video-session";
const QUANTUM: usize = 4096;

#[derive(Clone, Copy)]
enum Operation {
    Startup,
    Seek { milliseconds: i64, drain: bool },
}

#[derive(Default)]
struct Work {
    packets: u64,
    bytes: u64,
    digest: u64,
}

impl Work {
    fn fold(&mut self, word: u64) {
        fold(&mut self.digest, word);
    }

    fn packet(&mut self, timing: MatroskaPacketTiming, data: &[u8]) {
        self.packets += 1;
        self.bytes += data.len() as u64;
        self.fold(signed(timing.pts_ns.unwrap_or(i64::MIN)));
        self.fold(signed(timing.dts_ns.unwrap_or(i64::MIN)));
        self.fold(timing.duration_ns.unwrap_or(0));
        self.fold(data.len() as u64);
        let chunks = data.chunks_exact(8);
        let tail = chunks.remainder();
        for chunk in chunks {
            self.fold(u64::from_le_bytes(chunk.try_into().unwrap()));
        }
        if !tail.is_empty() {
            let mut chunk = [0; 8];
            chunk[..tail.len()].copy_from_slice(tail);
            self.fold(u64::from_le_bytes(chunk));
        }
    }

    fn accumulate(&mut self, operation: Work) {
        self.packets += operation.packets;
        self.bytes += operation.bytes;
        self.fold(operation.digest);
    }
}

fn run(pool: &Budget, raw: &Bytes, operation: Operation) -> Work {
    let cancel = AtomicBool::new(false);
    let id = SourceId(31);
    let reader = triergon::typed::Reader::new_with_immutable_seek_index(
        pool,
        id,
        raw.clone(),
        triergon::Limits::default(),
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
            discovery.poll(&mut WorkBudget::new(4096), &cancel).unwrap()
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
                        .read_output(&mut pending, &mut WorkBudget::new(4096), &cancel)
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
                        &mut WorkBudget::new(4096),
                        &cancel,
                    )
                    .unwrap()
            );
            let mut intent = None;
            loop {
                let progress = player
                    .read_output(&mut pending, &mut WorkBudget::new(4096), &cancel)
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
            let mut setup = MatroskaFeedSetup::new(pool, id, &configuration, track, AVC).unwrap();
            (Some(ready(|work| setup.step(work, &cancel))), None)
        }
        "V_MPEGH/ISO/HEVC" => {
            let mut setup =
                MatroskaHevcDolbySetup::new(pool, id, &configuration, track, HEVC).unwrap();
            (None, Some(ready(|work| setup.step(work, &cancel))))
        }
        _ => panic!("unknown video codec"),
    };
    let mut avc_clock = avc
        .as_ref()
        .map(|feed| feed.native_avc_clock().unwrap().unwrap());
    let mut hevc_clock = hevc
        .as_ref()
        .map(|feed| feed.native_hevc_clock().unwrap().unwrap());
    let mut result = Work::default();
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
                .observe(&ready(|work| job.step(work, &cancel)))
                .unwrap()
        } else {
            let mut job = hevc.as_ref().unwrap().packet(event).unwrap().unwrap();
            hevc_clock
                .as_mut()
                .unwrap()
                .observe(&ready(|work| job.step(work, &cancel)))
                .unwrap()
        };
        result.packet(timing, packet.packet.data.as_ref());
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
            .read_output(&mut pending, &mut WorkBudget::new(4096), &cancel)
            .unwrap();
        if let Some(event) = pending.take() {
            output(&event);
        }
        if matches!(progress, Progress::Ended) {
            break;
        }
    }
    result
}

/// The operation recorded in a capsule header.
fn operation(header: &[u8]) -> Option<Operation> {
    if header.len() < 17 || !header.starts_with(b"SMVCL001") {
        return None;
    }
    let milliseconds = i64::from_be_bytes(header[9..17].try_into().ok()?);
    match header[8] {
        0 => Some(Operation::Startup),
        kind @ (1 | 2) => Some(Operation::Seek {
            milliseconds,
            drain: kind == 2,
        }),
        _ => None,
    }
}

fn main() -> ExitCode {
    match work() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("playback-worker: {error}");
            ExitCode::from(2)
        }
    }
}

fn work() -> Result<(), Box<dyn std::error::Error>> {
    let all: Vec<String> = std::env::args().skip(1).collect();
    if all
        .first()
        .is_some_and(|mode| mode.starts_with("progressive-"))
    {
        return progressive::run(&all);
    }
    if all.first().is_some_and(|mode| mode.starts_with("hls-")) {
        return hls::run(&all);
    }
    if all
        .first()
        .is_some_and(|mode| mode.starts_with("mp4-range-"))
    {
        return mp4_range::run(&all);
    }
    if let Some(result) = runner::dispatch(&all) {
        return result;
    }
    let mut args = all.into_iter();
    let usage = "usage: playback-worker feed-bench CAPSULE ITERATIONS | feed-fold CAPSULE | feed-memory CAPSULE";
    let mode = args.next().ok_or(usage)?;
    let path = args.next().ok_or(usage)?;
    let iterations = match mode.as_str() {
        "feed-bench" => args.next().ok_or(usage)?.parse::<u64>()?,
        "feed-fold" | "feed-memory" => 1,
        _ => return Err(usage.into()),
    };
    if args.next().is_some() || !(1..=1_000_000).contains(&iterations) {
        return Err(usage.into());
    }
    let memory = mode == "feed-memory";
    let pool = Budget::new(64 << 20)?;
    let bytes = std::fs::read(path)?;
    let operation = operation(&bytes).ok_or("not a playback capsule")?;
    let raw = Bytes::copy_from_slice(&pool, &bytes[17..])?;
    drop(bytes);
    let mut stdout = io::stdout().lock();
    if memory {
        stdout.write_all(b"TRIERGON_BEGIN_V1\n")?;
        stdout.flush()?;
    }
    let start = Instant::now();
    let mut work = Work::default();
    for _ in 0..iterations {
        work.accumulate(run(&pool, &raw, operation));
    }
    let elapsed = if mode == "feed-bench" {
        start.elapsed().as_nanos()
    } else {
        0
    };
    if memory {
        stdout.write_all(b"TRIERGON_END_V1\n")?;
        stdout.flush()?;
    }
    writeln!(
        stdout,
        "{{\"schema\":1,\"operation\":\"{OPERATION}\",\"iterations\":{iterations},\"packets\":{},\"bytes\":{},\"digest\":{},\"elapsed_ns\":{elapsed},\"eof\":true}}",
        work.packets, work.bytes, work.digest,
    )?;
    stdout.flush()?;
    Ok(())
}
