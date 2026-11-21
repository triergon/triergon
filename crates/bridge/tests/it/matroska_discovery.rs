use crate::support::sequential::*;
use crate::support::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use triergon_bridge::matroska as matroska_feed;
use triergon_core::{Bytes, SourceCapabilities, Time, WorkBudget};
use triergon_playback::{ByteSource, Source, SourceReply};
use triergon_playback::{SourceOwner, *};

#[test]
fn matroska_discovery_stops_on_canvas_or_limits_and_replays_the_same_source() {
    use matroska_feed::{
        MatroskaBitmapClockProbe, MatroskaBitmapTrack, MatroskaDiscovered, MatroskaDiscovery,
        MatroskaDiscoveryCutoff, MatroskaDiscoveryLimits, MatroskaDiscoveryStep,
    };
    use triergon_matroska::Event as MkvEvent;

    struct Counted {
        inner: ByteSource,
        bytes: Arc<AtomicUsize>,
        ready: Arc<AtomicBool>,
    }
    impl Source for Counted {
        fn poll(
            &mut self,
            request: &triergon_core::InputRequest,
            work: &mut WorkBudget,
            cancel: &AtomicBool,
        ) -> Result<SourceReply> {
            if !self.ready.load(Ordering::Relaxed) {
                return Ok(SourceReply::Pending);
            }
            let reply = self.inner.poll(request, work, cancel)?;
            if let SourceReply::Data(window) = &reply {
                self.bytes
                    .fetch_add(window.bytes().len(), Ordering::Relaxed);
            }
            Ok(reply)
        }
        fn invalidate(&mut self) {
            self.inner.invalidate();
        }
        fn abort(&mut self) {
            self.inner.abort();
        }
        fn close(&mut self) {
            self.inner.close();
        }
    }

    let early =
        include_bytes!("../../../playback/tests/fixtures/matroska-subtitle-probe/early-canvas.mkv");
    let mixed = include_bytes!(
        "../../../playback/tests/fixtures/matroska-subtitle-probe/observed-relative.mkv"
    );
    let limits = MatroskaDiscoveryLimits::default();
    let cases: [(
        &[u8],
        MatroskaDiscoveryLimits,
        MatroskaDiscoveryCutoff,
        usize,
        usize,
    ); 8] = [
        (early, limits, MatroskaDiscoveryCutoff::StreamsReady, 1, 5),
        (
            include_bytes!(
                "../../../playback/tests/fixtures/matroska-subtitle-probe/bad-first-canvas.mkv"
            ),
            limits,
            MatroskaDiscoveryCutoff::StreamsReady,
            2,
            6,
        ),
        (
            include_bytes!(
                "../../../playback/tests/fixtures/matroska-subtitle-probe/combined-leading.mkv"
            ),
            limits,
            MatroskaDiscoveryCutoff::StreamsReady,
            1,
            5,
        ),
        (
            include_bytes!(
                "../../../playback/tests/fixtures/matroska-subtitle-probe/packet-cap.mkv"
            ),
            MatroskaDiscoveryLimits {
                packet_bytes: 32,
                ..limits
            },
            MatroskaDiscoveryCutoff::PacketBytes,
            2,
            7,
        ),
        (
            early,
            MatroskaDiscoveryLimits {
                source_bytes: 16,
                ..limits
            },
            MatroskaDiscoveryCutoff::SourceBytes,
            0,
            5,
        ),
        (
            early,
            MatroskaDiscoveryLimits {
                outputs: 1,
                ..limits
            },
            MatroskaDiscoveryCutoff::OutputCount,
            0,
            5,
        ),
        (mixed, limits, MatroskaDiscoveryCutoff::SourceEnd, 5, 5),
        (mixed, limits, MatroskaDiscoveryCutoff::StreamsReady, 1, 5),
    ];
    let target = Some(TrackKey::Matroska(3));
    for (fixture, limits, cutoff, prefix_packets, total_packets) in cases {
        let report_video = std::ptr::eq(fixture, mixed.as_slice())
            && cutoff == MatroskaDiscoveryCutoff::StreamsReady;
        let pool = pool();
        let engine = Engine::matroska(
            triergon_matroska::Reader::new(
                &pool,
                ID,
                SourceCapabilities {
                    length: Some(fixture.len() as u64),
                    random_access: true,
                },
                triergon_matroska::Limits::default(),
                4096,
            )
            .unwrap(),
        );
        let transferred = Arc::new(AtomicUsize::new(0));
        let ready = Arc::new(AtomicBool::new(false));
        let source = Counted {
            inner: ByteSource::new(ID, Bytes::copy_from_slice(&pool, fixture).unwrap(), 7).unwrap(),
            bytes: transferred.clone(),
            ready: ready.clone(),
        };
        let mut discovery = MatroskaDiscovery::new(
            engine,
            SourceOwner::new(source),
            Selection::All,
            PlayerPolicy::default(),
            limits,
        )
        .unwrap();
        loop {
            match discovery.poll(&mut WorkBudget::new(32), &off()).unwrap() {
                MatroskaDiscoveryStep::Progress(Progress::Yield) => {}
                MatroskaDiscoveryStep::Progress(Progress::NeedInput(_)) => break,
                step => panic!("starvation became {step:?}"),
            }
        }
        assert_eq!(transferred.load(Ordering::Relaxed), 0);
        ready.store(true, Ordering::Relaxed);
        let mut observed = None;
        let mut confirmed = false;
        for _ in 0..10_000 {
            if let MatroskaDiscoveryStep::Ready(stop) =
                discovery.poll(&mut WorkBudget::new(1), &off()).unwrap()
            {
                observed = Some(stop);
                break;
            }
            if report_video
                && !confirmed
                && let Some(Event::Matroska(MkvEvent::Configuration(config))) =
                    discovery.buffered().last().map(Output::event)
            {
                let revision = config.revision();
                assert!(discovery.confirm_stream_ready(1, revision + 1).is_err());
                assert!(discovery.confirm_stream_ready(3, revision).is_err());
                discovery.confirm_stream_ready(1, revision).unwrap();
                confirmed = true;
            }
        }
        assert_eq!(confirmed, report_video);
        assert_eq!(observed, Some(cutoff));
        let at_cutoff = transferred.load(Ordering::Relaxed);
        assert!(at_cutoff <= limits.source_bytes);
        if cutoff == MatroskaDiscoveryCutoff::SourceBytes {
            assert_eq!(at_cutoff, limits.source_bytes);
        }
        let MatroskaDiscovered {
            mut player,
            prefix,
            cutoff: actual,
        } = discovery.finish().unwrap();
        assert_eq!(actual, cutoff);
        let prefix = prefix.as_slice();
        assert!(prefix.iter().all(|output| player.may_deliver(output)));
        assert_eq!(
            prefix
                .iter()
                .filter(|output| output.event().track() == target)
                .count(),
            prefix_packets
        );
        let rest = collect(&mut player, 1).unwrap();
        assert_eq!(transferred.load(Ordering::Relaxed), fixture.len());
        let all = prefix.iter().chain(&rest);
        assert_eq!(
            all.clone()
                .filter(|output| output.event().track() == target)
                .count(),
            total_packets
        );

        if prefix_packets != 0 {
            let config = prefix
                .iter()
                .find(|output| {
                    matches!(output.event(), Event::Matroska(MkvEvent::Configuration(_)))
                })
                .unwrap();
            let track = MatroskaBitmapTrack::new(config, 3).unwrap();
            let mut probe = MatroskaBitmapClockProbe::new(&track, prefix, limits.outputs).unwrap();
            while !probe.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
            let mut clocks: Vec<_> = prefix
                .iter()
                .enumerate()
                .filter_map(|(index, _)| {
                    probe
                        .packet(index, index as u64)
                        .unwrap()
                        .map(|packet| packet.clock.pts)
                })
                .collect();
            let mut live = probe.into_live().unwrap();
            clocks.extend(rest.iter().enumerate().filter_map(|(index, output)| {
                live.packet(output, (prefix.len() + index) as u64)
                    .unwrap()
                    .map(|packet| packet.clock.pts)
            }));
            if cutoff == MatroskaDiscoveryCutoff::SourceEnd {
                assert_eq!(
                    clocks,
                    [
                        Some(0),
                        Some(25_000_000),
                        Some(50_000_000),
                        Some(75_000_000),
                        Some(100_000_000)
                    ]
                );
            } else {
                assert_eq!(clocks.last(), Some(&Some(0)));
                assert!(clocks[..clocks.len() - 1].iter().all(Option::is_none));
            }
        }
        if cutoff == MatroskaDiscoveryCutoff::SourceEnd {
            assert!(
                player
                    .seek_and_select(
                        TrackKey::Matroska(1),
                        Time::new(0, 1).unwrap(),
                        Selection::All,
                        &mut WorkBudget::new(4096),
                        &off(),
                    )
                    .unwrap()
            );
            assert!(prefix.iter().all(|output| !player.may_deliver(output)));
        }
    }
}

#[test]
fn matroska_discovery_observes_timed_audio_without_caller_confirmation() {
    use matroska_feed::{
        MatroskaDiscovery, MatroskaDiscoveryCutoff, MatroskaDiscoveryLimits, MatroskaDiscoveryStep,
    };
    use triergon_matroska::Event as MkvEvent;

    let cases: [(&[u8], &[usize], usize); 2] = [
        (
            include_bytes!("../../../playback/tests/fixtures/mkv-feed/audio-mixed-discovery.mkv"),
            &[1, 0, 2, 3],
            21,
        ),
        (
            include_bytes!(
                "../../../playback/tests/fixtures/mkv-feed/audio-bad-first-discovery.mkv"
            ),
            &[0, 1],
            8,
        ),
    ];
    for (fixture, expected_prefix, expected_count) in cases {
        let pool = pool();
        let engine = Engine::matroska(
            triergon_matroska::Reader::new(
                &pool,
                ID,
                SourceCapabilities {
                    length: Some(fixture.len() as u64),
                    random_access: true,
                },
                triergon_matroska::Limits::default(),
                4096,
            )
            .unwrap(),
        );
        let source =
            ByteSource::new(ID, Bytes::copy_from_slice(&pool, fixture).unwrap(), 7).unwrap();
        let mut discovery = MatroskaDiscovery::new(
            engine,
            SourceOwner::new(source),
            Selection::All,
            PlayerPolicy::default(),
            MatroskaDiscoveryLimits::default(),
        )
        .unwrap();
        let cutoff = (0..100_000)
            .find_map(
                |_| match discovery.poll(&mut WorkBudget::new(1), &off()).unwrap() {
                    MatroskaDiscoveryStep::Ready(cutoff) => Some(cutoff),
                    MatroskaDiscoveryStep::Progress(_) => None,
                },
            )
            .expect("audio discovery stalled");
        assert_eq!(cutoff, MatroskaDiscoveryCutoff::StreamsReady);
        let mut done = discovery.finish().unwrap();
        let track = |output: &Output| match output.event() {
            Event::Matroska(MkvEvent::Packet(packet)) => {
                assert!(
                    triergon_matroska::compat::clock::packet_timing(packet.packet.timing)
                        .unwrap()
                        .pts
                        .is_some()
                );
                Some(packet.packet.track)
            }
            _ => None,
        };
        let prefix: Vec<_> = done.prefix.as_slice().iter().filter_map(track).collect();
        assert_eq!(prefix, expected_prefix);
        let rest = collect(&mut done.player, 1).unwrap();
        assert_eq!(
            prefix.len() + rest.iter().filter_map(track).count(),
            expected_count
        );
    }
}

#[test]
fn matroska_discovery_observes_video_and_keeps_decode_order() {
    use matroska_feed::{
        MatroskaDiscovery, MatroskaDiscoveryCutoff, MatroskaDiscoveryLimits, MatroskaDiscoveryStep,
    };
    use triergon_matroska::Event as MkvEvent;

    use MatroskaDiscoveryCutoff::{SourceEnd, StreamsReady};

    for (fixture, expected_cutoff, packets, pictures, total) in [
        (
            include_bytes!("../../../playback/tests/fixtures/mkv-feed/three-tracks.mkv").as_slice(),
            StreamsReady,
            89,
            8,
            101,
        ),
        (
            include_bytes!("../../../playback/tests/fixtures/mkv-feed/hevc-dolby.mkv").as_slice(),
            StreamsReady,
            5,
            3,
            52,
        ),
        (
            include_bytes!("../../../playback/tests/fixtures/mkv-feed/avc-reordered-discovery.mkv")
                .as_slice(),
            StreamsReady,
            3,
            3,
            12,
        ),
        (
            include_bytes!(
                "../../../playback/tests/fixtures/mkv-feed/hevc-no-default-duration-discovery.mkv"
            )
            .as_slice(),
            SourceEnd,
            12,
            12,
            12,
        ),
    ] {
        let pool = pool();
        let engine = Engine::matroska(
            triergon_matroska::Reader::new(
                &pool,
                ID,
                SourceCapabilities {
                    length: Some(fixture.len() as u64),
                    random_access: true,
                },
                triergon_matroska::Limits::default(),
                4096,
            )
            .unwrap(),
        );
        let source =
            ByteSource::new(ID, Bytes::copy_from_slice(&pool, fixture).unwrap(), 7).unwrap();
        let mut discovery = MatroskaDiscovery::new(
            engine,
            SourceOwner::new(source),
            Selection::All,
            PlayerPolicy::default(),
            MatroskaDiscoveryLimits::default(),
        )
        .unwrap();
        let cutoff = (0..500_000)
            .find_map(
                |_| match discovery.poll(&mut WorkBudget::new(1), &off()).unwrap() {
                    MatroskaDiscoveryStep::Ready(cutoff) => Some(cutoff),
                    MatroskaDiscoveryStep::Progress(_) => None,
                },
            )
            .expect("video discovery stalled");
        assert_eq!(cutoff, expected_cutoff, "{packets}");
        let mut done = discovery.finish().unwrap();
        let packet = |output: &Output| match output.event() {
            Event::Matroska(MkvEvent::Packet(packet)) => {
                Some((packet.packet.track, packet.packet.pts_ns))
            }
            _ => None,
        };
        let prefix: Vec<_> = done.prefix.as_slice().iter().filter_map(packet).collect();
        assert_eq!(prefix.len(), packets);
        let video: Vec<_> = prefix
            .iter()
            .filter_map(|(track, pts)| (*track == 0).then_some(*pts))
            .collect();
        assert_eq!(video.len(), pictures);
        if packets == 5 {
            assert_eq!(video, [Some(0), Some(250_000_000), Some(167_000_000)]);
        }
        let rest = collect(&mut done.player, 1).unwrap();
        assert_eq!(prefix.len() + rest.iter().filter_map(packet).count(), total);
    }
}
