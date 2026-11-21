use super::{AccessUnit, Drive, Error, Event, PacketFormat, PacketInfo, ReaderLimits, TraceEvent};
use triergon_mpegts::MpegPictureCoding;

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

const IPB: &[u8] =
    include_bytes!("../../../../harness/features/mpegts-mpeg-video/inputs/mpeg2-ipb-ts188-25.ts");

#[test]
fn mpeg2_predictive_types_keep_decode_order_clocks_and_sync_flags() {
    for (raw, format, tick) in [
        (IPB, PacketFormat::Ts188, 3600),
        (include_bytes!("../../../../harness/features/mpegts-mpeg-video/inputs/mpeg2-ipb-ts188-30000-1001.ts").as_slice(), PacketFormat::Ts188, 3003),
        (include_bytes!("../../../../harness/features/mpegts-mpeg-video/inputs/mpeg2-ipb-m2ts192-25.m2ts").as_slice(), PacketFormat::M2ts192, 3600),
        (include_bytes!("../../../../harness/features/mpegts-mpeg-video/inputs/mpeg2-ipb-m2ts192-30000-1001.m2ts").as_slice(), PacketFormat::M2ts192, 3003),
    ] {
        for (chunk, work) in [(1, 1), (7, 7), (4096, 4096)] {
            let mut drive = Drive::new(raw, ReaderLimits { format, ..ReaderLimits::default() }, chunk, work);
            let mut index = 0;
            let mut end = 0;
            while let Some(event) = drive.next().unwrap() {
                if let Event::Packet(packet) = event {
                    let AccessUnit::MpegVideo(unit) = packet.access_unit() else { panic!("MPEG picture"); };
                    let temporal = [0, 2, 1][index];
                    assert_eq!(
                        unit.coding,
                        [
                            MpegPictureCoding::Intra,
                            MpegPictureCoding::Predictive,
                            MpegPictureCoding::Bidirectional,
                        ][index]
                    );
                    assert_eq!(unit.temporal_reference, temporal);
                    assert_eq!((unit.width, unit.height, unit.slice_count), (16, 16, 1));
                    assert_eq!(unit.picture_offset, end + 22);
                    assert_eq!(packet.pts(), Some(90000 + i64::from(temporal) * tick));
                    assert_eq!(packet.dts(), Some(84000 + index as i64 * tick));
                    assert_eq!(packet.duration(), Some(tick));
                    assert_eq!(packet.random_access(), if index == 0 {
                        triergon_core::container::RandomAccess::Sync
                    } else { triergon_core::container::RandomAccess::NotSync });
                    let [origin] = packet.sources() else { panic!("one source"); };
                    let TraceEvent::Pes(pes) = origin.pes.event() else { panic!("PES"); };
                    assert_eq!(packet.payload(), pes.spans()[0].as_ref());
                    assert!(unit.bytes().shares_storage_with(pes.spans()[0].backing_bytes()));
                    assert_eq!(packet.elementary_range().start, end);
                    end = packet.elementary_range().end;
                    index += 1;
                }
            }
            assert_eq!(index, 3);
        }
    }
}

#[test]
fn mpeg2_predictive_headers_reject_invalid_vectors_extra_bits_and_types() {
    // Isolate each predictive PES so refusal cannot be hidden by an earlier I packet.
    for kind in [2, 3] {
        let sequence = IPB
            .windows(4)
            .enumerate()
            .filter(|(_, b)| *b == [0, 0, 1, 0xb3])
            .nth(kind - 1)
            .unwrap()
            .0;
        let end = IPB[sequence..]
            .windows(4)
            .position(|b| b == [0, 0, 1, 0xb7])
            .unwrap()
            + sequence
            + 4;
        let body = &IPB[sequence..end];
        let picture = 22;
        let fields: &[(usize, u8)] = if kind == 2 {
            &[(29, 1), (30, 3), (33, 1)]
        } else {
            &[(29, 1), (30, 3), (33, 1), (34, 3), (37, 1)]
        };
        for reserved in [0, 4, 7] {
            let mut changed = body.to_vec();
            changed[picture + 5] = (changed[picture + 5] & !0x38) | (reserved << 3);
            refuse(&mpeg2_envelope(&changed));
        }
        for length in 0..5 {
            let mut changed = body[..picture + 4 + length].to_vec();
            changed.extend_from_slice(&body[picture + 9..]);
            refuse(&mpeg2_envelope(&changed));
        }
        for &(bit, width) in fields {
            let mut changed = body.to_vec();
            for bit in bit..bit + usize::from(width) {
                changed[picture + 4 + bit / 8] ^= 1 << (7 - bit % 8);
            }
            if width == 3 {
                for bit in bit..bit + 3 {
                    changed[picture + 4 + bit / 8] &= !(1 << (7 - bit % 8));
                }
            }
            refuse(&mpeg2_envelope(&changed));
        }
    }
}

fn mpeg2_envelope(body: &[u8]) -> Vec<u8> {
    let mut ts = super::Transport::new();
    ts.tables(&[(2, super::PID, &[])], 0);
    ts.pes(super::PID, 0xe0, body, None, None);
    let at = ts
        .bytes
        .windows(4)
        .position(|b| b == [0, 0, 1, 0xe0])
        .unwrap();
    ts.bytes[at + 6] |= 4;
    ts.bytes
}

// Put an unchanged finite PES into exactly two adaptation-stuffed carriers.
fn split_envelope(body: &[u8], mpeg2: bool, cut: usize, m2ts: bool) -> Vec<u8> {
    let raw = if mpeg2 {
        mpeg2_envelope(body)
    } else {
        envelope(body, None)
    };
    let start = raw.windows(4).position(|b| b == [0, 0, 1, 0xe0]).unwrap();
    let pes = &raw[start..start + 9 + body.len()];
    let mut ts = super::Transport::new();
    ts.bytes.extend_from_slice(&raw[..376]);
    for (index, payload) in [&pes[..9 + cut], &pes[9 + cut..]].into_iter().enumerate() {
        let mut carrier = [0xff; 188];
        carrier[..6].copy_from_slice(&[
            0x47,
            if index == 0 { 0x41 } else { 1 },
            0,
            0x30 | index as u8,
            (183 - payload.len()) as u8,
            0,
        ]);
        carrier[188 - payload.len()..].copy_from_slice(payload);
        ts.bytes.extend_from_slice(&carrier);
    }
    if m2ts { ts.m2ts() } else { ts.bytes }
}

#[test]
fn mpeg_video_every_carrier_split_preserves_picture_and_physical_ranges() {
    for (original, mpeg2) in [(TS1, false), (TS2, true)] {
        let start = original
            .windows(4)
            .position(|b| b == [0, 0, 1, 0xb3])
            .unwrap();
        let body = &original[start..564];
        for cut in 1..body.len() {
            for (m2ts, format) in [(false, PacketFormat::Ts188), (true, PacketFormat::M2ts192)] {
                let raw = split_envelope(body, mpeg2, cut, m2ts);
                for (chunk, work) in [(1, 1), (7, 7), (4096, 4096)] {
                    let mut drive = Drive::new(
                        &raw,
                        ReaderLimits {
                            format,
                            ..ReaderLimits::default()
                        },
                        chunk,
                        work,
                    );
                    let mut retained = Vec::new();
                    while let Some(event) = drive.next().unwrap() {
                        if let Event::Packet(packet) = event {
                            assert_eq!(packet.payload(), body);
                            assert_eq!(
                                (packet.pts(), packet.dts(), packet.duration()),
                                (None, None, Some(3600))
                            );
                            assert_eq!(packet.elementary_range(), 0..body.len() as u64);
                            let AccessUnit::MpegVideo(unit) = packet.access_unit() else {
                                panic!("picture");
                            };
                            assert_eq!(unit.picture_offset, if mpeg2 { 22 } else { 12 });
                            assert_eq!(
                                (unit.width, unit.height, unit.slice_count, unit.mpeg2),
                                (16, 16, 1, mpeg2)
                            );
                            let [origin] = packet.sources() else {
                                panic!("one PES");
                            };
                            assert_eq!(origin.payload_range, 0..body.len());
                            let TraceEvent::Pes(pes) = origin.pes.event() else {
                                panic!("PES");
                            };
                            assert_eq!(pes.spans().len(), 2);
                            assert_eq!(pes.spans()[0].as_ref(), &body[..cut]);
                            assert_eq!(pes.spans()[1].as_ref(), &body[cut..]);
                            for span in pes.spans() {
                                let range = span.source_range();
                                let offset = range.offset as usize;
                                assert_eq!(&raw[offset..offset + range.bytes], span.as_ref());
                            }
                            retained.push(packet);
                        }
                    }
                    drop(drive);
                    assert_eq!(retained.len(), 1);
                    assert_eq!(retained[0].payload(), body);
                }
            }
        }
    }
}

#[test]
fn mpeg_video_authored_split_controls_keep_exact_clocks_through_eof() {
    for (raw, format, size, mpeg2) in [
        (
            include_bytes!(
                "../../../../harness/features/mpegts-mpeg-video/inputs/mpeg1-split-ts188-25.ts"
            )
            .as_slice(),
            PacketFormat::Ts188,
            33,
            false,
        ),
        (
            include_bytes!(
                "../../../../harness/features/mpegts-mpeg-video/inputs/mpeg2-split-m2ts192-25.m2ts"
            )
            .as_slice(),
            PacketFormat::M2ts192,
            52,
            true,
        ),
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
            while let Some(event) = drive.next().unwrap() {
                if let Event::Packet(packet) = event {
                    assert_eq!(packet.payload().len(), size);
                    assert_eq!(
                        (packet.pts(), packet.dts(), packet.duration()),
                        (
                            Some(90000 + count * 3600),
                            Some(87000 + count * 3600),
                            Some(3600)
                        )
                    );
                    let AccessUnit::MpegVideo(unit) = packet.access_unit() else {
                        panic!("picture");
                    };
                    assert_eq!(unit.mpeg2, mpeg2);
                    assert_eq!(unit.temporal_reference, count as u16);
                    let TraceEvent::Pes(pes) = packet.sources()[0].pes.event() else {
                        panic!("PES");
                    };
                    assert_eq!(pes.spans().len(), 2);
                    assert_eq!(
                        packet.payload(),
                        pes.payload().flatten().copied().collect::<Vec<_>>()
                    );
                    count += 1;
                }
            }
            assert_eq!(count, 2);
        }
    }
}

fn advance_to_split_pes(drive: &mut Drive<'_>) {
    loop {
        match drive.next().unwrap() {
            Some(Event::Other(super::ReaderEvent::Source(source)))
                if matches!(source.event(), TraceEvent::Pes(_)) =>
            {
                return;
            }
            Some(Event::Other(_)) => (),
            other => panic!("source PES expected: {other:?}"),
        }
    }
}

#[test]
fn mpeg_video_split_freeze_retries_without_duplicate_bytes_or_sources() {
    let start = TS2.windows(4).position(|b| b == [0, 0, 1, 0xb3]).unwrap();
    let body = &TS2[start..564];
    let raw = split_envelope(body, true, 23, false);
    let mut drive = Drive::new(&raw, ReaderLimits::default(), 1, 1);
    advance_to_split_pes(&mut drive);
    let pressure = drive
        .pool
        .reserve(drive.pool.available() - body.len())
        .unwrap();
    let Error::Backpressure {
        requested,
        available,
    } = drive.next().unwrap_err()
    else {
        panic!("source-table pressure");
    };
    assert_eq!(available, body.len());
    assert!(requested > available);
    drop(pressure);
    // Allow the pending source table and AU bytes without the backing allocation header.
    let pressure = drive
        .pool
        .reserve(drive.pool.available() - body.len() - requested)
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            drive.next().unwrap_err(),
            Error::Backpressure {
                requested: triergon_core::Bytes::owner_overhead_bytes(),
                available: 0,
            }
        );
    }
    drop(pressure);
    let Some(Event::Packet(packet)) = drive.next().unwrap() else {
        panic!("picture after pressure");
    };
    assert_eq!(packet.payload(), body);
    assert_eq!(packet.sources().len(), 1);
    while let Some(event) = drive.next().unwrap() {
        assert!(!matches!(event, Event::Packet(_)));
    }
}

#[test]
fn mpeg_video_split_cancellation_releases_pending_storage() {
    let start = TS2.windows(4).position(|b| b == [0, 0, 1, 0xb3]).unwrap();
    let raw = split_envelope(&TS2[start..564], true, 23, false);
    let mut drive = Drive::new(&raw, ReaderLimits::default(), 1, 1);
    advance_to_split_pes(&mut drive);
    assert!(matches!(
        drive
            .reader
            .step(&mut triergon_core::WorkBudget::new(3), &super::OFF),
        Ok(triergon_core::Step::Yield)
    ));
    let mut work = triergon_core::WorkBudget::new(0);
    assert_eq!(
        drive
            .reader
            .step(&mut work, &std::sync::atomic::AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(work.spent(), 0);
    assert_eq!(
        drive.pool.used(),
        triergon_core::Budget::control_storage_bytes()
    );
}
