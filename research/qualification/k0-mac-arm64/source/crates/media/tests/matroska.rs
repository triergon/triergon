//! Original format-level fixtures, independent of the FFmpeg differential oracle.
use safe_media::{Error, Limits, matroska::Demuxer};
use std::{
    io::{self, Cursor, Read},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

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
fn collect(bytes: &[u8], limits: Limits) -> safe_media::Result<Vec<safe_media::matroska::Packet>> {
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
    let mut d = Demuxer::open(Cursor::new(bytes), Limits::default()).unwrap();
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
        assert!(matches!(collect(&bytes, limits), Err(Error::Limit(_))));
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
    let mut d = Demuxer::open(Cursor::new(bytes), Limits::default()).unwrap();
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
    let mut d = Demuxer::open(Cursor::new(bytes), Limits::default()).unwrap();
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
    let fail = Arc::new(AtomicBool::new(false));
    let source = Faulty {
        source: Cursor::new(fixture(cluster(0, block(0, true)), true)),
        fail: fail.clone(),
    };
    let mut d = Demuxer::open(source, Limits::default()).unwrap();
    fail.store(true, Ordering::Relaxed);
    assert!(
        matches!(d.next_packet(&AtomicBool::new(false)),Err(Error::Io(e)) if e.kind()==io::ErrorKind::ConnectionReset)
    );
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
