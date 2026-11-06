//! Original observed seek failure. Native gold is immutable f544 public-API
//! output; the subtitle's source duration/end remain unknown throughout.
use super::*;
use crate::Limits;
use crate::typed::BlockingReader;
use std::io::Cursor;

const INPUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/seek-subtitle-recovery/observed-11.mkv"
));
const NATIVE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/seek-subtitle-recovery/native-11.tsv"
));

fn reader() -> BlockingReader<Cursor<&'static [u8]>> {
    BlockingReader::open_seekable(Cursor::new(INPUT), Limits::default()).unwrap()
}

fn signed(value: &str) -> Option<i64> {
    (value != "null").then(|| value.parse().unwrap())
}

fn unsigned(value: &str) -> Option<u64> {
    (value != "null").then(|| value.parse().unwrap())
}

#[test]
fn seek_subtitle_recovery_observed_unknown_end_prefix_and_entire_native_suffix() {
    let cancel = AtomicBool::new(false);
    let mut reader = reader();
    let receipt = reader.seek_for_playback(0, 8_000_000_000, &cancel).unwrap();
    assert_eq!(receipt.actual_ns, 8_000_000_000);
    assert_eq!(receipt.requested_ns, 8_000_000_000);
    assert!(!receipt.replays_from_start);
    let mut count = 0;
    let mut bytes = 0;
    for (ordinal, line) in NATIVE.lines().enumerate() {
        let expected: Vec<_> = line.split('\t').collect();
        assert_eq!(expected.len(), 8, "literal native row {ordinal}");
        let actual = reader
            .next_typed_packet(&cancel)
            .unwrap()
            .unwrap_or_else(|| panic!("missing actual native row {ordinal}"));
        let packet = &actual.packet;
        assert_eq!(
            packet.track,
            expected[0].parse::<usize>().unwrap(),
            "row {ordinal} track"
        );
        assert_eq!(packet.pts_ns, signed(expected[1]), "row {ordinal} PTS");
        assert_eq!(packet.dts_ns, signed(expected[2]), "row {ordinal} DTS");
        assert_eq!(
            packet.duration_ns,
            unsigned(expected[3]),
            "row {ordinal} duration"
        );
        assert_eq!(packet.keyframe, expected[4] == "true", "row {ordinal} key");
        assert_eq!(
            packet.data.as_ref(),
            triergon_harness_protocol::unhex(expected[7]).as_slice(),
            "row {ordinal} payload"
        );
        // The raw trace retains both trim fields. This source has no discard
        // padding, and a noninitial replay has no beginning-of-source trim.
        assert_eq!((expected[5], expected[6]), ("0", "0"));
        assert_eq!(packet.discard_padding_ns, 0);
        if ordinal == 0 {
            assert_eq!(packet.track, 2);
            assert_eq!(packet.pts_ns, Some(7_200_000_000));
            assert_eq!(packet.data.as_ref(), b"Compiler sample 9");
            assert_eq!(packet.duration_ns, None);
            assert!(packet.timing.source().unwrap().duration.is_none());
            assert!(packet.timing.source().unwrap().pts.is_some());
        }
        count += 1;
        bytes += packet.data.len();
    }
    assert!(reader.next_typed_packet(&cancel).unwrap().is_none());
    assert_eq!((count, bytes), (78, 76_247));
}

#[test]
fn seek_subtitle_recovery_source_unknown_end_is_not_a_missing_timestamp() {
    let cancel = AtomicBool::new(false);
    let mut reader = reader();
    let mut subtitles = 0;
    let mut found = false;
    while let Some(actual) = reader.next_typed_packet(&cancel).unwrap() {
        let packet = actual.packet;
        if packet.track != 2 {
            continue;
        }
        subtitles += 1;
        assert_eq!(packet.duration_ns, None);
        assert!(packet.timing.source().unwrap().duration.is_none());
        if packet.data.as_ref() == b"Compiler sample 9" {
            assert_eq!(packet.pts_ns, Some(7_200_000_000));
            assert!(packet.timing.source().unwrap().pts.is_some());
            found = true;
        }
    }
    assert!(found);
    assert_eq!(subtitles, 21);
}

#[test]
fn seek_subtitle_recovery_previous_rap_operation_keeps_its_existing_boundary() {
    let cancel = AtomicBool::new(false);
    let mut reader = reader();
    let receipt = reader.seek_previous(0, 8_000_000_000, &cancel).unwrap();
    assert_eq!(receipt.actual_ns, 8_000_000_000);
    let actual = reader.next_typed_packet(&cancel).unwrap().unwrap();
    assert_eq!(actual.packet.track, 0);
    assert_eq!(actual.packet.pts_ns, Some(8_000_000_000));
    assert!(actual.packet.keyframe);
}
