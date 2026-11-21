//! Storage through the public API: packets, tracks and documents keep their exact
//! pool charges, outlive the reader and other threads, and stay within pool limits;
//! and the size of the reader workspace.
#[path = "support/deflate.rs"]
mod deflate_kit;
#[path = "support/ebml.rs"]
mod ebml_kit;
use ebml_kit::{element as e, mkv};
use std::{io::Cursor, sync::atomic::AtomicBool};
use triergon_core::{
    Budget, BudgetVec, Bytes, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_matroska::{BlockingReader, Error, Event, Limits, Reader};

#[test]
fn packet_track_and_catalogue_aliases_keep_one_live_pool_after_reader_drop() {
    let pool = Budget::new(1024 * 1024).unwrap();
    let baseline = pool.used();
    let mut demux = BlockingReader::open_in(
        Cursor::new(ebml_kit::avc_file_with_attachment()),
        Limits::default(),
        &pool,
    )
    .unwrap();
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
    let mut demux = BlockingReader::open_in(
        Cursor::new(ebml_kit::avc_file_with_attachment()),
        Limits::default(),
        &pool,
    )
    .unwrap();
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
fn earlier_packets_remain_charged_through_seek_and_cross_thread_release() {
    let pool = Budget::new(1024 * 1024).unwrap();
    let baseline = pool.used();
    let mut demux = BlockingReader::open_seekable_in(
        Cursor::new(ebml_kit::avc_file_with_attachment()),
        Limits::default(),
        &pool,
    )
    .unwrap();
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
    let bytes = mkv(&[e(0x1654ae6b, &e(0xae, &track)), cluster].concat());
    let mut reader = BlockingReader::open(
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
            if self.calls.is_multiple_of(5) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = output.len().min(2);
            self.bytes.read(&mut output[..count])
        }
    }
    let source = InterruptedShort {
        bytes: Cursor::new(ebml_kit::avc_file_with_attachment()),
        calls: 0,
    };
    let mut reader = BlockingReader::open(source, Limits::default()).unwrap();
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
        let demux = BlockingReader::open_in(
            Cursor::new(ebml_kit::avc_file_with_attachment()),
            Limits::default(),
            &pool,
        )
        .unwrap();
        let document = demux.metadata_documents()[0].clone();
        let used = pool.used();
        let alias = document.clone();
        assert_eq!(pool.used(), used, "aliases share one charged document");
        drop(demux);
        assert!(pool.used() > baseline);
        assert_eq!(alias.kind(), triergon_matroska::metadata::ATTACHMENTS);
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

fn fixture() -> Vec<u8> {
    let track = [
        e(0xd7, &[1]),
        e(0x73c5, &[1]),
        e(0x83, &[1]),
        e(0x86, b"V_AUTHORED/OPAQUE"),
        e(0x63a2, b"private"),
        e(0x536e, b"authored name"),
        e(0x22b59c, b"fra"),
        e(0x22b59d, b"fr-CA"),
    ]
    .concat();
    mkv(&[e(0x1654ae6b, &e(0xae, &track)), e(0x1f43b675, &[])].concat())
}
fn tracks() -> (triergon_matroska::Track, triergon_matroska::Track) {
    let reader = BlockingReader::open(Cursor::new(fixture()), Limits::default()).unwrap();
    let a = reader.tracks()[0].clone();
    let b = a.clone();
    drop(reader);
    (a, b)
}

#[test]
fn codec_private_clones_share_storage_after_reader_destruction() {
    let (a, b) = tracks();
    assert_eq!(&a.codec_private[..], b"private");
    assert_eq!(
        a.codec_private.as_ptr(),
        b.codec_private.as_ptr(),
        "retained Track::clone must not allocate an uncharged CodecPrivate copy"
    );
}

#[test]
fn source_metadata_clones_share_storage_after_reader_destruction() {
    let (a, b) = tracks();
    assert!(!a.source_metadata.is_empty());
    assert_eq!(
        a.source_metadata.as_ptr(),
        b.source_metadata.as_ptr(),
        "retained Track::clone must share the complete immutable source payload"
    );
}

#[test]
fn non_static_track_strings_share_storage_after_reader_destruction() {
    let (a, b) = tracks();
    assert_eq!(
        a.name.as_ptr(),
        b.name.as_ptr(),
        "track name was deep-cloned"
    );
    assert_eq!(
        a.codec.as_ptr(),
        b.codec.as_ptr(),
        "unknown codec ID was deep-cloned"
    );
    assert_eq!(
        a.language.as_ptr(),
        b.language.as_ptr(),
        "source language was deep-cloned"
    );
    assert_eq!(
        a.language_bcp47.as_ref().unwrap().as_ptr(),
        b.language_bcp47.as_ref().unwrap().as_ptr(),
        "BCP47 value was deep-cloned"
    );
}

#[test]
fn encoded_private_and_owned_text_are_checked_views_of_the_complete_source_entry() {
    let (track, _) = tracks();
    assert!(
        track
            .codec_private
            .shares_storage_with(&track.source_metadata)
    );
    let start = track.source_metadata.as_ptr() as usize;
    let end = start + track.source_metadata.len();
    for bytes in [
        track.codec_private.as_ref(),
        track.codec.as_bytes(),
        track.name.as_bytes(),
        track.language.as_bytes(),
        track.language_bcp47.as_ref().unwrap().as_bytes(),
    ] {
        let at = bytes.as_ptr() as usize;
        assert!(at >= start && at + bytes.len() <= end);
    }
    assert_eq!(track.codec_private.as_ref(), b"private");
    assert_eq!(track.name.as_str(), "authored name");
}

#[test]
fn sibling_source_views_keep_one_charged_tracks_owner_after_teardown() {
    let entry = |number| {
        [
            e(0xd7, &[number]),
            e(0x73c5, &[number]),
            e(0x83, &[1]),
            e(0x86, b"V_AUTHORED/OPAQUE"),
            e(0x63a2, b"private"),
        ]
        .concat()
    };
    let first = entry(1);
    let second = entry(2);
    let raw = [e(0xae, &first), e(0xae, &second)].concat();
    let bytes = mkv(&[e(0x1654ae6b, &raw), e(0x1f43b675, &[])].concat());
    let pool = Budget::new(64 * 1024).unwrap();
    let baseline = pool.used();
    let reader = BlockingReader::open_in(Cursor::new(bytes), Limits::default(), &pool).unwrap();
    let a = reader.tracks()[0].clone();
    let b = reader.tracks()[1].clone();
    assert_eq!(a.source_metadata.as_ref(), first.as_slice());
    assert_eq!(b.source_metadata.as_ref(), second.as_slice());
    assert!(a.source_metadata.shares_storage_with(&b.source_metadata));
    assert!(a.codec_private.shares_storage_with(&b.codec_private));
    let retained = a.codec_private.clone();
    assert!(retained.retained_capacity() >= raw.len());
    drop((reader, a, b));
    assert!(pool.used() > baseline);
    assert_eq!(retained.as_ref(), b"private");
    drop(retained);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn an_atomic_frame_larger_than_its_own_pool_minimum_is_a_permanent_limit_not_an_endless_wait() {
    let (compressed, _) = deflate_kit::repeated(128);
    let encoding = e(
        0x6d80,
        &e(
            0x6240,
            &[e(0x5032, &[1]), e(0x5034, &e(0x4254, &[0]))].concat(),
        ),
    );
    let track = e(
        0xae,
        &[
            e(0xd7, &[1]),
            e(0x73c5, &[1]),
            e(0x83, &[1]),
            e(0x86, b"V_AUTHORED/OPAQUE"),
            encoding,
        ]
        .concat(),
    );
    let block = [&[0x81, 0, 0, 0x80][..], compressed.as_slice()].concat();
    let raw = mkv(&[
        e(0x1654ae6b, &track),
        e(0x1f43b675, &[e(0xe7, &[0]), e(0xa3, &block)].concat()),
    ]
    .concat());
    let pool = Budget::new(4096).unwrap();
    let mut reader = Reader::new(
        &pool,
        SourceId(202),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        Limits::default(),
        1,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut limit = None;
    let mut pressure = 0;
    for _ in 0..20_000 {
        let mut work = WorkBudget::new(64);
        match reader.step(&mut work, &cancel) {
            Ok(Step::Yield) => (),
            Ok(Step::Ready(Event::Packet(_))) => {
                panic!("the complete decoded frame cannot fit this pool")
            }
            Ok(Step::Ready(_)) => (),
            Ok(Step::NeedInput(request)) => {
                let at = request.offset as usize;
                if at == raw.len() {
                    reader.finish(request.source, request.token).unwrap();
                } else {
                    let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1]).unwrap();
                    reader
                        .provide(
                            &InputWindow::new(request.source, request.token, request.offset, bytes)
                                .unwrap(),
                        )
                        .unwrap();
                }
            }
            Ok(Step::Backpressure(_)) => pressure += 1,
            Err(Error::Policy(message)) => {
                limit = Some(message);
                break;
            }
            other => panic!("valid compressed source must retain failure distinctions: {other:?}"),
        }
        assert!(work.spent() <= 64);
    }
    assert_eq!(
        limit,
        Some("operation retained storage"),
        "no caller retains a packet/configuration or reservation; repeated own-storage pressure ({pressure} polls) must be permanent"
    );
}

#[test]
fn oversized_growth_reports_the_pool_limit_not_overflow() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut vec: BudgetVec<u8> = BudgetVec::new(&pool);
    vec.try_extend_from_slice(&[1; 16]).unwrap();
    assert!(matches!(
        vec.try_reserve_exact(usize::MAX - 16),
        Err(Error::Limit { .. })
    ));
}

// Fixed workspace guards target the observed Mac host RSS/large-move failure.
// They are not heap policy exceptions or a proof of physical stack-frame size.
#[cfg(target_pointer_width = "64")]
#[test]
fn performance_reader_workspace_fits_one_16_kib_object_without_heap_indirection() {
    assert!(
        size_of::<Reader>() <= 16 * 1024,
        "the reader must not reserve two exclusive packet workspaces: {} bytes",
        size_of::<Reader>()
    );
}
