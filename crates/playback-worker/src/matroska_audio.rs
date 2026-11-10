//! Matroska audio startup and forward clocks through the playback engine: open a
//! preloaded seekable input read in 4 KiB windows, discover the streams, bind every
//! selected AAC-LC, AC-3 or E-AC-3 track to its feed, project the discovery prefix
//! through `native_aac_clock_probe` or `native_dolby_clock_probe`, continue with the
//! forward clock through the end of file and fold every selected packet with its
//! native PTS, DTS and duration in nanoseconds.
//!
//! AAC packets pass through the ADTS feed before their clock; Dolby packets use the
//! source clock, so a syncframe the decoder would refuse keeps its packet and time.

use crate::matroska::{AVC_AAC, HEVC_DOLBY, WORK, ready};
use crate::packet_fold::{Packet, Work, drive};
use crate::runner::Result;
use std::sync::atomic::AtomicBool;
use triergon_core::container::{CodecId, MediaKind, TrackInfo};
use triergon_core::{Budget, Bytes, SourceCapabilities, SourceId, WorkBudget};
use triergon_matroska::{
    matroska::LegacyAacProfile,
    typed::{Configuration, Event, Packet as MatroskaPacket},
};
use triergon_playback::{
    ByteSource, Engine, Output, PlayerPolicy, Progress, RawEvent, Selection,
    io::SourceOwner,
    matroska_feed::{
        MatroskaClock, MatroskaClockPrefix, MatroskaCompactDiscovery, MatroskaDiscoveryLimits,
        MatroskaDiscoveryStep, MatroskaFeedSetup, MatroskaHevcDolbyFeed, MatroskaHevcDolbySetup,
        MatroskaPacketTiming, MatroskaTrackFeed,
    },
};

pub const PREFIX: &str = "mkv-audio-";
const OPERATION: &str = "matroska-audio-clock";
const QUANTUM: usize = 4096;
const MAX_TRACKS: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Select {
    Aac,
    Dolby,
    All,
}

/// The feed an audio track's codec family binds to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Aac,
    Dolby,
}

impl Select {
    /// Select the reader's codec family within the feed's supported profiles.
    fn family(
        self,
        kind: MediaKind,
        codec: CodecId<'_>,
        legacy_aac: Option<LegacyAacProfile>,
    ) -> Option<Family> {
        match (kind, codec) {
            (MediaKind::Audio, CodecId::Aac)
                if self != Self::Dolby
                    && matches!(legacy_aac, None | Some(LegacyAacProfile::LowComplexity)) =>
            {
                Some(Family::Aac)
            }
            (MediaKind::Audio, CodecId::Ac3 | CodecId::Eac3) if self != Self::Aac => {
                Some(Family::Dolby)
            }
            _ => None,
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

fn output_packet(output: &Output) -> Option<&MatroskaPacket> {
    let RawEvent::Matroska(event) = output.event() else {
        return None;
    };
    source_packet(event)
}

fn source_packet(event: &Event) -> Option<&MatroskaPacket> {
    match event {
        Event::Packet(packet) => Some(packet),
        _ => None,
    }
}

fn fold(work: &mut Work, packet: &MatroskaPacket, timing: MatroskaPacketTiming) -> Result<()> {
    let data = packet.packet.data.as_ref();
    work.packet(Packet {
        track: packet.packet.track as u64,
        pts: timing.pts_ns,
        dts: timing.dts_ns,
        duration: timing.duration_ns.unwrap_or(0),
        flags: u64::from(packet.packet.keyframe),
        size: data.len(),
        payload: [data],
    })
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
        feeds.push(
            match select.family(
                descriptor.kind(),
                descriptor.codec(),
                descriptor.legacy_aac_profile(),
            ) {
                None => None,
                Some(Family::Aac) => {
                    let mut setup =
                        MatroskaFeedSetup::new(pool, id, configuration, track, AVC_AAC)?;
                    Some(Feed::Aac(ready(|work| setup.step(work, &cancel))?))
                }
                Some(Family::Dolby) => {
                    let mut setup =
                        MatroskaHevcDolbySetup::new(pool, id, configuration, track, HEVC_DOLBY)?;
                    Some(Feed::Dolby(ready(|work| setup.step(work, &cancel))?))
                }
            },
        );
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
    let mut discovery = MatroskaCompactDiscovery::new(
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
        .events()
        .find_map(|event| match event {
            Event::Configuration(configuration) => Some(configuration),
            _ => None,
        })
        .ok_or("no Matroska configuration before the first packet")?;
    let feeds = feeds(pool, id, configuration, select)?;

    // The discovery prefix is projected as one unit per selected track before any
    // of its rows is delivered, as the native clock resolves earlier unknown times.
    let mut counts = vec![0_usize; feeds.len()];
    for packet in prefix.events().filter_map(source_packet) {
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
                for (ordinal, event) in prefix.events().enumerate() {
                    if source_packet(event).is_some_and(|p| p.packet.track == track) {
                        let mut job = prefix
                            .feed_packet(ordinal, feed)?
                            .ok_or("AAC feed packet")?;
                        probe.push(&ready(|work| job.step(work, &cancel))?)?;
                    }
                }
                Some(Prefix::Aac(probe.finish()))
            }
            Some(Feed::Dolby(feed)) => {
                let mut probe = feed
                    .native_dolby_clock_probe(pool, count)?
                    .ok_or("Dolby clock probe")?;
                for packet in prefix.events().filter_map(source_packet) {
                    if packet.packet.track == track {
                        probe.push_source(packet)?;
                    }
                }
                Some(Prefix::Dolby(probe.finish()))
            }
        });
    }

    let mut ordinals = vec![0_usize; feeds.len()];
    for packet in prefix.events().filter_map(source_packet) {
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
            && let Some(packet) = output_packet(&output)
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

#[cfg(test)]
mod tests {
    use super::{Family, Select};
    use triergon_core::container::{CodecId, MediaKind};
    use triergon_matroska::matroska::LegacyAacProfile;

    #[test]
    fn selection_follows_the_codec_family_the_reader_identifies() {
        let audio = MediaKind::Audio;
        for (select, aac, dolby) in [
            (Select::Aac, Some(Family::Aac), None),
            (Select::Dolby, None, Some(Family::Dolby)),
            (Select::All, Some(Family::Aac), Some(Family::Dolby)),
        ] {
            assert_eq!(select.family(audio, CodecId::Aac, None), aac);
            assert_eq!(
                select.family(audio, CodecId::Aac, Some(LegacyAacProfile::LowComplexity)),
                aac
            );
            for profile in [
                LegacyAacProfile::Main,
                LegacyAacProfile::ScalableSamplingRate,
                LegacyAacProfile::LongTermPrediction,
                LegacyAacProfile::LowComplexitySbr,
            ] {
                assert_eq!(select.family(audio, CodecId::Aac, Some(profile)), None);
            }
            // A_AC3, A_AC3/BSID9 and A_AC3/BSID10 are all AC-3.
            assert_eq!(select.family(audio, CodecId::Ac3, None), dolby);
            assert_eq!(select.family(audio, CodecId::Eac3, None), dolby);
            assert_eq!(select.family(MediaKind::Video, CodecId::Aac, None), None);
            assert_eq!(select.family(audio, CodecId::Opus, None), None);
            assert_eq!(
                select.family(audio, CodecId::Other(b"A_AAC/MPEG4/LC/UNKNOWN"), None),
                None
            );
        }
    }
}
