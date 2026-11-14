//! VobSub `.idx` lines (`dvd::vobsub`) and the reference's index and packet
//! reading (`compat::dvd::VobSubIndex`). Expected values are the pinned FFmpeg
//! build's `vobsub` demuxer output for the same files.
#[path = "dvd-support/mod.rs"]
mod support;

use support::{pes, sector, subpicture};
use triergon_core::{Budget, BudgetVec, Error};
use triergon_subtitles::compat::dvd::{VobSubEntry, VobSubIndex};
use triergon_subtitles::dvd::vobsub::{Line, Lines};

fn lines(text: &[u8]) -> Vec<Result<Line<'_>, Error>> {
    Lines::new(text).map(|(_, line)| line).collect()
}

#[test]
fn every_line_kind_is_recognised() {
    let text = b"# VobSub index file, v7\n\nsize: 720x480\r\nlangidx: 0\nid: en, index: 3\n\
        id: de\ndelay: -00:00:01:500\ntimestamp: 01:02:03:004, filepos: 00001a800\n\
        timestamp:0:0:5:0,filepos:0\n";
    assert_eq!(
        lines(text),
        [
            Ok(Line::Comment),
            Ok(Line::Blank),
            Ok(Line::Setting(b"size: 720x480")),
            Ok(Line::LanguageIndex(b"0")),
            Ok(Line::Track {
                language: b"en",
                index: Some(3)
            }),
            Ok(Line::Track {
                language: b"de",
                index: None
            }),
            Ok(Line::Delay(-1500)),
            Ok(Line::Timestamp {
                milliseconds: 3_723_004,
                filepos: 0x1a800
            }),
            Ok(Line::Timestamp {
                milliseconds: 5000,
                filepos: 0
            }),
        ]
    );
}

#[test]
fn a_malformed_timestamp_is_an_error() {
    for line in [
        &b"timestamp: -00:00:01:000, filepos: 0"[..],
        b"timestamp: 00:00:01, filepos: 0",
        b"timestamp: 00:00:01:000",
        b"id: en, index: 300",
    ] {
        assert!(lines(line)[0].is_err(), "{}", String::from_utf8_lossy(line));
    }
    let budget = Budget::new(1 << 16).unwrap();
    let text = b"id: en, index: 0\ntimestamp: -00:00:01:000, filepos: 0\n";
    assert!(VobSubIndex::parse(&budget, text).is_err());
}

#[test]
fn the_configuration_is_every_setting_before_the_first_track() {
    let budget = Budget::new(1 << 16).unwrap();
    let text = b"# c\nsize: 720x576\r\norg: 0, 0\n\nlangidx: 0\ndelay: 00:00:01:000\n  indented\n\
        id: en, index: 0\npalette: 000000\ntimestamp: 00:00:01:000, filepos: 0\n";
    let index = VobSubIndex::parse(&budget, text).unwrap();
    assert_eq!(
        index.configuration(),
        b"size: 720x576\norg: 0, 0\n  indented\n"
    );
    let entries: Vec<_> = index.entries(0).collect();
    assert_eq!(entries[0].milliseconds, 2000);
}

#[test]
fn streams_are_created_by_their_first_timestamp() {
    let budget = Budget::new(1 << 16).unwrap();
    let text = b"id: en, index: 0\nid: de, index: 1\ntimestamp: 00:00:02:000, filepos: 800\n\
        id: fr, index: 2\ntimestamp: 00:00:01:000, filepos: 0\nid: en, index: 0\n\
        timestamp: 00:00:03:000, filepos: 1000\nid: de, index: 1\ntimestamp: 00:00:04:000, filepos: 1800\n";
    let index = VobSubIndex::parse(&budget, text).unwrap();
    assert_eq!(index.streams(), [1, 2, 0]);
    let times = |s| index.entries(s).map(|e| e.milliseconds).collect::<Vec<_>>();
    assert_eq!(times(0), [2000, 4000]);
    assert_eq!(times(1), [1000]);
    assert_eq!(times(2), [3000]);
}

/// `delay:` values as observed in the pinned FFmpeg build for 32-bit ARMv7:
/// spaces then an optional `-` for the whole delay, up to four decimal fields
/// saturating at 2³¹ − 1 and −2³¹, missing fields zero, never an error.
#[test]
fn delays_are_read_as_32_bit_arm_reads_them() {
    let delay = |value: &str| match lines(format!("delay:{value}").as_bytes())[..] {
        [Ok(Line::Delay(milliseconds))] => milliseconds,
        ref other => panic!("{value:?}: {other:?}"),
    };
    for (value, milliseconds) in [
        (" 00:00:01:000", 1000),
        (" -00:00:00:500", -500),
        ("-00:00:01:000", -1000),
        ("  -00:00:01:000", -1000),
        (" +00:00:01:000", 1000),
        ("  +00:00:01:000", 1000),
        // Only spaces come before the sign; the first field reads the rest.
        ("\t-00:00:01:000", 1000),
        (" --00:00:01:000", -1000),
        (" - -00:00:01:000", -1000),
        (" - 00:00:01:000", -1000),
        (" -+00:00:01:000", -1000),
        (" 0:0:0:-500", -500),
        (" -0:0:0:-500", 500),
        (" 00:00:-01:000", -1000),
        (" 00: 00:01:000", 1000),
        // Fields it does not reach are zero.
        (" 00:00:01", 1000),
        (" 9", 32_400_000),
        (" 9:8", 32_880_000),
        (" 00 :00:01:000", 0),
        (" 0x10:0:0:0", 0),
        (" garbage", 0),
        ("", 0),
        (" 00:00:01:000x", 1000),
        (" 1:2:3:4:5", 3_723_004),
        // Each field saturates at `int`; the sum has 64 bits.
        (" 596:31:23:648", 2_147_483_648),
        (" 1193:0:0:0", 4_294_800_000),
        (" 0:35791394:0:0", 2_147_483_640_000),
        (" 0:0:0:2147483648", 2_147_483_647),
        (" 0:0:0:4294967296", 2_147_483_647),
        (" 0:0:0:99999999999999999999", 2_147_483_647),
        (" 0:0:0:-2147483649", -2_147_483_648),
        (" -0:0:0:2147483648", -2_147_483_647),
        (" -0:0:0:-2147483649", 2_147_483_648),
        (" 4294967296:0:0:0", 7_730_941_129_200_000),
        (" -2147483648:0:0:0", -7_730_941_129_200_000),
        (" -4294967297:0:0:0", -7_730_941_129_200_000),
    ] {
        assert_eq!(delay(value), milliseconds, "{value:?}");
    }
    // Each line replaces the delay, whatever it says.
    let budget = Budget::new(1 << 16).unwrap();
    let text = b"id: en, index: 0\ndelay: 00:00:05:000\ndelay: garbage\n\
        timestamp: 00:00:01:000, filepos: 0\ndelay: 1:2:3:4\ndelay: 00:00:07\n\
        timestamp: 00:00:01:000, filepos: 800\n";
    let index = VobSubIndex::parse(&budget, text).unwrap();
    let times: Vec<_> = index.entries(0).map(|e| e.milliseconds).collect();
    assert_eq!(times, [1000, 8000]);
}

#[test]
fn units_are_ordered_by_position_then_time_and_each_ends_at_the_next() {
    let budget = Budget::new(1 << 16).unwrap();
    let text = b"id: en, index: 0\ntimestamp: 00:00:03:000, filepos: 2800\n\
        delay: 00:00:01:000\ntimestamp: 00:00:01:000, filepos: 0\ndelay: -00:00:00:500\n\
        timestamp: 00:00:02:000, filepos: 2000\ntimestamp: 00:00:01:000, filepos: 2000\n";
    let index = VobSubIndex::parse(&budget, text).unwrap();
    let entries: Vec<_> = index.entries(0).collect();
    let view: Vec<_> = entries
        .iter()
        .map(|e| (e.milliseconds, e.filepos, e.end))
        .collect();
    assert_eq!(
        view,
        [
            (2000, 0, Some(0x2000)),
            (500, 0x2000, Some(0x2000)),
            (1500, 0x2000, Some(0x2800)),
            (3000, 0x2800, None),
        ]
    );
}

fn entry(end: Option<u64>) -> VobSubEntry {
    VobSubEntry {
        milliseconds: 0,
        filepos: 0,
        end,
        substream: 0,
    }
}

fn read(entry: VobSubEntry, sub: &[u8]) -> Option<Vec<u8>> {
    let budget = Budget::new(1 << 20).unwrap();
    let mut out = BudgetVec::new(&budget);
    entry
        .read(sub, &mut out)
        .unwrap()
        .then(|| out.as_slice().to_vec())
}

#[test]
fn a_unit_gathers_packets_until_its_extent_or_another_subpicture_stream() {
    let first = sector(&[subpicture(0, &[1; 100], Some(90000))]);
    let rest = sector(&[subpicture(0, &[2; 50], None)]);
    let other = sector(&[subpicture(1, &[3; 50], Some(90000))]);
    let video = sector(&[pes(0xe0, &[4; 20], None)]);
    let navigation = sector(&[pes(0xbf, &[5; 20], None)]);
    let file = [
        first.clone(),
        video.clone(),
        navigation,
        rest.clone(),
        other,
        rest.clone(),
    ]
    .concat();
    // The video packet's data is taken as the unit's; navigation is skipped; a
    // packet of another subpicture stream ends the unit.
    let expected = [vec![1; 100], vec![4; 20], vec![2; 50]].concat();
    assert_eq!(read(entry(None), &file), Some(expected));
    // A packet that would end beyond the extent ends the unit.
    let end = (2048 + 14 + 6 + 3 + 20) as u64;
    assert_eq!(read(entry(Some(end - 1)), &file), Some(vec![1; 100]));
    let joined = [vec![1; 100], vec![4; 20]].concat();
    assert_eq!(read(entry(Some(end)), &file), Some(joined));
    // An empty extent still reads, and delivers an empty packet; nothing at
    // all to read delivers none.
    assert_eq!(read(entry(Some(0)), &file), Some(Vec::new()));
    assert_eq!(read(entry(None), &[0, 0, 1, 0xb9]), None);
}

#[test]
fn reading_from_inside_a_packet_starts_at_the_next_start_code() {
    let first = sector(&[subpicture(0, &[1; 100], Some(90000))]);
    let second = sector(&[subpicture(0, &[2; 50], Some(90000))]);
    let file = [first, second].concat();
    assert_eq!(
        read(entry(None), &file[8..]),
        Some([vec![1; 100], vec![2; 50]].concat())
    );
    assert_eq!(read(entry(None), &file[200..]), Some(vec![2; 50]));
}
