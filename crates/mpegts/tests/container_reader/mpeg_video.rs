use super::{AccessUnit, Drive, Error, Event, PacketFormat, PacketInfo, ReaderLimits, TraceEvent};

const TS1: &[u8] =
    include_bytes!("../../../../harness/features/mpegts-mpeg-video/inputs/mpeg1-ts188-25.ts");
const TS2: &[u8] =
    include_bytes!("../../../../harness/features/mpegts-mpeg-video/inputs/mpeg2-ts188-25.ts");
const M2TS1: &[u8] = include_bytes!(
    "../../../../harness/features/mpegts-mpeg-video/inputs/mpeg1-m2ts192-30000-1001.m2ts"
);
const M2TS2: &[u8] = include_bytes!(
    "../../../../harness/features/mpegts-mpeg-video/inputs/mpeg2-m2ts192-30000-1001.m2ts"
);

#[test]
fn mpeg_video_original_pictures_headers_clocks_and_pes_scopes() {
    for (raw, mpeg2, format, duration) in [
        (TS1, false, PacketFormat::Ts188, 3600),
        (TS2, true, PacketFormat::Ts188, 3600),
        (M2TS1, false, PacketFormat::M2ts192, 3003),
        (M2TS2, true, PacketFormat::M2ts192, 3003),
    ] {
        for (chunk, work) in [(1, 1), (7, 7), (4096, 4096)] {
            let mut drive = Drive::new(
                raw,
                ReaderLimits {
                    format,
                    ..ReaderLimits::default()
                },
                chunk,
                work,
            );
            let mut count = 0;
            let mut end = 0;
            while let Some(event) = drive.next().unwrap() {
                if let Event::Packet(packet) = event {
                    let AccessUnit::MpegVideo(unit) = packet.access_unit() else {
                        panic!("MPEG picture");
                    };
                    assert_eq!((unit.width, unit.height, unit.slice_count), (16, 16, 1));
                    assert_eq!(unit.mpeg2, mpeg2);
                    assert_eq!(unit.temporal_reference, count);
                    assert_eq!(unit.picture_offset, end + if mpeg2 { 22 } else { 12 });
                    assert_eq!(packet.pts(), Some(90000 + i64::from(count) * 3600));
                    assert_eq!(packet.dts(), Some(87000 + i64::from(count) * 3600));
                    assert_eq!(packet.duration(), Some(duration));
                    assert_eq!(
                        packet.random_access(),
                        triergon_core::container::RandomAccess::Sync
                    );
                    let [origin] = packet.sources() else {
                        panic!("one original PES");
                    };
                    let TraceEvent::Pes(pes) = origin.pes.event() else {
                        panic!("source PES");
                    };
                    assert!(pes.header.data_alignment_indicator);
                    assert_eq!(origin.payload_range, 0..packet.payload().len());
                    assert_eq!(packet.payload(), unit.bytes().as_ref());
                    assert_eq!(packet.payload(), pes.spans()[0].as_ref());
                    assert_eq!(packet.elementary_range().start, end);
                    end = packet.elementary_range().end;
                    count += 1;
                }
            }
            assert_eq!(count, 2);
        }
    }
}

fn refuse(raw: &[u8]) {
    let mut drive = Drive::new(raw, ReaderLimits::default(), 7, 1);
    loop {
        match drive.next() {
            Ok(Some(Event::Packet(_))) => panic!("unsupported picture published"),
            Ok(Some(_)) => (),
            Err(Error::Unsupported(_) | Error::Truncated) => break,
            other => panic!("expected explicit refusal: {other:?}"),
        }
    }
}

#[test]
fn mpeg_video_refuses_unadmitted_headers_and_multiple_pictures() {
    let sequence = TS2.windows(4).position(|b| b == [0, 0, 1, 0xb3]).unwrap();
    let picture = sequence + 22;
    let coding = picture + 8;
    for (offset, mask) in [
        (sequence + 11, 1),  // non-default matrix
        (sequence + 17, 8),  // interlaced sequence
        (picture + 5, 0x18), // different picture coding type
        (coding + 6, 1),     // field picture
        (coding + 7, 2),     // repeat_first_field
        (coding + 8, 0x80),  // non-progressive frame
        (coding + 3, 0),     // second picture start code instead of extension
    ] {
        let mut raw = TS2.to_vec();
        if mask == 0 {
            raw[offset] = 0;
        } else {
            raw[offset] ^= mask;
        }
        refuse(&raw);
    }
    let mut raw = TS1.to_vec();
    // Locate header independently: two carrier tables precede the first PES.
    let pes = raw.windows(4).position(|b| b == [0, 0, 1, 0xe0]).unwrap();
    raw[pes + 6] &= !4;
    refuse(&raw);
}

fn envelope(body: &[u8], pts: Option<u64>) -> Vec<u8> {
    let mut ts = super::Transport::new();
    ts.tables(&[(1, super::PID, &[])], 0);
    ts.pes(super::PID, 0xe0, body, pts, None);
    let at = ts
        .bytes
        .windows(4)
        .position(|b| b == [0, 0, 1, 0xe0])
        .unwrap();
    ts.bytes[at + 6] |= 4;
    ts.bytes
}

#[test]
fn mpeg_video_requires_sequence_end_and_single_span_and_preserves_absent_clocks() {
    let start = TS1.windows(4).position(|b| b == [0, 0, 1, 0xb3]).unwrap();
    let body = &TS1[start..564];
    assert_eq!(&body[body.len() - 4..], &[0, 0, 1, 0xb7]);
    refuse(&envelope(&body[..body.len() - 4], None));
    refuse(&envelope(&body[..body.len() - 1], None));
    let mut multiple = body.to_vec();
    multiple.extend_from_slice(body);
    refuse(&envelope(&multiple, None));
    let mut large = body[..body.len() - 4].to_vec();
    large.extend_from_slice(&[0x55; 400]);
    large.extend_from_slice(&[0, 0, 1, 0xb7]);
    refuse(&envelope(&large, None));
    let raw = envelope(body, None);
    let mut drive = Drive::new(&raw, ReaderLimits::default(), 1, 1);
    let mut retained = Vec::new();
    while let Some(event) = drive.next().unwrap() {
        if let Event::Packet(packet) = event {
            assert_eq!((packet.pts(), packet.dts()), (None, None));
            let AccessUnit::MpegVideo(unit) = packet.access_unit() else {
                panic!("MPEG picture");
            };
            let TraceEvent::Pes(pes) = packet.sources()[0].pes.event() else {
                panic!("source PES");
            };
            assert!(
                unit.bytes()
                    .shares_storage_with(pes.spans()[0].backing_bytes())
            );
            retained.push(packet);
        }
    }
    drop(drive);
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].payload(), body);
}
