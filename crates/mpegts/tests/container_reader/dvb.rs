use super::{
    AccessUnit, Budget, CodecId, Drive, Error, Event, MediaKind, OFF, PID, PROGRAM, Packet,
    PacketInfo, ReaderEvent, ReaderLimits, Step, TraceEvent, TrackInfo, Transport, WorkBudget,
    audio_eof_namespace, timestamp,
};
use std::sync::atomic::AtomicBool;
use triergon_core::Bytes;
use triergon_mpegts::compat::annex_b::VideoEofPolicy;
use triergon_mpegts::compat::dvb::TailPolicy;
use triergon_mpegts::compat::latm;
use triergon_mpegts::{DvbSubtitleUnit, Pes};

const COMPLETE: &[u8] = include_bytes!("../data/dvb/dvb-default-clut2.pes-data");
const TRUNCATED: &[u8] = include_bytes!("../data/dvb/dvb-truncated-segment.pes-data");
const SERVICES: &[u8] = &[0x59, 8, b'e', b'n', b'g', 0x10, 0, 1, 0, 2];

fn limits() -> ReaderLimits {
    ReaderLimits {
        descriptor_namespace: audio_eof_namespace(),
        ..ReaderLimits::default()
    }
}

fn transport() -> Transport {
    let mut ts = Transport::new();
    ts.tables(&[(0x06, PID, SERVICES)], 0);
    ts
}

fn subtitle_pes(ts: &mut Transport, payload: &[u8], pts: Option<u64>, stream_id: u8, align: bool) {
    let mut pes = vec![
        0,
        0,
        1,
        stream_id,
        0,
        0,
        0x80 | (u8::from(align) << 2),
        0,
        0,
    ];
    if let Some(pts) = pts {
        pes[7] = 0x80;
        pes[8] = 5;
        timestamp(&mut pes, 2, pts);
    }
    pes.extend_from_slice(payload);
    let length = u16::try_from(pes.len() - 6).unwrap();
    pes[4..6].copy_from_slice(&length.to_be_bytes());
    ts.payload(PID, &pes);
}

fn unit(packet: &Packet) -> &DvbSubtitleUnit {
    let AccessUnit::DvbSubtitle(unit) = packet.access_unit() else {
        panic!("expected DVB subtitle segments");
    };
    unit
}

fn source(packet: &Packet) -> &Pes {
    let [source] = packet.sources() else {
        panic!("expected one PES per segment group");
    };
    let TraceEvent::Pes(pes) = source.pes.event() else {
        panic!("missing original PES");
    };
    pes
}

fn advance_to_pes(drive: &mut Drive<'_>) {
    loop {
        let Some(Event::Other(ReaderEvent::Source(event))) = drive.next().unwrap() else {
            panic!("subtitle or EOF before the source PES");
        };
        if matches!(event.event(), TraceEvent::Pes(_)) {
            return;
        }
    }
}

#[test]
fn complete_dvb_segments_retain_service_and_source_clocks_after_close() {
    let raw = include_bytes!("../data/dvb/dvb-default-clut2.ts");
    let mut drive = Drive::new(raw, limits(), 4096, 1);
    let mut packets = Vec::new();
    while let Some(event) = drive.next().unwrap() {
        if let Event::Packet(packet) = event {
            packets.push(packet);
        }
    }
    let [packet] = packets.as_slice() else {
        panic!("expected one complete group");
    };
    let end = COMPLETE.len() - 1;
    assert_eq!(packet.payload(), &COMPLETE[2..end]);
    assert_eq!(packet.elementary_range(), 2..end as u64);
    assert_eq!(packet.sources()[0].payload_range, 2..end);
    assert_eq!(packet.track_info().kind(), MediaKind::Subtitle);
    assert_eq!(packet.track_info().codec(), CodecId::DvbSubtitle);
    assert_eq!(packet.track_info().stream().pid, PID);
    assert_eq!(
        packet.track_info().stream().descriptor_bytes.as_ref(),
        SERVICES
    );
    assert!(packet.track_info().configuration().is_empty());
    assert_eq!(packet.pts(), Some(90_000));
    assert_eq!(packet.dts(), None);
    assert_eq!(packet.duration(), None);
    assert_eq!(unit(packet).segment_count(), 4);
    assert_eq!(unit(packet).truncated_tail(), None);
    let pes = source(packet);
    assert_eq!(
        pes.payload().flatten().copied().collect::<Vec<_>>(),
        COMPLETE
    );
    assert_eq!(pes.header_source_ranges.as_slice()[0].offset, 483);
    assert!(
        packet
            .bytes()
            .shares_storage_with(pes.spans()[0].backing_bytes())
    );
    let retained = packet.clone();
    let charged = drive.pool.used();
    drive.reader.close();
    drop(packets);
    assert!(drive.pool.used() <= charged);
    assert_eq!(retained.payload(), &COMPLETE[2..end]);
    assert_eq!(source(&retained).payload_len, COMPLETE.len());
    drop(retained);
    assert_eq!(drive.pool.used(), Budget::control_storage_bytes());
}

#[test]
fn truncated_dvb_prefix_is_explicit_and_the_next_pes_has_its_own_clock() {
    let mut ts = transport();
    subtitle_pes(&mut ts, TRUNCATED, Some(90_000), 0xbd, true);
    subtitle_pes(&mut ts, COMPLETE, Some(180_000), 0xbd, true);

    let mut strict = Drive::new(&ts.bytes, limits(), 1, 1);
    loop {
        match strict.next() {
            Err(error) => {
                assert_eq!(error, Error::Truncated);
                break;
            }
            Ok(Some(Event::Other(ReaderEvent::Source(_)))) => (),
            other => panic!("strict framing accepted malformed input: {other:?}"),
        }
    }

    let mut drive = Drive::new(
        &ts.bytes,
        ReaderLimits {
            dvb_subtitle_tail_policy: TailPolicy::ReportCompletePrefix,
            ..limits()
        },
        1,
        1,
    );
    let mut prefix = None;
    let mut complete = None;
    while let Some(event) = drive.next().unwrap() {
        match event {
            Event::Other(ReaderEvent::DvbSubtitlePrefix(packet)) => {
                assert!(prefix.replace(packet).is_none());
            }
            Event::Packet(packet) => {
                assert!(prefix.is_some());
                assert!(complete.replace(packet).is_none());
            }
            Event::Other(ReaderEvent::Source(_)) => (),
            other => panic!("unexpected event: {other:?}"),
        }
    }
    let prefix = prefix.unwrap();
    assert_eq!(prefix.pts(), Some(90_000));
    assert_eq!(prefix.dts(), None);
    assert_eq!(prefix.payload(), &TRUNCATED[2..103]);
    assert_eq!(prefix.sources()[0].payload_range, 2..103);
    assert_eq!(unit(&prefix).segment_count(), 5);
    assert_eq!(unit(&prefix).truncated_tail(), Some(103..109));
    assert_eq!(
        source(&prefix)
            .payload()
            .flatten()
            .copied()
            .collect::<Vec<_>>(),
        TRUNCATED
    );
    let complete = complete.unwrap();
    assert_eq!(complete.pts(), Some(180_000));
    assert_eq!(
        complete.elementary_range().start,
        TRUNCATED.len() as u64 + 2
    );
    assert_eq!(complete.track_info().program(), PROGRAM);
    assert_eq!(complete.payload(), &COMPLETE[2..COMPLETE.len() - 1]);
}

#[test]
fn prefix_policy_refuses_invalid_headers_and_unproved_segment_boundaries() {
    for (pts, stream_id, align) in [
        (None, 0xbd, true),
        (Some(1), 0xbd, false),
        (Some(1), 0xc0, true),
    ] {
        let mut ts = transport();
        subtitle_pes(&mut ts, COMPLETE, pts, stream_id, align);
        assert_refused(&ts.bytes, Error::Invalid("DVB subtitle PES header"));
    }
    let malformed: &[(&[u8], Error)] = &[
        (
            &[0x20, 1, 0xff],
            Error::Invalid("DVB subtitle data field wrapper"),
        ),
        (
            &[0x20, 0, 0xff],
            Error::Invalid("DVB subtitle segment or end marker"),
        ),
        (&[0x20, 0, 0x0f, 0x80, 0, 1, 0, 1], Error::Truncated),
        (&COMPLETE[..COMPLETE.len() - 1], Error::Truncated),
        (&TRUNCATED[..106], Error::Truncated),
    ];
    for (payload, expected) in malformed {
        let mut ts = transport();
        subtitle_pes(&mut ts, payload, Some(1), 0xbd, true);
        assert_refused(&ts.bytes, *expected);
    }
}

fn assert_refused(raw: &[u8], expected: Error) {
    let mut drive = Drive::new(
        raw,
        ReaderLimits {
            dvb_subtitle_tail_policy: TailPolicy::ReportCompletePrefix,
            ..limits()
        },
        1,
        1,
    );
    loop {
        match drive.next() {
            Err(error) => {
                assert_eq!(error, expected);
                return;
            }
            Ok(Some(Event::Other(ReaderEvent::Source(_)))) => (),
            other => panic!("malformed subtitle was accepted: {other:?}"),
        }
    }
}

#[test]
fn scattered_segments_retry_freeze_and_keep_exact_opaque_bytes() {
    let mut payload = vec![0x20, 0, 0x0f, 0xa0, 0x12, 0x34, 1, 44];
    payload.extend((0..300).map(|byte| byte as u8));
    payload.extend([0x0f, 0x80, 0, 1, 0, 0, 0xff]);
    let expected = &payload[2..payload.len() - 1];
    let mut ts = transport();
    subtitle_pes(&mut ts, &payload, Some(90_000), 0xbd, true);

    let mut drive = Drive::new(&ts.bytes, limits(), 1, 1);
    advance_to_pes(&mut drive);
    let pressure = drive
        .pool
        .reserve(drive.pool.available() - expected.len())
        .unwrap();
    assert!(matches!(
        drive.next().unwrap_err(),
        Error::Backpressure { requested, available } if requested == expected.len() && available < requested
    ));
    drop(pressure);
    // The lane's source table now exists. Give the copy room for just its bytes.
    let pressure = drive
        .pool
        .reserve(drive.pool.available() - expected.len())
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            drive.next().unwrap_err(),
            Error::Backpressure {
                requested: Bytes::owner_overhead_bytes(),
                available: 0,
            }
        );
    }
    drop(pressure);
    let Some(Event::Packet(packet)) = drive.next().unwrap() else {
        panic!("expected complete group after pressure");
    };
    assert_eq!(packet.payload(), expected);
    assert_eq!(unit(&packet).segment_count(), 2);
    let pes = source(&packet);
    assert!(pes.spans().len() > 1);
    assert_eq!(
        pes.payload().flatten().copied().collect::<Vec<_>>(),
        payload
    );
    while let Some(event) = drive.next().unwrap() {
        assert!(matches!(event, Event::Other(ReaderEvent::Source(_))));
    }
}

#[test]
fn cancellation_after_a_dvb_source_event_releases_pending_storage() {
    let mut ts = transport();
    subtitle_pes(&mut ts, COMPLETE, Some(1), 0xbd, true);
    let mut drive = Drive::new(&ts.bytes, limits(), 1, 1);
    advance_to_pes(&mut drive);
    assert!(matches!(
        drive.reader.step(&mut WorkBudget::new(3), &OFF),
        Ok(Step::Yield)
    ));
    let mut work = WorkBudget::new(0);
    assert_eq!(
        drive
            .reader
            .step(&mut work, &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(work.spent(), 0);
    assert_eq!(drive.pool.used(), Budget::control_storage_bytes());
}

#[test]
fn complete_pes_framers_release_storage_before_unframed_video_at_eof() {
    let mut ts = Transport::new();
    let video = PID + 1;
    let audio = PID + 2;
    ts.tables(
        &[
            (0x06, PID, SERVICES),
            (0x1b, video, &[]),
            (0x11, audio, &[]),
        ],
        0,
    );
    subtitle_pes(&mut ts, COMPLETE, Some(90_000), 0xbd, true);
    ts.pes(audio, 0xc0, &[0x56, 0xe0, 1, 0x81], Some(91_000), None);
    ts.pes(video, 0xe0, b"opaque video", Some(92_000), None);
    let mut drive = Drive::new(
        &ts.bytes,
        ReaderLimits {
            video_eof_policy: VideoEofPolicy::ReportUnframed,
            latm_packet_policy: latm::PacketPolicy::PreservePes,
            ..limits()
        },
        19,
        1,
    );
    let mut packets = [0; 3];
    while let Some(event) = drive.next().unwrap() {
        match event {
            Event::Packet(packet) => {
                assert_eq!(packet.track_info().codec(), CodecId::DvbSubtitle);
                packets[0] += 1;
            }
            Event::Other(ReaderEvent::LatmPes(_)) => packets[1] += 1,
            Event::Other(ReaderEvent::IncompleteVideo(video)) => {
                assert_eq!(video.bytes().as_ref(), b"opaque video");
                packets[2] += 1;
            }
            Event::Other(ReaderEvent::Source(_)) => (),
            other => panic!("unexpected mixed program event: {other:?}"),
        }
    }
    assert_eq!(packets, [1, 1, 1]);
}
