use crate::support::sequential::*;
use crate::support::*;
use triergon_bridge::{bitmap as bitmap_bridge, text as text_bridge};
use triergon_core::{Budget, Bytes, Error as C, SourceCapabilities, Time, WorkBudget};
use triergon_playback::{ByteSource, mp4::Mp4Event};
use triergon_playback::{SourceOwner, mp4::ProgressiveLimits, *};

#[test]
fn tx3g_real_reader_handoff_retains_source_sample_and_exact_clock_after_teardown() {
    use text_bridge::*;
    use triergon_subtitles::text;
    let (mut p, b, _, _) = player(
        include_bytes!("../../../playback/tests/fixtures/mp4/r-subtitle-tx3g.mov"),
        false,
        ProgressiveLimits::default(),
    );
    let out = collect(&mut p, 1).unwrap();
    let output=out.into_iter().find(|o|matches!(o.event(),Event::Mp4(Mp4Event::Packet(p))if p.configuration.description.format==*b"tx3g")).unwrap();
    let source = TextSource::from_output(output).unwrap_or_else(|e| panic!("{:?}", e.error));
    let packet = source.mp4().unwrap();
    let raw = packet.source.payload.clone();
    let sample = packet.source.sample.clone();
    let generation = packet.source.token.generation();
    let track = packet.configuration.track.track_id;
    let mut bridge = TextBridge::new(
        &b,
        ID,
        u64::from(track),
        generation,
        text::Limits::default(),
    );
    bridge.provide(source).unwrap();
    let before = b.used();
    assert!(matches!(
        bridge.step(&mut WorkBudget::new(0), &off()).unwrap(),
        TextStep::Yield
    ));
    assert_eq!(b.used(), before);
    let result = loop {
        if let TextStep::Cue(o) = bridge.step(&mut WorkBudget::new(1), &off()).unwrap() {
            break o;
        }
    };
    p.stop();
    drop(p);
    drop(bridge);
    assert_eq!(
        result.source.mp4().unwrap().source.payload.as_ref(),
        raw.as_ref()
    );
    let cue = result.cue;
    assert_eq!(cue.input.payload.as_ref(), raw.as_ref());
    assert_eq!(
        cue.input.timing.raw_clock().unwrap().pts_ticks,
        sample.cts_ticks
    );
    assert_eq!(
        cue.input.container_position.unwrap().offset,
        sample.range.start
    );
}

#[test]
fn matroska_utf8_text_bridge_runs_real_reader_without_core_or_native_field_repair() {
    use text_bridge::*;
    use triergon_subtitles::text;
    let raw = include_bytes!("../../../playback/tests/fixtures/mkv/observed-11.mkv");
    let b = pool();
    let engine = Engine::matroska(
        triergon_matroska::Reader::new(
            &b,
            ID,
            SourceCapabilities {
                length: Some(raw.len() as u64),
                random_access: true,
            },
            triergon_matroska::Limits::default(),
            4096,
        )
        .unwrap(),
    );
    let mut p = Player::new(
        engine,
        SourceOwner::new(ByteSource::new(ID, Bytes::copy_from_slice(&b, raw).unwrap(), 7).unwrap()),
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    let out = collect(&mut p, 1).unwrap();
    let output=out.into_iter().find(|o|matches!(o.event(),Event::Matroska(triergon_matroska::Event::Packet(p))if p.configuration.tracks()[p.packet.track].codec.as_ref()=="S_TEXT/UTF8")).unwrap();
    assert_eq!(output.source_id(), ID);
    let source = TextSource::from_output(output).unwrap_or_else(|e| panic!("{:?}", e.error));
    let packet = source.matroska().unwrap();
    let expected = packet.packet.data.as_ref().to_vec();
    let pts = packet.source_pts().unwrap();
    let track = packet.configuration.tracks()[packet.packet.track].number;
    let mut bridge = TextBridge::new(
        &b,
        ID,
        track,
        packet.configuration.generation(),
        text::Limits::default(),
    );
    bridge.provide(source).unwrap();
    let result = loop {
        if let TextStep::Cue(o) = bridge.step(&mut WorkBudget::new(1), &off()).unwrap() {
            break o;
        }
    };
    let cue = result.cue;
    assert_eq!(cue.input.payload.as_ref(), expected);
    assert_eq!(cue.decoded_utf8.as_ref().unwrap().as_ref(), expected);
    assert_eq!(cue.start, pts);
    assert!(cue.input.timing.raw_clock().is_none());
    assert!(result.source.matroska().is_some());
}

#[test]
fn authored_ass_documents_keep_exact_source_bytes() {
    use text_bridge::*;
    use triergon_subtitles::text;
    let bytes = b"0,0,Default,,0,0,0,,{\\i1}raw\\Nbytes".as_slice();
    let b = pool();
    let payload = Bytes::copy_from_slice(&b, bytes).unwrap();
    let input = text::Input {
        format: text::Format::OpaqueAss,
        encoding: text::Encoding::Opaque,
        payload: payload.clone(),
        codec_id: None,
        codec_private: None,
        charset_label: None,
        addition: None,
        timing: text::Timing::unknown(),
        source_id: ID,
        generation: 7,
        sequence: 0,
        configuration_epoch: 0,
        container_position: None,
    };
    let mut bridge = TextBridge::new(&b, ID, 0, 7, text::Limits::default());
    bridge.provide(TextSource::document(&b, input)).unwrap();
    bridge.finish();
    let mut cues = 0;
    for _ in 0..100_000 {
        match bridge.step(&mut WorkBudget::new(1), &off()).unwrap() {
            TextStep::Cue(o) => {
                assert_eq!(o.cue.input.payload.as_ref(), bytes);
                assert!(o.cue.decoded_utf8.is_none());
                cues += 1;
            }
            TextStep::Finished => break,
            _ => {}
        }
    }
    assert_eq!(cues, 1);
    drop(bridge);
    drop(payload);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

fn bitmap_scope() -> bitmap_bridge::BitmapScope {
    bitmap_bridge::BitmapScope {
        source: ID,
        track: 3,
        generation: 7,
        configuration: 2,
    }
}

fn bitmap_input(b: &Budget, raw: &[u8]) -> bitmap_bridge::BitmapInput<&'static str> {
    bitmap_bridge::BitmapInput {
        scope: bitmap_scope(),
        sequence: 11,
        raw: Bytes::copy_from_slice(b, raw).unwrap(),
        clock: triergon_subtitles::pgs::PacketClock {
            pts: None,
            dts: None,
            timebase: triergon_core::TimeBase::new(1, 90_000).unwrap(),
        },
        duration: None,
        metadata: "original caller packet metadata",
    }
}

#[test]
fn pgs_compatibility_bridge_keeps_the_first_event_and_fences_packet_owners() {
    use bitmap_bridge::*;
    use triergon_subtitles::pgs;
    let b = pool();
    let raw =
        include_bytes!("../../../playback/tests/fixtures/bitmap/original-pgs-two-pixel.packet");
    let mut bridge = PgsBridge::new(
        &b,
        bitmap_scope(),
        pgs::DisplayLimits::default(),
        pgs::DisplayConfig::default(),
    )
    .unwrap();
    let cancel = off();
    let mut input = bitmap_input(&b, raw);
    input.clock.pts = Some(9_000);
    bridge
        .provide(input)
        .unwrap_or_else(|e| panic!("{:?}", e.error));
    let used = b.used();
    assert!(matches!(
        bridge.step(&mut WorkBudget::new(0), &cancel).unwrap(),
        PgsStep::Yield
    ));
    assert_eq!(b.used(), used);
    let retained = (0..100_000)
        .find_map(
            |_| match bridge.step(&mut WorkBudget::new(1), &cancel).unwrap() {
                PgsStep::Yield => None,
                PgsStep::Output(output) => Some(output),
                _ => panic!("expected native-only packet output"),
            },
        )
        .expect("bounded native-only output");
    let source = retained.packet.input();
    assert_eq!(source.sequence, 11);
    assert_eq!(source.clock.pts, Some(9_000));
    assert_eq!(source.clock.dts, None);
    assert!(
        source
            .raw
            .shares_storage_with(retained.decoded.raw_packet())
    );
    let event = retained.decoded.receiver_output().unwrap();
    assert_eq!(event.pts_microseconds, Some(100_000));
    assert_eq!(event.rectangles().len(), 1);
    assert_eq!(event.rectangles()[0].pixels().as_ref(), &[1, 2]);
    let Some(PgsQueueAction::Show(picture)) = retained.packet_clock_action().unwrap() else {
        panic!("PGS queue picture");
    };
    assert_eq!(picture.start_ns, 100_000_000);
    assert_eq!(picture.canvas, None);
    let rectangle = picture.rectangles().next().unwrap();
    assert!(std::ptr::eq(rectangle, &raw const event.rectangles()[0]));
    assert!(matches!(
        bridge.step(&mut WorkBudget::new(1), &cancel).unwrap(),
        PgsStep::NeedPacket
    ));

    let scope = BitmapScope {
        generation: 8,
        ..bitmap_scope()
    };
    assert!(
        bridge
            .reset(scope, &mut WorkBudget::new(1), &cancel)
            .unwrap()
    );
    let old = bitmap_input(&b, raw);
    let rejected = bridge.provide(old).err().unwrap();
    assert_eq!(
        rejected.error,
        C::StaleGeneration {
            expected: 8,
            received: 7
        }
    );
    drop(rejected);
    bridge.finish();
    assert!(matches!(
        bridge.step(&mut WorkBudget::new(8), &cancel).unwrap(),
        PgsStep::Finished
    ));
    drop(bridge);
    assert_eq!(retained.packet.input().scope, bitmap_scope());
    assert_eq!(
        retained.packet.input().metadata,
        "original caller packet metadata"
    );
    assert_eq!(
        retained.decoded.receiver_output().unwrap().rectangles()[0]
            .pixels()
            .as_ref(),
        &[1, 2]
    );
    drop(retained);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn pgs_packet_clock_queue_distinguishes_a_clear_from_no_output() {
    use bitmap_bridge::*;
    use triergon_subtitles::pgs;

    let b = pool();
    let cancel = off();
    let mut bridge = PgsBridge::new(
        &b,
        bitmap_scope(),
        pgs::DisplayLimits::default(),
        pgs::DisplayConfig::default(),
    )
    .unwrap();
    let pcs = [0x16, 0, 11, 7, 128, 4, 56, 0x10, 0, 1, 0x80, 0, 0, 0];
    for (index, packet) in [&pcs[..], &[0x80, 0, 0]].into_iter().enumerate() {
        let mut input = bitmap_input(&b, packet);
        input.sequence += index as u64;
        input.clock.dts = Some(18_000);
        bridge
            .provide(input)
            .unwrap_or_else(|e| panic!("{:?}", e.error));
        let output = loop {
            match bridge.step(&mut WorkBudget::new(32), &cancel).unwrap() {
                PgsStep::Yield => (),
                PgsStep::Output(output) => break output,
                _ => panic!("expected native-only packet output"),
            }
        };
        let action = output.packet_clock_action().unwrap();
        if index == 0 {
            assert!(action.is_none());
        } else {
            assert!(matches!(
                action,
                Some(PgsQueueAction::Clear { at_ns: 200_000_000 })
            ));
        }
        assert!(matches!(
            bridge.step(&mut WorkBudget::new(32), &cancel).unwrap(),
            PgsStep::NeedPacket
        ));
    }
}

#[test]
fn bitmap_foreign_pool_rejection_returns_original_packet_without_poisoning_session() {
    use bitmap_bridge::*;
    use triergon_subtitles::pgs;
    let b = pool();
    let foreign = pool();
    let raw =
        include_bytes!("../../../playback/tests/fixtures/bitmap/original-pgs-two-pixel.packet");
    let mut bridge = PgsBridge::new(
        &b,
        bitmap_scope(),
        pgs::DisplayLimits::default(),
        pgs::DisplayConfig::default(),
    )
    .unwrap();
    let input = bitmap_input(&foreign, raw);
    let payload = input.raw.clone();
    let error = bridge.provide(input).err().unwrap();
    assert_eq!(error.error, C::ForeignPool);
    assert!(error.input.raw.shares_storage_with(&payload));
    bridge
        .provide(bitmap_input(&b, raw))
        .unwrap_or_else(|e| panic!("{:?}", e.error));
    let before = b.used();
    assert!(matches!(
        bridge.step(&mut WorkBudget::new(0), &off()).unwrap(),
        PgsStep::Yield
    ));
    assert_eq!(b.used(), before);
    loop {
        if let PgsStep::Output(_) = bridge.step(&mut WorkBudget::new(1), &off()).unwrap() {
            break;
        }
    }
}

#[test]
fn bitmap_reset_uses_new_source_fence_and_keeps_old_output() {
    use bitmap_bridge::*;
    use triergon_subtitles::pgs;
    let b = pool();
    let raw =
        include_bytes!("../../../playback/tests/fixtures/bitmap/original-pgs-two-pixel.packet");
    let mut bridge = PgsBridge::new(
        &b,
        bitmap_scope(),
        pgs::DisplayLimits::default(),
        pgs::DisplayConfig::default(),
    )
    .unwrap();
    bridge
        .provide(bitmap_input(&b, raw))
        .unwrap_or_else(|e| panic!("{:?}", e.error));
    let old = loop {
        if let PgsStep::Output(o) = bridge.step(&mut WorkBudget::new(1), &off()).unwrap() {
            break o;
        }
    };
    let mut scope = bitmap_scope();
    scope.generation = 8;
    assert!(
        !bridge
            .reset(scope, &mut WorkBudget::new(0), &off())
            .unwrap()
    );
    assert!(
        bridge
            .reset(scope, &mut WorkBudget::new(1), &off())
            .unwrap()
    );
    let rejected = bridge.provide(bitmap_input(&b, raw)).err().unwrap();
    assert!(matches!(
        rejected.error,
        C::StaleGeneration {
            expected: 8,
            received: 7
        }
    ));
    let mut input = bitmap_input(&b, raw);
    input.scope = scope;
    bridge
        .provide(input)
        .unwrap_or_else(|e| panic!("{:?}", e.error));
    let next = loop {
        if let PgsStep::Output(o) = bridge.step(&mut WorkBudget::new(1), &off()).unwrap() {
            break o;
        }
    };
    assert_eq!(old.packet.input().scope.generation, 7);
    assert_eq!(next.packet.input().scope.generation, 8);
    let rectangles = old.decoded.receiver_output().unwrap().rectangles();
    assert_eq!(rectangles[0].pixels().as_ref(), &[1, 2]);
}

#[test]
fn dvd_receiver_presents_the_first_unit_of_a_packet_and_discards_the_tail() {
    use bitmap_bridge::*;
    let raw =
        include_bytes!("../../../playback/tests/fixtures/bitmap/original-dvd-two-units-tail.spu");
    let b = pool();
    let mut receiver = DvdReceiver::new(&b, bitmap_scope(), DvdBridgeConfig::default()).unwrap();
    assert!(matches!(
        receiver.step(&mut WorkBudget::new(0), &off()).unwrap(),
        DvdReceiverStep::NeedPacket
    ));
    receiver
        .provide(bitmap_input(&b, raw))
        .unwrap_or_else(|e| panic!("{:?}", e.error));
    let before = b.used();
    assert!(matches!(
        receiver.step(&mut WorkBudget::new(0), &off()).unwrap(),
        DvdReceiverStep::Yield
    ));
    // The decoder keeps a copy of the yielded unit to resume from.
    assert_eq!(b.used() - before, raw.len());
    let out = dvd_next(&mut receiver, 1).unwrap();
    assert_eq!(out.packet.input().raw.as_ref(), raw);
    assert_eq!(out.packet.input().clock.pts, None);
    let picture = out.picture.as_ref().unwrap();
    assert_eq!(
        (picture.x, picture.y, picture.width, picture.height),
        (0, 0, 1, 2)
    );
    assert_eq!((picture.start_ms, picture.end_ms), (193, 0));
    assert_eq!(
        picture.palette(),
        [0xff00_0000, 0xff54_5454, 0xffa9_a9a9, 0xfffe_fefe]
    );
    assert_eq!(
        (picture.row(0), picture.row(1), picture.row(2)),
        (Some(&[0][..]), Some(&[1][..]), None)
    );
    drop((out, receiver));
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn dvd_receiver_unit_limit_is_an_error_that_drops_the_packet() {
    use bitmap_bridge::*;
    use triergon_subtitles::compat::dvd::Limits;
    let b = pool();
    let raw =
        include_bytes!("../../../playback/tests/fixtures/bitmap/original-dvd-two-units-tail.spu");
    let config = DvdBridgeConfig {
        limits: Limits {
            max_unit: raw.len() - 1,
            ..Limits::default()
        },
        ..DvdBridgeConfig::default()
    };
    let mut receiver = DvdReceiver::new(&b, bitmap_scope(), config).unwrap();
    receiver
        .provide(bitmap_input(&b, raw))
        .unwrap_or_else(|e| panic!("{:?}", e.error));
    assert!(matches!(
        receiver.step(&mut WorkBudget::new(1), &off()),
        Err(C::Limit {
            requested: 62,
            maximum: 61
        })
    ));
    assert!(matches!(
        receiver.step(&mut WorkBudget::new(1), &off()).unwrap(),
        DvdReceiverStep::NeedPacket
    ));
    drop(receiver);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn dvd_presentation_preserves_precise_pts_and_explicit_clear() {
    use bitmap_bridge::*;
    use triergon_core::Time;

    let bytes =
        include_bytes!("../../../playback/tests/fixtures/bitmap/original-dvd-two-units-tail.spu");
    for (pts, dts, stop, start_ns) in [
        (Some(90_001), Some(-42), Some(90_u16), 1_000_011_111),
        (None, Some(-42), Some(0), -466_667),
        (Some(i64::MIN), Some(-42), None, -466_667),
        (None, Some(i64::MIN), None, 0),
        (Some(0), Some(-42), None, 0),
        (Some(i64::MAX), None, None, i64::MIN),
    ] {
        let b = pool();
        let mut raw = bytes[..30].to_vec();
        if let Some(date) = stop {
            raw[0..2].copy_from_slice(&36_u16.to_be_bytes());
            raw[8..10].copy_from_slice(&30_u16.to_be_bytes());
            raw.extend_from_slice(&date.to_be_bytes());
            raw.extend_from_slice(&[0, 30, 2, 0xff]);
        }
        let mut input = bitmap_input(&b, &raw);
        input.clock.pts = pts;
        input.clock.dts = dts;
        let mut config = DvdBridgeConfig::default();
        config.settings.canvas = Some([720, 480]);
        let mut receiver = DvdReceiver::new(&b, bitmap_scope(), config).unwrap();
        receiver
            .provide(input)
            .unwrap_or_else(|e| panic!("{:?}", e.error));
        let mut out = dvd_next(&mut receiver, 32).unwrap();
        let presentation = out.presentation().unwrap().unwrap();
        let at = |ms: u32| {
            pts.map(|pts| Time::new(i128::from(pts) + i128::from(ms) * 90, 90_000).unwrap())
        };
        assert_eq!(
            presentation.show,
            DvdDisplayBoundary {
                milliseconds: 193,
                at: at(193),
            }
        );
        let end = stop.map_or(0, |date| u32::from(date) * 1024 / 90);
        assert_eq!(
            presentation.clear,
            (end != 0).then(|| DvdDisplayBoundary {
                milliseconds: end,
                at: at(end),
            })
        );
        assert!(!presentation.picture.forced);
        assert_eq!(presentation.picture.row(1).unwrap(), &[1]);
        let queued = out.packet_clock_picture().unwrap().unwrap();
        assert_eq!(queued.canvas, Some([720, 480]));
        assert_eq!(queued.start_ns, start_ns);
        assert_eq!(queued.picture.palette(), presentation.picture.palette());
        // The queue places nothing on a canvas it cannot use.
        out.canvas = Some([159, 480]);
        assert_eq!(out.packet_clock_picture().unwrap().unwrap().canvas, None);
        drop((out, receiver));
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn text_document_foreign_pool_returns_original_source_and_generation_fence() {
    use text_bridge::*;
    use triergon_subtitles::text;
    let b = pool();
    let foreign = pool();
    let payload = Bytes::copy_from_slice(&foreign, b"exact source").unwrap();
    let input = text::Input {
        format: text::Format::Utf8Packet,
        encoding: text::Encoding::Utf8,
        payload: payload.clone(),
        codec_id: None,
        codec_private: None,
        charset_label: None,
        addition: None,
        timing: text::Timing::unknown(),
        source_id: ID,
        generation: 7,
        sequence: 0,
        configuration_epoch: 0,
        container_position: None,
    };
    let mut bridge = TextBridge::new(&b, ID, 0, 7, text::Limits::default());
    let error = bridge
        .provide(TextSource::document(&b, input))
        .err()
        .unwrap();
    assert!(matches!(error.error, C::ForeignPool));
    drop(error);
    drop(payload);
    assert_eq!(foreign.used(), Budget::control_storage_bytes());
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn dvd_stop_only_packet_produces_no_packet_clock_picture() {
    use bitmap_bridge::*;

    let b = pool();
    let raw = [0, 10, 0, 4, 0, 0, 0, 4, 2, 0xff];
    let mut receiver = DvdReceiver::new(&b, bitmap_scope(), DvdBridgeConfig::default()).unwrap();
    receiver
        .provide(bitmap_input(&b, &raw))
        .unwrap_or_else(|e| panic!("{:?}", e.error));
    let out = dvd_next(&mut receiver, 32).unwrap();
    assert!(out.picture.is_none());
    assert!(out.packet_clock_picture().unwrap().is_none());
}

#[test]
fn dvd_receiver_carries_colours_across_packets_and_fences_reset() {
    use bitmap_bridge::*;

    let b = pool();
    let scope = bitmap_scope();
    let fixture =
        include_bytes!("../../../playback/tests/fixtures/bitmap/original-dvd-two-units-tail.spu");
    let first = &fixture[..30];
    // The same unit without its colour and contrast commands.
    let mut inherited = [first[..11].as_ref(), &first[17..]].concat();
    let size = u16::try_from(inherited.len()).unwrap();
    inherited[..2].copy_from_slice(&size.to_be_bytes());

    let mut receiver = DvdReceiver::new(&b, scope, DvdBridgeConfig::default()).unwrap();
    receiver
        .provide(bitmap_input(&b, first))
        .unwrap_or_else(|error| panic!("{:?}", error.error));
    let rejected = receiver.provide(bitmap_input(&b, first)).err().unwrap();
    assert_eq!(rejected.input.raw.as_ref(), first);
    drop(rejected);

    // Memory pressure keeps the packet pending and the decoder unchanged.
    let pressure = b.reserve(b.available()).unwrap();
    assert!(matches!(
        dvd_next(&mut receiver, 1),
        Err(C::Backpressure { .. })
    ));
    drop(pressure);
    let first = dvd_next(&mut receiver, 1).unwrap();
    let palette = first.picture.as_ref().unwrap().palette().to_vec();
    assert!(receiver.provide(bitmap_input(&b, &inherited)).is_err());

    let mut malformed = bitmap_input(&b, &[0, 6, 0, 4, 0xff, 0xff]);
    malformed.sequence = 12;
    receiver
        .provide(malformed)
        .unwrap_or_else(|error| panic!("{:?}", error.error));
    assert!(dvd_next(&mut receiver, 1).unwrap().picture.is_none());

    let mut second = bitmap_input(&b, &inherited);
    second.sequence = 13;
    second.clock.pts = Some(90_000);
    receiver
        .provide(second)
        .unwrap_or_else(|error| panic!("{:?}", error.error));
    receiver.finish();
    let second = dvd_next(&mut receiver, 1).unwrap();
    assert_eq!(second.picture.as_ref().unwrap().palette(), palette);
    assert_eq!(
        second.presentation().unwrap().unwrap().show.at,
        Some(Time::new(1193, 1000).unwrap())
    );
    assert!(matches!(
        receiver.step(&mut WorkBudget::new(1), &off()).unwrap(),
        DvdReceiverStep::Finished
    ));

    let mut new_scope = scope;
    new_scope.generation += 1;
    let config = DvdBridgeConfig::default();
    assert!(
        !receiver
            .reset(new_scope, config.clone(), &mut WorkBudget::new(0), &off())
            .unwrap()
    );
    assert!(
        receiver
            .reset(new_scope, config, &mut WorkBudget::new(1), &off())
            .unwrap()
    );
    let stale = receiver
        .provide(bitmap_input(&b, &inherited))
        .err()
        .unwrap();
    assert!(matches!(stale.error, C::StaleGeneration { .. }));
    assert_eq!(stale.input.raw.as_ref(), inherited);
    drop(stale);
    // A reset forgets the carried colours: the inherited unit is transparent.
    let mut fresh = bitmap_input(&b, &inherited);
    fresh.scope = new_scope;
    receiver
        .provide(fresh)
        .unwrap_or_else(|error| panic!("{:?}", error.error));
    let fresh = dvd_next(&mut receiver, 1).unwrap();
    assert!(fresh.picture.is_none());
    assert_eq!(first.packet.input().scope, scope);
    assert_eq!(second.packet.input().scope, scope);
    assert_eq!(fresh.packet.input().scope, new_scope);
    drop((first, second, fresh, receiver));
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

/// Step a DVD receiver until it outputs the pending packet.
pub(crate) fn dvd_next<M>(
    receiver: &mut bitmap_bridge::DvdReceiver<M>,
    quantum: u64,
) -> Result<bitmap_bridge::DvdOutput<M>> {
    for _ in 0..100_000 {
        match receiver.step(&mut WorkBudget::new(quantum), &off())? {
            bitmap_bridge::DvdReceiverStep::Yield => {}
            bitmap_bridge::DvdReceiverStep::Output(output) => return Ok(output),
            _ => panic!("DVD receiver left the admitted packet"),
        }
    }
    panic!("DVD receiver exhausted work limit");
}
