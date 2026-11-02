//! Authored ownership and incremental-input regressions. No reference code.
use std::{io::Cursor, sync::atomic::AtomicBool};
use triergon::{Error, Limits, matroska::Demuxer};
use triergon_core::{Budget, Bytes, InputWindow, SourceCapabilities, SourceId, WorkBudget};

fn e(id: u64, bytes: &[u8]) -> Vec<u8> {
    let mut out: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    let width = (1..=8)
        .find(|w| (bytes.len() as u64) < (1_u64 << (7 * w)) - 1)
        .unwrap();
    out.extend(((1_u64 << (7 * width)) | bytes.len() as u64).to_be_bytes()[8 - width..].iter());
    out.extend(bytes);
    out
}
fn fixture() -> Vec<u8> {
    let track = [
        e(0xd7, &[1]),
        e(0x73c5, &[1]),
        e(0x83, &[1]),
        e(0x86, b"V_MPEG4/ISO/AVC"),
        e(0x63a2, b"private"),
        e(0x536e, b"authored track"),
        e(0x22b59d, b"en-GB"),
    ]
    .concat();
    let attachment = e(
        0x61a7,
        &[
            e(0x46ae, &[1]),
            e(0x466e, b"a.bin"),
            e(0x4660, b"application/octet-stream"),
            e(0x465c, b"opaque"),
        ]
        .concat(),
    );
    let cluster = e(
        0x1f43b675,
        &[
            e(0xe7, &[0]),
            e(0xa3, &[0x81, 0, 0, 0x80, 1, 2, 3]),
            e(0xa3, &[0x81, 0, 1, 0x80, 4, 5, 6]),
        ]
        .concat(),
    );
    [
        e(0x1a45dfa3, &e(0x4282, b"matroska")),
        e(
            0x18538067,
            &[
                e(0x1654ae6b, &e(0xae, &track)),
                e(0x1941a469, &attachment),
                cluster,
            ]
            .concat(),
        ),
    ]
    .concat()
}

#[test]
fn packet_track_and_catalogue_aliases_keep_one_live_pool_after_reader_drop() {
    let pool = Budget::new(1024 * 1024).unwrap();
    let baseline = pool.used();
    let mut demux = Demuxer::open_in(Cursor::new(fixture()), Limits::default(), &pool).unwrap();
    let track = demux.tracks()[0].clone();
    let document = demux.metadata_documents()[0].clone();
    let attachment = document.attachments().next().unwrap();
    let packet = demux.next_packet(&AtomicBool::new(false)).unwrap().unwrap();
    let before_clones = pool.used();
    let alias = track.clone();
    let payload = packet.data.clone();
    let file_alias = attachment.clone();
    assert_eq!(
        pool.used(),
        before_clones,
        "shared clones must not allocate payloads"
    );
    drop(demux);
    assert!(pool.used() > baseline);
    assert_eq!(&*alias.codec_private, b"private");
    assert_eq!(alias.effective_language(), "en-GB");
    assert_eq!(payload.as_ref(), &[1, 2, 3]);
    assert_eq!(file_alias.data().unwrap(), b"opaque");
    drop((
        track, alias, document, attachment, file_alias, packet, payload,
    ));
    assert_eq!(
        pool.used(),
        baseline,
        "last storage owner returns its charge"
    );
}

#[test]
fn packet_output_backpressure_retries_the_same_event_without_reading_ahead() {
    let pool = Budget::new(1024 * 1024).unwrap();
    let mut demux = Demuxer::open_in(Cursor::new(fixture()), Limits::default(), &pool).unwrap();
    let occupied = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        demux.next_packet(&AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    drop(occupied);
    let packet = demux.next_packet(&AtomicBool::new(false)).unwrap().unwrap();
    assert_eq!(packet.pts_ns, Some(0));
    assert_eq!(packet.data.as_ref(), &[1, 2, 3]);
}

#[test]
fn one_byte_windows_and_one_unit_work_preserve_incremental_envelopes() {
    use triergon::incremental::{ElementEvent, ElementReader, ParseStep};
    let bytes = fixture();
    let pool = Budget::new(1024 * 1024).unwrap();
    let mut reader = ElementReader::new(
        &pool,
        SourceId(7),
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: false,
        },
        Limits::default(),
        1,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut blocks = Vec::new();
    let mut cursor = 0;
    loop {
        let mut work = WorkBudget::new(1);
        match reader.step(&mut work, &cancel).unwrap() {
            ParseStep::NeedInput(request) => {
                assert_eq!(request.offset, cursor as u64);
                if cursor == bytes.len() {
                    reader.finish(request.source, request.token).unwrap();
                } else {
                    let data = Bytes::copy_from_slice(&pool, &bytes[cursor..cursor + 1]).unwrap();
                    let window =
                        InputWindow::new(request.source, request.token, request.offset, data)
                            .unwrap();
                    reader.provide(&window).unwrap();
                    cursor += 1;
                }
            }
            ParseStep::Event(ElementEvent::Leaf { header, data }) if header.id == 0xa3 => {
                blocks.push(data);
            }
            ParseStep::Event(_) | ParseStep::Yield => (),
            ParseStep::Backpressure { .. } => panic!("ample pool"),
            ParseStep::Finished => break,
        }
        assert!(work.spent() <= 1);
    }
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].as_ref(), &[0x81, 0, 0, 0x80, 1, 2, 3]);
    assert_eq!(blocks[1].as_ref(), &[0x81, 0, 1, 0x80, 4, 5, 6]);
    assert_eq!(reader.position(), bytes.len() as u64);
}

#[test]
fn earlier_packets_remain_charged_through_seek_and_cross_thread_release() {
    let pool = Budget::new(1024 * 1024).unwrap();
    let baseline = pool.used();
    let mut demux =
        Demuxer::open_seekable_in(Cursor::new(fixture()), Limits::default(), &pool).unwrap();
    let first = demux.next_packet(&AtomicBool::new(false)).unwrap().unwrap();
    let alias = first.data.clone();
    let receipt = demux.seek_previous(0, 0, &AtomicBool::new(false)).unwrap();
    assert_eq!(receipt.actual_ns, 0);
    let replay = demux.next_packet(&AtomicBool::new(false)).unwrap().unwrap();
    assert_eq!(replay.data.as_ref(), first.data.as_ref());
    drop((demux, first, replay));
    assert!(pool.used() > baseline);
    std::thread::spawn(move || {
        assert_eq!(alias.as_ref(), &[1, 2, 3]);
        drop(alias);
    })
    .join()
    .unwrap();
    assert_eq!(pool.used(), baseline);
}

#[test]
fn incremental_starvation_transport_failure_and_explicit_completion_are_distinct() {
    use triergon::incremental::{ElementReader, ParseStep};
    use triergon_core::{Error as CoreError, SourceFailure};
    let pool = Budget::new(4096).unwrap();
    let mut reader = ElementReader::new(
        &pool,
        SourceId(1),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        Limits::default(),
        32,
    )
    .unwrap();
    assert!(matches!(
        reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        ParseStep::NeedInput(_)
    ));
    let request = reader.request();
    let empty = Bytes::copy_from_slice(&pool, &[]).unwrap();
    reader
        .provide(&InputWindow::new(request.source, request.token.clone(), 0, empty).unwrap())
        .unwrap();
    assert!(matches!(
        reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        ParseStep::NeedInput(_)
    ));
    reader
        .fail(request.source, request.token, SourceFailure::Transport)
        .unwrap();
    assert!(matches!(
        reader.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Core(CoreError::Transport))
    ));
    reader.reset().unwrap();
    let request = reader.request();
    reader.finish(request.source, request.token).unwrap();
    assert!(matches!(
        reader.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Truncated)
    ));
}

#[test]
fn cluster_range_reset_invalidates_old_input_but_preserves_earlier_leaf_storage() {
    use triergon::incremental::{ElementEvent, ElementReader, ParseStep};
    let bytes = fixture();
    let pool = Budget::new(64 * 1024).unwrap();
    let mut reader = ElementReader::new(
        &pool,
        SourceId(9),
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: true,
        },
        Limits::default(),
        bytes.len(),
    )
    .unwrap();
    let request = reader.request();
    let old_token = request.token.clone();
    let window = InputWindow::new(
        request.source,
        request.token,
        0,
        Bytes::copy_from_slice(&pool, &bytes).unwrap(),
    )
    .unwrap();
    reader.provide(&window).unwrap();
    drop(window);
    let mut checkpoint = None;
    let mut earlier = None;
    for _ in 0..10_000 {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Event(ElementEvent::Start(header)) if header.id == 0x1f43b675 => {
                checkpoint = reader.cluster_checkpoint()
            }
            ParseStep::Event(ElementEvent::Leaf { header, data }) if header.id == 0xa3 => {
                earlier = Some(data);
                break;
            }
            ParseStep::Event(_) | ParseStep::Yield => (),
            _ => panic!("unexpected input transition"),
        }
    }
    let earlier = earlier.unwrap();
    let checkpoint = checkpoint.unwrap();
    let request = reader.reset_to_cluster(&checkpoint).unwrap();
    assert_eq!(request.offset, checkpoint.offset());
    assert_ne!(reader.generation(), old_token.generation());
    let stale = InputWindow::new(
        request.source,
        old_token,
        request.offset,
        Bytes::copy_from_slice(&pool, &[]).unwrap(),
    )
    .unwrap();
    assert!(reader.provide(&stale).is_err());
    let start = request.offset as usize;
    let replay = InputWindow::new(
        request.source,
        request.token,
        request.offset,
        Bytes::copy_from_slice(&pool, &bytes[start..]).unwrap(),
    )
    .unwrap();
    reader.provide(&replay).unwrap();
    drop(replay);
    let mut found = false;
    for _ in 0..10_000 {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Event(ElementEvent::Leaf { header, data }) if header.id == 0xa3 => {
                assert_eq!(data.as_ref(), earlier.as_ref());
                found = true;
                break;
            }
            ParseStep::Event(_) | ParseStep::Yield => (),
            _ => panic!("unexpected replay transition"),
        }
    }
    assert!(found);
    drop(reader);
    assert_eq!(earlier.as_ref(), &[0x81, 0, 0, 0x80, 1, 2, 3]);
}

#[test]
fn skipped_block_outside_cluster_does_not_reset_scan_policy() {
    use triergon::incremental::{ElementReader, ParseStep};
    // These elements are skipped at Segment level. Their Block ID alone must
    // not let an irrelevant scan repeatedly replenish its operation budget.
    let outside = e(0xa3, &[0x81, 0, 0, 0x80, 1]).repeat(32);
    let bytes = [
        e(0x1a45dfa3, &e(0x4282, b"matroska")),
        e(0x18538067, &outside),
    ]
    .concat();
    let pool = Budget::new(64 * 1024).unwrap();
    let mut reader = ElementReader::new(
        &pool,
        SourceId(21),
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: false,
        },
        Limits {
            scan_bytes: 64,
            ..Limits::default()
        },
        bytes.len(),
    )
    .unwrap();
    let request = reader.request();
    let window = InputWindow::new(
        request.source,
        request.token,
        0,
        Bytes::copy_from_slice(&pool, &bytes).unwrap(),
    )
    .unwrap();
    reader.provide(&window).unwrap();
    let mut outcome = None;
    for _ in 0..10_000 {
        match reader.step(&mut WorkBudget::new(3), &AtomicBool::new(false)) {
            Ok(ParseStep::NeedInput(request)) => {
                reader.finish(request.source, request.token).unwrap()
            }
            Ok(ParseStep::Event(_)) | Ok(ParseStep::Yield) => (),
            Ok(ParseStep::Finished) => break,
            Ok(ParseStep::Backpressure { .. }) => panic!("ample pool"),
            Err(error) => {
                outcome = Some(error);
                break;
            }
        }
    }
    assert!(
        matches!(outcome, Some(Error::Limit("operation read bytes"))),
        "a skipped Block must not replenish a bounded scan: {outcome:?}"
    );
}

#[test]
fn completed_leaf_survives_freeze_backpressure_without_consuming_more_input() {
    use triergon::incremental::{ElementEvent, ElementReader, ParseStep};
    let bytes = fixture();
    let pool = Budget::new(64 * 1024).unwrap();
    let source = Bytes::copy_from_slice(&pool, &bytes).unwrap();
    let mut reader = ElementReader::new(
        &pool,
        SourceId(22),
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: false,
        },
        Limits::default(),
        bytes.len(),
    )
    .unwrap();
    let request = reader.request();
    let window = InputWindow::new(request.source, request.token, 0, source.clone()).unwrap();
    reader.provide(&window).unwrap();
    for _ in 0..20_000 {
        if reader.position() == bytes.len() as u64 {
            break;
        }
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Event(_) | ParseStep::Yield => (),
            _ => panic!("whole source is supplied with ample storage"),
        }
    }
    assert_eq!(reader.position(), bytes.len() as u64);
    // Keep the source alias so copying the final span cannot release its owner
    // and accidentally supply the space that this regression intentionally holds.
    let occupied = pool.reserve(pool.available()).unwrap();
    let mut blocked = false;
    for _ in 0..100 {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Backpressure { .. } => {
                blocked = true;
                break;
            }
            ParseStep::Yield => (),
            _ => panic!("completed leaf must wait for its storage header"),
        }
    }
    assert!(blocked);
    assert_eq!(reader.position(), bytes.len() as u64);
    drop(occupied);
    let mut payload = None;
    for _ in 0..100 {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Event(ElementEvent::Leaf { header, data }) if header.id == 0xa3 => {
                payload = Some(data);
                break;
            }
            ParseStep::Yield => (),
            _ => panic!("retry must publish the same completed leaf"),
        }
    }
    assert_eq!(payload.unwrap().as_ref(), &[0x81, 0, 1, 0x80, 4, 5, 6]);
    assert_eq!(source.as_ref(), bytes.as_slice());
}

#[test]
fn small_stored_zlib_laces_keep_the_existing_decoded_capacity_policy() {
    // RFC-authored final stored block: "abc", with LEN/NLEN and Adler-32.
    // The encoded SimpleBlock occupies 33 bytes. Both decoded frames together
    // require 6 bytes; speculative output capacity must not exhaust this policy.
    let compressed = [120, 1, 1, 3, 0, 252, 255, 97, 98, 99, 2, 77, 1, 39];
    let encoding = e(0x6d80, &e(0x6240, &e(0x5034, &e(0x4254, &[0]))));
    let track = [
        e(0xd7, &[1]),
        e(0x73c5, &[1]),
        e(0x83, &[2]),
        e(0x86, b"A_PCM/INT/LIT"),
        encoding,
    ]
    .concat();
    let block = [
        vec![0x81, 0, 0, 0x84, 1],
        compressed.to_vec(),
        compressed.to_vec(),
    ]
    .concat();
    assert_eq!(block.len(), 33);
    let cluster = e(0x1f43b675, &[e(0xe7, &[0]), e(0xa3, &block)].concat());
    let bytes = [
        e(0x1a45dfa3, &e(0x4282, b"matroska")),
        e(
            0x18538067,
            &[e(0x1654ae6b, &e(0xae, &track)), cluster].concat(),
        ),
    ]
    .concat();
    let mut reader = Demuxer::open(
        Cursor::new(bytes),
        Limits {
            block_bytes: 33,
            ..Limits::default()
        },
    )
    .unwrap();
    let mut packets = Vec::new();
    while let Some(packet) = reader.next_packet(&AtomicBool::new(false)).unwrap() {
        packets.push(packet);
    }
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].data.as_ref(), b"abc");
    assert_eq!(packets[1].data.as_ref(), b"abc");
    assert!(
        packets
            .iter()
            .map(|p| p.data.retained_capacity())
            .sum::<usize>()
            <= 33
    );
}

#[test]
fn direct_destination_reads_handle_short_reads_and_interruptions_without_zero_packets() {
    use std::io::{self, Read};
    struct InterruptedShort {
        bytes: Cursor<Vec<u8>>,
        calls: usize,
    }
    impl Read for InterruptedShort {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls % 5 == 0 {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = output.len().min(2);
            self.bytes.read(&mut output[..count])
        }
    }
    let source = InterruptedShort {
        bytes: Cursor::new(fixture()),
        calls: 0,
    };
    let mut reader = Demuxer::open(source, Limits::default()).unwrap();
    let mut payloads = Vec::new();
    while let Some(packet) = reader.next_packet(&AtomicBool::new(false)).unwrap() {
        payloads.push(packet.data.as_ref().to_vec());
    }
    assert_eq!(payloads, vec![vec![1, 2, 3], vec![4, 5, 6]]);
    assert_eq!(reader.tracks()[0].codec_private.as_ref(), b"private");
    assert_eq!(reader.tracks()[0].name.as_str(), "authored track");
}

#[test]
fn opaque_document_handles_keep_the_charge_through_cross_thread_last_release() {
    let pool = Budget::new(1024 * 1024).unwrap();
    let baseline = pool.used();
    for _ in 0..16 {
        let demux = Demuxer::open_in(Cursor::new(fixture()), Limits::default(), &pool).unwrap();
        let document = demux.metadata_documents()[0].clone();
        let used = pool.used();
        let alias = document.clone();
        assert_eq!(pool.used(), used, "aliases share one charged document");
        drop(demux);
        assert!(pool.used() > baseline);
        assert_eq!(alias.kind(), triergon::metadata::ATTACHMENTS);
        assert_eq!(
            alias.attachments().next().unwrap().data().unwrap(),
            b"opaque"
        );
        drop(alias);
        assert!(pool.used() > baseline);
        std::thread::spawn(move || {
            assert_eq!(
                document.attachments().next().unwrap().filename().unwrap(),
                "a.bin"
            );
            drop(document);
        })
        .join()
        .unwrap();
        assert_eq!(
            pool.used(),
            baseline,
            "last opaque owner returns all charges"
        );
    }
}
