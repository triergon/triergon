use super::{
    AccessUnit, CodecId, Drive, Error, Event, MediaKind, PID, PacketInfo, ReaderEvent,
    ReaderLimits, TrackInfo, Transport, audio_eof_frame, eac3,
};
use triergon_mpegts::psi::DescriptorNamespace;

fn limits() -> ReaderLimits {
    ReaderLimits {
        descriptor_namespace: DescriptorNamespace::Atsc,
        ..ReaderLimits::default()
    }
}

#[test]
fn direct_audio_is_scoped_to_the_declared_namespace() {
    for (kind, codec, descriptors, frame) in [
        (
            0x81,
            CodecId::Ac3,
            &([0x81, 3, 8, 8, 5])[..],
            audio_eof_frame(true),
        ),
        (
            0x87,
            CodecId::Eac3,
            &([0xcc, 2, 0x80, 0xc2])[..],
            eac3::coded_primary(),
        ),
    ] {
        let mut ts = Transport::new();
        ts.tables(&[(kind, PID, descriptors)], 0);
        ts.pes(PID, 0xbd, &frame.repeat(2), Some(90_000), None);
        let mut unspecified = Drive::new(&ts.bytes, ReaderLimits::default(), 188, 4096);
        let mut unsupported = false;
        while let Some(event) = unspecified.next().unwrap() {
            match event {
                Event::Other(ReaderEvent::Unsupported { track, .. }) => {
                    assert_eq!(track.codec(), CodecId::Other(&[kind]));
                    unsupported = true;
                }
                Event::Packet(_) => panic!("private code point without its namespace"),
                _ => (),
            }
        }
        assert!(unsupported);

        let mut declared = Drive::new(&ts.bytes, limits(), 1, 1);
        let mut units = 0;
        while let Some(event) = declared.next().unwrap() {
            if let Event::Packet(packet) = event {
                assert!(matches!(packet.access_unit(), AccessUnit::Ac3(_)));
                let track = packet.track_info();
                assert_eq!(track.kind(), MediaKind::Audio);
                assert_eq!(track.codec(), codec);
                assert_eq!(track.codec_tag(), [kind]);
                assert_eq!(track.stream().descriptor_bytes.as_ref(), descriptors);
                assert_eq!(packet.payload(), frame);
                assert_eq!(packet.pts(), if units == 0 { Some(90_000) } else { None });
                assert_eq!(packet.dts(), None);
                assert_eq!(packet.sources().len(), 1);
                let start = units * frame.len();
                assert_eq!(
                    packet.sources()[0].payload_range,
                    start..start + frame.len()
                );
                units += 1;
            }
        }
        assert_eq!(units, 2);
    }
}

#[test]
fn direct_enhanced_audio_refuses_a_group_split_between_pes_packets() {
    let frame = eac3::coded_primary();
    let mut ts = Transport::new();
    ts.tables(&[(0x87, PID, &[0xcc, 2, 0x80, 0xc2])], 0);
    let cut = frame.len() / 2;
    ts.pes(PID, 0xbd, &frame[..cut], Some(90_000), None);
    ts.pes(PID, 0xbd, &frame[cut..], None, None);

    let mut drive = Drive::new(&ts.bytes, limits(), 11, 5);
    loop {
        match drive.next() {
            Err(error) => {
                assert_eq!(error, Error::Invalid("ATSC E-AC-3 access unit crosses PES"));
                break;
            }
            Ok(Some(Event::Packet(_)) | None) => panic!("a split group was accepted"),
            Ok(Some(_)) => (),
        }
    }
}
