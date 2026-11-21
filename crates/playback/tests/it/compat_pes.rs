use crate::support::players::*;
use crate::support::*;
use triergon_core::{Budget, Bytes, Error as C, InputCursor, SourceCapabilities, Time, WorkBudget};
use triergon_mpegts::stream::TraceEvent;
use triergon_mpegts::{PacketFormat, clock::SourceTimePolicy};
use triergon_playback::ByteSource;
use triergon_playback::compat::pes::PrivateAudioSelection;
use triergon_playback::{SourceOwner, *};

#[test]
fn full_native_ts_packets_are_distinct_from_source_pes_and_keep_literal_fields() {
    use triergon_mpegts::compat::pes::{
        DemuxBoundary, DemuxPayloadKind, RegisteredCarriage as Carriage,
    };
    for (raw, pid, carriage, stream_id) in [
        (
            include_bytes!("../fixtures/ts/ts188-basic.ts").as_slice(),
            256,
            Carriage::Avc,
            0xe0,
        ),
        (
            include_bytes!("../fixtures/ts/carriage-hevc-direct.ts").as_slice(),
            257,
            Carriage::Hevc,
            0xe0,
        ),
        (
            include_bytes!("../fixtures/ts/carriage-aac-adts-direct.ts").as_slice(),
            257,
            Carriage::AacAdts,
            0xc0,
        ),
    ] {
        let b = pool();
        let input = InputCursor::new(
            &b,
            ID,
            SourceCapabilities {
                length: Some(raw.len() as u64),
                random_access: true,
            },
            64,
        )
        .unwrap();
        let engine = compat::pes::open(
            input,
            1,
            triergon_mpegts::ParseLimits::default(),
            SourceTimePolicy::Unanchored,
            triergon_mpegts::psi::DescriptorNamespace::Unspecified,
            &[compat::pes::PidSelection::registered(pid, carriage)],
        )
        .unwrap();
        let mut p = Player::new(
            engine,
            SourceOwner::new(
                ByteSource::new(ID, Bytes::copy_from_slice(&b, raw).unwrap(), 1).unwrap(),
            ),
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
        p.set_clock_view(ClockChoice::Demux).unwrap();
        let out = collect(&mut p, 1).unwrap();
        assert_eq!(pes(&out).len(), 1);
        assert!(
            out.iter()
                .filter(|o| matches!(
                    o.event(),
                    Event::Ts(TsEvent {
                        source: TraceEvent::Pes(_),
                        ..
                    })
                ))
                .all(|o| o.delivery() == Delivery::Source)
        );
        let native: Vec<_> = out
            .iter()
            .filter_map(|o| {
                if let Event::DemuxPes { packet, clock } = o.event() {
                    assert_eq!(o.delivery(), Delivery::Present);
                    Some((packet, clock))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(native.len(), 1);
        let (packet, clock) = native[0];
        assert_eq!(packet.pid, pid);
        assert_eq!(packet.position, 376);
        assert_eq!(packet.flags, 0);
        assert_eq!(packet.duration90_raw, 0);
        assert_eq!(packet.stream_id, stream_id);
        assert_eq!(packet.boundary, DemuxBoundary::FiniteLength);
        assert_eq!(packet.payload_kind, DemuxPayloadKind::DeclaredComplete);
        assert_eq!(
            packet.payload().flatten().copied().collect::<Vec<_>>(),
            b"lane-ts-original-payload\x00\x47\xff"
        );
        assert_eq!(clock.pts90, Some(90_000));
        assert_eq!(clock.dts90, Some(90_000));
        assert_eq!(packet.source_header.dts_raw33, None);
        assert_eq!(packet.header_source_ranges.as_slice()[0].offset, 523);
        assert_eq!(packet.spans.as_slice()[0].source.offset, 537);
        assert_eq!(packet.spans.as_slice()[0].context.source_event_ordinal, 8);
        drop(out);
        drop(p);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn native_carriage_refusal_keeps_source_and_is_not_a_native_codec_error() {
    use triergon_mpegts::compat::pes::RegisteredCarriage;
    let raw = include_bytes!("../fixtures/ts/ts188-basic.ts");
    let b = pool();
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        64,
    )
    .unwrap();
    let engine = compat::pes::open(
        input,
        1,
        triergon_mpegts::ParseLimits::default(),
        SourceTimePolicy::RelativeEpochZero,
        triergon_mpegts::psi::DescriptorNamespace::Unspecified,
        &[compat::pes::PidSelection::registered(
            256,
            RegisteredCarriage::Hevc,
        )],
    )
    .unwrap();
    let mut p = Player::new(
        engine,
        SourceOwner::new(ByteSource::new(ID, Bytes::copy_from_slice(&b, raw).unwrap(), 7).unwrap()),
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    let out = collect(&mut p, 1).unwrap();
    assert_eq!(pes(&out).len(), 1);
    assert!(out.iter().any(|o| matches!(
        o.event(),
        Event::PesRefusal {
            error: C::Unsupported(_),
            ..
        }
    )));
    assert!(
        out.iter()
            .all(|o| !matches!(o.event(), Event::DemuxPes { .. }))
    );
}

#[test]
fn m2ts_source_pes_needs_no_startup_evidence() {
    let (mut p, _) = ts(
        include_bytes!("../fixtures/ts/m2ts192-opaque-prefix.m2ts"),
        PacketFormat::M2ts192,
        7,
        SourceTimePolicy::Unanchored,
    )
    .unwrap();
    assert_eq!(pes(&collect(&mut p, 1).unwrap()).len(), 1);
}

#[test]
fn native_ts_projection_only_replaces_source_pes_for_its_selected_pid() {
    use triergon_mpegts::compat::pes::RegisteredCarriage;

    let video = include_bytes!("../fixtures/ts/ts188-basic.ts");
    let audio = include_bytes!("../fixtures/ts/carriage-aac-adts-direct.ts");
    let mut raw = video[..188].to_vec();
    // Build one active PMT from the two authored one-stream controls. Their
    // elementary entries keep the original types, PIDs and descriptor bytes.
    let mut section = [
        0x02, 0xb0, 0x17, 0, 1, 0xc1, 0, 0, 0xe1, 0, 0xf0, 0, 0x1b, 0xe1, 0, 0xf0, 0, 0x0f, 0xe1,
        1, 0xf0, 0, 0, 0, 0, 0,
    ];
    let mut crc = u32::MAX;
    for &byte in &section[..22] {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = (crc << 1) ^ if crc >> 31 == 1 { 0x04c1_1db7 } else { 0 };
        }
    }
    section[22..].copy_from_slice(&crc.to_be_bytes());
    let mut pmt = [0xff; 188];
    pmt[..5].copy_from_slice(&[0x47, 0x50, 0x00, 0x10, 0]);
    pmt[5..31].copy_from_slice(&section);
    raw.extend_from_slice(&pmt);
    raw.extend_from_slice(&video[376..]);
    raw.extend_from_slice(&audio[376..]);

    let b = pool();
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: false,
        },
        64,
    )
    .unwrap();
    let engine = compat::pes::open(
        input,
        1,
        triergon_mpegts::ParseLimits::default(),
        SourceTimePolicy::RelativeEpochZero,
        triergon_mpegts::psi::DescriptorNamespace::Unspecified,
        &[compat::pes::PidSelection::registered(
            256,
            RegisteredCarriage::Avc,
        )],
    )
    .unwrap();
    let mut p = Player::new(
        engine,
        SourceOwner::new(
            ByteSource::new(ID, Bytes::copy_from_slice(&b, &raw).unwrap(), 7).unwrap(),
        ),
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    p.set_clock_view(ClockChoice::Demux).unwrap();
    let out = collect(&mut p, 1).unwrap();
    let source_packets: Vec<_> = out
        .iter()
        .filter_map(|o| {
            if let Event::Ts(TsEvent {
                source: TraceEvent::Pes(pes),
                ..
            }) = o.event()
            {
                Some((pes.pid, o.delivery()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        source_packets,
        [(256, Delivery::Source), (257, Delivery::Present)]
    );
    assert_eq!(
        out.iter()
            .filter(|o| matches!(o.event(), Event::DemuxPes { .. }))
            .count(),
        1
    );
}

fn assert_source_pes_match(left: &[Output], right: &[Output]) {
    let left = pes(left);
    let right = pes(right);
    assert_eq!(left.len(), right.len());
    for (a, b) in left.into_iter().zip(right) {
        let (TraceEvent::Pes(x), TraceEvent::Pes(y)) = (&a.source, &b.source) else {
            panic!("source PES expected");
        };
        assert_eq!(a.clock_key, b.clock_key);
        assert_eq!(a.source_pts, b.source_pts);
        assert_eq!(a.source_dts, b.source_dts);
        assert_eq!(x.pid, y.pid);
        assert_eq!(x.epochs, y.epochs);
        assert_eq!(x.byte_run_epoch, y.byte_run_epoch);
        assert_eq!(x.elementary_offset, y.elementary_offset);
        assert_eq!(x.header.as_bytes(), y.header.as_bytes());
        assert_eq!(
            x.header_source_ranges.as_slice(),
            y.header_source_ranges.as_slice()
        );
        assert!(x.source_ranges().eq(y.source_ranges()));
        assert!(x.payload().flatten().eq(y.payload().flatten()));
    }
}

fn private_audio_player(
    raw: &[u8],
    carriage: triergon_mpegts::compat::pes::PrivateAudioCarriage,
    selection: Selection,
    quantum: usize,
) -> (Player<ByteSource>, Budget) {
    let budget = pool();
    let input = InputCursor::new(
        &budget,
        ID,
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        64,
    )
    .unwrap();
    let engine = compat::pes::open_private_audio(
        input,
        1,
        triergon_mpegts::ParseLimits::default(),
        SourceTimePolicy::RelativeEpochZero,
        PrivateAudioSelection { pid: 257, carriage },
    )
    .unwrap();
    let source = SourceOwner::new(
        ByteSource::new(ID, Bytes::copy_from_slice(&budget, raw).unwrap(), quantum).unwrap(),
    );
    let player = Player::new(engine, source, selection, PlayerPolicy::default()).unwrap();
    (player, budget)
}

#[test]
fn private_audio_playback_matches_all_four_archived_packet_controls() {
    use triergon_mpegts::compat::pes::{
        DemuxBoundary, DemuxPayloadKind, PrivateAudioCarriage as Carriage,
    };

    for (raw, carriage) in [
        (
            include_bytes!("../../../mpegts/tests/data/carriage-ac3-dvb.ts"),
            Carriage::DvbAc3Tag,
        ),
        (
            include_bytes!("../../../mpegts/tests/data/carriage-eac3-dvb.ts"),
            Carriage::DvbEac3Tag,
        ),
        (
            include_bytes!("../../../mpegts/tests/data/carriage-ac3-direct.ts"),
            Carriage::PrivateType81Ac3,
        ),
        (
            include_bytes!("../../../mpegts/tests/data/carriage-eac3-direct.ts"),
            Carriage::PrivateType87Eac3,
        ),
    ] {
        for quantum in [1, 64] {
            let (mut p, budget) = private_audio_player(
                raw,
                carriage,
                Selection::Tracks {
                    video: None,
                    audio: Some(TrackKey::Ts {
                        program: 1,
                        pid: 257,
                    }),
                    subtitle: None,
                },
                quantum,
            );
            p.set_clock_view(ClockChoice::Demux).unwrap();
            let out = collect(&mut p, 1).unwrap();
            let (mut strict, _) = ts(
                raw,
                PacketFormat::Ts188,
                quantum,
                SourceTimePolicy::RelativeEpochZero,
            )
            .unwrap();
            let strict_out = collect(&mut strict, 1).unwrap();
            assert_source_pes_match(&out, &strict_out);
            assert!(
                out.iter()
                    .all(|o| !matches!(o.event(), Event::PesRefusal { .. }))
            );
            let mut native = out.iter().filter_map(|o| {
                if let Event::DemuxPes { packet, clock } = o.event() {
                    assert_eq!(o.delivery(), Delivery::Present);
                    Some((packet, clock))
                } else {
                    None
                }
            });
            let (packet, clock) = native.next().expect("admitted private audio");
            assert!(native.next().is_none());
            assert_eq!((packet.program, packet.pid), (1, 257));
            assert_eq!(packet.stream_id, 0xbd);
            assert_eq!(
                (packet.position, packet.duration90_raw, packet.flags),
                (376, 0, 0)
            );
            assert_eq!(packet.boundary, DemuxBoundary::FiniteLength);
            assert_eq!(packet.payload_kind, DemuxPayloadKind::DeclaredComplete);
            assert_eq!(packet.source_header.as_bytes(), &raw[523..537]);
            assert_eq!(packet.source_header.pts_raw33, Some(90_000));
            assert_eq!(packet.source_header.dts_raw33, None);
            assert_eq!((clock.pts90, clock.dts90), (Some(90_000), Some(90_000)));
            assert_eq!(packet.spans.as_slice()[0].source.offset, 537);
            assert_eq!(packet.spans.as_slice()[0].context.source_event_ordinal, 8);
            drop(p);
            assert!(
                packet
                    .payload()
                    .flatten()
                    .copied()
                    .eq(raw[537..].iter().copied())
            );
            drop(out);
            assert_eq!(budget.used(), Budget::control_storage_bytes());
        }
    }
}

#[test]
fn private_audio_mismatch_reports_refusal_and_retains_source_packet() {
    use triergon_mpegts::compat::pes::PrivateAudioCarriage as Carriage;
    let raw = include_bytes!("../../../mpegts/tests/data/carriage-ac3-dvb.ts");
    let (mut p, _) = private_audio_player(raw, Carriage::DvbEac3Tag, Selection::All, 7);
    let out = collect(&mut p, 1).unwrap();
    let (mut strict, _) = ts(
        raw,
        PacketFormat::Ts188,
        7,
        SourceTimePolicy::RelativeEpochZero,
    )
    .unwrap();
    assert_source_pes_match(&out, &collect(&mut strict, 1).unwrap());
    assert!(
        out.iter()
            .all(|o| !matches!(o.event(), Event::DemuxPes { .. }))
    );
    assert_eq!(
        out.iter()
            .filter(|o| matches!(
                o.event(),
                Event::PesRefusal {
                    track: TrackKey::Ts {
                        program: 1,
                        pid: 257
                    },
                    error: C::Invalid("native private audio carriage mismatch"),
                    ..
                },
            ))
            .count(),
        1
    );
}

#[test]
fn private_audio_policy_and_pid_selection_replay_the_same_packets_after_seek() {
    let raw = include_bytes!("../fixtures/ts/program-private-avc-dvb.ts");
    let open = |general| {
        let capabilities = SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        };
        let io = byte_input(&pool(), ID, raw, 188, capabilities, 188).unwrap();
        let audio = PrivateAudioSelection {
            pid: 257,
            carriage: triergon_mpegts::compat::pes::PrivateAudioCarriage::DvbAc3Tag,
        };
        let limits = triergon_mpegts::ParseLimits::default();
        let clock = SourceTimePolicy::RelativeEpochZero;
        let engine = if general {
            compat::pes::open(
                io.input,
                1,
                limits,
                clock,
                triergon_mpegts::psi::DescriptorNamespace::Unspecified,
                &[compat::pes::PidSelection {
                    pid: audio.pid,
                    carriage: compat::pes::Carriage::PrivateAudio(audio.carriage),
                }],
            )
        } else {
            compat::pes::open_private_audio(io.input, 1, limits, clock, audio)
        }
        .unwrap();
        let mut player =
            Player::new(engine, io.source, Selection::All, PlayerPolicy::default()).unwrap();
        player.set_clock_view(ClockChoice::Demux).unwrap();
        player
    };
    let (mut general, mut policy) = (open(true), open(false));
    for replay in [false, true] {
        if replay {
            for player in [&mut general, &mut policy] {
                let video = TrackKey::Ts {
                    program: 1,
                    pid: 256,
                };
                assert!(
                    player
                        .seek_and_select(
                            video,
                            Time::new(0, 1).unwrap(),
                            Selection::All,
                            &mut WorkBudget::new(128),
                            &off()
                        )
                        .unwrap()
                );
            }
        }
        let old = collect(&mut general, 128).unwrap();
        let new = collect(&mut policy, 128).unwrap();
        assert_eq!(
            (general.state(), policy.state()),
            (State::Ended, State::Ended)
        );
        assert_source_pes_match(&old, &new);
        let audio_packets = |out: &[Output]| {
            out.iter()
                .filter_map(|row| match row.event() {
                    Event::DemuxPes { packet, clock } => Some((
                        row.delivery(),
                        packet.pid,
                        clock.pts90,
                        clock.dts90,
                        packet.payload().flatten().copied().collect::<Vec<_>>(),
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let expected = audio_packets(&old);
        assert!(!expected.is_empty());
        assert_eq!(expected, audio_packets(&new));
    }
}

#[test]
fn native_pressure_retries_instead_of_disabling_the_observer() {
    use triergon_mpegts::compat::pes::PrivateAudioCarriage as Carriage;
    let raw = include_bytes!("../../../mpegts/tests/data/carriage-ac3-dvb.ts");
    let (mut player, budget) = private_audio_player(raw, Carriage::DvbAc3Tag, Selection::All, 7);
    for _ in 0..4096 {
        player.advance(&mut WorkBudget::new(1), &off()).unwrap();
        if let Some(event) = player.pop()
            && matches!(
                event.event(),
                Event::Ts(TsEvent { source: TraceEvent::Transport(packet), .. })
                    if packet.pid == 257
            )
        {
            let pressure = budget.reserve(budget.available()).unwrap();
            let mut blocked = false;
            for _ in 0..512 {
                match player.advance(&mut WorkBudget::new(1), &off()).unwrap() {
                    Progress::Backpressure { .. } => {
                        blocked = true;
                        break;
                    }
                    Progress::Queued => panic!("native observer escaped allocation pressure"),
                    _ => {}
                }
            }
            assert!(blocked, "native observer pressure must be retryable");
            drop(pressure);
            let out = collect(&mut player, 1).unwrap();
            assert!(
                out.iter()
                    .any(|o| matches!(o.event(), Event::DemuxPes { packet, .. }
                    if packet.payload().flatten().copied().eq(raw[537..].iter().copied())))
            );
            assert!(
                out.iter()
                    .all(|o| !matches!(o.event(), Event::PesRefusal { .. }))
            );
            return;
        }
    }
    panic!("missing selected carrier");
}
