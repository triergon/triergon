use crate::support::players::*;
use crate::support::*;
use triergon_core::{SourceCapabilities, Time, WorkBudget};
use triergon_mpegts::stream::TraceEvent;
use triergon_mpegts::{PacketFormat, clock::SourceTimePolicy};
use triergon_playback::mp4::Mp4Event;
use triergon_playback::{mp4::PlanLimits, *};

#[test]
fn track_views_report_each_format_s_own_tracks() {
    use triergon_core::container::{CodecId, ContainerReader, Event as Neutral, TrackInfo};
    use triergon_isobmff::{Reader, ReaderEvent, ReaderLimits, config::ConfigurationLimits};
    let same = |view: &Track<'_>, reference: &dyn TrackInfo| {
        assert_eq!(view.id(), reference.id());
        assert_eq!(view.kind(), reference.kind());
        assert_eq!(view.codec_tag(), reference.codec_tag());
        assert_eq!(view.codec(), reference.codec());
        assert_eq!(view.configuration(), reference.configuration());
    };

    let raw = include_bytes!("../fixtures/mp4/avc-aac-program.mov");
    let (opened, b) = open(raw, 7, 17).unwrap();
    let mut job = index_job(&opened.movie, View::Source, PlanLimits::default()).unwrap();
    run_index(&mut job, 17).unwrap();
    let mut plan = job.finish().unwrap();
    let mut reference = triergon_core::blocking::Blocking::seekable(
        Reader::new(
            &b,
            ID,
            SourceCapabilities {
                length: Some(raw.len() as u64),
                random_access: true,
            },
            ReaderLimits::default(),
            4096,
        )
        .unwrap(),
        std::io::Cursor::new(&raw[..]),
        &b,
    );
    while !matches!(
        reference.next_event().unwrap(),
        Some(Neutral::Other(ReaderEvent::Movie))
    ) {}
    let reference = reference.reader();
    let expected = |id| {
        (0..reference.track_count())
            .filter_map(|index| reference.track(index))
            .find(|track| track.id() == id)
            .unwrap()
    };
    let codecs = |plan: &mp4::PreparedMp4| {
        plan.tracks()
            .iter()
            .map(|track| (track.codec(), track.configuration().is_empty()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        codecs(&plan),
        [(CodecId::H264, true), (CodecId::Other, true)]
    );
    while !plan
        .identify_tracks(ConfigurationLimits::default(), &mut WorkBudget::new(1))
        .unwrap()
    {}
    assert_eq!(
        codecs(&plan),
        [(CodecId::H264, false), (CodecId::Aac, false)]
    );
    let engine = Engine::mp4(opened.input, plan, 1 << 20).unwrap();
    assert_eq!(engine.track_count(), 2);
    for index in 0..engine.track_count() {
        let track = engine.track(index).unwrap();
        let TrackKey::Mp4(id) = track.key() else {
            panic!("an MP4 track key of another format");
        };
        assert_eq!(u64::from(id), track.id().0);
        same(&track, expected(track.id()));
    }
    assert!(engine.track(engine.track_count()).is_none());

    let raw = include_bytes!("../fixtures/mkv/observed-11.mkv");
    let (selected, b, _) = probe(raw, 7, true);
    let mut p = Player::new(
        Engine::matroska(
            triergon_matroska::Reader::new(
                &b,
                ID,
                selected.input.capabilities(),
                triergon_matroska::Limits::default(),
                4096,
            )
            .unwrap(),
        ),
        selected.source,
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    assert_eq!(p.engine().unwrap().track_count(), 0);
    first_packet(&mut p);
    let engine = p.engine().unwrap();
    let Engine::Matroska(_) = engine else {
        panic!("a Matroska engine");
    };
    let configuration = (0..engine.track_count())
        .map(|index| engine.track(index).unwrap())
        .collect::<Vec<_>>();
    assert!(!configuration.is_empty());
    for track in &configuration {
        let Track::Matroska(source) = track else {
            panic!("a Matroska track view of another format");
        };
        same(track, *source);
        assert_eq!(track.key(), TrackKey::Matroska(source.number));
    }

    let (mut p, _) = ts(
        include_bytes!("../fixtures/ts/program-eof-avc-aac.ts"),
        PacketFormat::Ts188,
        7,
        SourceTimePolicy::Unanchored,
    )
    .unwrap();
    assert_eq!(p.engine().unwrap().track_count(), 0);
    let map = loop {
        p.advance(&mut WorkBudget::new(17), &off()).unwrap();
        if let Some(map) = std::iter::from_fn(|| p.pop()).find_map(|o| match o.into_event() {
            Event::Ts(TsEvent {
                source:
                    TraceEvent::ProgramMap {
                        map, active: true, ..
                    },
                ..
            }) => Some(map),
            _ => None,
        }) {
            break map;
        }
    };
    let engine = p.engine().unwrap();
    assert_eq!(engine.track_count(), map.streams.len());
    for (index, entry) in map.streams.iter().enumerate() {
        let track = engine.track(index).unwrap();
        same(&track, entry);
        assert_eq!(
            track.key(),
            TrackKey::Ts {
                program: map.header.extension,
                pid: entry.pid
            }
        );
    }
}

#[test]
fn packet_views_carry_the_source_clock_and_the_demux_clock_beside_it() {
    use triergon_core::container::{PacketInfo, TrackId};
    let (mut p, _) = mp4(
        include_bytes!("../fixtures/mp4/r-multitrack.mov"),
        View::Demux,
        7,
        1,
    )
    .unwrap();
    let out = collect(&mut p, 17).unwrap();
    let mut mp4_packets = 0;
    for o in &out {
        let Some(packet) = o.packet() else {
            continue;
        };
        let Event::Mp4(Mp4Event::Packet(source)) = o.event() else {
            panic!("a packet view of another event");
        };
        assert_eq!(
            packet.track(),
            TrackId(u64::from(source.configuration.track.track_id))
        );
        assert_eq!(packet.payload(), source.source.payload.as_ref());
        assert_eq!(packet.pts_time().unwrap(), o.event().source_pts().unwrap());
        let demux = packet.demux_times().unwrap();
        let demux_pts = demux
            .pts
            .map(|ticks| Time::from_ticks(i128::from(ticks), demux.time_base).unwrap());
        assert_eq!(demux_pts, o.event().demux_pts().unwrap());
        mp4_packets += 1;
    }
    assert_eq!(mp4_packets, 12);

    let raw = include_bytes!("../fixtures/mkv/observed-11.mkv");
    let (selected, b, _) = probe(raw, 7, true);
    let engine = Engine::matroska(
        triergon_matroska::Reader::new(
            &b,
            ID,
            selected.input.capabilities(),
            triergon_matroska::Limits::default(),
            4096,
        )
        .unwrap(),
    );
    let mut p = Player::new(
        engine,
        selected.source,
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    let out = collect(&mut p, 17).unwrap();
    let mut matroska_packets = 0;
    for o in &out {
        let Some(packet) = o.packet() else {
            continue;
        };
        let Event::Matroska(triergon_matroska::Event::Packet(source)) = o.event() else {
            panic!("a packet view of another event");
        };
        assert_eq!(packet.track(), PacketInfo::track(source));
        assert_eq!(packet.payload(), source.packet.data.as_ref());
        assert_eq!(packet.pts(), source.packet.pts_ns);
        assert_eq!(packet.demux_times().unwrap().pts, source.packet.pts_ns);
        matroska_packets += 1;
    }
    assert!(matroska_packets > 0);
    assert!(out.iter().all(|o| match o.event() {
        Event::Ts(_) | Event::DemuxPes { .. } => o.packet().is_none(),
        _ => true,
    }));
}
