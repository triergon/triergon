use crate::framing::{annex_b, annex_b_session, ready};
use crate::support::{View, byte_input, collect, index_job, off, run_index, run_open};
use triergon_bridge::access::{AccessUnitLimits, AccessUnits};
use triergon_core::container::{CodecId, PacketInfo, TrackInfo};
use triergon_core::{Budget, SourceCapabilities, SourceId, WorkBudget};
use triergon_framing::BuildStep;
use triergon_isobmff::config::ConfigurationLimits;
use triergon_playback::{
    Engine, Event, Player, PlayerPolicy, Selection,
    mp4::{Mp4Event, Mp4Open, OpenLimits, PlanLimits},
};

const ID: SourceId = SourceId(173);

#[test]
fn access_units_frame_mp4_avc_as_annex_b_and_aac_as_adts() {
    let raw = include_bytes!("../../../playback/tests/fixtures/mp4/encoded-avc-aac-front.mp4");
    let pool = Budget::new(64 << 20).unwrap();
    let capabilities = SourceCapabilities {
        length: Some(raw.len() as u64),
        random_access: true,
    };
    let mut open = Mp4Open::new(
        byte_input(&pool, ID, raw, 4096, capabilities, 4096).unwrap(),
        OpenLimits::default(),
    )
    .unwrap();
    run_open(&mut open, 17).unwrap();
    let opened = open.finish().unwrap();
    let mut job = index_job(&opened.movie, View::Source, PlanLimits::default()).unwrap();
    run_index(&mut job, 17).unwrap();
    let mut plan = job.finish().unwrap();
    while !plan
        .identify_tracks(ConfigurationLimits::default(), &mut WorkBudget::new(1))
        .unwrap()
    {}
    let engine = Engine::mp4(opened.input, plan, 1 << 20).unwrap();
    let mut units = (0..engine.track_count())
        .map(|index| {
            let track = engine.track(index).unwrap();
            let mut setup =
                AccessUnits::for_track(&pool, &track, AccessUnitLimits::default()).unwrap();
            (
                track.id(),
                track.codec(),
                ready(|work| setup.step(work, &off()).unwrap()),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        units.iter().map(|(_, codec, _)| *codec).collect::<Vec<_>>(),
        [CodecId::H264, CodecId::Aac]
    );
    let mut player = Player::new(
        engine,
        opened.source,
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    let out = collect(&mut player, 17).unwrap();
    let mut session = None;
    let (mut video, mut audio) = (0, 0);
    for output in &out {
        let (Some(packet), Event::Mp4(Mp4Event::Packet(source))) =
            (output.packet(), output.event())
        else {
            continue;
        };
        let (_, codec, framing) = units
            .iter_mut()
            .find(|(id, _, _)| *id == packet.track())
            .unwrap();
        let mut job = framing.packet(&packet).unwrap();
        let unit = ready(|work| job.step(work, &off()).unwrap());
        assert_eq!(unit.track(), packet.track());
        assert_eq!(unit.pts(), packet.pts());
        assert_eq!(unit.dts(), packet.dts());
        assert_eq!(unit.duration(), packet.duration());
        assert_eq!(unit.time_base(), packet.time_base());
        assert_eq!(unit.random_access(), packet.random_access());
        if *codec == CodecId::H264 {
            let session = session.get_or_insert_with(|| annex_b_session(&pool, source));
            assert_eq!(
                unit.payload(),
                annex_b(session, source).bytes().as_ref(),
                "the session the sample entry's avcC configures"
            );
            video += 1;
        } else {
            let payload = packet.payload();
            let adts = unit.payload();
            assert_eq!(adts.len(), payload.len() + 7);
            assert_eq!((adts[0], adts[1] & 0xf6), (0xff, 0xf0));
            let length = (usize::from(adts[3] & 3) << 11)
                | (usize::from(adts[4]) << 3)
                | usize::from(adts[5] >> 5);
            assert_eq!(length, adts.len());
            assert_eq!(&adts[7..], payload);
            audio += 1;
        }
    }
    assert!(video > 0 && audio > 0);
}

#[test]
fn access_units_take_a_packet_of_a_track_without_a_record_as_it_is() {
    let raw = include_bytes!("../../../playback/tests/fixtures/mp4/encoded-avc-aac-front.mp4");
    let pool = Budget::new(64 << 20).unwrap();
    let capabilities = SourceCapabilities {
        length: Some(raw.len() as u64),
        random_access: true,
    };
    let mut open = Mp4Open::new(
        byte_input(&pool, ID, raw, 4096, capabilities, 4096).unwrap(),
        OpenLimits::default(),
    )
    .unwrap();
    run_open(&mut open, 17).unwrap();
    let opened = open.finish().unwrap();
    let mut job = index_job(&opened.movie, View::Source, PlanLimits::default()).unwrap();
    run_index(&mut job, 17).unwrap();
    // Without identification an `mp4a` track names no codec and no record.
    let engine = Engine::mp4(opened.input, job.finish().unwrap(), 1 << 20).unwrap();
    let audio = (0..engine.track_count())
        .map(|index| engine.track(index).unwrap())
        .find(|track| track.codec() == CodecId::Other)
        .unwrap();
    let mut setup = AccessUnits::for_track(&pool, &audio, AccessUnitLimits::default()).unwrap();
    let mut framing = ready(|work| setup.step(work, &off()).unwrap());
    assert!(matches!(
        setup.step(&mut WorkBudget::new(1), &off()).unwrap(),
        BuildStep::Complete
    ));
    let id = audio.id();
    let mut player = Player::new(
        engine,
        opened.source,
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    let out = collect(&mut player, 17).unwrap();
    let packet = out
        .iter()
        .filter_map(triergon_playback::Output::packet)
        .find(|packet| packet.track() == id)
        .unwrap();
    let mut job = framing.packet(&packet).unwrap();
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &off()).unwrap(),
        BuildStep::Yield
    ));
    let unit = ready(|work| job.step(work, &off()).unwrap());
    assert_eq!(unit.payload(), packet.payload());
    assert!(unit.framed().is_none());
    assert!(matches!(
        job.step(&mut WorkBudget::new(1), &off()).unwrap(),
        BuildStep::Complete
    ));
}
