//! Playback seeks to a subtitle that started before the cut: an unknown or known
//! end, the kept source prefix and the whole expected suffix, also when repeated
//! and after end of file.
#[path = "support/subtitle_recovery.rs"]
mod subtitle_recovery_kit;
use std::io::Cursor;
use std::sync::atomic::AtomicBool;
use subtitle_recovery_kit::{INPUT, assert_prefix};
use triergon_matroska::{BlockingReader, Limits, TypedPacket};

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

#[test]
fn subtitle_recovery_repeated_and_after_eof_replay_keep_the_same_whole_source_sequence() {
    let cancel = AtomicBool::new(false);
    let mut reader = BlockingReader::open_seekable(Cursor::new(INPUT), Limits::default()).unwrap();
    let mut previous = None;
    for _ in 0..3 {
        let receipt = reader.seek_for_playback(0, 8_000_000_000, &cancel).unwrap();
        assert_eq!(receipt.actual_ns, 8_000_000_000);
        assert!(!receipt.replays_from_start);
        let mut rows = Vec::new();
        let mut bytes = 0;
        while let Some(packet) = reader.next_typed_packet(&cancel).unwrap() {
            if rows.is_empty() {
                assert_prefix(&packet);
            }
            let packet = packet.packet;
            bytes += packet.data.len();
            rows.push((
                packet.track,
                packet.pts_ns,
                packet.dts_ns,
                packet.duration_ns,
                packet.keyframe,
                packet.discard_padding_ns,
                packet.data.as_ref().to_vec(),
            ));
        }
        assert_eq!((rows.len(), bytes), (78, 76_247));
        match &previous {
            Some(old) => assert_eq!(&rows, old),
            None => previous = Some(rows),
        }
    }
}

// These files use known DefaultDuration values. The playback seek keeps the
// preceding subtitle even if its known end precedes the shifted replay cutoff.
// The source duration is never rewritten.
fn known_default_duration_recovery(input: &[u8], duration: u64) {
    let cancel = AtomicBool::new(false);
    let mut reader = BlockingReader::open_seekable(Cursor::new(input), Limits::default()).unwrap();
    let receipt = reader.seek_for_playback(0, 8_000_000_000, &cancel).unwrap();
    assert_eq!(receipt.actual_ns, 8_000_000_000);
    let first = reader.next_typed_packet(&cancel).unwrap().unwrap();
    assert_eq!(first.packet.track, 2);
    assert_eq!(first.packet.pts_ns, Some(7_200_000_000));
    assert_eq!(first.packet.dts_ns, None);
    assert_eq!(first.packet.duration_ns, Some(duration));
    assert_eq!(
        first.configuration.tracks()[2].default_duration_ns,
        Some(duration)
    );
    assert!(first.packet.timing.duration_ticks.is_none());
    assert_eq!(first.packet.timing.default_duration_ns, Some(duration));
    let source_duration = first.packet.timing.source().unwrap().duration.unwrap();
    assert_eq!(
        (source_duration.numerator(), source_duration.denominator()),
        (i128::from(duration), 1)
    );
    assert_eq!(first.packet.data.as_ref(), b"Recovery 9/0");
    let mut count = 1;
    while reader.next_typed_packet(&cancel).unwrap().is_some() {
        count += 1;
    }
    assert_eq!(count, 15);
}

#[test]
fn subtitle_recovery_known_end_before_cut_remains_source_packet_not_visibility_inference() {
    known_default_duration_recovery(
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/seek-subtitle-recovery/native-controls/ended-before-cut.mkv"
        )),
        200_000_000,
    );
}

#[test]
fn subtitle_recovery_known_end_equal_cut_preserves_default_and_absent_block_duration() {
    known_default_duration_recovery(
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/seek-subtitle-recovery/native-controls/end-equals-cut.mkv"
        )),
        785_000_000,
    );
}

#[test]
fn subtitle_recovery_known_end_after_cut_preserves_default_and_absent_block_duration() {
    known_default_duration_recovery(
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/seek-subtitle-recovery/native-controls/overlaps-cut.mkv"
        )),
        800_000_000,
    );
}
