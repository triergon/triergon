use crate::subtitle_receivers::dvd_next;
use crate::support::sequential::*;
use crate::support::*;
use std::sync::atomic::AtomicBool;
use triergon_bridge::{bitmap as bitmap_bridge, matroska as matroska_feed, ts::dvb as dvb_ts};
use triergon_core::{Bytes, Error as C, SourceCapabilities, Time, WorkBudget};
use triergon_playback::ByteSource;
use triergon_playback::{SourceOwner, *};

#[test]
fn matroska_bitmap_tracks_feed_native_receivers_without_copying_frames() {
    use bitmap_bridge::*;
    use matroska_feed::{
        MatroskaBitmapClockProbe, MatroskaBitmapCodec, MatroskaBitmapTrack, MatroskaDvbStep,
    };
    use triergon_matroska::Event as MkvEvent;
    use triergon_subtitles::{dvb, pgs};

    fn element(id: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        if data.len() < 127 {
            out.push(0x80 | data.len() as u8);
        } else {
            assert!(data.len() < 16_383);
            out.extend_from_slice(&(0x4000 | data.len() as u16).to_be_bytes());
        }
        out.extend_from_slice(data);
        out
    }
    fn track(number: u8, kind: u8, codec: &[u8], extra: &[u8]) -> Vec<u8> {
        element(
            &[0xae],
            &[
                element(&[0xd7], &[number]),
                element(&[0x73, 0xc5], &[number]),
                element(&[0x83], &[kind]),
                element(&[0x86], codec),
                extra.to_vec(),
            ]
            .concat(),
        )
    }
    fn block(number: u8, delta: i16, data: &[u8]) -> Vec<u8> {
        element(
            &[0xa3],
            &[&[0x80 | number], &delta.to_be_bytes()[..], &[0x80], data].concat(),
        )
    }
    fn group(number: u8, delta: i16, duration: u8, data: &[u8]) -> Vec<u8> {
        let payload = [&[0x80 | number], &delta.to_be_bytes()[..], &[0], data].concat();
        element(
            &[0xa0],
            &[element(&[0xa1], &payload), element(&[0x9b], &[duration])].concat(),
        )
    }

    let pgs_frame =
        include_bytes!("../../../playback/tests/fixtures/bitmap/original-pgs-two-pixel.packet");
    let mut remaining = pgs_frame.as_slice();
    let mut pgs_segments = Vec::new();
    let mut pgs_blocks = Vec::new();
    while !remaining.is_empty() {
        let length = usize::from(u16::from_be_bytes([remaining[1], remaining[2]])) + 3;
        let (segment, tail) = remaining.split_at(length);
        pgs_segments.push(segment);
        pgs_blocks.extend(if tail.is_empty() {
            group(3, -5, 25, segment)
        } else {
            block(3, -5, segment)
        });
        remaining = tail;
    }
    let dvd_frame =
        &include_bytes!("../../../playback/tests/fixtures/bitmap/original-dvd-two-units-tail.spu")
            [..30];
    let pes = include_bytes!("../../../mpegts/tests/data/dvb/dvb2-literals-fields.pes-data");
    let dvb_frame = &pes[2..pes.len() - 1];
    // A new page version without regions: a mode change that clears the page.
    let dvb_clear = [0x0f, 0x10, 0, 1, 0, 2, 2, 0x1b, 0x0f, 0x80, 0, 1, 0, 0];
    let dvb_private = [0, 1, 0, 2, 0x10];
    let palette = b"size: 720x480\npalette: 112233, 445566, 778899, 000000, 000000, 000000, 000000, 000000, 000000, 000000, 000000, 000000, 000000, 000000, 000000, 000000\n";
    let adler = dvd_frame.iter().fold((1_u32, 0_u32), |(a, b), &byte| {
        let a = (a + u32::from(byte)) % 65_521;
        (a, (b + a) % 65_521)
    });
    let mut compressed = vec![0x78, 1, 1];
    let len = dvd_frame.len() as u16;
    compressed.extend_from_slice(&len.to_le_bytes());
    compressed.extend_from_slice(&(!len).to_le_bytes());
    compressed.extend_from_slice(dvd_frame);
    compressed.extend_from_slice(&((adler.1 << 16) | adler.0).to_be_bytes());
    let zlib = element(
        &[0x6d, 0x80],
        &element(&[0x62, 0x40], &element(&[0x50, 0x34], &[])),
    );
    let data = [
        element(
            &[0x1a, 0x45, 0xdf, 0xa3],
            &element(&[0x42, 0x82], b"matroska"),
        ),
        element(
            &[0x18, 0x53, 0x80, 0x67],
            &[
                element(
                    &[0x16, 0x54, 0xae, 0x6b],
                    &[
                        track(1, 1, b"V_VP9", &[]),
                        track(3, 17, b"S_HDMV/PGS", &[]),
                        track(
                            7,
                            17,
                            b"S_VOBSUB",
                            &[element(&[0x63, 0xa2], palette), zlib].concat(),
                        ),
                        track(8, 17, b"S_DVBSUB", &element(&[0x63, 0xa2], &dvb_private)),
                        track(
                            9,
                            17,
                            b"S_DVBSUB",
                            &element(&[0x63, 0xa2], &dvb_private[..4]),
                        ),
                    ]
                    .concat(),
                ),
                element(
                    &[0x1f, 0x43, 0xb6, 0x75],
                    &[
                        element(&[0xe7], &[4]),
                        block(1, 0, &[0]),
                        pgs_blocks,
                        block(7, 5, &compressed),
                        group(8, 15, 42, dvb_frame),
                        block(8, 90, &dvb_clear),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        ),
    ]
    .concat();
    let pool = pool();
    let engine = Engine::matroska(
        triergon_matroska::Reader::new(
            &pool,
            ID,
            SourceCapabilities {
                length: Some(data.len() as u64),
                random_access: true,
            },
            triergon_matroska::Limits::default(),
            4096,
        )
        .unwrap(),
    );
    let source = ByteSource::new(ID, Bytes::copy_from_slice(&pool, &data).unwrap(), 7).unwrap();
    let mut player = Player::new(
        engine,
        SourceOwner::new(source),
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    let outputs = collect(&mut player, 1).unwrap();
    let configuration = outputs
        .iter()
        .find(|o| matches!(o.event(), Event::Matroska(MkvEvent::Configuration(_))))
        .unwrap();
    let pgs_track = MatroskaBitmapTrack::new(configuration, 3).unwrap();
    let dvd_track = MatroskaBitmapTrack::new(configuration, 7).unwrap();
    let dvb_track = MatroskaBitmapTrack::new(configuration, 8).unwrap();
    assert_eq!(pgs_track.codec(), MatroskaBitmapCodec::Pgs);
    assert_eq!(dvd_track.codec(), MatroskaBitmapCodec::VobSub);
    assert_eq!(dvb_track.codec(), MatroskaBitmapCodec::Dvb);
    assert!(MatroskaBitmapTrack::new(configuration, 1).is_err());
    assert!(matches!(
        MatroskaBitmapTrack::new(configuration, 9),
        Err(C::Truncated)
    ));

    let dvd_input = outputs
        .iter()
        .find_map(|o| dvd_track.packet(o, 2).unwrap())
        .unwrap();
    let check_alias = |input: &BitmapInput<matroska_feed::MatroskaBitmapSource>, frame: &[u8]| {
        assert_eq!(&*input.raw, frame);
        assert!(
            input.raw.shares_storage_with(
                &input
                    .metadata
                    .packet
                    .packet
                    .data
                    .shared_bytes()
                    .unwrap()
                    .unwrap()
            )
        );
    };
    check_alias(&dvd_input, dvd_frame);
    assert_eq!(dvd_input.clock.pts, Some(9_000_000));
    assert_eq!(dvd_input.metadata.delivery, Delivery::Present);

    let mut pgs = PgsBridge::new(
        &pool,
        pgs_track.scope(),
        pgs::DisplayLimits::default(),
        pgs::DisplayConfig::default(),
    )
    .unwrap();
    let mut pgs_count = 0;
    for (index, output) in outputs.iter().enumerate() {
        let Some(input) = pgs_track.packet(output, index as u64).unwrap() else {
            continue;
        };
        check_alias(&input, pgs_segments[pgs_count]);
        pgs_count += 1;
        // The queue sees unknown PTS, while the owner keeps negative source time.
        assert_eq!(input.clock.pts, None);
        assert_eq!(
            input.metadata.packet.source_pts().unwrap(),
            Some(Time::new(-1, 1000).unwrap())
        );
        pgs.provide(input)
            .unwrap_or_else(|e| panic!("{:?}", e.error));
        let mut showed = false;
        loop {
            match pgs.step(&mut WorkBudget::new(1), &off()).unwrap() {
                PgsStep::Yield => {}
                PgsStep::Output(output) => {
                    if let Some(PgsQueueAction::Show(picture)) =
                        output.packet_clock_action().unwrap()
                    {
                        assert_eq!(picture.start_ns, 0);
                        assert_eq!(output.packet.input().duration, Some(25_000_000));
                        assert_eq!(pgs_count, pgs_segments.len());
                        showed = true;
                    }
                }
                PgsStep::NeedPacket => break,
                PgsStep::Finished => panic!("unexpected PGS receiver state"),
            }
        }
        assert_eq!(showed, pgs_count == pgs_segments.len());
    }
    assert_eq!(pgs_count, pgs_segments.len());

    assert!(pgs_track.vobsub_settings().is_err());
    let settings = dvd_track.vobsub_settings().unwrap();
    assert_eq!(settings.palette.unwrap()[0], 0x112233);
    assert_eq!(settings.canvas, Some([720, 480]));
    let mut dvd = DvdReceiver::new(
        &pool,
        dvd_track.scope(),
        DvdBridgeConfig {
            settings,
            ..DvdBridgeConfig::default()
        },
    )
    .unwrap();
    dvd.provide(dvd_input)
        .unwrap_or_else(|e| panic!("{:?}", e.error));
    let dvd_output = dvd_next(&mut dvd, 1).unwrap();
    let queued = dvd_output.packet_clock_picture().unwrap().unwrap();
    assert_eq!(queued.start_ns, 9_000_000);
    assert_eq!(queued.canvas, Some([720, 480]));
    assert_eq!(queued.picture.row(1).unwrap(), &[1]);

    let config = dvb_track.dvb_configuration().unwrap();
    assert_eq!(&**config.original(), &dvb_private);
    assert_eq!(config.service().composition_page, 1);
    assert_eq!(config.service().ancillary_page, 2);
    let mut decoder = dvb_track.dvb_decoder().unwrap();
    let mut dvb_clock = MatroskaBitmapClockProbe::new(&dvb_track, &outputs, outputs.len()).unwrap();
    while !dvb_clock.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let mut dvb_actions = Vec::new();
    let mut dvb_inputs = Vec::new();
    for (index, output) in outputs.iter().enumerate() {
        let Some(mut input) = dvb_clock.dvb_packet(index, index as u64).unwrap() else {
            continue;
        };
        let raw = dvb_track.dvb_packet(output, index as u64).unwrap().unwrap();
        assert_eq!(raw.packet.pts, input.packet.pts);
        assert!(raw.packet.source.unwrap().timing().dts.is_none());
        let source = input.packet.source.as_ref().unwrap();
        let timing = source.timing();
        assert_eq!(timing.pts, timing.dts);
        let data = input
            .source
            .packet
            .packet
            .data
            .shared_bytes()
            .unwrap()
            .unwrap();
        assert!(input.packet.payload.shares_storage_with(&data));
        assert_eq!(input.packet.envelope, dvb::PacketEnvelope::Segments);
        if input.packet.pts == Some(Time::new(19, 1000).unwrap()) {
            assert_eq!(timing.duration.unwrap().ticks, 42_000_000);
            assert_eq!(source.container_position().unwrap().source, ID);
        }
        let mut work = WorkBudget::new(u64::MAX);
        match input.decode(&mut decoder, &mut work, &off()).unwrap() {
            MatroskaDvbStep::Action(dvb_ts::DvbQueueAction::Show(picture)) => {
                assert_eq!(picture.start_ns, 19_000_000);
                assert_eq!(
                    picture.rectangles().next().unwrap().pixels,
                    include_bytes!(
                        "../../../subtitles/tests/boundary-fixtures/dvb2-literals-fields.indices"
                    )
                );
                dvb_actions.push("show");
                // The same packet as preroll updates the decoder without an action.
                input.source.delivery = Delivery::DecodeOnly {
                    until: Time::new(20, 1000).unwrap(),
                };
                let mut preroll = dvb_track.dvb_decoder().unwrap();
                let mut work = WorkBudget::new(u64::MAX);
                assert!(matches!(
                    input.decode(&mut preroll, &mut work, &off()).unwrap(),
                    MatroskaDvbStep::Decoded
                ));
            }
            MatroskaDvbStep::Action(dvb_ts::DvbQueueAction::Clear { at_ns }) => {
                assert_eq!(at_ns, 94_000_000);
                dvb_actions.push("clear");
            }
            step => panic!("DVB packet was not queued: {step:?}"),
        }
        dvb_inputs.push(input);
    }
    assert_eq!(dvb_actions, ["show", "clear"]);
    // A packet the decoder keeps after a cancellation is resumed only by
    // itself: the next packet drops it and is decoded on its own.
    let [shown, cleared] = &dvb_inputs[..] else {
        panic!("two DVB packets")
    };
    let mut decoder = dvb_track.dvb_decoder().unwrap();
    assert!(matches!(
        shown.decode(
            &mut decoder,
            &mut WorkBudget::new(u64::MAX),
            &AtomicBool::new(true)
        ),
        Err(Error::Cancelled)
    ));
    assert!(decoder.has_kept_packet());
    assert!(matches!(
        cleared
            .decode(&mut decoder, &mut WorkBudget::new(u64::MAX), &off())
            .unwrap(),
        MatroskaDvbStep::Action(dvb_ts::DvbQueueAction::Clear { at_ns: 94_000_000 })
    ));
    assert!(!decoder.has_kept_packet());

    let select = Selection::Tracks {
        video: Some(TrackKey::Matroska(1)),
        audio: None,
        subtitle: Some(TrackKey::Matroska(7)),
    };
    let target = Time::new(10, 1000).unwrap();
    assert!(
        player
            .seek_and_select(
                TrackKey::Matroska(1),
                target,
                select,
                &mut WorkBudget::new(4096),
                &off(),
            )
            .unwrap()
    );
    let after_seek = collect(&mut player, 1).unwrap();
    let next_dvd = after_seek
        .iter()
        .find(|output| output.event().track() == Some(TrackKey::Matroska(7)))
        .unwrap();
    assert!(matches!(
        dvd_track.packet(next_dvd, 3),
        Err(C::StaleGeneration { .. })
    ));
    let next_track = MatroskaBitmapTrack::new(next_dvd, 7).unwrap();
    let old_dvd = outputs
        .iter()
        .find(|output| output.event().track() == Some(TrackKey::Matroska(7)))
        .unwrap();
    assert!(matches!(
        next_track.packet(old_dvd, 3),
        Err(C::StaleGeneration { .. })
    ));
    let next_input = next_track.packet(next_dvd, 3).unwrap().unwrap();
    assert_eq!(next_input.clock.pts, Some(9_000_000));
    assert_eq!(
        next_input.metadata.delivery,
        Delivery::DecodeOnly { until: target }
    );
    assert!(after_seek.iter().all(|output| {
        pgs_track
            .packet(output, 4)
            .is_ok_and(|input| input.is_none())
    }));
}

#[test]
fn matroska_bitmap_probe_matches_ffmpeg_startup_clocks() {
    use bitmap_bridge::{PgsBridge, PgsQueueAction, PgsStep};
    use matroska_feed::{MatroskaBitmapClockProbe, MatroskaBitmapTrack};
    use triergon_matroska::Event as MkvEvent;
    use triergon_subtitles::pgs;

    type Timing = (Option<i64>, Option<i64>);
    let timed = |pts, duration| (Some(pts), Some(duration));
    let cases: [(&[u8], [Timing; 5]); 7] = [
        (
            include_bytes!(
                "../../../playback/tests/fixtures/matroska-subtitle-probe/observed-relative.mkv"
            ),
            [
                timed(0, 25),
                timed(25, 25),
                timed(50, 25),
                timed(75, 25),
                timed(100, 25),
            ],
        ),
        (
            include_bytes!(
                "../../../playback/tests/fixtures/matroska-subtitle-probe/known-anchor.mkv"
            ),
            [
                timed(100, 25),
                timed(125, 25),
                timed(150, 25),
                timed(175, 25),
                timed(200, 25),
            ],
        ),
        (
            include_bytes!(
                "../../../playback/tests/fixtures/matroska-subtitle-probe/duration-after-known.mkv"
            ),
            [
                timed(150, 25),
                timed(175, 25),
                (Some(200), None),
                (Some(201), None),
                timed(202, 25),
            ],
        ),
        (
            include_bytes!(
                "../../../playback/tests/fixtures/matroska-subtitle-probe/all-unknown.mkv"
            ),
            [(None, None); 5],
        ),
        (
            include_bytes!(
                "../../../playback/tests/fixtures/matroska-subtitle-probe/running-relative.mkv"
            ),
            [
                timed(140, 10),
                timed(150, 20),
                (None, None),
                timed(170, 30),
                (Some(200), None),
            ],
        ),
        (
            include_bytes!(
                "../../../playback/tests/fixtures/matroska-subtitle-probe/known-then-duration.mkv"
            ),
            [
                timed(75, 15),
                timed(90, 15),
                timed(105, 15),
                (None, None),
                timed(120, 20),
            ],
        ),
        (
            include_bytes!(
                "../../../playback/tests/fixtures/matroska-subtitle-probe/known-zero.mkv"
            ),
            [
                timed(0, 15),
                timed(15, 15),
                timed(30, 15),
                (None, None),
                timed(125, 30),
            ],
        ),
    ];

    let read_clock = |input: &bitmap_bridge::BitmapInput<matroska_feed::MatroskaBitmapSource>| {
        assert_eq!(input.clock.pts, input.clock.dts);
        (
            input.clock.pts.map(|pts| pts / 1_000_000),
            input.duration.map(|duration| duration / 1_000_000),
        )
    };

    for (case, (fixture, expected)) in cases.into_iter().enumerate() {
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
        let mut player = Player::new(
            engine,
            SourceOwner::new(source),
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
        let outputs = collect(&mut player, 1).unwrap();
        let config = outputs
            .iter()
            .find(|output| matches!(output.event(), Event::Matroska(MkvEvent::Configuration(_))))
            .unwrap();
        let track = MatroskaBitmapTrack::new(config, 3).unwrap();
        let target = Some(TrackKey::Matroska(3));
        assert!(matches!(
            MatroskaBitmapClockProbe::new(&track, &outputs, outputs.len() - 1),
            Err(C::Limit { .. })
        ));
        let mut probe = MatroskaBitmapClockProbe::new(&track, &outputs, outputs.len()).unwrap();
        assert!(!probe.poll(&mut WorkBudget::new(0), &off()).unwrap());
        assert!(probe.packet(0, 0).is_err());
        assert!(matches!(
            probe.poll(&mut WorkBudget::new(128), &AtomicBool::new(true)),
            Err(C::Cancelled)
        ));
        let pressure = pool.reserve(pool.limit() - pool.used()).unwrap();
        assert!(matches!(
            probe.poll(&mut WorkBudget::new(4096), &off()),
            Err(C::Backpressure { .. })
        ));
        drop(pressure);
        while !probe.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
        let mut receiver = (case == 0).then(|| {
            PgsBridge::new(
                &pool,
                track.scope(),
                pgs::DisplayLimits::default(),
                pgs::DisplayConfig::default(),
            )
            .unwrap()
        });
        let mut clocks = Vec::new();
        let mut shown_at = Vec::new();
        for (index, output) in outputs.iter().enumerate() {
            if let Some(projected) = probe.packet(index, index as u64).unwrap() {
                let original = track.packet(output, index as u64).unwrap().unwrap();
                assert!(projected.raw.shares_storage_with(&original.raw));
                assert_eq!(
                    projected.metadata.packet.source_pts().unwrap(),
                    original.metadata.packet.source_pts().unwrap(),
                );
                assert_eq!(projected.metadata.delivery, original.metadata.delivery);
                clocks.push(read_clock(&projected));
                if let Some(receiver) = &mut receiver {
                    receiver
                        .provide(projected)
                        .unwrap_or_else(|failure| panic!("{:?}", failure.error));
                    loop {
                        match receiver.step(&mut WorkBudget::new(1), &off()).unwrap() {
                            PgsStep::Yield => {}
                            PgsStep::Output(event) => {
                                if let Some(PgsQueueAction::Show(picture)) =
                                    event.packet_clock_action().unwrap()
                                {
                                    shown_at.push(picture.start_ns);
                                }
                            }
                            PgsStep::NeedPacket => break,
                            PgsStep::Finished => panic!("unexpected PGS receiver state"),
                        }
                    }
                }
            } else {
                assert_ne!(output.event().track(), target);
            }
        }
        assert_eq!(clocks, expected, "startup clock case {case}");
        if case == 0 {
            assert_eq!(shown_at, [100_000_000]);
        }

        if case == 4 {
            // Replay a shorter probe, then deliver the remainder in order.
            // A future anchor cannot move clocks that have reached the queue.
            let split = outputs
                .iter()
                .enumerate()
                .filter(|(_, output)| output.event().track() == target)
                .nth(2)
                .unwrap()
                .0;
            let (prefix, live) = outputs.split_at(split);
            let mut probe = MatroskaBitmapClockProbe::new(&track, prefix, prefix.len()).unwrap();
            assert!(probe.poll(&mut WorkBudget::new(128), &off()).unwrap());
            let first: Vec<_> = prefix
                .iter()
                .enumerate()
                .filter_map(|(index, _)| {
                    probe
                        .packet(index, index as u64)
                        .unwrap()
                        .as_ref()
                        .map(read_clock)
                })
                .collect();
            assert_eq!(first, [timed(0, 10), timed(10, 20)]);
            let mut clock = probe.into_live().unwrap();
            let rest: Vec<_> = live
                .iter()
                .enumerate()
                .filter_map(|(index, output)| {
                    clock
                        .packet(output, (split + index) as u64)
                        .unwrap()
                        .as_ref()
                        .map(read_clock)
                })
                .collect();
            assert_eq!(rest, [(None, None), timed(30, 30), (Some(200), None)]);
        }
    }
}
