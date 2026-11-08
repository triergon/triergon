//! Matroska audio startup and forward clocks through the playback engine: open a
//! preloaded seekable input read in 4 KiB windows, discover the streams, bind every
//! selected AAC-LC, AC-3 or E-AC-3 track to its feed, project the discovery prefix
//! through `native_aac_clock_probe` or `native_dolby_clock_probe`, continue with the
//! forward clock through the end of file and fold every selected packet with its
//! native PTS, DTS and duration in nanoseconds.
//!
//! AAC packets pass through the ADTS feed before their clock; Dolby packets use the
//! source clock, so a syncframe the decoder would refuse keeps its packet and time.

use crate::packet_fold::{Packet, Work, drive};
use crate::runner::Result;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, SourceCapabilities, SourceId, WorkBudget};
use triergon_framing::{
    AacConfigLimits, AvcAnnexBLimits, AvcConfigLimits, BuildStep, HvcConfigLimits,
};
use triergon_matroska::typed::{Configuration, Event, Packet as MatroskaPacket};
use triergon_playback::{
    ByteSource, Engine, Output, PlayerPolicy, Progress, RawEvent, Selection,
    io::SourceOwner,
    matroska_feed::{
        MatroskaClock, MatroskaClockPrefix, MatroskaDiscovery, MatroskaDiscoveryLimits,
        MatroskaDiscoveryStep, MatroskaDolbyLimits, MatroskaFeedLimits, MatroskaFeedSetup,
        MatroskaHevcDolbyFeed, MatroskaHevcDolbyLimits, MatroskaHevcDolbySetup,
        MatroskaPacketTiming, MatroskaTrackFeed,
    },
};

pub const PREFIX: &str = "mkv-audio-";
const OPERATION: &str = "matroska-audio-clock";
const QUANTUM: usize = 4096;
const WORK: u64 = 4096;
const MAX_TRACKS: usize = 64;

const VIDEO: AvcAnnexBLimits = AvcAnnexBLimits {
    maximum_packet_bytes: 32768,
    maximum_output_bytes: 49152,
    maximum_nals: 64,
};
const AAC: MatroskaFeedLimits = MatroskaFeedLimits {
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
const DOLBY: MatroskaHevcDolbyLimits = MatroskaHevcDolbyLimits {
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Select {
    Aac,
    Dolby,
    All,
}

impl Select {
    fn admits(self, codec: &str) -> bool {
        match codec {
            "A_AAC" => self != Self::Dolby,
            "A_AC3" | "A_EAC3" => self != Self::Aac,
            _ => false,
        }
    }
}

enum Feed {
    Aac(MatroskaTrackFeed),
    Dolby(MatroskaHevcDolbyFeed),
}

enum Prefix<'a> {
    Aac(MatroskaClockPrefix<'a, MatroskaTrackFeed>),
    Dolby(MatroskaClockPrefix<'a, MatroskaHevcDolbyFeed>),
}

enum Clock<'a> {
    Aac(MatroskaClock<'a, MatroskaTrackFeed>),
    Dolby(MatroskaClock<'a, MatroskaHevcDolbyFeed>),
}

fn ready<T>(
    mut step: impl FnMut(&mut WorkBudget) -> triergon_playback::Result<BuildStep<T>>,
) -> Result<T> {
    for _ in 0..4096 {
        match step(&mut WorkBudget::new(WORK))? {
            BuildStep::Ready(value) => return Ok(value),
            BuildStep::Yield => {}
            BuildStep::Complete => break,
        }
    }
    Err("selected feed did not finish".into())
}

fn source_packet(output: &Output) -> Option<&MatroskaPacket> {
    match output.event() {
        RawEvent::Matroska(Event::Packet(packet)) => Some(packet),
        _ => None,
    }
}

fn fold(work: &mut Work, packet: &MatroskaPacket, timing: MatroskaPacketTiming) -> Result<()> {
    let data = packet.packet.data.as_ref();
    work.packet(
        Packet::new(
            packet.packet.track as u64,
            timing.pts_ns,
            timing.dts_ns,
            data.len(),
            [data],
        )
        .duration(timing.duration_ns.unwrap_or(0))
        .flags(u64::from(packet.packet.keyframe)),
    )
}

fn feeds(
    pool: &Budget,
    id: SourceId,
    configuration: &Configuration,
    select: Select,
) -> Result<Vec<Option<Feed>>> {
    let tracks = configuration.tracks();
    if tracks.len() > MAX_TRACKS {
        return Err("too many Matroska tracks".into());
    }
    let cancel = AtomicBool::new(false);
    let mut feeds = Vec::with_capacity(tracks.len());
    for (track, descriptor) in tracks.iter().enumerate() {
        let codec = descriptor.codec.as_str();
        feeds.push(if descriptor.kind != 2 || !select.admits(codec) {
            None
        } else if codec == "A_AAC" {
            let mut setup = MatroskaFeedSetup::new(pool, id, configuration, track, AAC)?;
            Some(Feed::Aac(ready(|work| setup.step(work, &cancel))?))
        } else {
            let mut setup = MatroskaHevcDolbySetup::new(pool, id, configuration, track, DOLBY)?;
            Some(Feed::Dolby(ready(|work| setup.step(work, &cancel))?))
        });
    }
    if feeds.iter().all(Option::is_none) {
        return Err("no selected audio track".into());
    }
    Ok(feeds)
}

fn operation(pool: &Budget, raw: &Bytes, select: Select, mut work: Work) -> Result<Work> {
    let cancel = AtomicBool::new(false);
    let id = SourceId(41);
    let engine = Engine::matroska(
        pool,
        id,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        triergon_matroska::Limits::default(),
        QUANTUM,
    )?;
    let mut discovery = MatroskaDiscovery::new(
        engine,
        SourceOwner::new(ByteSource::new(id, raw.clone(), QUANTUM)?),
        Selection::All,
        PlayerPolicy::default(),
        MatroskaDiscoveryLimits::default(),
    )?;
    while !matches!(
        discovery.poll(&mut WorkBudget::new(WORK), &cancel)?,
        MatroskaDiscoveryStep::Ready(_)
    ) {}
    let discovered = discovery.finish()?;
    let mut player = discovered.player;
    let prefix = discovered.prefix;
    let configuration = prefix
        .as_slice()
        .iter()
        .find_map(|output| match output.event() {
            RawEvent::Matroska(Event::Configuration(configuration)) => Some(configuration),
            _ => None,
        })
        .ok_or("no Matroska configuration before the first packet")?;
    let feeds = feeds(pool, id, configuration, select)?;

    // The discovery prefix is projected as one unit per selected track before any
    // of its rows is delivered, as the native clock resolves earlier unknown times.
    let mut counts = vec![0_usize; feeds.len()];
    for packet in prefix.as_slice().iter().filter_map(source_packet) {
        if let Some(count) = counts.get_mut(packet.packet.track) {
            *count += 1;
        }
    }
    let mut prefixes = Vec::with_capacity(feeds.len());
    for (track, (feed, &count)) in feeds.iter().zip(&counts).enumerate() {
        prefixes.push(match feed {
            None => None,
            Some(Feed::Aac(feed)) => {
                let mut probe = feed
                    .native_aac_clock_probe(pool, count)?
                    .ok_or("AAC clock probe")?;
                for output in prefix.as_slice() {
                    if source_packet(output).is_some_and(|p| p.packet.track == track) {
                        let mut job = feed.packet(output)?.ok_or("AAC feed packet")?;
                        probe.push(&ready(|work| job.step(work, &cancel))?)?;
                    }
                }
                Some(Prefix::Aac(probe.finish()))
            }
            Some(Feed::Dolby(feed)) => {
                let mut probe = feed
                    .native_dolby_clock_probe(pool, count)?
                    .ok_or("Dolby clock probe")?;
                for packet in prefix.as_slice().iter().filter_map(source_packet) {
                    if packet.packet.track == track {
                        probe.push_source(packet)?;
                    }
                }
                Some(Prefix::Dolby(probe.finish()))
            }
        });
    }

    let mut ordinals = vec![0_usize; feeds.len()];
    for packet in prefix.as_slice().iter().filter_map(source_packet) {
        let track = packet.packet.track;
        let Some(Some(clock)) = prefixes.get(track) else {
            continue;
        };
        let ordinal = ordinals[track];
        ordinals[track] += 1;
        let timing = match clock {
            Prefix::Aac(clock) => clock.timing(ordinal)?,
            Prefix::Dolby(clock) => clock.timing(ordinal)?,
        };
        fold(&mut work, packet, timing)?;
    }
    drop(prefix);
    let mut clocks: Vec<Option<Clock<'_>>> = prefixes
        .into_iter()
        .map(|prefix| {
            prefix.map(|prefix| match prefix {
                Prefix::Aac(prefix) => Clock::Aac(prefix.into_live()),
                Prefix::Dolby(prefix) => Clock::Dolby(prefix.into_live()),
            })
        })
        .collect();

    let mut pending = None;
    loop {
        let progress = player.read_output(&mut pending, &mut WorkBudget::new(WORK), &cancel)?;
        if let Some(output) = pending.take()
            && let Some(packet) = source_packet(&output)
        {
            let track = packet.packet.track;
            if let Some(Some(clock)) = clocks.get_mut(track) {
                let timing = match (clock, &feeds[track]) {
                    (Clock::Aac(clock), Some(Feed::Aac(feed))) => {
                        let mut job = feed.packet(&output)?.ok_or("AAC feed packet")?;
                        clock.observe(&ready(|work| job.step(work, &cancel))?)?
                    }
                    (Clock::Dolby(clock), _) => clock.observe_source(packet)?,
                    _ => return Err("feed and clock disagree".into()),
                };
                fold(&mut work, packet, timing)?;
            }
        }
        match progress {
            Progress::Ended => break,
            Progress::NeedInput(_) | Progress::Yield | Progress::Queued => {}
            other => return Err(format!("unexpected player progress: {other:?}").into()),
        }
    }
    Ok(work)
}

/// `mkv-audio-fold FILE SELECT`, `mkv-audio-bench FILE ITERATIONS SELECT` or
/// `mkv-audio-memory FILE SELECT`; SELECT is `aac`, `dolby` or `all`.
pub fn run(args: &[String]) -> Result<()> {
    drive(args, PREFIX, OPERATION, operate)
}

/// One operation on a preloaded input with the case argument.
pub fn operate(pool: &Budget, raw: &Bytes, argument: &str, work: Work) -> Result<Work> {
    let select = match argument {
        "aac" => Select::Aac,
        "dolby" => Select::Dolby,
        "all" => Select::All,
        _ => return Err("expected aac, dolby or all".into()),
    };
    operation(pool, raw, select, work)
}
