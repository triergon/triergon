use super::{
    AccessUnit, AudioEofPolicy, CodecId, Drive, Error, Event, MediaKind, PID, PacketInfo,
    ReaderEvent, ReaderLimits, Time, TrackInfo, Transport, audio_eof_namespace,
};

// EN 300 468 G.2, corresponding to a 48 kHz, 512-sample mono core.
const DESCRIPTOR: &[u8] = &[0x7b, 5, 0xd2, 0x87, 0x83, 0xfe, 0x00];
const BYTES: &[u8] = include_bytes!("../../../../harness/fixtures/mpegts/dts-core-mono.dts");
const FRAME: usize = 512;

fn limits() -> ReaderLimits {
    ReaderLimits {
        descriptor_namespace: audio_eof_namespace(),
        ..ReaderLimits::default()
    }
}

#[test]
fn frames_core_across_pes_and_preserves_source_clocks() {
    let mut ts = Transport::new();
    ts.tables(&[(0x06, PID, DESCRIPTOR)], 0);
    ts.pes(PID, 0xbd, &BYTES[..7], Some(90_000), None);
    ts.pes(PID, 0xbd, &BYTES[7..3 * FRAME], Some(90_960), Some(90_100));
    let mut drive = Drive::new(&ts.bytes, limits(), 7, 1);
    let mut count = 0;
    while let Some(event) = drive.next().unwrap() {
        let Event::Packet(packet) = event else {
            assert!(!matches!(
                event,
                Event::Other(ReaderEvent::Unsupported { .. })
            ));
            continue;
        };
        let AccessUnit::Dts(frame) = packet.access_unit() else {
            panic!("expected a DTS core frame");
        };
        let start = count * FRAME;
        let end = start + FRAME;
        assert_eq!(packet.track_info().codec(), CodecId::Dts);
        assert_eq!(packet.track_info().kind(), MediaKind::Audio);
        assert_eq!(frame.elementary_range(), start as u64..end as u64);
        assert_eq!(packet.payload(), &BYTES[start..end]);
        assert_eq!(frame.sample_span(), Time::new(512, 48_000).unwrap());
        assert_eq!(packet.duration(), Some(960));
        match count {
            0 => {
                assert_eq!((packet.pts(), packet.dts()), (Some(90_000), None));
                assert_eq!(packet.sources().len(), 2);
                assert_eq!(packet.sources()[0].payload_range, 0..7);
                assert_eq!(packet.sources()[1].payload_range, 0..FRAME - 7);
            }
            1 => assert_eq!((packet.pts(), packet.dts()), (Some(90_960), Some(90_100))),
            _ => assert_eq!((packet.pts(), packet.dts()), (None, None)),
        }
        count += 1;
    }
    assert_eq!(count, 3);
}

#[test]
fn requires_dvb_signalling_and_a_complete_core_frame() {
    for (descriptors, limits) in [
        (DESCRIPTOR, ReaderLimits::default()),
        (&[0x7b, 4, 0, 0, 0, 0], limits()),
    ] {
        let mut ts = Transport::new();
        ts.tables(&[(0x06, PID, descriptors)], 0);
        ts.pes(PID, 0xbd, &BYTES[..FRAME], Some(90_000), None);
        let mut drive = Drive::new(&ts.bytes, limits, 188, 4096);
        let mut unsupported = false;
        while let Some(event) = drive.next().unwrap() {
            assert!(!matches!(event, Event::Packet(_)));
            unsupported |= matches!(event, Event::Other(ReaderEvent::Unsupported { .. }));
        }
        assert!(unsupported);
    }
    let mut ts = Transport::new();
    ts.tables(&[(0x06, PID, DESCRIPTOR)], 0);
    ts.pes(PID, 0xbd, &BYTES[..FRAME - 1], Some(90_000), None);
    let mut drive = Drive::new(&ts.bytes, limits(), 188, 4096);
    loop {
        match drive.next() {
            Ok(Some(Event::Other(_))) => (),
            Err(Error::Truncated) => break,
            _ => panic!("an incomplete core frame must return a truncation error"),
        }
    }
}

#[test]
fn hd_descriptor_does_not_enable_the_core_only_lane() {
    // EN 300 468 V1.18.1 G.3: DTS-HD descriptor, core present, one
    // mono/48-kHz/16-bit asset, construction 1, fixed 384 kbit/s.
    let hd = &[0x7f, 8, 0x0e, 0x87, 5, 1, 0x63, 0x08, 0x06, 0x03];
    let mut ts = Transport::new();
    ts.tables(&[(0x06, PID, hd)], 0);
    ts.pes(PID, 0xbd, &BYTES[..FRAME], Some(90_000), None);
    let mut drive = Drive::new(&ts.bytes, limits(), 7, 1);
    let mut unsupported = false;
    while let Some(event) = drive.next().unwrap() {
        assert!(!matches!(event, Event::Packet(_)));
        unsupported |= matches!(event, Event::Other(ReaderEvent::Unsupported { .. }));
    }
    assert!(unsupported);
}

#[test]
fn core_signalling_does_not_discard_an_extension_frame() {
    let mut extension = [0; FRAME];
    extension[..9].copy_from_slice(&[0x64, 0x58, 0x20, 0x25, 0xa5, 0x03, 0xe0, 0x3f, 0xe0]);
    let mut ts = Transport::new();
    ts.tables(&[(0x06, PID, DESCRIPTOR)], 0);
    ts.pes(PID, 0xbd, &BYTES[..FRAME], Some(90_000), None);
    ts.pes(PID, 0xbd, &extension, None, None);
    let mut drive = Drive::new(&ts.bytes, limits(), 7, 1);
    let mut frames = 0;
    loop {
        match drive.next() {
            Ok(Some(Event::Packet(packet))) => {
                assert_eq!(packet.payload(), &BYTES[..FRAME]);
                assert_eq!((packet.pts(), packet.dts()), (Some(90_000), None));
                frames += 1;
            }
            Ok(Some(_)) => (),
            Err(Error::Unsupported("DTS extension-substream input")) => break,
            _ => panic!("core-only framing must refuse an extension substream"),
        }
    }
    assert_eq!(frames, 1);
}

fn dts_hd_extension() -> [u8; 16] {
    // A short size prefix for extension 0. The declared 12-byte header is
    // followed by four opaque asset bytes.
    let mut extension = [0; 16];
    extension[..9].copy_from_slice(&[0x64, 0x58, 0x20, 0x25, 0xa5, 0x01, 0x60, 0x01, 0xe0]);
    let mut crc = u16::MAX;
    for &byte in &extension[5..10] {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    extension[10..12].copy_from_slice(&crc.to_be_bytes());
    extension[12..].copy_from_slice(&[0xab, 0xcd, 0x12, 0x34]);
    extension
}

#[test]
fn aligned_dts_hd_keeps_the_complete_unit_and_original_core() {
    let hd = &[
        0x7f, 14, 0x0e, 0xc0, 5, 1, 0x60, 0x08, 0, 0, 5, 1, 0x60, 0x48, 0, 0,
    ];
    for corrupt in [false, true] {
        let mut extension = dts_hd_extension();
        if corrupt {
            extension[9] ^= 1;
        }
        let payload = [&BYTES[..FRAME], &extension].concat();
        let mut ts = Transport::new();
        ts.tables(&[(0x06, PID, hd)], 0);
        let pes_at = ts.bytes.len() + 4;
        ts.pes(PID, 0xbd, &payload, Some(90_000), Some(89_900));
        assert!(ts.bytes[pes_at..].starts_with(&[0, 0, 1, 0xbd]));
        ts.bytes[pes_at + 6] |= 4;
        for (chunk, grant) in [(7, 1), (4096, 8192)] {
            let mut drive = Drive::new(&ts.bytes, limits(), chunk, grant);
            loop {
                match drive.next() {
                    Ok(Some(Event::Packet(packet))) => {
                        assert!(!corrupt);
                        let AccessUnit::Dts(frame) = packet.access_unit() else {
                            panic!("expected a DTS-HD access unit");
                        };
                        assert_eq!(packet.track_info().codec(), CodecId::Dts);
                        assert_eq!(packet.payload(), payload);
                        assert_eq!(frame.core_bytes(), &BYTES[..FRAME]);
                        assert_eq!(packet.pts(), Some(90_000));
                        assert_eq!(packet.dts(), Some(89_900));
                        assert_eq!(packet.duration(), Some(960));
                        assert_eq!(packet.sources()[0].payload_range, 0..payload.len());
                        break;
                    }
                    Ok(Some(Event::Other(_))) => (),
                    Err(Error::Invalid("DTS-HD extension header CRC")) if corrupt => break,
                    other => panic!("unexpected DTS-HD result {other:?}"),
                }
            }
        }
    }
}

#[test]
fn short_dts_hd_extension_keeps_truncation_and_original_bytes() {
    let hd = &[
        0x7f, 14, 0x0e, 0xc0, 5, 1, 0x60, 0x08, 0, 0, 5, 1, 0x60, 0x48, 0, 0,
    ];
    let extension = dts_hd_extension();
    let payload = [&BYTES[..FRAME], &extension[..15]].concat();
    let mut ts = Transport::new();
    ts.tables(&[(0x06, PID, hd)], 0);
    let pes_at = ts.bytes.len() + 4;
    ts.pes(PID, 0xbd, &payload, Some(90_000), Some(89_900));
    ts.bytes[pes_at + 6] |= 4;
    for report in [false, true] {
        let settings = ReaderLimits {
            audio_eof_policy: if report {
                AudioEofPolicy::ReportIncomplete
            } else {
                AudioEofPolicy::RejectIncomplete
            },
            ..limits()
        };
        let mut drive = Drive::new(&ts.bytes, settings, 7, 1);
        let mut fragment = false;
        loop {
            match drive.next() {
                Ok(Some(Event::Other(ReaderEvent::IncompleteAudio(packet)))) => {
                    assert!(report && !fragment);
                    assert_eq!(packet.payload(), payload);
                    assert_eq!(packet.pts(), Some(90_000));
                    assert_eq!(packet.sources()[0].payload_range, 0..payload.len());
                    fragment = true;
                }
                Ok(Some(Event::Other(_))) => (),
                Ok(None) if report && fragment => break,
                Err(Error::Truncated) if !report => break,
                result => panic!("unexpected short extension result {result:?}"),
            }
        }
    }
}
