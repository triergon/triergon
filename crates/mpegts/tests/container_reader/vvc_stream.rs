use super::{
    AccessUnit, CodecId, Drive, Event, PID, Packet, PacketInfo, ReaderLimits, TrackInfo, Transport,
};
use triergon_core::container::RandomAccess;

const PH: &[u8] = &[0, 0, 0, 1, 0, 0x99, 0x80];
const SLICE: &[u8] = &[0, 0, 1, 0, 1, 0x40];
const NEXT: &[u8] = &[0, 0, 0, 1, 0, 1, 0x80];

fn collect(ts: &Transport, window: usize, work: u64) -> Vec<Packet> {
    let mut drive = Drive::new(&ts.bytes, ReaderLimits::default(), window, work);
    let mut output = Vec::new();
    while let Some(event) = drive.next().unwrap() {
        if let Event::Packet(packet) = event {
            assert_eq!(packet.track_info().codec(), CodecId::Vvc);
            assert!(matches!(packet.access_unit(), AccessUnit::Vvc(_)));
            output.push(packet);
        }
    }
    output
}

#[test]
fn pictures_cross_finite_pes_and_carriers() {
    let mut first = [PH, SLICE].concat();
    first.extend(std::iter::repeat_n(0x55, 380));
    first.extend_from_slice(SLICE);
    for at in (1..8).chain([181, 184, 258, first.len() - 5, first.len()]) {
        let mut ts = Transport::new();
        ts.tables(&[(0x33, PID, &[])], 0);
        ts.pes(PID, 0xe0, &first[..at], Some(90_000), Some(88_000));
        let second_pes = [&first[at..], &NEXT[..2]].concat();
        ts.pes(PID, 0xe0, &second_pes, None, None);
        ts.pes(PID, 0xe0, &NEXT[2..], Some(180_000), None);

        for (window, work) in [(1, 1), (188, 200)] {
            let output = collect(&ts, window, work);
            assert_eq!(output.len(), 2, "split at {at}");
            assert!(
                output
                    .iter()
                    .all(|packet| packet.random_access() == RandomAccess::Unknown)
            );
            assert_eq!(output[0].payload(), first);
            assert_eq!(output[0].pts(), Some(90_000));
            assert_eq!(output[0].dts(), Some(88_000));
            assert_eq!(output[1].payload(), NEXT);
            assert_eq!(output[1].elementary_range().start, first.len() as u64);
            for packet in &output {
                let range = packet.elementary_range();
                let mut end = range.start;
                for source in packet.sources() {
                    assert_eq!(source.elementary_range.start, end);
                    end = source.elementary_range.end;
                    assert_eq!(
                        source.elementary_range.end - source.elementary_range.start,
                        source.payload_range.len() as u64,
                    );
                }
                assert_eq!(end, range.end);
            }
        }
    }
}

#[test]
fn idr_types_survive_split_pes_and_mixed_pictures_remain_unknown() {
    // A standalone picture header permits multiple slices in one picture.
    let slice = |kind: u8| [0, 0, 1, 0, (kind << 3) | 1, 0x40];
    for (types, expected) in [
        (vec![7], RandomAccess::Sync),
        (vec![8], RandomAccess::Sync),
        (vec![7, 7], RandomAccess::Sync),
        (vec![8, 8], RandomAccess::Sync),
        (vec![0], RandomAccess::Unknown),
        (vec![9], RandomAccess::Unknown),
        (vec![10], RandomAccess::Unknown),
        (vec![7, 0], RandomAccess::Unknown),
        (vec![0, 7], RandomAccess::Unknown),
    ] {
        let mut picture = PH.to_vec();
        for &kind in &types {
            picture.extend_from_slice(&slice(kind));
        }
        for split in 1..picture.len() {
            let mut ts = Transport::new();
            ts.tables(&[(0x33, PID, &[])], 0);
            ts.pes(PID, 0xe0, &picture[..split], Some(90_000), None);
            ts.pes(PID, 0xe0, &picture[split..], None, None);
            ts.pes(PID, 0xe0, NEXT, None, None);
            let output = collect(&ts, 1, 1);
            assert_eq!(output.len(), 2);
            assert_eq!(output[0].payload(), picture, "split {split}");
            assert_eq!(
                output[0].random_access(),
                expected,
                "types {types:?}, split {split}"
            );
            let AccessUnit::Vvc(unit) = output[0].access_unit() else {
                panic!("VVC")
            };
            assert_eq!(unit.first_vcl_type, types[0]);
            assert_eq!(output[1].random_access(), RandomAccess::Unknown);
        }
    }
}

#[test]
fn multiple_pictures_in_a_pes_and_terminal_picture() {
    let mut ts = Transport::new();
    ts.tables(&[(0x33, PID, &[])], 0);
    ts.pes(PID, 0xe0, &NEXT.repeat(3), Some(90_000), None);
    let output = collect(&ts, 7, 1);
    assert_eq!(output.len(), 3);
    for (index, packet) in output.iter().enumerate() {
        assert_eq!(packet.payload(), NEXT);
        let start = (index * NEXT.len()) as u64;
        assert_eq!(packet.elementary_range(), start..start + NEXT.len() as u64);
        assert_eq!(packet.sources().len(), 1);
    }
    assert_eq!(output[0].pts(), Some(90_000));
    assert_eq!(output[0].dts(), None);
    assert_eq!(output[1].pts(), None);
    assert_eq!(output[2].pts(), None);
}

#[test]
fn a_signalled_discontinuity_discards_a_pending_picture() {
    use super::{ReaderEvent, TraceEvent};
    use triergon_mpegts::ContinuityOutcome;

    let mut ts = Transport::new();
    ts.tables(&[(0x33, PID, &[])], 0);
    ts.pes(PID, 0xe0, NEXT, Some(90_000), None);
    let mut boundary = [0xff; 188];
    boundary[..6].copy_from_slice(&[0x47, (PID >> 8) as u8, PID as u8, 0x28, 183, 0x80]);
    ts.bytes.extend_from_slice(&boundary);
    ts.counters[usize::from(PID)] = 9;
    ts.pes(PID, 0xe0, NEXT, Some(180_000), None);
    let mut drive = Drive::new(&ts.bytes, ReaderLimits::default(), 7, 1);
    let mut interrupted = false;
    let mut emitted = false;
    while let Some(event) = drive.next().unwrap() {
        match event {
            Event::Other(ReaderEvent::Interrupted {
                track,
                elementary_range,
                cause,
            }) => {
                assert_eq!(track.stream().pid, PID);
                assert_eq!(elementary_range, 0..NEXT.len() as u64);
                assert!(matches!(
                    cause.event(),
                    TraceEvent::Continuity { pid, outcome: ContinuityOutcome::SignalledDiscontinuity }
                        if *pid == PID
                ));
                assert!(!interrupted);
                interrupted = true;
            }
            Event::Packet(packet) => {
                assert!(interrupted);
                assert!(!emitted);
                assert_eq!(packet.payload(), NEXT);
                let clock = packet.clock().unwrap();
                assert!(clock.quarantined);
                assert_eq!(clock.pts.raw33, Some(180_000));
                assert_eq!(packet.pts(), None);
                emitted = true;
            }
            _ => {}
        }
    }
    assert!(interrupted && emitted);
}

use crate::sha256;

#[test]
#[ignore = "requires pinned FFprobe 9.0.2 and the original VVC FATE input"]
fn original_vvc_pictures_wrapped_in_transport_match_the_reference() {
    use std::{
        io::Write,
        path::PathBuf,
        process::{Command, Stdio},
    };

    let root = PathBuf::from(std::env::var_os("TRIERGON_VVC_FATE").unwrap());
    let ffprobe = std::env::var_os("TRIERGON_VVC_FFPROBE").unwrap();
    let version = Command::new(&ffprobe).arg("-version").output().unwrap();
    assert!(version.status.success());
    assert!(
        String::from_utf8(version.stdout)
            .unwrap()
            .starts_with("ffprobe version 9.0.2 ")
    );
    let path = root.join("mv-clip-wrap.vvc");
    let source = std::fs::read(&path).unwrap();
    assert_eq!(
        sha256::file_hex(&path).unwrap(),
        "757766c97e854f5108fa9129b6899c7355a3d2ae2d0a3a3d3b1f59ffbc5a4597"
    );
    for split in [1, 2, 6, 41, 80, 81] {
        let mut ts = Transport::new();
        ts.tables(&[(0x33, PID, &[])], 0);
        ts.pes(PID, 0xe0, &source[..split], Some(90_000), Some(88_000));
        if split < 81 {
            ts.pes(PID, 0xe0, &source[split..81], None, None);
        }
        ts.pes(PID, 0xe0, &source[81..], Some(180_000), Some(178_000));
        let mut child = Command::new(&ffprobe)
            .args([
                "-v",
                "error",
                "-fflags",
                "+nofillin",
                "-f",
                "mpegts",
                "-show_entries",
                "packet=pts,dts,size,flags,data_hash",
                "-show_data_hash",
                "sha256",
                "-of",
                "compact=p=0",
                "pipe:0",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&ts.bytes).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        let reference = String::from_utf8(output.stdout).unwrap();
        let packets = collect(&ts, 17, 5);
        let reference_fields: Vec<_> = reference
            .lines()
            .map(|line| {
                let (mut fields, mut sync) = (Vec::new(), None);
                for field in line.trim_end_matches('|').split('|') {
                    if let Some(flags) = field.strip_prefix("flags=") {
                        assert!(sync.replace(flags.contains('K')).is_none());
                    } else {
                        fields.push(field);
                    }
                }
                (fields.join("|"), sync.unwrap())
            })
            .collect();
        let actual = packets
            .iter()
            .map(|packet| {
                format!(
                    "pts={}|dts={}|size={}|data_hash=SHA256:{}",
                    packet
                        .pts()
                        .map_or_else(|| "N/A".into(), |pts| pts.to_string()),
                    packet
                        .dts()
                        .map_or_else(|| "N/A".into(), |dts| dts.to_string()),
                    packet.payload().len(),
                    sha256::hex(packet.payload())
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            actual.iter().map(String::as_str).collect::<Vec<_>>(),
            reference_fields
                .iter()
                .map(|(row, _)| row.as_str())
                .collect::<Vec<_>>(),
            "split at {split}"
        );
        let sync = |packet: &Packet| packet.random_access() == RandomAccess::Sync;
        assert!(packets.iter().any(sync));
        assert_eq!(
            packets.iter().map(sync).collect::<Vec<_>>(),
            reference_fields
                .iter()
                .map(|(_, sync)| *sync)
                .collect::<Vec<_>>(),
            "split at {split}"
        );
    }
}

#[test]
fn clock_anchors_at_the_start_code_after_leading_padding() {
    let mut ts = Transport::new();
    ts.tables(&[(0x33, PID, &[])], 0);
    ts.pes(PID, 0xe0, &[0], None, None);
    ts.pes(PID, 0xe0, NEXT, Some(90_000), Some(88_000));
    let output = collect(&ts, 1, 1);
    assert_eq!(output.len(), 1);
    assert_eq!(output[0].payload(), [&[0][..], NEXT].concat());
    assert_eq!(output[0].elementary_range(), 0..8);
    assert_eq!(output[0].sources().len(), 2);
    assert_eq!(output[0].pts(), Some(90_000));
    assert_eq!(output[0].dts(), Some(88_000));
}

#[test]
fn a_mid_picture_restart_reports_vvc_failure_and_keeps_audio() {
    use super::{ReaderEvent, adts};

    let mut ts = Transport::new();
    ts.tables(&[(0x33, PID, &[]), (0x0f, PID + 1, &[])], 0);
    ts.pes(PID, 0xe0, NEXT, Some(90_000), None);
    let mut boundary = [0xff; 188];
    boundary[..6].copy_from_slice(&[0x47, (PID >> 8) as u8, PID as u8, 0x28, 183, 0x80]);
    ts.bytes.extend_from_slice(&boundary);
    ts.counters[usize::from(PID)] = 9;
    ts.pes(PID, 0xe0, &[0x80, 0x55, 0x33], Some(180_000), None);
    let audio = adts(b"independent audio");
    ts.pes(PID + 1, 0xc0, &audio, Some(192_000), None);

    let mut drive = Drive::new(&ts.bytes, ReaderLimits::default(), 7, 1);
    let mut reports = 0;
    let mut packets = 0;
    while let Some(event) = drive.next().unwrap() {
        match event {
            Event::Other(ReaderEvent::Unsupported { track, error }) => {
                assert_eq!(track.stream().pid, PID);
                assert!(matches!(error, super::Error::Invalid(_)));
                reports += 1;
            }
            Event::Packet(packet) => {
                assert_eq!(packet.track_info().stream().pid, PID + 1);
                assert_eq!(packet.payload(), audio);
                packets += 1;
            }
            _ => {}
        }
    }
    assert_eq!((reports, packets), (1, 1));
}
