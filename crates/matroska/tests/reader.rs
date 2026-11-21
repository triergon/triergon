//! The incremental reader through its public API: configuration, packets, catalogue
//! documents, seeks, input states under work-one scheduling, and the bounded
//! header hint.
#[path = "support/ebml.rs"]
mod ebml_kit;
use ebml_kit::{abc_laces, cluster, element, mkv, simple, track, uint, zlib_encoding};
use std::io::Cursor;
use std::sync::atomic::AtomicBool;
use triergon_core::Step;
use triergon_core::{
    Budget, Bytes, InputWindow, SourceCapabilities, SourceFailure, SourceId, WorkBudget,
};
use triergon_matroska::{BlockingReader, Error, Event, Limits, ParseStep, Reader};

fn stored_laced_source(bad: bool) -> Vec<u8> {
    let block = abc_laces(bad);
    let tracks = element(
        0x1654ae6b,
        &track(
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
    mkv(&[
        tracks,
        element(0x1f43b675, &[uint(0xe7, 2), element(0xa3, &block)].concat()),
    ]
    .concat())
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
            other @ Step::Backpressure(_) => panic!("ample-pool source must progress: {other:?}"),
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
    let tracks = element(0x1654ae6b, &[track(1, 1, &[]), track(1, 1, &[])].concat());
    let raw = mkv(&[tracks, element(0x1f43b675, &[])].concat());
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
    let raw = mkv(&[
        element(0x1654ae6b, &track(1, 1, &[])),
        chapters,
        element(0x1f43b675, &[]),
    ]
    .concat());
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
    let raw = mkv(&[
        element(0x1654ae6b, &track(1, 1, &[])),
        element(0x1f43b675, &[uint(0xe7, 10), group].concat()),
    ]
    .concat());
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
        &[track(1, 1, &[]), track(2, 2, &uint(0x56bb, 32_000_000))].concat(),
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
    let raw = mkv(&[tracks, first, second, cues].concat());
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
fn playback_and_compatibility_seeks_can_land_on_the_first_keyframe() {
    let tracks = element(0x1654ae6b, &track(1, 1, &[]));
    let cluster = cluster(1000, simple(1, 0, 0x80, b"first"));
    let cue = element(
        0xbb,
        &[
            uint(0xb3, 1000),
            element(
                0xb7,
                &[uint(0xf7, 1), uint(0xf1, tracks.len() as u64)].concat(),
            ),
        ]
        .concat(),
    );
    let cues = element(0x1c53bb6b, &cue);
    let input = mkv(&[tracks, cluster, cues].concat());
    let cancel = AtomicBool::new(false);
    for block_start in [false, true] {
        for target in [0, 250_000_000] {
            let mut reader =
                BlockingReader::open_seekable(Cursor::new(&input), Limits::default()).unwrap();
            for _ in 0..2 {
                let landed = if block_start {
                    triergon_matroska::compat::seek::at_block_start(&mut reader, 0, target, &cancel)
                } else {
                    reader.seek_for_playback(0, target, &cancel)
                }
                .unwrap();
                assert_eq!(landed.actual_ns, 1_000_000_000);
                assert_eq!(landed.discard_until_ns, target);
                assert!(landed.replays_from_start);
                let packet = reader.next_typed_packet(&cancel).unwrap().unwrap();
                assert_eq!(packet.packet.data.as_ref(), b"first");
                assert!(reader.next_typed_packet(&cancel).unwrap().is_none());
            }
            assert!(matches!(
                reader.seek_previous(0, target, &cancel),
                Err(Error::Unsupported("no previous random-access block"))
            ));
        }
    }
}

#[test]
fn compatibility_seek_prefers_an_earlier_uncued_keyframe() {
    let tracks = element(0x1654ae6b, &track(1, 1, &[]));
    let first = cluster(1000, simple(1, 0, 0x80, b"uncued"));
    let later = cluster(10_000, simple(1, 0, 0x80, b"cued"));
    let cue = element(
        0xbb,
        &[
            uint(0xb3, 10_000),
            element(
                0xb7,
                &[
                    uint(0xf7, 1),
                    uint(0xf1, (tracks.len() + first.len()) as u64),
                ]
                .concat(),
            ),
        ]
        .concat(),
    );
    let cues = element(0x1c53bb6b, &cue);
    let input = mkv(&[tracks, first, later, cues].concat());
    let cancel = AtomicBool::new(false);
    let mut reader = BlockingReader::open_seekable(Cursor::new(input), Limits::default()).unwrap();
    while reader.next_typed_packet(&cancel).unwrap().is_some() {}
    let landing =
        triergon_matroska::compat::seek::at_block_start(&mut reader, 0, 2_000_000_000, &cancel)
            .unwrap();
    assert_eq!(landing.actual_ns, 1_000_000_000);
}

#[test]
fn compatibility_seek_keeps_the_zero_clamped_preroll_landing() {
    let tracks = element(
        0x1654ae6b,
        &[track(1, 1, &[]), track(2, 2, &uint(0x56bb, 80_000_000))].concat(),
    );
    let initial = cluster(
        0,
        [
            simple(1, -40, 0x80, b"before zero"),
            simple(1, 0, 0x80, b"at zero"),
        ]
        .concat(),
    );
    let input = mkv(&[tracks, initial].concat());
    let cancel = AtomicBool::new(false);
    let mut reader = BlockingReader::open_seekable(Cursor::new(input), Limits::default()).unwrap();
    let landing =
        triergon_matroska::compat::seek::at_block_start(&mut reader, 0, 0, &cancel).unwrap();
    assert_eq!(landing.actual_ns, 0);
}

#[test]
fn playback_recovery_keeps_audio_laces_that_can_cross_the_cutoff() {
    const A: &[u8] = b"first";
    const B: &[u8] = b"next!";
    let second = cluster(2000, simple(1, 0, 0x80, b"target"));
    let cancel = AtomicBool::new(false);
    for (duration, default_duration, padding, first_frame) in [
        (Some(0), None, 0_i64, 0),
        (None, None, 0, 0),
        (Some(0), Some(10_000_000), 0, 1),
        (Some(0), Some(10_000_000), -15_000_000, 0),
        (Some(10), None, 0, 2),
        (Some(20), None, 0, 1),
        (Some(40), None, 0, 0),
        (Some(10), None, -5_000_000, 2),
        (Some(20), None, -15_000_000, 0),
        (None, Some(10_000_000), -15_000_000, 0),
        (Some(20), None, 15_000_000, 1),
    ] {
        let audio = element(
            0xae,
            &[
                uint(0xd7, 2),
                uint(0x73c5, 2),
                uint(0x83, 2),
                element(0x86, b"A_OPUS"),
                element(0xe1, &element(0xb5, &48_000_f64.to_be_bytes())),
                uint(0x56aa, 6_500_000),
                uint(0x56bb, 80_000_000),
                default_duration
                    .map(|ns| uint(0x23e383, ns))
                    .unwrap_or_default(),
            ]
            .concat(),
        );
        let tracks = element(
            0x1654ae6b,
            &[track(1, 1, &[]), audio, track(3, 17, &[])].concat(),
        );
        let mut block_group = element(0xa1, &[&[0x82, 0x03, 0xde, 4, 1][..], A, B].concat());
        if let Some(duration) = duration {
            block_group.extend(uint(0x9b, duration));
        }
        if padding != 0 {
            block_group.extend(element(0x75a2, &padding.to_be_bytes()));
        }
        let first = cluster(
            1000,
            [
                simple(1, 0, 0x80, b"earlier"),
                simple(3, 900, 0x80, b"subtitle"),
                element(0xa0, &block_group),
            ]
            .concat(),
        );
        let cue = element(
            0xbb,
            &[
                uint(0xb3, 2000),
                element(
                    0xb7,
                    &[
                        uint(0xf7, 1),
                        uint(0xf1, (tracks.len() + first.len()) as u64),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        );
        let cues = element(0x1c53bb6b, &cue);
        let raw = mkv(&[tracks.as_slice(), &first, &second, &cues].concat());
        let mut reader =
            BlockingReader::open_seekable(Cursor::new(raw), Limits::default()).unwrap();
        let mut audio_packets = Vec::new();
        for at_block_start in [false, true, false] {
            let landed = if at_block_start {
                triergon_matroska::compat::seek::at_block_start(
                    &mut reader,
                    0,
                    2_080_000_000,
                    &cancel,
                )
                .unwrap()
            } else {
                reader.seek_for_playback(0, 2_080_000_000, &cancel).unwrap()
            };
            assert_eq!(landed.actual_ns, 2_000_000_000);
            assert_eq!(landed.preroll_ns, 80_000_000);
            assert!(!landed.replays_from_start, "{landed:?}");
            let mut other_packets = Vec::new();
            while let Some(packet) = reader.next_typed_packet(&cancel).unwrap() {
                let packet = packet.packet;
                if packet.track == 1 {
                    assert_eq!(packet.discard_padding_ns, padding);
                    audio_packets.push((
                        packet.timing.frame,
                        packet.timing.duration_ticks,
                        packet.pts_ns,
                        packet.data.as_ref().to_vec(),
                    ));
                } else {
                    other_packets.push(packet.data.as_ref().to_vec());
                }
            }
            assert_eq!(other_packets, [b"subtitle".to_vec(), b"target".to_vec()]);
            let start = 1_983_500_000;
            let expected = [
                (0, duration, Some(start), A.to_vec()),
                (
                    1,
                    duration,
                    duration
                        .map(|ticks| ticks * 500_000)
                        .or(default_duration)
                        .map(|ns| start + ns as i64),
                    B.to_vec(),
                ),
            ];
            let first_frame = if at_block_start {
                expected.len()
            } else {
                first_frame
            };
            assert_eq!(
                audio_packets,
                expected[first_frame..],
                "duration {duration:?}, default {default_duration:?}, padding {padding}, block-start seek {at_block_start}"
            );
            audio_packets.clear();
        }
    }
}

#[test]
fn known_length_starvation_and_transport_failure_stay_distinct_from_end() {
    let raw = mkv(&[
        element(0x1654ae6b, &track(1, 1, &[])),
        element(0x1f43b675, &[]),
    ]
    .concat());
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
        Err(Error::Transport)
    ));
}

#[test]
fn header_refill_canonical_short_prefix_observes_actual_zero_one_work_and_cancel() {
    let mut reader = Reader::new(
        &Budget::new(128 * 1024).unwrap(),
        SourceId(2103),
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        Limits::default(),
        8192,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let request = match reader
        .step_parse(&mut WorkBudget::new(64), &cancel)
        .unwrap()
    {
        ParseStep::NeedInput(request) => request,
        other => panic!("initial header: {other:?}"),
    };
    assert_eq!(request.maximum, 4096);
    let bytes = Bytes::copy_from_slice(reader.budget(), &[0x1a]).unwrap();
    reader
        .provide(&InputWindow::new(request.source, request.token, request.offset, bytes).unwrap())
        .unwrap();
    let mut zero = WorkBudget::new(0);
    assert!(matches!(
        reader.step_parse(&mut zero, &cancel).unwrap(),
        ParseStep::Yield
    ));
    assert_eq!((reader.position(), zero.spent()), (0, 0));
    let mut one = WorkBudget::new(1);
    assert!(matches!(
        reader.step_parse(&mut one, &cancel).unwrap(),
        ParseStep::NeedInput(_) | ParseStep::Yield
    ));
    assert_eq!((reader.position(), one.spent()), (1, 1));
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(matches!(
        reader.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    ));
    assert_eq!(reader.position(), 1);
}

// The engine, not only the blocking adapter, issues the bounded default header hint.
// The engine, not only the blocking adapter, issues the bounded default header hint.
#[test]
fn ordinary_reader_default_issues_the_same_bounded_header_hint() {
    let pool = Budget::new(128 * 1024).unwrap();
    let mut reader = Reader::new(
        &pool,
        SourceId(2105),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        Limits::default(),
        8192,
    )
    .unwrap();
    let mut work = WorkBudget::new(64);
    let request = match reader
        .step_parse(&mut work, &AtomicBool::new(false))
        .unwrap()
    {
        ParseStep::NeedInput(request) => request,
        other => panic!("initial canonical header request: {other:?}"),
    };
    assert_eq!(reader.position(), 0);
    assert_eq!(work.spent(), 0);
    assert_eq!(
        request.maximum, 4096,
        "ordinary engine and blocking supply share one bounded hint"
    );
}
