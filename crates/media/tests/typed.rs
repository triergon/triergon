//! Runtime contracts for the typed reader.
use std::sync::atomic::AtomicBool;
use triergon_core::Step;
use triergon_core::{
    Budget, Bytes, InputWindow, SourceCapabilities, SourceFailure, SourceId, WorkBudget,
};
use triergon_matroska::{
    Error, Limits,
    typed::{Event, Reader},
};

fn element(id: u64, data: &[u8]) -> Vec<u8> {
    let mut out: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    let width = (1..=8)
        .find(|w| (data.len() as u64) < (1_u64 << (7 * w)) - 1)
        .unwrap();
    out.extend(((1_u64 << (7 * width)) | data.len() as u64).to_be_bytes()[8 - width..].iter());
    out.extend(data);
    out
}
fn uint(id: u64, value: u64) -> Vec<u8> {
    element(id, &value.to_be_bytes())
}
fn entry(number: u64, kind: u64, extra: &[u8]) -> Vec<u8> {
    element(
        0xae,
        &[
            uint(0xd7, number),
            uint(0x73c5, number),
            uint(0x83, kind),
            element(0x86, b"V_AUTHORED/OPAQUE"),
            extra.to_vec(),
        ]
        .concat(),
    )
}
fn document(payload: &[u8]) -> Vec<u8> {
    [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(0x18538067, payload),
    ]
    .concat()
}
const ABC: [u8; 14] = [120, 1, 1, 3, 0, 252, 255, 97, 98, 99, 2, 77, 1, 39];
fn zlib_encoding(scope: u64) -> Vec<u8> {
    element(
        0x6d80,
        &element(
            0x6240,
            &[uint(0x5032, scope), element(0x5034, &uint(0x4254, 0))].concat(),
        ),
    )
}
fn stored_laced_source(bad: bool) -> Vec<u8> {
    let mut block = vec![0x81, 0, 0, 0x84, 1];
    block.extend(ABC);
    block.extend(ABC);
    if bad {
        *block.last_mut().unwrap() ^= 1;
    }
    let tracks = element(
        0x1654ae6b,
        &entry(
            1,
            1,
            &[
                zlib_encoding(1),
                element(0x63a2, b"private"),
                element(0x536e, b"authored name"),
                element(0x22b59d, b"fr-CA"),
            ]
            .concat(),
        ),
    );
    document(
        &[
            tracks,
            element(0x1f43b675, &[uint(0xe7, 2), element(0xa3, &block)].concat()),
        ]
        .concat(),
    )
}
fn reader(raw: &[u8], known: bool, limits: Limits, pool: &Budget) -> Reader {
    Reader::new(
        pool,
        SourceId(101),
        SourceCapabilities {
            length: known.then_some(raw.len() as u64),
            random_access: true,
        },
        limits,
        1,
    )
    .unwrap()
}
fn drive(
    reader: &mut Reader,
    raw: &[u8],
    mut event: impl FnMut(Event),
) -> triergon_matroska::Result<()> {
    let cancel = AtomicBool::new(false);
    for _ in 0..200_000 {
        let mut work = WorkBudget::new(1);
        let step = reader.step(&mut work, &cancel)?;
        assert!(work.spent() <= 1);
        match step {
            Step::Yield => (),
            Step::Ready(value) => event(value),
            Step::End(_) => return Ok(()),
            Step::NeedInput(request) => {
                let at = usize::try_from(request.offset).unwrap();
                assert!(at <= raw.len());
                if at == raw.len() {
                    reader.finish(request.source, request.token)?;
                } else {
                    let bytes = Bytes::copy_from_slice(reader.budget(), &raw[at..at + 1])?;
                    reader.provide(&InputWindow::new(
                        request.source,
                        request.token,
                        request.offset,
                        bytes,
                    )?)?;
                }
            }
            other => panic!("ample-pool source must progress: {other:?}"),
        }
    }
    panic!("typed operation did not complete under work-one scheduling")
}

#[test]
fn configuration_precedes_atomic_laced_packets_and_retains_original_source_views() {
    for known in [false, true] {
        let raw = stored_laced_source(false);
        let pool = Budget::new(128 * 1024).unwrap();
        let baseline = pool.used();
        let limits = Limits {
            block_bytes: 33,
            ..Limits::default()
        };
        let mut reader = reader(&raw, known, limits, &pool);
        let mut configuration = None;
        let mut packets = Vec::new();
        let mut packet_before_configuration = false;
        drive(&mut reader, &raw, |event| match event {
            Event::Configuration(value) => configuration = Some(value),
            Event::Packet(value) => {
                packet_before_configuration |= configuration.is_none();
                packets.push(value);
            }
            _ => (),
        })
        .unwrap();
        assert!(
            configuration.is_some(),
            "a complete Tracks transaction must publish typed configuration"
        );
        assert!(!packet_before_configuration);
        assert_eq!(packets.len(), 2, "both verified laces must publish");
        let configuration = configuration.unwrap();
        let track = &configuration.tracks()[0];
        assert_eq!(track.codec_private.as_ref(), b"private");
        assert!(
            track
                .codec_private
                .shares_storage_with(&track.source_metadata)
        );
        assert_eq!(track.effective_language(), "fr-CA");
        assert_eq!(track.name.as_str(), "authored name");
        assert_eq!(packets[0].packet.pts_ns, Some(2_000_000));
        assert_eq!(packets[1].packet.pts_ns, None);
        assert!(packets.iter().all(|p| p.packet.data.as_ref() == b"abc"));
        let old_generation = configuration.generation();
        reader.reset().unwrap();
        assert_ne!(reader.generation(), old_generation);
        drop(reader);
        assert!(pool.used() > baseline);
        assert_eq!(packets[0].configuration.generation(), old_generation);
        let alias = configuration.clone();
        assert_eq!(
            alias.tracks()[0].source_metadata.as_ptr(),
            configuration.tracks()[0].source_metadata.as_ptr()
        );
        drop((alias, configuration, packets));
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn late_lace_crc_failure_cannot_publish_a_packet_or_succeed_at_eof() {
    let raw = stored_laced_source(true);
    let pool = Budget::new(128 * 1024).unwrap();
    let mut reader = reader(&raw, false, Limits::default(), &pool);
    let mut packets = 0;
    let result = drive(&mut reader, &raw, |event| {
        if matches!(event, Event::Packet(_)) {
            packets += 1;
        }
    });
    assert!(
        matches!(result, Err(Error::Invalid("Adler-32 mismatch"))),
        "late lace checksum must be rejected, got {result:?}"
    );
    assert_eq!(packets, 0);
}

#[test]
fn duplicate_track_rejects_the_whole_configuration_transaction() {
    let tracks = element(0x1654ae6b, &[entry(1, 1, &[]), entry(1, 1, &[])].concat());
    let raw = document(&[tracks, element(0x1f43b675, &[])].concat());
    let pool = Budget::new(128 * 1024).unwrap();
    let mut reader = reader(&raw, false, Limits::default(), &pool);
    let mut configurations = 0;
    let result = drive(&mut reader, &raw, |event| {
        if matches!(event, Event::Configuration(_)) {
            configurations += 1;
        }
    });
    assert!(
        matches!(result, Err(Error::Invalid("duplicate track number"))),
        "duplicate TrackEntry must fail, got {result:?}"
    );
    assert_eq!(configurations, 0);
}

#[test]
fn metadata_catalogue_requires_bounded_full_validation_before_document_publication() {
    let atom = |uid| {
        element(
            0xb6,
            &[
                uint(0x73c4, uid),
                uint(0x91, 0),
                element(0x80, &element(0x85, "épisode".as_bytes())),
            ]
            .concat(),
        )
    };
    let chapters = element(0x1043a770, &element(0x45b9, &[atom(1), atom(1)].concat()));
    let raw = document(
        &[
            element(0x1654ae6b, &entry(1, 1, &[])),
            chapters,
            element(0x1f43b675, &[]),
        ]
        .concat(),
    );
    let pool = Budget::new(128 * 1024).unwrap();
    let mut reader = reader(&raw, false, Limits::default(), &pool);
    let mut documents = 0;
    let result = drive(&mut reader, &raw, |event| {
        if matches!(event, Event::Document(_)) {
            documents += 1;
        }
    });
    assert!(
        matches!(result, Err(Error::Invalid("duplicate metadata UID"))),
        "UID validation must really run before publication, got {result:?}"
    );
    assert_eq!(documents, 0);
}

#[test]
fn block_group_trailing_duration_reference_and_padding_apply_before_the_first_lace() {
    let group = element(
        0xa0,
        &[
            element(0xa1, &[0x81, 0, 5, 0x04, 1, b'A', b'B']),
            uint(0x9b, 4),
            element(0xfb, &[0xff]),
            element(0x75a2, &[0xff]),
        ]
        .concat(),
    );
    let raw = document(
        &[
            element(0x1654ae6b, &entry(1, 1, &[])),
            element(0x1f43b675, &[uint(0xe7, 10), group].concat()),
        ]
        .concat(),
    );
    let pool = Budget::new(128 * 1024).unwrap();
    let mut reader = reader(&raw, false, Limits::default(), &pool);
    let mut packets = Vec::new();
    drive(&mut reader, &raw, |event| {
        if let Event::Packet(packet) = event {
            packets.push(packet);
        }
    })
    .unwrap();
    assert_eq!(
        packets.len(),
        2,
        "the validated BlockGroup must emit two typed packets"
    );
    assert_eq!(packets[0].packet.pts_ns, Some(15_000_000));
    assert_eq!(packets[1].packet.pts_ns, Some(17_000_000));
    assert!(packets.iter().all(|p| !p.packet.keyframe
        && p.packet.duration_ns == Some(2_000_000)
        && p.packet.discard_padding_ns == -1));
}

#[test]
fn cues_and_all_track_preroll_produce_verified_receipt_and_replay_interleaved_audio() {
    let tracks = element(
        0x1654ae6b,
        &[entry(1, 1, &[]), entry(2, 2, &uint(0x56bb, 32_000_000))].concat(),
    );
    let first = element(
        0x1f43b675,
        &[
            uint(0xe7, 0),
            element(0xa3, &[0x82, 0, 0, 0x80, b'A']),
            element(0xa3, &[0x81, 0, 0, 0x80, b'V']),
        ]
        .concat(),
    );
    let second = element(
        0x1f43b675,
        &[uint(0xe7, 100), element(0xa3, &[0x81, 0, 0, 0x80, b'E'])].concat(),
    );
    let cue = |ticks, position| {
        element(
            0xbb,
            &[
                uint(0xb3, ticks),
                element(0xb7, &[uint(0xf7, 1), uint(0xf1, position)].concat()),
            ]
            .concat(),
        )
    };
    let cues = element(
        0x1c53bb6b,
        &[
            cue(0, tracks.len() as u64),
            cue(100, (tracks.len() + first.len()) as u64),
        ]
        .concat(),
    );
    let raw = document(&[tracks, first, second, cues].concat());
    let pool = Budget::new(256 * 1024).unwrap();
    let mut reader = reader(&raw, true, Limits::default(), &pool);
    drive(&mut reader, &raw, |_| ()).unwrap();
    let generation = reader.generation();
    reader.seek_for_playback(0, 90_000_000).unwrap();
    assert_ne!(reader.generation(), generation);
    let mut receipt = None;
    let mut packets = Vec::new();
    let mut packet_before_receipt = false;
    drive(&mut reader, &raw, |event| match event {
        Event::Seek(value) => receipt = Some(value),
        Event::Packet(value) => {
            packet_before_receipt |= receipt.is_none();
            packets.push(value);
        }
        _ => (),
    })
    .unwrap();
    assert!(
        receipt.is_some(),
        "a generation reset/Cluster replay alone is not a verified seek receipt"
    );
    let receipt = receipt.unwrap();
    assert!(!packet_before_receipt);
    assert_eq!(receipt.actual_ns, 0);
    assert_eq!(receipt.preroll_ns, 32_000_000);
    assert_eq!(receipt.discard_until_ns, 90_000_000);
    assert!(receipt.used_index && receipt.replays_from_start);
    assert!(!packets.is_empty());
    assert_eq!(packets[0].packet.track, 1);
    assert_eq!(packets[0].packet.data.as_ref(), b"A");
}

#[test]
fn known_length_starvation_and_transport_failure_stay_distinct_from_end() {
    let raw = document(
        &[
            element(0x1654ae6b, &entry(1, 1, &[])),
            element(0x1f43b675, &[]),
        ]
        .concat(),
    );
    let pool = Budget::new(128 * 1024).unwrap();
    let mut reader = reader(&raw, true, Limits::default(), &pool);
    let cancel = AtomicBool::new(false);
    let mut at_boundary = false;
    for _ in 0..200_000 {
        match reader.step(&mut WorkBudget::new(1), &cancel).unwrap() {
            Step::Yield | Step::Ready(_) => (),
            Step::NeedInput(request) => {
                let at = request.offset as usize;
                if at == raw.len() {
                    assert_eq!(request.maximum, 0);
                    reader
                        .fail(request.source, request.token, SourceFailure::Transport)
                        .unwrap();
                    at_boundary = true;
                    break;
                }
                let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1]).unwrap();
                reader
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
            }
            other => panic!("known length is not explicit source completion: {other:?}"),
        }
    }
    assert!(at_boundary);
    assert!(matches!(
        reader.step(&mut WorkBudget::new(1000), &cancel),
        Err(triergon_core::Error::Transport)
    ));
}
