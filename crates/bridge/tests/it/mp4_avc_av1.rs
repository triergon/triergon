use crate::framing::*;
use crate::support::players::*;
use crate::support::*;
use triergon_core::{Error as C, SourceId, Time, WorkBudget};
use triergon_playback::mp4::{Mp4Event, Mp4Packet};
use triergon_playback::*;

fn mp4_packets(v: &[Output]) -> Vec<&Mp4Packet> {
    v.iter()
        .filter_map(|o| {
            if let Event::Mp4(Mp4Event::Packet(p)) = o.event() {
                Some(p)
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn real_mp4_avc_packets_frame_as_annex_b_from_their_source_bytes() {
    let raw = include_bytes!("../../../playback/tests/fixtures/mp4/encoded-avc-aac-front.mp4");
    let (mut p, b) = mp4(raw, View::Source, 7, 17).unwrap();
    let out = collect(&mut p, 17).unwrap();
    let packets: Vec<_> = mp4_packets(&out)
        .into_iter()
        .filter(|p| p.configuration.description.format == *b"avc1")
        .collect();
    assert!(!packets.is_empty());
    let mut session = annex_b_session(&b, packets[0]);
    for packet in packets {
        let result = annex_b(&mut session, packet);
        assert_eq!(
            result.source().bytes.as_ref(),
            packet.source.payload.as_ref()
        );
        assert_eq!(result.source().generation, packet.source.token.generation());
        assert!(
            result.bytes().starts_with(&[0, 0, 0, 1]) || result.bytes().starts_with(&[0, 0, 1])
        );
    }
}

#[test]
fn av1_bridge_matches_delimiter_framing_from_real_mp4_and_webm_packets() {
    use triergon_bridge::av1::{Av1BridgeSetup, Av1Input};
    use triergon_framing::{
        BuildStep,
        av1::{Av1ConfigLimits, Av1ObuLimits, Av1TemporalUnitLimits},
    };

    let mp4_raw = include_bytes!("../../../playback/tests/fixtures/av1/six-frames.mp4");
    let mut dolby_entry = mp4_raw.to_vec();
    let table = mp4_raw.windows(4).position(|kind| kind == b"stsd").unwrap();
    let entry = table + 16; // FullBox fields, entry count and sample entry size.
    assert_eq!(&dolby_entry[entry..entry + 4], b"av01");
    dolby_entry[entry..entry + 4].copy_from_slice(b"dav1");
    let webm_raw = include_bytes!("../../../playback/tests/fixtures/av1/six-frames.webm");
    let expected = include_bytes!("../../../playback/tests/fixtures/av1/six-frames.obu");
    let obu_limits = Av1ObuLimits {
        maximum_obu_bytes: 16_384,
        maximum_obus: 64,
        maximum_input_window: 128,
    };
    let config_limits = Av1ConfigLimits {
        maximum_record_bytes: 16_384,
        obu: obu_limits,
    };
    let packet_limits = Av1TemporalUnitLimits {
        maximum_sample_bytes: 16_384,
        maximum_output_bytes: 32_768,
        maximum_obus: 64,
    };

    let bind = |mut setup: Av1BridgeSetup| {
        for _ in 0..4096 {
            match setup.step(&mut WorkBudget::new(1), &off()).unwrap() {
                BuildStep::Yield => {}
                BuildStep::Ready(bound) => {
                    assert!(matches!(
                        setup.step(&mut WorkBudget::new(1), &off()).unwrap(),
                        BuildStep::Complete
                    ));
                    return bound;
                }
                BuildStep::Complete => panic!("bridge setup lost its configuration"),
            }
        }
        panic!("bridge setup stalled");
    };

    for (mp4_raw, quantum) in [
        (mp4_raw.as_slice(), 7),
        (mp4_raw.as_slice(), 4096),
        (&dolby_entry, 7),
    ] {
        let (mut mp4, mp4_pool) = mp4(mp4_raw, View::Source, quantum, 3).unwrap();
        let (selected, mkv_pool, _) = probe(webm_raw, quantum, false);
        let engine = Engine::matroska(
            triergon_matroska::Reader::new(
                &mkv_pool,
                ID,
                selected.input.capabilities(),
                triergon_matroska::Limits::default(),
                4096,
            )
            .unwrap(),
        );
        let mut mkv = Player::new(
            engine,
            selected.source,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
        let mp4_packets = collect(&mut mp4, 7).unwrap();
        let mkv_packets = collect(&mut mkv, 7).unwrap();

        for (outputs, pool) in [(&mp4_packets, &mp4_pool), (&mkv_packets, &mkv_pool)] {
            let mut bridge = None;
            let mut bytes = Vec::new();
            let mut packets = 0;
            for output in outputs {
                let (packet, payload) = match output.event() {
                    Event::Mp4(Mp4Event::Configuration(config)) => {
                        assert_eq!(packets, 0);
                        bridge = Some(bind(
                            Av1BridgeSetup::for_mp4(pool, ID, config, config_limits, packet_limits)
                                .unwrap(),
                        ));
                        continue;
                    }
                    Event::Matroska(triergon_matroska::Event::Configuration(config)) => {
                        assert_eq!(packets, 0);
                        let track = config
                            .tracks()
                            .iter()
                            .position(|track| track.codec.as_str() == "V_AV1")
                            .unwrap();
                        bridge = Some(bind(
                            Av1BridgeSetup::for_matroska(
                                pool,
                                ID,
                                config,
                                track,
                                config_limits,
                                packet_limits,
                            )
                            .unwrap(),
                        ));
                        continue;
                    }
                    Event::Mp4(Mp4Event::Packet(packet)) => {
                        (Av1Input::Mp4(packet), packet.source.payload.as_ref())
                    }
                    Event::Matroska(triergon_matroska::Event::Packet(packet)) => {
                        (Av1Input::Matroska(packet), packet.packet.data.as_ref())
                    }
                    _ => continue,
                };
                let bound = bridge.as_ref().expect("configuration must precede packets");
                if let Av1Input::Mp4(packet) = packet {
                    let mut stale = packet.clone();
                    stale.configuration.revision += 1;
                    assert!(matches!(
                        bound.packet(Av1Input::Mp4(&stale)),
                        Err(C::Invalid(_))
                    ));
                    stale.configuration.revision = packet.configuration.revision;
                    stale.source.source = SourceId(ID.0 + 1);
                    assert!(matches!(
                        bound.packet(Av1Input::Mp4(&stale)),
                        Err(C::Invalid(_))
                    ));
                }
                let mut job = bound.packet(packet).unwrap();
                for _ in 0..128 {
                    match job.step(&mut WorkBudget::new(1), &off()).unwrap() {
                        BuildStep::Yield => {}
                        BuildStep::Ready(unit) => {
                            assert!(std::ptr::eq(unit.source(), payload));
                            let before = bytes.len();
                            for span in unit.segments() {
                                bytes.extend_from_slice(span);
                            }
                            assert_eq!(bytes.len() - before, unit.byte_len());
                            packets += 1;
                            break;
                        }
                        BuildStep::Complete => panic!("unit was not delivered"),
                    }
                }
            }
            assert_eq!(packets, 6);
            assert_eq!(bytes.as_slice(), expected);

            if std::ptr::eq(pool, &raw const mkv_pool) {
                let track = outputs.iter().find_map(|o| o.event().track()).unwrap();
                assert!(
                    mkv.seek_and_select(
                        track,
                        Time::new(0, 1).unwrap(),
                        Selection::All,
                        &mut WorkBudget::new(4096),
                        &off(),
                    )
                    .unwrap()
                );
                let replay = first_packet(&mut mkv);
                let Event::Matroska(triergon_matroska::Event::Packet(packet)) = replay.event()
                else {
                    panic!("seek did not return an AV1 packet");
                };
                let packet = Av1Input::Matroska(packet);
                assert!(matches!(bridge.unwrap().packet(packet), Err(C::Invalid(_))));
                let mut setup =
                    Av1BridgeSetup::new(pool, ID, packet, config_limits, packet_limits).unwrap();
                let BuildStep::Ready(bound) =
                    setup.step(&mut WorkBudget::new(4096), &off()).unwrap()
                else {
                    panic!("could not rebind the seek generation");
                };
                assert!(matches!(
                    bound
                        .packet(packet)
                        .unwrap()
                        .step(&mut WorkBudget::new(4096), &off(),)
                        .unwrap(),
                    BuildStep::Ready(_)
                ));
            }
        }
    }
}
