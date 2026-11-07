use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputCursor, SourceCapabilities, SourceId, Time, WorkBudget,
};
use triergon_mpegts::{
    PacketFormat, ParseLimits, TraceEvent,
    clock::SourceTimePolicy,
    native_pes::{
        NativePesBoundary, NativePesPacket, NativePrivateAudioCarriage, NativeRegisteredCarriage,
    },
    psi::{DescriptorNamespace, DvbNamespaceEvidence},
};
use triergon_playback::{
    ByteSource, ClockChoice, Delivery, Engine, Output, Player, PlayerPolicy, Progress, RawEvent,
    Selection, TrackKey, TsEvent, TsNativeCarriage, TsNativeTrackSelection, io::SourceOwner,
};

const ENTITY: SourceId = SourceId(77);
const NAMESPACE: DescriptorNamespace =
    DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile);
const MIXED: &[u8] = include_bytes!("fixtures/ts/program-private-avc-dvb.ts");
const RETYPE: &[u8] = include_bytes!("fixtures/ts/program-pmt-retype.ts");
const EOF: &[u8] = include_bytes!("fixtures/ts/program-eof-avc-aac.ts");
const AVC: TsNativeTrackSelection = TsNativeTrackSelection {
    pid: 256,
    carriage: TsNativeCarriage::Registered(NativeRegisteredCarriage::Avc),
};
const AAC: TsNativeTrackSelection = TsNativeTrackSelection {
    pid: 257,
    carriage: TsNativeCarriage::Registered(NativeRegisteredCarriage::AacAdts),
};
const AC3: TsNativeTrackSelection = TsNativeTrackSelection {
    pid: 257,
    carriage: TsNativeCarriage::PrivateAudio(NativePrivateAudioCarriage::DvbAc3Tag),
};
const FOLLOW_AUDIO: TsNativeTrackSelection = TsNativeTrackSelection {
    pid: 257,
    carriage: TsNativeCarriage::AudioFromPmt,
};
const DVB: TsNativeTrackSelection = TsNativeTrackSelection {
    pid: 258,
    carriage: TsNativeCarriage::DvbSubtitles,
};

fn budget() -> Budget {
    Budget::new(4 * 1024 * 1024).unwrap()
}

fn engine(
    budget: &Budget,
    selections: &[TsNativeTrackSelection],
    format: PacketFormat,
    namespace: DescriptorNamespace,
) -> triergon_playback::Result<Engine> {
    let input = InputCursor::new(
        budget,
        ENTITY,
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        188,
    )
    .unwrap();
    Engine::ts_native_program_v1(
        input,
        format,
        1,
        ParseLimits::default(),
        SourceTimePolicy::Unanchored,
        namespace,
        selections,
    )
}

fn player(
    raw: &[u8],
    selections: &[TsNativeTrackSelection],
    selection: Selection,
    quantum: usize,
) -> (Player<ByteSource>, Budget) {
    player_with_namespace(raw, selections, selection, quantum, NAMESPACE)
}

fn player_with_namespace(
    raw: &[u8],
    selections: &[TsNativeTrackSelection],
    selection: Selection,
    quantum: usize,
    namespace: DescriptorNamespace,
) -> (Player<ByteSource>, Budget) {
    let budget = budget();
    let source = ByteSource::new(
        ENTITY,
        Bytes::copy_from_slice(&budget, raw).unwrap(),
        quantum,
    )
    .unwrap();
    let mut player = Player::new(
        engine(&budget, selections, PacketFormat::Ts188, namespace).unwrap(),
        SourceOwner::new(source),
        selection,
        PlayerPolicy::default(),
    )
    .unwrap();
    player
        .set_clock_view(ClockChoice::ExplicitNativeProjection)
        .unwrap();
    (player, budget)
}

fn collect(player: &mut Player<ByteSource>, work: u64) -> Vec<Output> {
    let cancel = AtomicBool::new(false);
    let mut output = Vec::new();
    for _ in 0..20_000 {
        let progress = player.advance(&mut WorkBudget::new(work), &cancel).unwrap();
        while let Some(value) = player.pop() {
            output.push(value);
        }
        if matches!(progress, Progress::Ended) {
            return output;
        }
    }
    panic!("native program playback stalled");
}

// Captured by the no-parser FFmpeg 9.0.2 demux reference for these fixtures.
const AV_PAYLOAD: u64 = 0xd3fd_4412_38d4_6078;
const DVB_PAYLOAD: u64 = 0xa551_170f_0aa9_5055;

fn digest(packet: &NativePesPacket) -> u64 {
    packet
        .payload()
        .flatten()
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
        })
}

fn key(pid: u16) -> TrackKey {
    TrackKey::Ts { program: 1, pid }
}

fn segment_player(raw: &[u8], quantum: usize) -> (Player<ByteSource>, Budget) {
    let budget = budget();
    let cursor = InputCursor::new(
        &budget,
        ENTITY,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: false,
        },
        188,
    )
    .unwrap();
    let engine = Engine::ts_native_segment_v1(
        cursor,
        ParseLimits::default(),
        SourceTimePolicy::Unanchored,
        NAMESPACE,
    )
    .unwrap();
    assert!(engine.has_native_packet_view());
    let source = ByteSource::new(
        ENTITY,
        Bytes::copy_from_slice(&budget, raw).unwrap(),
        quantum,
    )
    .unwrap();
    let mut player = Player::new(
        engine,
        SourceOwner::new(source),
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    player
        .set_clock_view(ClockChoice::ExplicitNativeProjection)
        .unwrap();
    (player, budget)
}

#[test]
fn independent_ts_segment_refuses_multiple_programs_or_missing_pmt() {
    let multi = include_bytes!("../../mpegts/tests/data/pat-atomic-two-sections.ts");
    for (raw, expected) in [
        (
            multi.as_slice(),
            Error::Unsupported("segment PAT has multiple media programs"),
        ),
        (
            &EOF[..188],
            Error::Invalid("TS segment ended without an active PMT"),
        ),
    ] {
        let (mut player, _) = segment_player(raw, 23);
        let mut error = None;
        for _ in 0..2000 {
            let progress = player.advance(&mut WorkBudget::new(1), &AtomicBool::new(false));
            while let Some(item) = player.pop() {
                assert!(!matches!(item.event(), RawEvent::NativeTs { .. }));
            }
            match progress {
                Ok(Progress::Ended) => panic!("invalid segment was treated as clean EOF"),
                Err(actual) => {
                    error = Some(actual);
                    break;
                }
                other => assert!(other.is_ok(), "{other:?}"),
            }
        }
        assert_eq!(error, Some(expected));
    }
}

#[test]
fn independent_ts_segments_discover_tracks_and_emit_configuration_before_packets() {
    for (quantum, work) in [(1, 1), (128, 64)] {
        for (raw, expected_audio) in [
            (EOF, [Some(AAC.carriage), None]),
            (
                RETYPE,
                [
                    Some(AC3.carriage),
                    Some(TsNativeCarriage::Registered(
                        NativeRegisteredCarriage::AacLatm,
                    )),
                ],
            ),
        ] {
            let (mut automatic, b) = segment_player(raw, quantum);
            let output = collect(&mut automatic, work);
            let mut announced = [None; 2];
            let mut audio = Vec::new();
            let mut actual = Vec::new();
            let mut map_epoch = None;
            for item in &output {
                match item.event() {
                    RawEvent::Ts(TsEvent {
                        source:
                            TraceEvent::ProgramMap {
                                active: true,
                                configuration_epoch,
                                ..
                            },
                        ..
                    }) => {
                        announced = [None; 2];
                        map_epoch = Some(*configuration_epoch);
                    }
                    RawEvent::NativeTsConfiguration(config) => {
                        assert_eq!(Some(config.configuration_epoch), map_epoch);
                        let slot = if config.track == key(256) {
                            assert_eq!(config.carriage, AVC.carriage);
                            0
                        } else {
                            assert_eq!(config.track, key(257));
                            audio.push(config.carriage);
                            1
                        };
                        assert!(
                            announced[slot]
                                .replace(config.configuration_epoch)
                                .is_none()
                        );
                    }
                    RawEvent::NativeTs { packet, clock } => {
                        let slot = usize::from(packet.pid != 256);
                        assert_eq!(packet.pid, 256 + slot as u16);
                        assert_eq!(announced[slot], Some(packet.source_epochs.configuration));
                        actual.push((packet.pid, clock.pts90, clock.dts90, digest(packet)));
                    }
                    RawEvent::NativeRefusal { error, .. } => panic!("unexpected refusal {error:?}"),
                    _ => {}
                }
            }
            assert_eq!(
                audio,
                expected_audio.into_iter().flatten().collect::<Vec<_>>()
            );
            let (mut explicit, _) = player(raw, &[AVC, FOLLOW_AUDIO], Selection::All, quantum);
            let expected: Vec<_> = collect(&mut explicit, work)
                .iter()
                .filter_map(|item| match item.event() {
                    RawEvent::NativeTs { packet, clock } => {
                        Some((packet.pid, clock.pts90, clock.dts90, digest(packet)))
                    }
                    _ => None,
                })
                .collect();
            assert!(!actual.is_empty());
            assert_eq!(actual, expected);
            drop(output);
            drop(automatic);
            assert_eq!(b.used(), Budget::control_storage_bytes());
        }
    }
}

#[test]
fn mixed_avc_ac3_dvb_keep_carrier_order_and_a_common_clock() {
    for (quantum, work) in [(1, 1), (128, 64)] {
        let (mut player, budget) = player(
            MIXED,
            &[DVB, AVC, AC3],
            Selection::Tracks {
                video: Some(key(256)),
                audio: Some(key(257)),
                subtitle: Some(key(258)),
            },
            quantum,
        );
        let output = collect(&mut player, work);
        let foreign_pool = Budget::new(4 * 1024 * 1024).unwrap();
        let mut packets = Vec::new();
        let mut sources = Vec::new();
        for item in &output {
            assert!(item.belongs_to(&budget));
            assert!(!item.belongs_to(&foreign_pool));
            match item.event() {
                RawEvent::NativeTs { packet, clock } => {
                    assert_eq!(item.delivery(), Delivery::Present);
                    assert_eq!(
                        clock.origin.unwrap().first_reference_raw90,
                        (1 << 33) - 90_000
                    );
                    packets.push((
                        packet.pid,
                        packet.native_stream_id,
                        packet.native_position,
                        clock.pts90,
                        clock.dts90,
                        packet.payload_len,
                        digest(packet),
                    ));
                    assert_eq!(packet.source_header.dts_raw33, None);
                    assert_eq!(packet.boundary, NativePesBoundary::FiniteLength);
                }
                RawEvent::Ts(TsEvent {
                    source: TraceEvent::Pes(p),
                    ..
                }) => {
                    assert_eq!(item.delivery(), Delivery::Source);
                    sources.push((p.pid, p.header.pts_raw33, p.header.dts_raw33));
                }
                RawEvent::NativeRefusal { error, .. } => panic!("unexpected refusal {error:?}"),
                _ => {}
            }
        }
        assert_eq!(
            packets,
            [
                (257, 0xbd, 376, Some(-90_000), Some(-90_000), 27, AV_PAYLOAD),
                (256, 0xe0, 564, Some(90_000), Some(90_000), 27, AV_PAYLOAD),
                (258, 0xbd, 752, Some(90_000), Some(90_000), 126, DVB_PAYLOAD)
            ]
        );
        assert_eq!(
            sources,
            [
                (257, Some((1 << 33) - 90_000), None),
                (256, Some(90_000), None),
                (258, Some(90_000), None)
            ]
        );
        drop(player);
        assert!(budget.used() > Budget::control_storage_bytes());
        assert!(output.iter().all(|item| item.belongs_to(&budget)));
        drop(output);
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn unbounded_packets_drain_by_pid_only_after_logical_eof() {
    let (mut player, _) = player(EOF, &[AAC, AVC], Selection::All, 7);
    let output = collect(&mut player, 1);
    let packets: Vec<_> = output
        .iter()
        .filter_map(|item| match item.event() {
            RawEvent::NativeTs { packet, .. } => {
                assert_eq!(packet.boundary, NativePesBoundary::LogicalEof);
                assert_eq!((packet.payload_len, digest(packet)), (27, AV_PAYLOAD));
                Some((packet.pid, packet.native_position))
            }
            RawEvent::NativeRefusal { error, .. } => panic!("unexpected refusal {error:?}"),
            _ => None,
        })
        .collect();
    assert_eq!(packets, [(256, 564), (257, 376)]);
}

#[test]
fn pmt_retype_only_refuses_the_changed_pid() {
    let (mut player, _) = player(RETYPE, &[AVC, AC3, DVB], Selection::All, 17);
    let output = collect(&mut player, 1);
    let mut packets = Vec::new();
    let mut refusals = Vec::new();
    let mut maps = Vec::new();
    for (index, item) in output.iter().enumerate() {
        match item.event() {
            RawEvent::NativeTs { packet, .. } => packets.push(packet.pid),
            RawEvent::NativeRefusal { track, .. } => refusals.push((index, *track)),
            RawEvent::Ts(TsEvent {
                source: TraceEvent::ProgramMap { active: true, .. },
                ..
            }) => maps.push(index),
            _ => {}
        }
    }
    assert_eq!(packets, [257, 256, 258, 256]);
    assert_eq!(maps.len(), 2);
    assert_eq!(refusals, [(maps[1] + 1, key(257))]);
}

#[test]
fn selected_audio_follows_pmt_and_matches_native_carrier_order() {
    let selection = Selection::Tracks {
        video: Some(key(256)),
        audio: Some(key(257)),
        subtitle: Some(key(258)),
    };
    for (quantum, work) in [(1, 1), (128, 64)] {
        let (mut player, _) = player(RETYPE, &[AVC, FOLLOW_AUDIO, DVB], selection, quantum);
        let output = collect(&mut player, work);
        let mut map = None;
        let mut audio = None;
        let mut configurations = Vec::new();
        let mut packets = Vec::new();
        let mut sources = Vec::new();
        for (index, item) in output.iter().enumerate() {
            match item.event() {
                RawEvent::Ts(TsEvent {
                    source:
                        TraceEvent::ProgramMap {
                            active: true,
                            configuration_epoch,
                            ..
                        },
                    ..
                }) => {
                    map = Some((index, *configuration_epoch));
                    audio = None;
                }
                RawEvent::NativeTsConfiguration(config) => {
                    assert_eq!(item.delivery(), Delivery::Source);
                    assert_eq!(config.track, key(257));
                    assert_eq!(map, Some((index - 1, config.configuration_epoch)));
                    audio = Some(config.configuration_epoch);
                    configurations.push(config.carriage);
                }
                RawEvent::NativeTs { packet, clock } => {
                    assert_eq!(item.delivery(), Delivery::Present);
                    if packet.pid == 257 {
                        assert_eq!(audio, Some(packet.source_epochs.configuration));
                    }
                    packets.push((
                        packet.pid,
                        packet.native_stream_id,
                        packet.native_position,
                        clock.pts90,
                        clock.dts90,
                        packet.payload_len,
                        digest(packet),
                    ));
                }
                RawEvent::Ts(TsEvent {
                    source: TraceEvent::Pes(p),
                    ..
                }) if p.pid == 257 => {
                    assert_eq!(item.delivery(), Delivery::Source);
                    sources.push((p.header.stream_id, p.header.pts_raw33, p.header.dts_raw33));
                }
                RawEvent::NativeRefusal { error, .. } => panic!("unexpected refusal {error:?}"),
                _ => {}
            }
        }
        assert_eq!(
            configurations,
            [
                AC3.carriage,
                TsNativeCarriage::Registered(NativeRegisteredCarriage::AacLatm)
            ]
        );
        assert_eq!(
            packets,
            [
                (257, 0xbd, 376, Some(-90_000), Some(-90_000), 27, AV_PAYLOAD),
                (256, 0xe0, 564, Some(90_000), Some(90_000), 27, AV_PAYLOAD),
                (258, 0xbd, 752, Some(90_000), Some(90_000), 126, DVB_PAYLOAD),
                (257, 0xc0, 1128, Some(90_000), Some(90_000), 27, AV_PAYLOAD),
                (256, 0xe0, 1316, Some(90_000), Some(90_000), 27, AV_PAYLOAD),
            ]
        );
        assert_eq!(
            sources,
            [
                (0xbd, Some((1 << 33) - 90_000), None),
                (0xc0, Some(90_000), None)
            ]
        );
    }
}

#[test]
fn missing_dvb_context_preserves_private_source_and_later_latm_recovers() {
    let (mut player, _) = player_with_namespace(
        RETYPE,
        &[AVC, FOLLOW_AUDIO],
        Selection::All,
        11,
        DescriptorNamespace::Unspecified,
    );
    let output = collect(&mut player, 2);
    let mut audio = Vec::new();
    for item in &output {
        match item.event() {
            RawEvent::NativeRefusal { track, error, .. } => {
                assert_eq!(*track, key(257));
                assert!(matches!(
                    error,
                    Error::Unsupported("native PMT audio needs explicit DVB context")
                ));
                audio.push("refusal");
            }
            RawEvent::Ts(TsEvent {
                source: TraceEvent::Pes(p),
                ..
            }) if p.pid == 257 && p.header.stream_id == 0xbd => {
                assert_eq!(item.delivery(), Delivery::Source);
                audio.push("private source");
            }
            RawEvent::NativeTsConfiguration(config) => {
                assert_eq!(
                    config.carriage,
                    TsNativeCarriage::Registered(NativeRegisteredCarriage::AacLatm)
                );
                audio.push("latm configuration");
            }
            RawEvent::NativeTs { packet, .. } if packet.pid == 257 => {
                assert_eq!(packet.native_stream_id, 0xc0);
                audio.push("latm native");
            }
            _ => {}
        }
    }
    assert_eq!(
        audio,
        [
            "refusal",
            "private source",
            "latm configuration",
            "latm native"
        ]
    );
}

#[test]
fn seeking_replays_the_current_audio_configurations_in_a_new_generation() {
    let (mut player, _) = player(RETYPE, &[AVC, FOLLOW_AUDIO], Selection::All, 97);
    let initial = collect(&mut player, 5);
    let last_audio = initial
        .iter()
        .rev()
        .find(|item| {
            matches!(
                item.event(), RawEvent::NativeTs { packet, .. } if packet.pid == 257
            )
        })
        .unwrap();
    assert!(player.may_deliver(last_audio));
    assert!(
        player
            .seek_and_select(
                key(256),
                Time::new(0, 1).unwrap(),
                Selection::All,
                &mut WorkBudget::new(1),
                &AtomicBool::new(false),
            )
            .unwrap()
    );
    assert!(!player.may_deliver(last_audio));
    let replay = collect(&mut player, 3);
    let carriages: Vec<_> = replay
        .iter()
        .filter_map(|item| match item.event() {
            RawEvent::NativeTsConfiguration(config) => Some(config.carriage),
            _ => None,
        })
        .collect();
    assert_eq!(
        carriages,
        [
            AC3.carriage,
            TsNativeCarriage::Registered(NativeRegisteredCarriage::AacLatm)
        ]
    );
}

#[test]
fn selections_reject_duplicate_and_out_of_scope_pids_and_strides() {
    let b = budget();
    assert!(matches!(
        engine(&b, &[AVC, AVC], PacketFormat::Ts188, NAMESPACE),
        Err(Error::Invalid("duplicate native program selection PID"))
    ));
    let invalid = TsNativeTrackSelection { pid: 0x1fff, ..AVC };
    assert!(matches!(
        engine(&b, &[invalid], PacketFormat::Ts188, NAMESPACE),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        engine(&b, &[], PacketFormat::Ts188, NAMESPACE),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        engine(&b, &[AVC; 65], PacketFormat::Ts188, NAMESPACE),
        Err(Error::Limit { .. })
    ));
    assert!(matches!(
        engine(&b, &[AVC, AAC], PacketFormat::M2ts192, NAMESPACE),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
