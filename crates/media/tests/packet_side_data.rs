//! Additional typed-API feature guards, activated only with the private overlay.
//! Opaque payloads establish wire/ownership behavior, not coded-frame decoding.
use std::{io::Cursor, sync::atomic::AtomicBool};
use triergon::{
    Error, Limits,
    incremental::ParseStep,
    typed::{BlockingReader, Event, Reader},
};
use triergon_core::{Budget, Bytes, InputWindow, SourceCapabilities, SourceId, WorkBudget};

fn e(id: u64, bytes: &[u8]) -> Vec<u8> {
    let mut out: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|byte| *byte == 0)
        .collect();
    let width = (1..=8)
        .find(|width| (bytes.len() as u64) < (1_u64 << (7 * width)) - 1)
        .unwrap();
    out.extend(((1_u64 << (7 * width)) | bytes.len() as u64).to_be_bytes()[8 - width..].iter());
    out.extend(bytes);
    out
}
fn u(id: u64, value: u64) -> Vec<u8> {
    e(id, &value.to_be_bytes())
}
fn track(extra: &[u8]) -> Vec<u8> {
    e(
        0xae,
        &[
            u(0xd7, 1),
            u(0x73c5, 1),
            u(0x83, 1),
            e(0x86, b"V_AUTHORED/OPAQUE"),
            e(0x63a2, b"initial"),
            extra.to_vec(),
        ]
        .concat(),
    )
}
fn file(extra: &[u8], groups: &[Vec<u8>]) -> Vec<u8> {
    let mut cluster = u(0xe7, 7);
    for group in groups {
        cluster.extend(group);
    }
    let payload = [e(0x1654ae6b, &track(extra)), e(0x1f43b675, &cluster)].concat();
    [
        e(0x1a45dfa3, &e(0x4282, b"matroska")),
        e(0x18538067, &payload),
    ]
    .concat()
}
fn block() -> Vec<u8> {
    e(0xa1, &[0x81, 0, 0, 0, b'V'])
}
fn additions(mores: &[Vec<u8>]) -> Vec<u8> {
    e(0x75a1, &mores.concat())
}
fn more(id: Option<&[u8]>, bytes: &[u8]) -> Vec<u8> {
    let mut raw = Vec::new();
    if let Some(id) = id {
        raw.extend(e(0xee, id));
    }
    raw.extend(e(0xa5, bytes));
    e(0xa6, &raw)
}
fn drive(raw: &[u8], known: bool, events: &mut Vec<Event>) -> triergon::Result<()> {
    let pool = Budget::new(256 * 1024)?;
    let mut reader = Reader::new(
        &pool,
        SourceId(17),
        SourceCapabilities {
            length: known.then_some(raw.len() as u64),
            random_access: true,
        },
        Limits::default(),
        12,
    )?;
    let cancel = AtomicBool::new(false);
    for _ in 0..200_000 {
        let mut work = WorkBudget::new(1);
        let step = reader.step(&mut work, &cancel)?;
        assert!(work.spent() <= 1);
        match step {
            ParseStep::Yield => (),
            ParseStep::Event(event) => events.push(event),
            ParseStep::Finished => return Ok(()),
            ParseStep::NeedInput(request) => {
                let at = usize::try_from(request.offset).unwrap();
                assert!(at <= raw.len());
                if at == raw.len() {
                    reader.finish(request.source, request.token)?;
                } else {
                    assert_ne!(request.maximum, 0);
                    let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1])?;
                    reader.provide(&InputWindow::new(
                        request.source,
                        request.token,
                        request.offset,
                        bytes,
                    )?)?;
                }
            }
            other => panic!("ample-pool work-one job must progress: {other:?}"),
        }
    }
    panic!("work-one job did not finish")
}
#[test]
fn laces_share_complete_addition_sources_and_mapping_fields_survive_reader_drop() {
    use triergon::typed::{ExtensionScope, Presence};
    let pool = Budget::new(256 * 1024).unwrap();
    let baseline = pool.used();
    let mapping = e(
        0x41e4,
        &[
            u(0x41f0, 4),
            u(0x41e7, u64::MAX),
            e(0x41a4, b"opaque name\0\0"),
            e(0x41ed, b"extra"),
            e(0x4fff, b"unknown-map"),
        ]
        .concat(),
    );
    let block = e(0xa1, &[0x81, 0, 0, 4, 1, b'A', b'B']);
    let encoded_more = more(Some(&[]), b"default-id");
    let group = e(
        0xa0,
        &[
            block,
            additions(&[encoded_more.clone(), more(Some(&[4]), b"opaque-4")]),
            e(0x4fff, b"group-extension"),
        ]
        .concat(),
    );
    let raw = file(&[u(0x55ee, 4), mapping.clone()].concat(), &[group]);
    let mut reader = BlockingReader::open_in(Cursor::new(raw), Limits::default(), &pool).unwrap();
    let cancel = AtomicBool::new(false);
    let a = reader.next_typed_packet(&cancel).unwrap().unwrap();
    let b = reader.next_typed_packet(&cancel).unwrap().unwrap();
    assert_eq!(a.packet.data.as_ref(), b"A");
    assert_eq!(b.packet.data.as_ref(), b"B");
    let source = a.group.as_ref().unwrap();
    assert!(source.shares_owner_with(b.group.as_ref().unwrap()));
    assert_eq!(
        source
            .additions()
            .map(|value| value.id())
            .collect::<Vec<_>>(),
        [1, 4]
    );
    let first = source.additions().next().unwrap();
    assert_eq!(first.id_presence(), Presence::Empty);
    assert_eq!(first.data(), b"default-id");
    assert_eq!(first.raw_more(), encoded_more.as_slice());
    let extension = source.extensions().next().unwrap();
    assert_eq!(extension.id(), 0x4fff);
    assert_eq!(extension.scope(), ExtensionScope::Group);
    assert_eq!(extension.data(), b"group-extension");
    let extensions = finish_mapping(
        a.configuration
            .track_extensions_job(0, Limits::default())
            .unwrap(),
    );
    assert_eq!(
        extensions.raw_track_entry().as_ptr(),
        a.configuration.tracks()[0].source_metadata.as_ptr()
    );
    let info = extensions.mappings().next().unwrap();
    assert_eq!(info.value(), Some(4));
    assert_eq!(info.kind(), u64::MAX);
    assert_eq!(info.name().unwrap(), Some("opaque name"));
    assert_eq!(info.raw_name(), Some(&b"opaque name\0\0"[..]));
    assert_eq!(info.extra_data(), Some(&b"extra"[..]));
    assert_eq!(info.raw(), mapping.as_slice());
    assert_eq!(
        extensions.unknown_mapping_fields().next().unwrap().data(),
        b"unknown-map"
    );
    use triergon::typed::{BlockAdditionContainer::Matroska, NativeBlockAddition};
    let before_projection = pool.used();
    for packet in [&a, &b] {
        assert!(matches!(
            packet
                .project_native_block_additions_v1(
                    &extensions,
                    Matroska,
                    &mut WorkBudget::new(0),
                    &cancel,
                )
                .unwrap(),
            ParseStep::Yield
        ));
        let ParseStep::Event([Some(projected), None]) = packet
            .project_native_block_additions_v1(
                &extensions,
                Matroska,
                &mut WorkBudget::new(18),
                &cancel,
            )
            .unwrap()
        else {
            panic!("expected one native opaque addition");
        };
        assert_eq!(
            projected,
            NativeBlockAddition::Opaque {
                id: u64::MAX,
                payload: b"opaque-4"
            }
        );
        let (prefix, data) = projected.wire_parts();
        assert_eq!(prefix, Some(u64::MAX.to_be_bytes()));
        assert_eq!(
            data.as_ptr(),
            source.additions().nth(1).unwrap().data().as_ptr()
        );
    }
    assert_eq!(pool.used(), before_projection);
    let group_alias = source.clone();
    let before_clone = pool.used();
    let another = group_alias.clone();
    assert_eq!(
        pool.used(),
        before_clone,
        "aliases cannot charge a second shared allocation"
    );
    drop((reader, a, b, another));
    assert!(pool.used() > baseline);
    assert_eq!(group_alias.additions().count(), 2);
    assert_eq!(
        extensions.mappings().next().unwrap().extra_data(),
        Some(&b"extra"[..])
    );
    drop((group_alias, extensions));
    assert_eq!(pool.used(), baseline);
}

#[test]
fn native_projection_keeps_lcevc_and_rejects_foreign_mapping_context() {
    use triergon::typed::{BlockAdditionContainer::Matroska, NativeBlockAddition};

    let mapped = e(0x41e4, &[u(0x41f0, 4), u(0x41e7, 4)].concat());
    let config = [u(0x55ee, 4), mapped].concat();
    let packet = e(
        0xa0,
        &[
            block(),
            additions(&[
                more(Some(&[4]), b"\xb4\x00\x50\x00ab"),
                more(None, b"opaque"),
            ]),
        ]
        .concat(),
    );
    let raw = file(&config, &[packet]);
    let cancel = AtomicBool::new(false);
    let open = || {
        let mut reader = BlockingReader::open(Cursor::new(&raw), Limits::default()).unwrap();
        let packet = reader.next_typed_packet(&cancel).unwrap().unwrap();
        let extensions = finish_mapping(
            packet
                .configuration
                .track_extensions_job(0, Limits::default())
                .unwrap(),
        );
        (packet, extensions)
    };
    let (packet, extensions) = open();
    let (_, foreign) = open();
    assert!(matches!(
        packet.project_native_block_additions_v1(
            &foreign,
            Matroska,
            &mut WorkBudget::new(18),
            &cancel
        ),
        Err(Error::Invalid("packet additions TrackEntry mismatch"))
    ));
    let mut work = WorkBudget::new(18);
    let ParseStep::Event(result) = packet
        .project_native_block_additions_v1(&extensions, Matroska, &mut work, &cancel)
        .unwrap()
    else {
        panic!("projection has its exact grant");
    };
    assert_eq!(
        result,
        [
            Some(NativeBlockAddition::Lcevc(b"ab")),
            Some(NativeBlockAddition::Opaque {
                id: 1,
                payload: b"opaque"
            })
        ]
    );
    assert_eq!(work.spent(), 18);
    assert!(matches!(
        packet.project_native_block_additions_v1(
            &extensions,
            Matroska,
            &mut work,
            &AtomicBool::new(true)
        ),
        Err(Error::Cancelled)
    ));
}

#[test]
fn codec_state_has_exact_owned_bytes_and_preserves_initial_track_evidence() {
    use triergon::typed::InitializationOrigin;
    let pool = Budget::new(2 * 1024 * 1024).unwrap();
    let baseline = pool.used();
    let body = [block(), e(0xec, &vec![0; 128 * 1024]), e(0xa4, b"new")].concat();
    let raw = file(&[], &[e(0xa0, &body)]);
    let ebml_header = e(0x1a45dfa3, &e(0x4282, b"matroska"));
    let tracks = e(0x1654ae6b, &track(&[]));
    let timestamp = u(0xe7, 7);
    let cluster_body = [timestamp.clone(), e(0xa0, &body)].concat();
    let cluster = e(0x1f43b675, &cluster_body);
    let segment_body = [tracks.clone(), cluster.clone()].concat();
    let segment_data =
        (ebml_header.len() + e(0x18538067, &segment_body).len() - segment_body.len()) as u64;
    let group_header = segment_data
        + tracks.len() as u64
        + (cluster.len() - cluster_body.len()) as u64
        + timestamp.len() as u64;
    let group_data = group_header + (e(0xa0, &body).len() - body.len()) as u64;
    let state_header = group_data + (block().len() + e(0xec, &vec![0; 128 * 1024]).len()) as u64;
    let mut reader = BlockingReader::open_in(Cursor::new(raw), Limits::default(), &pool).unwrap();
    let cancel = AtomicBool::new(false);
    let packet = reader.next_typed_packet(&cancel).unwrap().unwrap();
    assert_eq!(packet.configuration.revision(), 2);
    let state = packet.configuration.codec_state(0).unwrap().clone();
    assert_eq!(state.data(), b"new");
    assert_eq!(
        state.retained_capacity(),
        3,
        "a tiny initialization must not retain the whole BlockGroup buffer"
    );
    let position = state.position().unwrap();
    assert_eq!(position.source, SourceId(1));
    assert_eq!(position.header, state_header);
    assert_eq!(position.data, state_header + 2);
    assert_eq!(position.length, 3);
    assert_eq!(position.segment_position, state_header - segment_data);
    assert_eq!(
        packet
            .group
            .as_ref()
            .unwrap()
            .position()
            .unwrap()
            .unwrap()
            .header,
        group_header
    );
    assert_eq!(
        packet.configuration.tracks()[0].codec_private.as_ref(),
        b"initial"
    );
    let initialization = packet.configuration.initialization(0).unwrap();
    assert_eq!(initialization.data, b"new");
    assert_eq!(
        initialization.origin,
        InitializationOrigin::CodecState(Some(position))
    );
    assert!(
        packet.configuration.tracks()[0]
            .source_metadata
            .windows(7)
            .any(|value| value == b"initial")
    );
    drop((packet, reader));
    assert_eq!(pool.used(), baseline + 3 + Bytes::owner_overhead_bytes());
    assert_eq!(state.data(), b"new");
    drop(state);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn default_maximum_prohibits_additions_and_type_zero_conflict_is_not_normalized() {
    for maximum in [Vec::new(), e(0x55ee, &[]), u(0x55ee, 0)] {
        let raw = file(
            &maximum,
            &[e(0xa0, &[block(), additions(&[more(None, b"a")])].concat())],
        );
        let mut events = Vec::new();
        assert!(matches!(
            drive(&raw, true, &mut events),
            Err(Error::Invalid("BlockAddID exceeds track maximum"))
        ));
        assert!(!events.iter().any(|event| matches!(event, Event::Packet(_))));
    }
    let mapping = e(
        0x41e4,
        &[u(0x41f0, 2), e(0x41e7, &[]), e(0x41ed, b"opaque")].concat(),
    );
    let raw = file(&mapping, &[e(0xa0, &block())]);
    let mut reader = BlockingReader::open(Cursor::new(raw), Limits::default()).unwrap();
    let packet = reader
        .next_typed_packet(&AtomicBool::new(false))
        .unwrap()
        .unwrap();
    let result = finish_mapping(
        packet
            .configuration
            .track_extensions_job(0, Limits::default())
            .unwrap(),
    );
    let info = result.mappings().next().unwrap();
    assert_eq!(info.value(), Some(2));
    assert_eq!(info.kind(), 0);
    assert!(info.type_zero_value_conflict());
    assert_eq!(
        info.raw(),
        mapping.as_slice(),
        "conflicting primary meanings cannot rewrite the source"
    );
}

#[test]
fn state_revisions_are_track_local_and_do_not_change_parser_generation_or_clocks() {
    let entry2 = e(
        0xae,
        &[
            u(0xd7, 2),
            u(0x73c5, 2),
            u(0x83, 2),
            e(0x86, b"A_AUTHORED/OPAQUE"),
            e(0x63a2, b"audio-initial"),
        ]
        .concat(),
    );
    let tracks = e(0x1654ae6b, &[track(&[]), entry2].concat());
    let groups = [
        e(0xa0, &[block(), e(0xa4, b"video-new")].concat()),
        e(
            0xa0,
            &[e(0xa1, &[0x82, 0, 0, 0, b'A']), e(0xa4, b"audio-new")].concat(),
        ),
    ];
    let cluster = e(0x1f43b675, &[u(0xe7, 7), groups.concat()].concat());
    let raw = [
        e(0x1a45dfa3, &e(0x4282, b"matroska")),
        e(0x18538067, &[tracks, cluster].concat()),
    ]
    .concat();
    let mut events = Vec::new();
    drive(&raw, false, &mut events).unwrap();
    let configurations: Vec<_> = events
        .iter()
        .filter_map(|event| {
            if let Event::Configuration(value) = event {
                Some(value)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(configurations.len(), 3);
    assert!(
        configurations
            .iter()
            .all(|configuration| configuration.generation() == configurations[0].generation())
    );
    assert_eq!(
        configurations[0].initialization(0).unwrap().data,
        b"initial"
    );
    assert_eq!(
        configurations[1].initialization(0).unwrap().data,
        b"video-new"
    );
    assert_eq!(
        configurations[1].initialization(1).unwrap().data,
        b"audio-initial"
    );
    assert_eq!(
        configurations[2].initialization(0).unwrap().data,
        b"video-new"
    );
    assert_eq!(
        configurations[2].initialization(1).unwrap().data,
        b"audio-new"
    );
    for event in &events {
        if let Event::Packet(packet) = event {
            assert_eq!(packet.packet.pts_ns, Some(7_000_000));
            assert!(packet.packet.dts_ns.is_none());
        }
    }
}

fn finish_mapping(
    mut job: triergon::typed::TrackExtensionsJob,
) -> triergon::typed::TrackExtensions {
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(1);
        let result = job.step(&mut work, &AtomicBool::new(false)).unwrap();
        assert!(work.spent() <= 1);
        match result {
            ParseStep::Event(value) => return value,
            ParseStep::Yield => (),
            other => panic!("mapping: {other:?}"),
        }
    }
    panic!("bounded mapping job did not finish")
}

fn epoch_fixture(indexed: bool, forged: u8) -> Vec<u8> {
    let head = |position| {
        e(
            0x114d9b74,
            &e(
                0x4dbb,
                &[u(0x53ab, 0x1c53bb6b), u(0x53ac, position)].concat(),
            ),
        )
    };
    let audio = e(
        0xae,
        &[
            u(0xd7, 2),
            u(0x73c5, 2),
            u(0x83, 2),
            e(0x86, b"A_AUTHORED/OPAQUE"),
            e(0x63a2, b"audio-initial"),
            u(0x56bb, 15_000_000),
        ]
        .concat(),
    );
    let tracks = e(0x1654ae6b, &[track(&[]), audio].concat());
    let simple = |number: u8, value: u8| e(0xa3, &[0x80 | number, 0, 0, 0x80, value]);
    let state_group = |number: u8, value: u8, state: &[u8]| {
        let block = e(0xa1, &[0x80 | number, 0, 0, 0, value]);
        let body = [block.clone(), e(0xa4, state)].concat();
        let group = e(0xa0, &body);
        let at = group.len() - body.len() + block.len();
        (group, at)
    };
    let (va, va_at) = state_group(1, b'1', b"video-A");
    let (aa, aa_at) = state_group(2, b'c', b"audio-A");
    let (vb, vb_at) = state_group(1, b'3', b"video-B");
    let bodies = [
        [u(0xe7, 0), simple(1, b'0'), simple(2, b'a')].concat(),
        [u(0xe7, 10), va, simple(2, b'b')].concat(),
        [u(0xe7, 20), aa, simple(1, b'2')].concat(),
        [u(0xe7, 30), vb, simple(2, b'd')].concat(),
        [u(0xe7, 40), simple(1, b'4'), simple(2, b'e')].concat(),
    ];
    let clusters: Vec<_> = bodies.iter().map(|body| e(0x1f43b675, body)).collect();
    let mut positions = Vec::new();
    let mut at = tracks.len() + if indexed { head(0).len() } else { 0 };
    for cluster in &clusters {
        positions.push(at as u64);
        at += cluster.len();
    }
    let state_at = |index: usize, offset: usize| {
        positions[index]
            + (clusters[index].len() - bodies[index].len()
                + u(0xe7, index as u64 * 10).len()
                + offset) as u64
    };
    let video_a = state_at(1, va_at);
    let audio_a = state_at(2, aa_at);
    let video_b = state_at(3, vb_at);
    let states = [
        0,
        video_a,
        match forged {
            1 => video_a + 2,
            2 => audio_a,
            3 => 0,
            _ => video_a,
        },
        video_b,
        video_b,
    ];
    let points: Vec<_> = positions
        .iter()
        .enumerate()
        .map(|(index, position)| {
            e(
                0xbb,
                &[
                    u(0xb3, index as u64 * 10),
                    e(
                        0xb7,
                        &[u(0xf7, 1), u(0xf1, *position), u(0xea, states[index])].concat(),
                    ),
                ]
                .concat(),
            )
        })
        .collect();
    let payload = [
        if indexed { head(at as u64) } else { Vec::new() },
        tracks,
        clusters.concat(),
        if indexed {
            e(0x1c53bb6b, &points.concat())
        } else {
            Vec::new()
        },
    ]
    .concat();
    [
        e(0x1a45dfa3, &e(0x4282, b"matroska")),
        e(0x18538067, &payload),
    ]
    .concat()
}

#[test]
fn plain_and_preroll_seek_reconstruct_track_states_at_the_actual_replay_boundary() {
    for indexed in [false, true] {
        for after_eof in [false, true] {
            let mut reader = BlockingReader::open_seekable(
                Cursor::new(epoch_fixture(indexed, 0)),
                Limits::default(),
            )
            .unwrap();
            let cancel = AtomicBool::new(false);
            if after_eof {
                while reader.next_typed_packet(&cancel).unwrap().is_some() {}
            }
            let receipt = reader.seek_previous(0, 25_000_000, &cancel).unwrap();
            assert_eq!(receipt.actual_ns, 20_000_000);
            let packet = reader.next_typed_packet(&cancel).unwrap().unwrap();
            assert_eq!(packet.packet.data.as_ref(), b"2");
            assert_eq!(
                packet.configuration.initialization(0).unwrap().data,
                b"video-A"
            );
            assert_eq!(
                packet.configuration.initialization(1).unwrap().data,
                b"audio-A"
            );
            let old_generation = packet.configuration.generation();
            let receipt = reader.seek_previous(0, 0, &cancel).unwrap();
            assert_eq!(receipt.actual_ns, 0);
            let initial = reader.next_typed_packet(&cancel).unwrap().unwrap();
            assert_eq!(initial.packet.data.as_ref(), b"0");
            assert_eq!(
                initial.configuration.initialization(0).unwrap().data,
                b"initial"
            );
            assert_eq!(
                initial.configuration.initialization(1).unwrap().data,
                b"audio-initial"
            );
            assert_ne!(initial.configuration.generation(), old_generation);
            assert_eq!(
                packet.configuration.initialization(0).unwrap().data,
                b"video-A",
                "older consumer snapshots remain immutable"
            );
            let receipt = reader.seek_for_playback(0, 35_000_000, &cancel).unwrap();
            assert_eq!(receipt.actual_ns, 20_000_000);
            assert_eq!(receipt.preroll_ns, 15_000_000);
            let first = reader.next_typed_packet(&cancel).unwrap().unwrap();
            assert_eq!(first.packet.track, 1);
            assert_eq!(first.packet.data.as_ref(), b"c");
            assert_eq!(
                first.configuration.initialization(0).unwrap().data,
                b"video-A"
            );
            assert_eq!(
                first.configuration.initialization(1).unwrap().data,
                b"audio-A"
            );
        }
    }
}

#[test]
fn cue_state_payload_wrong_track_and_false_initial_pointers_are_rejected_before_receipt() {
    for forged in [1, 2, 3] {
        let mut reader = BlockingReader::open_seekable(
            Cursor::new(epoch_fixture(true, forged)),
            Limits::default(),
        )
        .unwrap();
        let result = reader.seek_previous(0, 25_000_000, &AtomicBool::new(false));
        assert!(
            matches!(
                result,
                Err(Error::Invalid(
                    "cue CodecState does not match referenced block"
                ))
            ),
            "forged pointer kind {forged}: {result:?}"
        );
    }
}

#[test]
fn compatibility_packet_shape_explicitly_refuses_side_data_instead_of_discarding_it() {
    let raw = file(
        &u(0x55ee, 1),
        &[e(
            0xa0,
            &[block(), additions(&[more(None, b"required-opaque")])].concat(),
        )],
    );
    let mut reader = BlockingReader::open(Cursor::new(raw), Limits::default()).unwrap();
    assert!(matches!(
        reader.next_packet(&AtomicBool::new(false)),
        Err(Error::Unsupported(
            "Block additions/CodecState require next_typed_packet"
        ))
    ));
}

#[test]
fn malformed_mapping_name_rejects_the_entire_initial_configuration_transaction() {
    let mapping = e(
        0x41e4,
        &[u(0x41f0, 4), u(0x41e7, 1), e(0x41a4, b"bad\0tail")].concat(),
    );
    let raw = file(&[u(0x55ee, 4), mapping].concat(), &[e(0xa0, &block())]);
    let mut events = Vec::new();
    assert!(matches!(
        drive(&raw, false, &mut events),
        Err(Error::Invalid("metadata string terminator"))
    ));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Configuration(_) | Event::Packet(_)))
    );
}

#[test]
fn unknown_more_children_preserve_scope_and_cannot_become_a_group_codec_state() {
    use triergon::typed::ExtensionScope;
    let nested = e(
        0xa6,
        &[e(0xa4, b"opaque-nested-state"), e(0xa5, b"one")].concat(),
    );
    let additions = e(
        0x75a1,
        &[e(0x4fff, b"additions-extension"), nested].concat(),
    );
    let raw = file(
        &u(0x55ee, 1),
        &[e(
            0xa0,
            &[block(), additions, e(0x4fff, b"group-extension")].concat(),
        )],
    );
    let mut reader = BlockingReader::open(Cursor::new(raw), Limits::default()).unwrap();
    let packet = reader
        .next_typed_packet(&AtomicBool::new(false))
        .unwrap()
        .unwrap();
    let source = packet.group.as_ref().unwrap();
    assert!(source.codec_state().is_none());
    assert!(packet.configuration.codec_state(0).is_none());
    let scopes: Vec<_> = source.extensions().map(|field| field.scope()).collect();
    assert_eq!(
        scopes,
        [
            ExtensionScope::Additions,
            ExtensionScope::More(0),
            ExtensionScope::Group
        ]
    );
    let nested = source.extensions().nth(1).unwrap();
    assert_eq!(nested.id(), 0xa4);
    assert_eq!(nested.data(), b"opaque-nested-state");
    assert_eq!(source.additions().next().unwrap().data(), b"one");
}

#[test]
fn initial_private_encoding_does_not_imply_codec_state_or_additional_encoding() {
    let abc = [120, 1, 1, 3, 0, 252, 255, b'a', b'b', b'c', 2, 77, 1, 39];
    let encoding = e(
        0x6d80,
        &e(0x6240, &[u(0x5032, 2), e(0x5034, &u(0x4254, 0))].concat()),
    );
    // Build one CodecPrivate (rather than the convenience track()'s initial).
    let entry = e(
        0xae,
        &[
            u(0xd7, 1),
            u(0x73c5, 1),
            u(0x83, 1),
            e(0x86, b"V_AUTHORED/OPAQUE"),
            e(0x63a2, &abc),
            u(0x55ee, 1),
            encoding,
        ]
        .concat(),
    );
    let group = e(
        0xa0,
        &[block(), e(0xa4, &abc), additions(&[more(None, &abc)])].concat(),
    );
    let raw = [
        e(0x1a45dfa3, &e(0x4282, b"matroska")),
        e(
            0x18538067,
            &[
                e(0x1654ae6b, &entry),
                e(0x1f43b675, &[u(0xe7, 7), group].concat()),
            ]
            .concat(),
        ),
    ]
    .concat();
    let mut reader = BlockingReader::open(Cursor::new(raw), Limits::default()).unwrap();
    let packet = reader
        .next_typed_packet(&AtomicBool::new(false))
        .unwrap()
        .unwrap();
    assert_eq!(
        packet.configuration.tracks()[0].codec_private.as_ref(),
        b"abc"
    );
    assert_eq!(packet.configuration.codec_state(0).unwrap().data(), abc);
    assert_eq!(
        packet
            .group
            .as_ref()
            .unwrap()
            .additions()
            .next()
            .unwrap()
            .data(),
        abc
    );
}

fn initial_epoch_fixture(video_state: bool, false_zero: bool) -> (Vec<u8>, usize, usize) {
    let head = |position| {
        e(
            0x114d9b74,
            &e(
                0x4dbb,
                &[u(0x53ab, 0x1c53bb6b), u(0x53ac, position)].concat(),
            ),
        )
    };
    let audio = e(
        0xae,
        &[
            u(0xd7, 2),
            u(0x73c5, 2),
            u(0x83, 2),
            e(0x86, b"A_AUTHORED/OPAQUE"),
            e(0x63a2, b"audio-initial"),
        ]
        .concat(),
    );
    let tracks = e(0x1654ae6b, &[track(&[]), audio].concat());
    let group = |number: u8, offset: i16, value: u8, state: Option<&[u8]>| {
        let ticks = offset.to_be_bytes();
        let block = e(0xa1, &[0x80 | number, ticks[0], ticks[1], 0, value]);
        let body = [
            block.clone(),
            state.map_or(Vec::new(), |value| e(0xa4, value)),
        ]
        .concat();
        let encoded = e(0xa0, &body);
        let state_offset = encoded.len() - body.len() + block.len();
        (encoded, state_offset)
    };
    let (audio_a, _) = group(2, 0, b'A', Some(b"audio-A"));
    let (video_a, first_state_offset) = group(1, 5, b'V', video_state.then_some(&b"video-A"[..]));
    let first_body = [u(0xe7, 0), audio_a.clone(), video_a].concat();
    let first = e(0x1f43b675, &first_body);
    let (audio_b, _) = group(2, 0, b'B', Some(b"audio-B"));
    let (video_b, last_state_offset) = group(1, 0, b'W', Some(b"video-B"));
    let last_body = [u(0xe7, 20), audio_b.clone(), video_b].concat();
    let last = e(0x1f43b675, &last_body);
    let first_position = (head(0).len() + tracks.len()) as u64;
    let last_position = first_position + first.len() as u64;
    let first_state = if video_state && !false_zero {
        first_position
            + (first.len() - first_body.len()
                + u(0xe7, 0).len()
                + audio_a.len()
                + first_state_offset) as u64
    } else {
        0
    };
    let last_state = last_position
        + (last.len() - last_body.len() + u(0xe7, 20).len() + audio_b.len() + last_state_offset)
            as u64;
    let cue = |ticks, position, state| {
        e(
            0xbb,
            &[
                u(0xb3, ticks),
                e(
                    0xb7,
                    &[u(0xf7, 1), u(0xf1, position), u(0xea, state)].concat(),
                ),
            ]
            .concat(),
        )
    };
    let cues = e(
        0x1c53bb6b,
        &[
            cue(5, first_position, first_state),
            cue(20, last_position, last_state),
        ]
        .concat(),
    );
    let count = first.len();
    let index_bytes = cues.len();
    let payload = [
        head(last_position + last.len() as u64),
        tracks,
        first,
        last,
        cues,
    ]
    .concat();
    (
        [
            e(0x1a45dfa3, &e(0x4282, b"matroska")),
            e(0x18538067, &payload),
        ]
        .concat(),
        count,
        index_bytes,
    )
}

#[test]
fn whole_first_cluster_replay_restores_all_tracks_without_a_second_input_pass() {
    let cancel = AtomicBool::new(false);
    for video_state in [false, true] {
        for after_eof in [false, true] {
            let (raw, first_cluster_bytes, index_bytes) = initial_epoch_fixture(video_state, false);
            let mut reader =
                BlockingReader::open_seekable(Cursor::new(raw), Limits::default()).unwrap();
            if after_eof {
                while reader.next_typed_packet(&cancel).unwrap().is_some() {}
            }
            let receipt = reader.seek_for_playback(0, 5_000_000, &cancel).unwrap();
            assert_eq!(receipt.actual_ns, 5_000_000);
            assert!(receipt.replays_from_start);
            let expected = first_cluster_bytes + if after_eof { 0 } else { index_bytes };
            assert_eq!(
                receipt.scanned_bytes, expected as u64,
                "one validated first Cluster plus the uncached index; no redundant epoch pass"
            );
            let audio = reader.next_typed_packet(&cancel).unwrap().unwrap();
            assert_eq!(audio.packet.track, 1);
            assert_eq!(audio.packet.data.as_ref(), b"A");
            assert_eq!(
                audio.configuration.initialization(0).unwrap().data,
                b"initial",
                "a later video state cannot leak into earlier audio preroll"
            );
            assert_eq!(
                audio.configuration.initialization(1).unwrap().data,
                b"audio-A"
            );
            let video = reader.next_typed_packet(&cancel).unwrap().unwrap();
            assert_eq!(video.packet.data.as_ref(), b"V");
            assert_eq!(
                video.configuration.initialization(0).unwrap().data,
                if video_state {
                    &b"video-A"[..]
                } else {
                    &b"initial"[..]
                }
            );
            assert_eq!(
                video.configuration.initialization(1).unwrap().data,
                b"audio-A"
            );
        }
    }
    let (raw, _, _) = initial_epoch_fixture(true, true);
    let mut reader = BlockingReader::open_seekable(Cursor::new(raw), Limits::default()).unwrap();
    assert!(
        matches!(
            reader.seek_for_playback(0, 5_000_000, &cancel),
            Err(Error::Invalid(
                "cue CodecState does not match referenced block"
            ))
        ),
        "a false initial-state Cue is still invalid in the first Cluster"
    );
}

// Independently packed App 4 control with one window and a 2 × 3 peak grid.
const HDR10_PLUS: &[u8] = &[
    0xb5, 0x0, 0x3c, 0x0, 0x1, 0x4, 0x1, 0x40, 0x0, 0xc, 0x84, 0x43, 0x12, 0x34, 0x56, 0x0, 0x32,
    0x0, 0x32, 0x0, 0x25, 0x80, 0x3, 0x21, 0xb4, 0x30, 0x39, 0x19, 0x10, 0x64, 0xc, 0x82, 0x19,
    0xc, 0x89, 0x0,
];

#[test]
fn hdr10_plus_preserves_wire_values_and_borrowed_grid() {
    use triergon::typed::Hdr10Plus;
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        Hdr10Plus::parse_t35_v1(HDR10_PLUS, &mut WorkBudget::new(0), &cancel).unwrap(),
        ParseStep::Yield
    ));
    let ParseStep::Event(metadata) =
        Hdr10Plus::parse_t35_v1(HDR10_PLUS, &mut WorkBudget::new(4096), &cancel).unwrap()
    else {
        panic!("bounded HDR10+ control");
    };
    assert_eq!(metadata.raw().as_ptr(), HDR10_PLUS.as_ptr());
    assert_eq!(metadata.body_bits(), 235);
    assert_eq!(
        (metadata.application_version, metadata.target_luminance),
        (1, 400)
    );
    let grid = metadata.target_grid.unwrap();
    assert_eq!((grid.rows(), grid.columns()), (2, 3));
    assert_eq!(grid.values().collect::<Vec<_>>(), [1, 2, 3, 4, 5, 6]);
    let window = &metadata.windows()[0];
    assert!(window.geometry.is_none() && metadata.mastering_grid.is_none());
    assert_eq!(window.max_scl, [100, 200, 300]);
    assert_eq!(window.average_max_rgb, 50);
    assert_eq!(window.percentiles()[0].max_rgb, 12345);
    assert_eq!(window.fraction_bright_pixels, 100);
    assert_eq!(window.tone_curve.unwrap().knee, [100, 200]);
    assert_eq!(window.tone_curve.unwrap().anchors(), [100, 200]);
    assert_eq!(window.saturation_weight, Some(8));
    for length in 6..HDR10_PLUS.len() {
        assert!(
            matches!(
                Hdr10Plus::parse_t35_v1(&HDR10_PLUS[..length], &mut WorkBudget::new(4096), &cancel),
                Err(Error::Truncated)
            ),
            "truncated prefix length {length}"
        );
    }
    let mut oversized = HDR10_PLUS.to_vec();
    oversized.resize(914, 0);
    assert!(matches!(
        Hdr10Plus::parse_t35_v1(&oversized, &mut WorkBudget::new(8192), &cancel),
        Err(Error::Limit("HDR10+ payload"))
    ));
}

#[test]
fn implicit_vp9_hdr10_plus_admission_is_bound_to_the_webm_track() {
    use triergon::typed::{BlockAdditionContainer::WebM, PayloadOwnership};
    use triergon_core::OwnedInputBytes;
    let cancel = AtomicBool::new(false);
    for (doctype, codec, maximum, profile, admitted) in [
        (b"webm".as_slice(), b"V_VP9".as_slice(), None, true, true),
        (b"webm".as_slice(), b"V_VP9".as_slice(), None, false, false),
        (
            b"matroska".as_slice(),
            b"V_VP9".as_slice(),
            None,
            true,
            false,
        ),
        (b"webm".as_slice(), b"V_AV1".as_slice(), None, true, false),
        (
            b"webm".as_slice(),
            b"V_VP9".as_slice(),
            Some(0),
            true,
            false,
        ),
    ] {
        let mut track = [u(0xd7, 1), u(0x73c5, 1), u(0x83, 1), e(0x86, codec)].concat();
        if let Some(maximum) = maximum {
            track.extend(u(0x55ee, maximum));
        }
        let group = e(
            0xa0,
            &[block(), additions(&[more(Some(&[4]), HDR10_PLUS)])].concat(),
        );
        let cluster = e(0x1f43b675, &[u(0xe7, 0), group].concat());
        let data = [
            e(0x1a45dfa3, &e(0x4282, doctype)),
            e(
                0x18538067,
                &[e(0x1654ae6b, &e(0xae, &track)), cluster].concat(),
            ),
        ]
        .concat();
        let pool = Budget::new(256 * 1024).unwrap();
        let source = OwnedInputBytes::try_from_vec(&pool, data).unwrap();
        let open = if profile {
            BlockingReader::open_owned_seekable_webm_vp9_t35_v1
        } else {
            BlockingReader::open_owned_seekable_in
        };
        let mut reader = open(
            source,
            Limits::default(),
            &pool,
            PayloadOwnership::SharedBounded,
        )
        .unwrap();
        let result = reader.next_typed_packet(&cancel);
        if !admitted {
            assert!(matches!(
                result,
                Err(Error::Invalid("BlockAddID exceeds track maximum"))
            ));
            continue;
        }
        let packet = result.unwrap().unwrap();
        let map = finish_mapping(
            packet
                .configuration
                .track_extensions_job(0, Limits::default())
                .unwrap(),
        );
        let ParseStep::Event(Some(hdr)) = packet
            .project_native_hdr10_plus_v1(&map, WebM, &mut WorkBudget::new(4096), &cancel)
            .unwrap()
        else {
            panic!("implicit T.35 event");
        };
        assert_eq!(hdr.raw(), HDR10_PLUS);
        assert_eq!(map.max_block_addition_id(), 0);
        assert!(reader.next_typed_packet(&cancel).unwrap().is_none());
        let seek = reader.seek_for_playback(0, 0, &cancel).unwrap();
        assert_eq!(seek.actual_ns, 0);
        let replay = reader.next_typed_packet(&cancel).unwrap().unwrap();
        assert_eq!(
            replay.group.unwrap().additions().next().unwrap().data(),
            HDR10_PLUS
        );
    }
}
