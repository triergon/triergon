//! Authored ownership and incremental-input regressions. No reference code.
#[path = "support/ebml.rs"]
mod ebml;
use ebml::{element as e, mkv};
use std::{io::Cursor, sync::atomic::AtomicBool};
use triergon_core::Budget;
use triergon_matroska::{BlockingReader as Demuxer, Error, Limits};

#[test]
fn packet_track_and_catalogue_aliases_keep_one_live_pool_after_reader_drop() {
    let pool = Budget::new(1024 * 1024).unwrap();
    let baseline = pool.used();
    let mut demux = Demuxer::open_in(
        Cursor::new(ebml::avc_file_with_attachment()),
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
    let mut demux = Demuxer::open_in(
        Cursor::new(ebml::avc_file_with_attachment()),
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
    let mut demux = Demuxer::open_seekable_in(
        Cursor::new(ebml::avc_file_with_attachment()),
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
            if self.calls.is_multiple_of(5) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = output.len().min(2);
            self.bytes.read(&mut output[..count])
        }
    }
    let source = InterruptedShort {
        bytes: Cursor::new(ebml::avc_file_with_attachment()),
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
        let demux = Demuxer::open_in(
            Cursor::new(ebml::avc_file_with_attachment()),
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
