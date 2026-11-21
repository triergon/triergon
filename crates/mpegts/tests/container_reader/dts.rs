use super::{
    AccessUnit, CodecId, Drive, Error, Event, MediaKind, PID, PacketInfo, ReaderEvent,
    ReaderLimits, Time, TrackInfo, Transport, audio_eof_namespace,
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
