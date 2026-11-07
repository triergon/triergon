//! Blocking adapter: one-byte short reads and whole-buffer reads of the same
//! authored sources yield identical tracks, metadata, seek receipts and packets.
#[path = "support/ebml.rs"]
mod ebml;
use ebml::{element as e, mkv, track, uint as u};
use std::{
    io::{self, Cursor, Read},
    sync::atomic::AtomicBool,
};
use triergon_matroska::{Limits, matroska::Packet, typed::BlockingReader};
const ABC: [u8; 14] = [120, 1, 1, 3, 0, 252, 255, 97, 98, 99, 2, 77, 1, 39];
fn packets_equal(a: &Packet, b: &Packet) {
    assert_eq!(
        (
            a.track,
            a.pts_ns,
            a.dts_ns,
            a.duration_ns,
            a.keyframe,
            a.discard_padding_ns
        ),
        (
            b.track,
            b.pts_ns,
            b.dts_ns,
            b.duration_ns,
            b.keyframe,
            b.discard_padding_ns
        )
    );
    assert_eq!(a.data.as_ref(), b.data.as_ref());
    let a = a.timing.source().unwrap();
    let b = b.timing.source().unwrap();
    assert_eq!(
        a.pts.map(|t| (t.numerator(), t.denominator())),
        b.pts.map(|t| (t.numerator(), t.denominator()))
    );
}
#[test]
fn sequential_short_reads_restore_private_and_laced_packets_without_a_second_parser() {
    let encoding = |order, algorithm, settings: &[u8]| {
        e(
            0x6240,
            &[
                u(0x5031, order),
                u(0x5032, 3),
                e(
                    0x5034,
                    &[u(0x4254, algorithm), e(0x4255, settings)].concat(),
                ),
            ]
            .concat(),
        )
    };
    let extra = [
        e(0x63a2, &ABC),
        e(0x536e, "épisode".as_bytes()),
        e(0x22b59d, b"fr-CA"),
        e(
            0x6d80,
            &[encoding(1, 0, &[]), encoding(0, 3, b"P")].concat(),
        ),
    ]
    .concat();
    let mut block = vec![0x81, 0, 0, 0x84, 1];
    block.extend(ABC);
    block.extend(ABC);
    let raw = mkv(&[
        e(0x1654ae6b, &track(1, 1, &extra)),
        e(0x1f43b675, &[u(0xe7, 2), e(0xa3, &block)].concat()),
    ]
    .concat());
    struct Short(Cursor<Vec<u8>>);
    impl Read for Short {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let len = out.len().min(1);
            self.0.read(&mut out[..len])
        }
    }
    let mut old = BlockingReader::open(Cursor::new(raw.clone()), Limits::default()).unwrap();
    let mut new = BlockingReader::open(Short(Cursor::new(raw)), Limits::default()).unwrap();
    let a = &old.tracks()[0];
    let b = &new.tracks()[0];
    assert_eq!(a.source_metadata.as_ref(), b.source_metadata.as_ref());
    assert_eq!(a.codec_private.as_ref(), b.codec_private.as_ref());
    assert_eq!(b.codec_private.as_ref(), b"Pabc");
    assert_eq!(a.name.as_str(), b.name.as_str());
    assert_eq!(a.effective_language(), b.effective_language());
    let cancel = AtomicBool::new(false);
    loop {
        match (
            old.next_packet(&cancel).unwrap(),
            new.next_packet(&cancel).unwrap(),
        ) {
            (Some(a), Some(b)) => packets_equal(&a, &b),
            (None, None) => break,
            _ => panic!("packet count differs"),
        }
    }
}

#[test]
fn seekable_adapter_resolves_late_catalogue_and_matches_preroll_receipt_and_replayed_packets() {
    let target = |id, position| e(0x4dbb, &[u(0x53ab, id), u(0x53ac, position)].concat());
    let make_head = |cues, chapters| {
        e(
            0x114d9b74,
            &[target(0x1c53bb6b, cues), target(0x1043a770, chapters)].concat(),
        )
    };
    let placeholder = make_head(0, 0);
    let tracks = e(
        0x1654ae6b,
        &[track(1, 1, &[]), track(2, 2, &u(0x56bb, 32_000_000))].concat(),
    );
    let first = e(
        0x1f43b675,
        &[
            u(0xe7, 5),
            e(0xa3, &[0x82, 0, 0, 0x80, b'A']),
            e(0xa3, &[0x81, 0, 0, 0x80, b'V']),
        ]
        .concat(),
    );
    let second = e(
        0x1f43b675,
        &[u(0xe7, 100), e(0xa3, &[0x81, 0, 0, 0x80, b'E'])].concat(),
    );
    let cue = |ticks, position| {
        e(
            0xbb,
            &[
                u(0xb3, ticks),
                e(0xb7, &[u(0xf7, 1), u(0xf1, position)].concat()),
            ]
            .concat(),
        )
    };
    let first_position = placeholder.len() + tracks.len();
    let cues = e(
        0x1c53bb6b,
        &[
            cue(5, first_position as u64),
            cue(100, (first_position + first.len()) as u64),
        ]
        .concat(),
    );
    let chapters = e(
        0x1043a770,
        &e(
            0x45b9,
            &e(
                0xb6,
                &[
                    u(0x73c4, 1),
                    u(0x91, 0),
                    e(0x80, &e(0x85, "épisode".as_bytes())),
                ]
                .concat(),
            ),
        ),
    );
    let cues_position = first_position + first.len() + second.len();
    let head = make_head(cues_position as u64, (cues_position + cues.len()) as u64);
    assert_eq!(head.len(), placeholder.len());
    let raw = mkv(&[head, tracks, first, second, cues, chapters].concat());
    let mut old =
        BlockingReader::open_seekable(Cursor::new(raw.clone()), Limits::default()).unwrap();
    let mut new = BlockingReader::open_seekable(Cursor::new(raw), Limits::default()).unwrap();
    let a: Vec<_> = old
        .opaque_metadata()
        .map(|(id, raw)| (id, raw.to_vec()))
        .collect();
    let b: Vec<_> = new
        .opaque_metadata()
        .map(|(id, raw)| (id, raw.to_vec()))
        .collect();
    assert_eq!(a, b);
    assert_eq!(a.len(), 1);
    let cancel = AtomicBool::new(false);
    let a = old.seek_for_playback(0, 90_000_000, &cancel).unwrap();
    let b = new.seek_for_playback(0, 90_000_000, &cancel).unwrap();
    assert_eq!(
        (
            a.requested_ns,
            a.actual_ns,
            a.scanned_bytes,
            a.used_index,
            a.discard_until_ns,
            a.preroll_ns,
            a.replays_from_start
        ),
        (
            b.requested_ns,
            b.actual_ns,
            b.scanned_bytes,
            b.used_index,
            b.discard_until_ns,
            b.preroll_ns,
            b.replays_from_start
        )
    );
    assert_eq!(b.actual_ns, 5_000_000);
    assert!(b.replays_from_start);
    loop {
        match (
            old.next_packet(&cancel).unwrap(),
            new.next_packet(&cancel).unwrap(),
        ) {
            (Some(a), Some(b)) => packets_equal(&a, &b),
            (None, None) => break,
            _ => panic!("replayed packet count differs"),
        }
    }
}

#[test]
fn retaining_only_an_existing_static_codec_id_does_not_retain_the_tracks_allocation() {
    use triergon_core::Budget;
    let entry = e(
        0xae,
        &[u(0xd7, 1), u(0x73c5, 1), u(0x83, 2), e(0x86, b"A_AAC")].concat(),
    );
    let raw = mkv(&[e(0x1654ae6b, &entry), e(0x1f43b675, &[])].concat());
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let reader = BlockingReader::open_in(Cursor::new(raw), Limits::default(), &pool).unwrap();
    let codec = reader.tracks()[0].codec.clone();
    drop(reader);
    assert_eq!(codec.as_str(), "A_AAC");
    assert_eq!(
        pool.used(),
        baseline,
        "the qualified source interns this exact codec literal"
    );
}
