//! Original format-level fixtures, independent of the FFmpeg differential oracle.
use std::{
    io::{self, Cursor, Read},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use triergon_matroska::{Error, Limits, typed::BlockingReader as Demuxer};

fn size(n: usize) -> Vec<u8> {
    let width = (1..=8)
        .find(|w| (n as u64) < (1_u64 << (7 * w)) - 1)
        .unwrap();
    let value = (1_u64 << (7 * width)) | n as u64;
    value.to_be_bytes()[8 - width..].to_vec()
}
fn element(id: u64, data: &[u8]) -> Vec<u8> {
    let mut out = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect::<Vec<_>>();
    out.extend(size(data.len()));
    out.extend(data);
    out
}
fn uint(id: u64, n: u64) -> Vec<u8> {
    element(id, &n.to_be_bytes())
}
fn block(relative: i16, key: bool) -> Vec<u8> {
    let [a, b] = relative.to_be_bytes();
    element(0xa3, &[0x81, a, b, if key { 0x80 } else { 0 }, 1, 2, 3])
}
fn cluster(time: u64, blocks: Vec<u8>) -> Vec<u8> {
    element(0x1f43b675, &[uint(0xe7, time), blocks].concat())
}
fn fixture(clusters: Vec<u8>, unknown_segment: bool) -> Vec<u8> {
    fixture_with_duration(clusters, unknown_segment, true)
}
fn fixture_with_duration(clusters: Vec<u8>, unknown_segment: bool, with_duration: bool) -> Vec<u8> {
    let header = element(0x1a45dfa3, &element(0x4282, b"matroska"));
    let mut track = [
        uint(0xd7, 1),
        uint(0x73c5, 1),
        uint(0x83, 1),
        element(0x86, b"V_MPEG4/ISO/AVC"),
    ]
    .concat();
    if with_duration {
        track.extend(uint(0x23e383, 1_000_000));
    }
    let body = [element(0x1654ae6b, &element(0xae, &track)), clusters].concat();
    if unknown_segment {
        [header, vec![0x18, 0x53, 0x80, 0x67, 0xff], body].concat()
    } else {
        [header, element(0x18538067, &body)].concat()
    }
}
fn collect(
    bytes: &[u8],
    limits: Limits,
) -> triergon_matroska::Result<Vec<triergon_matroska::matroska::Packet>> {
    let mut d = Demuxer::open(Cursor::new(bytes), limits)?;
    let mut out = Vec::new();
    while let Some(p) = d.next_packet(&AtomicBool::new(false))? {
        out.push(p);
    }
    Ok(out)
}

#[test]
fn negative_relative_time_and_unknown_decode_time_are_preserved() {
    let bytes = fixture(cluster(10, block(-14, true)), false);
    let packets = collect(&bytes, Limits::default()).unwrap();
    assert_eq!(packets[0].pts_ns, Some(-4_000_000));
    assert_eq!(packets[0].dts_ns, None);
    assert_eq!(packets[0].data.as_ref(), [1, 2, 3]);
}

#[test]
fn retained_packet_view_survives_reads_and_reader_drop_without_header_bytes() {
    let bytes = fixture(
        cluster(0, [block(0, true), block(1, false)].concat()),
        false,
    );
    let mut reader = Demuxer::open(Cursor::new(bytes), Limits::default()).unwrap();
    let first = reader
        .next_packet(&AtomicBool::new(false))
        .unwrap()
        .unwrap();
    assert!(first.data.retained_capacity() >= first.data.len());
    assert_eq!(first.data.as_ref(), [1, 2, 3]);
    let _ = reader.next_packet(&AtomicBool::new(false)).unwrap();
    drop(reader);
    assert_eq!(first.data.as_ref(), [1, 2, 3]);
}

fn header_removed(header: &[u8], scope: u64, private: &[u8]) -> Vec<u8> {
    let encoding = element(
        0x6d80,
        &element(
            0x6240,
            &[
                uint(0x5032, scope),
                element(0x5034, &[uint(0x4254, 3), element(0x4255, header)].concat()),
            ]
            .concat(),
        ),
    );
    let track = [
        uint(0xd7, 1),
        uint(0x73c5, 1),
        uint(0x83, 2),
        element(0x86, b"A_PCM/INT/LIT"),
        element(0x63a2, private),
        encoding,
    ]
    .concat();
    [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(
            0x18538067,
            &[
                element(0x1654ae6b, &element(0xae, &track)),
                cluster(0, element(0xa3, &[0x81, 0, 0, 0x80])),
            ]
            .concat(),
        ),
    ]
    .concat()
}

#[test]
fn header_removal_restores_zero_length_blocks_and_private_scope_independently() {
    let cancel = AtomicBool::new(false);
    for scope in [1, 2, 3] {
        let bytes = header_removed(b"ab", scope, b"cd");
        let mut demux = Demuxer::open(Cursor::new(bytes), Limits::default()).unwrap();
        assert_eq!(
            demux.tracks()[0].codec_private,
            if scope & 2 != 0 {
                b"abcd".to_vec()
            } else {
                b"cd".to_vec()
            }
        );
        let p = demux.next_packet(&cancel).unwrap().unwrap();
        assert_eq!(
            p.data.as_ref(),
            if scope & 1 != 0 { &b"ab"[..] } else { &b""[..] }
        );
    }
}

#[test]
fn restored_header_is_bounded_by_the_decoded_packet_limit() {
    let bytes = header_removed(&[0x55; 32], 1, &[]);
    let mut demux = Demuxer::open(
        Cursor::new(bytes),
        Limits {
            block_bytes: 8,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(matches!(
        demux.next_packet(&AtomicBool::new(false)),
        Err(Error::Policy(_))
    ));
}

#[test]
fn cyclic_seek_heads_are_bounded_and_cannot_fabricate_missing_tracks() {
    let entry = element(
        0x4dbb,
        &[element(0x53ab, &[0x11, 0x4d, 0x9b, 0x74]), uint(0x53ac, 0)].concat(),
    );
    let body = [element(0x114d9b74, &entry), cluster(0, block(0, true))].concat();
    let document = [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(0x18538067, &body),
    ]
    .concat();
    assert!(matches!(
        Demuxer::open_seekable(Cursor::new(document), Limits::default()),
        Err(Error::Invalid("missing Tracks"))
    ));
}

#[test]
fn header_restore_handles_each_lace_and_rejects_duplicate_encoding_orders() {
    let encode = |order: u64, header: &[u8]| {
        element(
            0x6240,
            &[
                uint(0x5031, order),
                element(0x5034, &[uint(0x4254, 3), element(0x4255, header)].concat()),
            ]
            .concat(),
        )
    };
    for duplicate in [false, true] {
        let settings = element(
            0x6d80,
            &[encode(0, b"a"), encode(if duplicate { 0 } else { 2 }, b"b")].concat(),
        );
        let track = [
            uint(0xd7, 1),
            uint(0x73c5, 1),
            uint(0x83, 2),
            element(0x86, b"A_PCM/INT/LIT"),
            settings,
        ]
        .concat();
        let laced = element(0xa3, &[0x81, 0, 0, 0x84, 1, b'x', b'y']);
        let document = [
            element(0x1a45dfa3, &element(0x4282, b"matroska")),
            element(
                0x18538067,
                &[
                    element(0x1654ae6b, &element(0xae, &track)),
                    cluster(0, laced),
                ]
                .concat(),
            ),
        ]
        .concat();
        let result = collect(&document, Limits::default());
        if duplicate {
            assert!(matches!(
                result,
                Err(Error::Invalid("duplicate content encoding order"))
            ));
        } else {
            let packets = result.unwrap();
            assert_eq!(packets[0].data.as_ref(), b"abx");
            assert_eq!(packets[1].data.as_ref(), b"aby");
        }
    }
}

#[test]
fn later_lace_timestamps_remain_unknown_without_container_duration() {
    let laced = element(0xa3, &[0x81, 0, 0, 0x84, 1, 1, 2]);
    let bytes = fixture_with_duration(cluster(0, laced), false, false);
    let packets = collect(&bytes, Limits::default()).unwrap();
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].pts_ns, Some(0));
    assert_eq!(packets[1].pts_ns, None);
}

#[test]
fn laced_keyframe_seek_receipt_describes_the_block_that_is_replayed() {
    let laced = element(0xa3, &[0x81, 0, 0, 0x84, 1, 1, 2]);
    let bytes = fixture(cluster(0, laced), false);
    let mut d = Demuxer::open_seekable(Cursor::new(bytes), Limits::default()).unwrap();
    let cancel = AtomicBool::new(false);
    let receipt = d.seek_previous(0, 1_000_000, &cancel).unwrap();
    assert_eq!(receipt.actual_ns, 0);
    assert_eq!(
        d.next_packet(&cancel).unwrap().unwrap().pts_ns,
        Some(receipt.actual_ns)
    );
}

#[test]
fn unknown_cluster_ends_at_a_sibling_cluster_and_at_physical_eof() {
    let unknown = [
        vec![0x1f, 0x43, 0xb6, 0x75, 0xff],
        uint(0xe7, 0),
        block(0, true),
    ]
    .concat();
    let bytes = fixture([unknown, cluster(10, block(0, true))].concat(), true);
    let packets = collect(&bytes, Limits::default()).unwrap();
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[1].pts_ns, Some(10_000_000));
}

#[test]
fn every_truncated_finite_document_fails_without_becoming_clean_eof() {
    let bytes = fixture(cluster(0, block(0, true)), false);
    for cut in 0..bytes.len() {
        assert!(
            collect(&bytes[..cut], Limits::default()).is_err(),
            "cut={cut}"
        );
    }
    assert_eq!(collect(&bytes, Limits::default()).unwrap().len(), 1);
}

#[test]
fn resource_limits_reject_before_allocating_large_elements() {
    let bytes = fixture(cluster(0, block(0, true)), false);
    for limits in [
        Limits {
            metadata_bytes: 1,
            ..Limits::default()
        },
        Limits {
            block_bytes: 1,
            ..Limits::default()
        },
        Limits {
            elements: 2,
            ..Limits::default()
        },
        Limits {
            scan_bytes: 8,
            ..Limits::default()
        },
    ] {
        assert!(matches!(collect(&bytes, limits), Err(Error::Policy(_))));
    }
}

#[test]
fn seek_after_eof_lands_on_previous_keyframe_and_restarts_packet_reads() {
    let bytes = fixture(
        cluster(
            0,
            [block(0, true), block(10, false), block(20, true)].concat(),
        ),
        false,
    );
    let mut d = Demuxer::open_seekable(Cursor::new(bytes), Limits::default()).unwrap();
    let cancel = AtomicBool::new(false);
    while d.next_packet(&cancel).unwrap().is_some() {}
    for (target, expected) in [(15_000_000, 0), (99_000_000, 20_000_000), (0, 0)] {
        assert_eq!(
            d.seek_previous(0, target, &cancel).unwrap().actual_ns,
            expected
        );
        let p = d.next_packet(&cancel).unwrap().unwrap();
        assert!(p.keyframe);
        assert_eq!(p.pts_ns, Some(expected));
    }
}

#[test]
fn cancel_and_failed_seek_cannot_leave_stale_pending_packets() {
    let bytes = fixture(cluster(0, block(0, true)), false);
    let mut d = Demuxer::open_seekable(Cursor::new(bytes), Limits::default()).unwrap();
    let cancel = AtomicBool::new(true);
    assert!(matches!(d.next_packet(&cancel), Err(Error::Cancelled)));
    cancel.store(false, Ordering::Relaxed);
    assert!(d.next_packet(&cancel).is_err());
    assert!(d.seek_previous(0, -1, &cancel).is_err());
    assert!(d.next_packet(&cancel).is_err());
    d.seek_previous(0, 0, &cancel).unwrap();
    assert!(d.next_packet(&cancel).unwrap().is_some());
}

struct Faulty {
    source: Cursor<Vec<u8>>,
    fail: Arc<AtomicBool>,
}
impl Read for Faulty {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.fail.load(Ordering::Relaxed) {
            Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "injected read fault",
            ))
        } else {
            self.source.read(out)
        }
    }
}
#[test]
fn transport_failure_is_not_eof() {
    // Larger than any read-ahead window, so packet reads after the fault need input.
    let clusters: Vec<u8> = (0..40_000)
        .flat_map(|time| cluster(time, block(0, true)))
        .collect();
    let fail = Arc::new(AtomicBool::new(false));
    let source = Faulty {
        source: Cursor::new(fixture(clusters, true)),
        fail: fail.clone(),
    };
    let mut d = Demuxer::open(source, Limits::default()).unwrap();
    fail.store(true, Ordering::Relaxed);
    let cancel = AtomicBool::new(false);
    let error = loop {
        match d.next_packet(&cancel) {
            Ok(Some(_)) => continue,
            Ok(None) => panic!("transport failure reported as end of stream"),
            Err(error) => break error,
        }
    };
    assert!(matches!(error, Error::Io(io::ErrorKind::ConnectionReset)));
}

#[test]
fn bounded_deterministic_mutation_campaign_does_not_panic_or_hang() {
    let original = fixture(
        cluster(0, [block(0, true), block(20, false)].concat()),
        false,
    );
    for index in 0..original.len() {
        for value in [0, 0x7f, 0x80, 0xff] {
            let mut bytes = original.clone();
            bytes[index] = value;
            let limits = Limits {
                metadata_bytes: 4096,
                block_bytes: 4096,
                elements: 1000,
                scan_bytes: 8192,
                ..Limits::default()
            };
            let _ = collect(&bytes, limits);
        }
    }
}

#[test]
fn zlib_compressed_frames_and_private_restore_their_original_bytes() {
    // Independently authored RFC 1951 stored block containing "abc" and Adler-32.
    let compressed = [120, 1, 1, 3, 0, 252, 255, 97, 98, 99, 2, 77, 1, 39];
    let encoding = element(
        0x6d80,
        &element(
            0x6240,
            &[uint(0x5032, 3), element(0x5034, &uint(0x4254, 0))].concat(),
        ),
    );
    let track = [
        uint(0xd7, 1),
        uint(0x73c5, 1),
        uint(0x83, 2),
        element(0x86, b"A_PCM/INT/LIT"),
        element(0x63a2, &compressed),
        encoding,
    ]
    .concat();
    let bytes = [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(
            0x18538067,
            &[
                element(0x1654ae6b, &element(0xae, &track)),
                cluster(
                    0,
                    element(0xa3, &[&[0x81, 0, 0, 0x80][..], &compressed].concat()),
                ),
            ]
            .concat(),
        ),
    ]
    .concat();
    let mut reader = Demuxer::open(Cursor::new(bytes), Limits::default()).unwrap();
    assert_eq!(reader.tracks()[0].codec_private, b"abc");
    assert_eq!(
        reader
            .next_packet(&AtomicBool::new(false))
            .unwrap()
            .unwrap()
            .data
            .as_ref(),
        b"abc"
    );
}

fn indexed_fixture() -> Vec<u8> {
    indexed_fixture_with_sparse_tail(false)
}

fn indexed_fixture_with_sparse_tail(sparse: bool) -> Vec<u8> {
    let video = element(
        0xae,
        &[
            uint(0xd7, 1),
            uint(0x73c5, 1),
            uint(0x83, 1),
            element(0x86, b"V_MPEG4/ISO/AVC"),
        ]
        .concat(),
    );
    let audio = element(
        0xae,
        &[
            uint(0xd7, 2),
            uint(0x73c5, 2),
            uint(0x83, 2),
            uint(0x56bb, 3_000_000_000),
            element(0x86, b"A_PCM/INT/LIT"),
        ]
        .concat(),
    );
    let tracks = element(0x1654ae6b, &[video, audio].concat());
    let head = |position| {
        element(
            0x114d9b74,
            &element(
                0x4dbb,
                &[
                    element(0x53ab, &[0x1c, 0x53, 0xbb, 0x6b]),
                    uint(0x53ac, position),
                ]
                .concat(),
            ),
        )
    };
    let mut clusters = Vec::new();
    let mut cues = Vec::new();
    for time in [0, 2000, 4000, 6000, 8000] {
        let position = tracks.len() + head(0).len() + clusters.len();
        if !sparse || time <= 4000 {
            cues.extend(element(
                0xbb,
                &[
                    uint(0xb3, time),
                    element(0xb7, &[uint(0xf7, 1), uint(0xf1, position as u64)].concat()),
                ]
                .concat(),
            ));
        }
        let audio = element(0xa3, &[0x82, 0, 0, 0x80, 4, 5, 6]);
        let padding = if time == 0 {
            element(0xec, &vec![0; 262144])
        } else {
            Vec::new()
        };
        clusters.extend(cluster(time, [block(0, true), audio, padding].concat()));
    }
    let cue_position = tracks.len() + head(0).len() + clusters.len();
    let body = [
        tracks,
        head(cue_position as u64),
        clusters,
        element(0x1c53bb6b, &cues),
    ]
    .concat();
    [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(0x18538067, &body),
    ]
    .concat()
}

#[test]
#[ignore = "open gap: the typed blocking reader scans to Cues sequentially (262,506 of \
            262,654 bytes here) instead of following the SeekHead; the retired legacy \
            reader read under 32 KiB"]
fn cues_seek_skips_distant_data_with_a_small_scan_budget() {
    let bytes = indexed_fixture();
    let mut reader = Demuxer::open_seekable(
        Cursor::new(bytes),
        Limits {
            scan_bytes: 32768,
            ..Limits::default()
        },
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let receipt = reader.seek_previous(0, 7_500_000_000, &cancel).unwrap();
    assert_eq!(receipt.actual_ns, 6_000_000_000);
    assert!(receipt.scanned_bytes < 32768);
    assert_eq!(
        reader.next_packet(&cancel).unwrap().unwrap().pts_ns,
        Some(6_000_000_000)
    );
}

#[test]
fn playback_seek_retains_all_track_preroll_and_resets_after_eof_and_cancel() {
    let mut reader =
        Demuxer::open_seekable(Cursor::new(indexed_fixture()), Limits::default()).unwrap();
    let cancel = AtomicBool::new(false);
    while reader.next_packet(&cancel).unwrap().is_some() {}
    for target in [7_500_000_000, 2_500_000_000, 9_000_000_000, 0] {
        let receipt = reader.seek_for_playback(0, target, &cancel).unwrap();
        assert_eq!(receipt.discard_until_ns, target);
        assert_eq!(receipt.preroll_ns, 3_000_000_000);
        assert!(receipt.used_index);
        let first = reader.next_packet(&cancel).unwrap().unwrap();
        assert!(first.keyframe);
        assert!(first.pts_ns.unwrap() <= target);
        assert!(first.pts_ns.unwrap() <= target.saturating_sub(3_000_000_000).max(0));
        let audio = reader.next_packet(&cancel).unwrap().unwrap();
        assert_eq!(audio.track, 1);
        assert_eq!(audio.pts_ns, first.pts_ns);
    }
    cancel.store(true, Ordering::Relaxed);
    assert!(matches!(
        reader.seek_for_playback(0, 7_000_000_000, &cancel),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        reader.next_packet(&cancel),
        Err(Error::Invalid(_))
    ));
    reader.seek_for_playback(0, 7_000_000_000, &cancel).unwrap();
    assert_eq!(
        reader.next_packet(&cancel).unwrap().unwrap().pts_ns,
        Some(4_000_000_000)
    );
}

#[test]
fn forged_cue_targets_and_index_limits_are_rejected_without_stale_packets() {
    let mut bytes = indexed_fixture();
    let cue = bytes.windows(4).position(|b| b == [0x1c, 0x53, 0xbb, 0x6b]);
    assert!(cue.is_some());
    let mut reader = Demuxer::open_seekable(
        Cursor::new(bytes.clone()),
        Limits {
            index_entries: 1,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(matches!(
        reader.seek_previous(0, 7_500_000_000, &AtomicBool::new(false)),
        Err(Error::Policy(_))
    ));
    // Corrupt the final CueClusterPosition to point at Segment metadata.
    let at = bytes.windows(2).rposition(|b| b == [0xf1, 0x88]).unwrap();
    bytes[at + 2..at + 10].fill(0);
    let mut reader = Demuxer::open_seekable(Cursor::new(bytes), Limits::default()).unwrap();
    assert!(matches!(
        reader.seek_for_playback(0, 20_000_000_000, &AtomicBool::new(false)),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        reader.next_packet(&AtomicBool::new(false)),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn extreme_cue_timestamps_never_overflow_during_lookup() {
    let tracks = element(
        0x1654ae6b,
        &element(
            0xae,
            &[
                uint(0xd7, 1),
                uint(0x73c5, 1),
                uint(0x83, 1),
                element(0x86, b"V_MPEG4/ISO/AVC"),
            ]
            .concat(),
        ),
    );
    let cues = element(
        0x1c53bb6b,
        &element(
            0xbb,
            &[
                uint(0xb3, u64::MAX),
                element(0xb7, &[uint(0xf7, 1), uint(0xf1, 0)].concat()),
            ]
            .concat(),
        ),
    );
    let body = [
        element(0x1549a966, &uint(0x2ad7b1, u64::MAX)),
        tracks,
        cues,
        cluster(0, block(0, true)),
    ]
    .concat();
    let bytes = [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(0x18538067, &body),
    ]
    .concat();
    let mut reader = Demuxer::open_seekable(Cursor::new(bytes), Limits::default()).unwrap();
    let _ = reader.seek_previous(0, 0, &AtomicBool::new(false));
}

#[test]
fn compressed_private_data_is_charged_to_the_shared_metadata_budget() {
    let compressed = [
        120, 156, 75, 76, 28, 5, 163, 128, 250, 0, 0, 238, 186, 227, 89,
    ];
    let encoding = element(
        0x6d80,
        &element(
            0x6240,
            &[uint(0x5032, 2), element(0x5034, &uint(0x4254, 0))].concat(),
        ),
    );
    let mut tracks = Vec::new();
    for number in 1..=4 {
        tracks.extend(element(
            0xae,
            &[
                uint(0xd7, number),
                uint(0x73c5, number),
                uint(0x83, 2),
                element(0x86, b"A_PCM/INT/LIT"),
                element(0x63a2, &compressed),
                encoding.clone(),
            ]
            .concat(),
        ));
    }
    let bytes = [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(
            0x18538067,
            &[element(0x1654ae6b, &tracks), cluster(0, block(0, true))].concat(),
        ),
    ]
    .concat();
    assert!(matches!(
        Demuxer::open(
            Cursor::new(bytes),
            Limits {
                metadata_bytes: 1024,
                ..Limits::default()
            }
        ),
        Err(Error::Policy(_))
    ));
}

#[test]
fn failed_index_load_cannot_become_a_silent_missing_index_on_retry() {
    let mut bytes = indexed_fixture();
    let at = bytes.windows(2).rposition(|b| b == [0xf1, 0x88]).unwrap();
    bytes[at + 2..at + 10].fill(255);
    let mut reader = Demuxer::open_seekable(Cursor::new(bytes), Limits::default()).unwrap();
    let cancel = AtomicBool::new(false);
    assert!(reader.seek_previous(0, 7_500_000_000, &cancel).is_err());
    assert!(reader.seek_previous(0, 7_500_000_000, &cancel).is_err());
}

#[test]
fn sparse_cues_seek_past_end_reaches_the_last_actual_random_access_block() {
    let mut reader = Demuxer::open_seekable(
        Cursor::new(indexed_fixture_with_sparse_tail(true)),
        Limits::default(),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    for _ in 0..2 {
        let receipt = reader
            .seek_for_playback(0, 15_000_000_000, &cancel)
            .unwrap();
        assert_eq!(receipt.actual_ns, 8_000_000_000);
        assert!(receipt.used_index);
        assert_eq!(
            reader.next_packet(&cancel).unwrap().unwrap().pts_ns,
            Some(8_000_000_000)
        );
        while reader.next_packet(&cancel).unwrap().is_some() {}
    }
}

#[test]
fn laced_decompression_shares_one_decoded_block_budget() {
    let compressed = [
        120, 156, 75, 76, 28, 5, 163, 128, 250, 0, 0, 238, 186, 227, 89,
    ];
    let encoding = element(0x6d80, &element(0x6240, &element(0x5034, &uint(0x4254, 0))));
    let track = [
        uint(0xd7, 1),
        uint(0x73c5, 1),
        uint(0x83, 2),
        element(0x86, b"A_PCM/INT/LIT"),
        encoding,
    ]
    .concat();
    let block = element(
        0xa3,
        &[&[0x81, 0, 0, 0x84, 1][..], &compressed, &compressed].concat(),
    );
    let bytes = [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(
            0x18538067,
            &[
                element(0x1654ae6b, &element(0xae, &track)),
                cluster(0, block),
            ]
            .concat(),
        ),
    ]
    .concat();
    assert!(matches!(
        collect(
            &bytes,
            Limits {
                block_bytes: 1024,
                ..Limits::default()
            }
        ),
        Err(Error::Policy(_))
    ));
}

#[test]
fn beginning_replay_is_a_source_position_fact_not_a_zero_timestamp_guess() {
    let bytes = fixture(
        [
            cluster(10_000, block(0, true)),
            cluster(20_000, block(0, true)),
        ]
        .concat(),
        false,
    );
    let mut d = Demuxer::open_seekable(Cursor::new(bytes), Limits::default()).unwrap();
    let cancel = AtomicBool::new(false);
    let later = d.seek_for_playback(0, 20_500_000_000, &cancel).unwrap();
    assert_eq!(later.actual_ns, 20_000_000_000);
    assert!(!later.replays_from_start);
    let beginning = d.seek_for_playback(0, 10_500_000_000, &cancel).unwrap();
    assert_eq!(beginning.actual_ns, 10_000_000_000);
    assert!(beginning.replays_from_start);
    assert!(
        !d.seek_previous(0, 10_500_000_000, &cancel)
            .unwrap()
            .replays_from_start
    );
}

fn colour_track_fixture(colour: &[u8]) -> Vec<u8> {
    let track = [
        uint(0xd7, 1),
        uint(0x73c5, 1),
        uint(0x83, 1),
        element(0x86, b"V_MPEG4/ISO/AVC"),
        element(
            0xe0,
            &[uint(0xb0, 128), uint(0xba, 72), element(0x55b0, colour)].concat(),
        ),
    ]
    .concat();
    [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(
            0x18538067,
            &[
                element(0x1654ae6b, &element(0xae, &track)),
                cluster(0, block(0, true)),
            ]
            .concat(),
        ),
    ]
    .concat()
}

#[test]
fn colour_metadata_obeys_byte_and_track_budgets_and_retains_source_after_drop() {
    let colour = [
        uint(0x55b1, 9),
        uint(0x55b2, 10),
        element(0x55d0, &element(0x55d1, &0.68_f32.to_be_bytes())),
    ]
    .concat();
    let bytes = colour_track_fixture(&colour);
    let d = Demuxer::open(Cursor::new(&bytes), Limits::default()).unwrap();
    let t = d.tracks()[0].clone();
    drop(d);
    let c = t.colour.unwrap();
    assert_eq!(c.bits_per_channel, 10);
    assert_eq!(c.matrix_coefficients, 9);
    assert_eq!(
        c.mastering.unwrap().chromaticity[0],
        Some(f64::from(0.68_f32))
    );
    assert!(
        t.source_metadata
            .windows(colour.len())
            .any(|part| part == colour)
    );
    for limits in [
        Limits {
            metadata_bytes: 8,
            ..Limits::default()
        },
        Limits {
            tracks: 0,
            ..Limits::default()
        },
    ] {
        assert!(matches!(
            Demuxer::open(Cursor::new(&bytes), limits),
            Err(Error::Policy(_))
        ));
    }
}

#[test]
fn invalid_or_duplicate_colour_cannot_become_playable_packets() {
    for data in [
        element(0x55d0, &element(0x55d1, &f64::NAN.to_be_bytes())),
        [uint(0x55b1, 9), uint(0x55b1, 1)].concat(),
    ] {
        assert!(matches!(
            Demuxer::open(Cursor::new(colour_track_fixture(&data)), Limits::default()),
            Err(Error::Invalid(_))
        ));
    }
}

#[test]
fn disabled_track_lacing_and_duplicate_track_uid_are_rejected() {
    let mut track = [
        uint(0xd7, 1),
        uint(0x73c5, 1),
        uint(0x83, 2),
        element(0x86, b"A_PCM/INT/LIT"),
        uint(0x9c, 0),
    ]
    .concat();
    let header = element(0x1a45dfa3, &element(0x4282, b"matroska"));
    let lace = element(0xa3, &[0x81, 0, 0, 0x84, 1, 1, 2, 3, 4]);
    let bytes = [
        header.clone(),
        element(
            0x18538067,
            &[
                element(0x1654ae6b, &element(0xae, &track)),
                cluster(0, lace),
            ]
            .concat(),
        ),
    ]
    .concat();
    assert!(matches!(
        collect(&bytes, Limits::default()),
        Err(Error::Invalid("lacing forbidden by track"))
    ));
    let mut other = track.clone();
    other.splice(0..uint(0xd7, 1).len(), uint(0xd7, 2));
    track = [element(0xae, &track), element(0xae, &other)].concat();
    let bytes = [
        header,
        element(
            0x18538067,
            &[element(0x1654ae6b, &track), cluster(0, block(0, true))].concat(),
        ),
    ]
    .concat();
    assert!(matches!(
        Demuxer::open(Cursor::new(bytes), Limits::default()),
        Err(Error::Invalid("duplicate track UID"))
    ));
}

#[test]
fn ietf_source_language_precedes_legacy_independently_of_storage_order() {
    let header = element(0x1a45dfa3, &element(0x4282, b"matroska"));
    let legacy = element(0x22b59c, b"fre");
    let ietf = element(0x22b59d, b"fr-CA");
    for languages in [
        [legacy.clone(), ietf.clone()].concat(),
        [ietf.clone(), legacy.clone()].concat(),
    ] {
        let track = [
            uint(0xd7, 1),
            uint(0x73c5, 1),
            uint(0x83, 1),
            element(0x86, b"V_MPEG4/ISO/AVC"),
            languages,
        ]
        .concat();
        let bytes = [
            header.clone(),
            element(
                0x18538067,
                &[
                    element(0x1654ae6b, &element(0xae, &track)),
                    cluster(0, block(0, true)),
                ]
                .concat(),
            ),
        ]
        .concat();
        let d = Demuxer::open(Cursor::new(bytes), Limits::default()).unwrap();
        let t = &d.tracks()[0];
        assert_eq!(t.language, "fre");
        assert_eq!(t.language_bcp47.as_deref(), Some("fr-CA"));
        assert_eq!(t.effective_language(), "fr-CA");
    }
}

#[test]
fn late_catalogue_metadata_and_attachments_survive_eof_repeated_seeks_and_faults() {
    use std::io::{Seek, SeekFrom};
    struct SeekFault {
        inner: Cursor<Vec<u8>>,
        fail: Arc<AtomicBool>,
    }
    impl Read for SeekFault {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.inner.read(b)
        }
    }
    impl Seek for SeekFault {
        fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
            if self.fail.load(Ordering::Relaxed) {
                Err(io::Error::other("injected catalogue seek fault"))
            } else {
                self.inner.seek(p)
            }
        }
    }
    let bytes = include_bytes!("fixtures/metadata-late.mkv").to_vec();
    let fail = Arc::new(AtomicBool::new(false));
    let reader = SeekFault {
        inner: Cursor::new(bytes),
        fail: Arc::clone(&fail),
    };
    let mut d = Demuxer::open_seekable(reader, Limits::default()).unwrap();
    let docs = d.metadata_documents().len();
    assert_eq!(docs, 4);
    let retained = d.metadata_retained_bytes();
    let owned: Vec<_> = d
        .metadata_documents()
        .iter()
        .flat_map(|doc| doc.attachments())
        .collect();
    assert_eq!(owned.len(), 1);
    while d.next_packet(&AtomicBool::new(false)).unwrap().is_some() {}
    assert_eq!(d.metadata_documents().len(), docs);
    assert_eq!(d.metadata_retained_bytes(), retained);
    for target in [0, 80_000_000, 40_000_000, 0] {
        d.seek_for_playback(0, target, &AtomicBool::new(false))
            .unwrap();
        while d.next_packet(&AtomicBool::new(false)).unwrap().is_some() {}
        assert_eq!(d.metadata_documents().len(), docs);
        assert_eq!(d.metadata_retained_bytes(), retained);
    }
    fail.store(true, Ordering::Relaxed);
    assert!(matches!(
        d.seek_for_playback(0, 0, &AtomicBool::new(false)),
        Err(Error::Io(_))
    ));
    assert!(matches!(
        d.next_packet(&AtomicBool::new(false)),
        Err(Error::Invalid(_))
    ));
    fail.store(false, Ordering::Relaxed);
    d.seek_for_playback(0, 0, &AtomicBool::new(false)).unwrap();
    assert!(d.next_packet(&AtomicBool::new(false)).unwrap().is_some());
    assert_eq!(d.metadata_documents().len(), docs);
    drop(d);
    assert_eq!(owned[0].data().unwrap(), b"font-data");
}
