use super::{
    AccessUnit, CodecId, Drive, Error, Event, PID, PacketInfo, ReaderEvent, ReaderLimits,
    TraceEvent, TrackInfo, Transport,
};
use triergon_framing::{MpegLayer, MpegVersion};

fn silence<const N: usize>(header: [u8; 4]) -> [u8; N] {
    let mut frame = [0; N];
    frame[..4].copy_from_slice(&header);
    frame
}

#[test]
fn frames_all_mpeg_audio_layers_and_preserves_source_bytes_and_clocks() {
    for (kind, frame, version, layer, ticks) in [
        (
            0x03,
            silence::<32>([0xff, 0xff, 0x14, 0xc0]).to_vec(),
            MpegVersion::Mpeg1,
            MpegLayer::I,
            720,
        ),
        (
            0x03,
            silence::<192>([0xff, 0xfd, 0x44, 0xc0]).to_vec(),
            MpegVersion::Mpeg1,
            MpegLayer::II,
            2160,
        ),
        (
            0x04,
            silence::<192>([0xff, 0xf5, 0x44, 0xc0]).to_vec(),
            MpegVersion::Mpeg2,
            MpegLayer::II,
            4320,
        ),
        (
            0x04,
            silence::<96>([0xff, 0xf3, 0x44, 0xc0]).to_vec(),
            MpegVersion::Mpeg2,
            MpegLayer::III,
            2160,
        ),
    ] {
        let mut ts = Transport::new();
        ts.tables(&[(kind, PID, &[])], 0);
        ts.pes(PID, 0xc0, &frame[..2], Some(90_000), None);
        ts.pes(
            PID,
            0xc0,
            &[&frame[2..], &frame].concat(),
            Some(94_320),
            Some(93_000),
        );
        for (window, work) in [(1, 1), (4096, 4096)] {
            let mut reader = Drive::new(&ts.bytes, ReaderLimits::default(), window, work);
            let mut index = 0;
            while let Some(event) = reader.next().unwrap() {
                let Event::Packet(packet) = event else {
                    assert!(!matches!(
                        event,
                        Event::Other(ReaderEvent::Unsupported { .. })
                    ));
                    continue;
                };
                let AccessUnit::MpegAudio(audio) = packet.access_unit() else {
                    panic!("expected an MPEG audio frame");
                };
                assert_eq!(packet.track_info().codec(), CodecId::MpegAudio);
                assert_eq!(audio.header().version, version);
                assert_eq!(audio.header().layer, layer);
                assert_eq!(packet.payload(), frame);
                let length = frame.len() as u64;
                assert_eq!(
                    audio.elementary_range(),
                    index * length..(index + 1) * length
                );
                assert_eq!(packet.duration(), Some(ticks));
                if index == 0 {
                    assert_eq!((packet.pts(), packet.dts()), (Some(90_000), None));
                    assert_eq!(packet.sources().len(), 2);
                    assert_eq!(packet.sources()[0].payload_range, 0..2);
                    assert_eq!(packet.sources()[1].payload_range, 0..frame.len() - 2);
                } else {
                    assert_eq!((packet.pts(), packet.dts()), (Some(94_320), Some(93_000)));
                }
                index += 1;
            }
            assert_eq!(index, 2);
        }
    }
}

#[test]
fn complete_frames_share_pes_backing() {
    let frame = silence::<96>([0xff, 0xfb, 0x14, 0xc0]);
    let mut ts = Transport::new();
    ts.tables(&[(0x03, PID, &[])], 0);
    ts.pes(PID, 0xc0, &frame, Some(90_000), None);
    let mut reader = Drive::new(&ts.bytes, ReaderLimits::default(), 4096, 4096);
    let packet = loop {
        match reader.next().unwrap() {
            Some(Event::Packet(packet)) => break packet,
            Some(Event::Other(_)) => (),
            _ => panic!("expected an MPEG audio frame before EOF"),
        }
    };
    let TraceEvent::Pes(pes) = packet.sources()[0].pes.event() else {
        panic!("expected original PES");
    };
    assert!(
        packet
            .bytes()
            .shares_storage_with(pes.spans()[0].backing_bytes())
    );
}

#[test]
fn fractional_sample_duration_remains_exact() {
    let frame = silence::<32>([0xff, 0xff, 0x10, 0xc0]);
    let mut ts = Transport::new();
    ts.tables(&[(0x03, PID, &[])], 0);
    ts.pes(PID, 0xc0, &frame, Some(90_000), None);
    let mut reader = Drive::new(&ts.bytes, ReaderLimits::default(), 4096, 4096);
    let packet = loop {
        match reader.next().unwrap() {
            Some(Event::Packet(packet)) => break packet,
            Some(Event::Other(_)) => (),
            _ => panic!("expected an MPEG audio frame before EOF"),
        }
    };
    assert_eq!(packet.duration(), None);
    assert_eq!(
        packet.sample_duration(),
        Some(super::Time::new(384, 44_100).unwrap())
    );
}

#[test]
fn malformed_or_unsupported_input_cannot_produce_a_complete_frame() {
    for (stream_id, payload, expected) in [
        (0xc0, [0xff, 0xfd, 0x44].as_slice(), Error::Truncated),
        (0xc0, [0xff, 0xfd, 0x44, 0xc0].as_slice(), Error::Truncated),
        (
            0xc0,
            [0xff, 0xfd, 0x04, 0xc0].as_slice(),
            Error::Unsupported("free-format MPEG audio"),
        ),
        (
            0xc0,
            [0xff, 0xe5, 0x44, 0xc0].as_slice(),
            Error::Unsupported("MPEG-2.5 audio in MPEG-TS"),
        ),
        (
            0xc0,
            [0, 0, 0, 0].as_slice(),
            Error::Invalid("MPEG audio frame header"),
        ),
        (
            0xc0,
            [0xff, 0xfd, 0xe4, 0xc0].as_slice(),
            Error::Limit {
                requested: 1152,
                maximum: 256,
            },
        ),
        (
            0xbd,
            [0xff, 0xfd, 0x44, 0xc0].as_slice(),
            Error::Invalid("MPEG audio PES stream ID"),
        ),
    ] {
        let mut ts = Transport::new();
        ts.tables(&[(0x03, PID, &[])], 0);
        ts.pes(PID, stream_id, payload, Some(90_000), None);
        let limits = ReaderLimits {
            maximum_audio_frame_bytes: 256,
            ..ReaderLimits::default()
        };
        let mut reader = Drive::new(&ts.bytes, limits, 1, 1);
        loop {
            match reader.next() {
                Err(error) => {
                    assert_eq!(error, expected);
                    break;
                }
                Ok(Some(Event::Other(_))) => (),
                Ok(_) => panic!("incomplete or invalid bytes became a packet"),
            }
        }
    }
}
