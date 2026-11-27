use super::{
    AccessUnit, CodecId, Drive, Error, Event, PID, PacketInfo, ReaderEvent, ReaderLimits,
    TraceEvent, TrackInfo, Transport,
};
use triergon_framing::mpeg_audio::{MpegLayer, MpegVersion};

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
        (0xc0, [0xff, 0xfd, 0x04, 0xc0].as_slice(), Error::Truncated),
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

#[test]
fn confirms_free_format_spacing_across_pes_and_preserves_padding_and_clocks() {
    let mut first = silence::<97>([0xff, 0xfb, 0x02, 0xc0]);
    // The fake header's spacing does not predict the next header.
    first[28..32].copy_from_slice(&[0xff, 0xfb, 0x00, 0xc0]);
    let second = silence::<96>([0xff, 0xfb, 0x00, 0xc0]);
    let third = silence::<97>([0xff, 0xfb, 0x02, 0xc0]);
    let fourth = silence::<96>([0xff, 0xfb, 0x00, 0xc0]);
    let mut ts = Transport::new();
    ts.tables(&[(0x03, PID, &[])], 0);
    ts.pes(PID, 0xc0, &first[..2], Some(90_000), None);
    ts.pes(PID, 0xc0, &first[2..], None, None);
    ts.pes(
        PID,
        0xc0,
        &[second.as_slice(), third.as_slice(), fourth.as_slice()].concat(),
        Some(100_000),
        None,
    );
    let frames = [
        (first.as_slice(), Some(90_000)),
        (second.as_slice(), Some(100_000)),
        (third.as_slice(), None),
        (fourth.as_slice(), None),
    ];
    for (window, work) in [(1, 1), (4096, 4096)] {
        let mut reader = Drive::new(&ts.bytes, ReaderLimits::default(), window, work);
        let mut index = 0;
        let mut offset = 0;
        while let Some(event) = reader.next().unwrap() {
            let Event::Packet(packet) = event else {
                continue;
            };
            let (payload, pts) = frames[index];
            assert_eq!(packet.payload(), payload);
            assert_eq!(packet.pts(), pts);
            assert_eq!(
                packet.elementary_range(),
                offset..offset + payload.len() as u64
            );
            assert_eq!(
                packet.sample_duration(),
                Some(super::Time::new(1152, 44_100).unwrap())
            );
            let AccessUnit::MpegAudio(audio) = packet.access_unit() else {
                panic!("expected a free-format audio frame");
            };
            assert_eq!(audio.header().bitrate_kbps, 0);
            assert_eq!(packet.sources().len(), if index == 0 { 2 } else { 1 });
            if index == 3 && window == 4096 {
                let pes = packet.sources()[0].pes.event().pes_source().unwrap();
                assert!(
                    pes.spans()
                        .iter()
                        .any(|span| { packet.bytes().shares_storage_with(span.backing_bytes()) })
                );
            }
            offset += payload.len() as u64;
            index += 1;
        }
        assert_eq!(index, frames.len());
    }
}

#[test]
fn free_format_layer_one_padding_uses_four_byte_slots() {
    let short = silence::<32>([0xff, 0xff, 0x04, 0xc0]);
    let long = silence::<36>([0xff, 0xff, 0x06, 0xc0]);
    let mut ts = Transport::new();
    ts.tables(&[(0x03, PID, &[])], 0);
    ts.pes(
        PID,
        0xc0,
        &[long.as_slice(), short.as_slice(), long.as_slice()].concat(),
        None,
        None,
    );
    let mut reader = Drive::new(&ts.bytes, ReaderLimits::default(), 8, 8);
    let mut lengths = Vec::new();
    while let Some(event) = reader.next().unwrap() {
        if let Event::Packet(packet) = event {
            lengths.push(packet.payload().len());
        }
    }
    assert_eq!(lengths, [36, 32, 36]);
}

#[test]
fn free_format_needs_two_spacings_and_reports_a_truncated_last_frame() {
    let frame = silence::<96>([0xff, 0xfb, 0x00, 0xc0]);
    let full = frame.repeat(2);
    let partial = [&full[..], &frame[..42]].concat();
    for (payload, want_packets, maximum, error) in [
        (full.as_slice(), 0, 8192, Error::Truncated),
        (partial.as_slice(), 2, 8192, Error::Truncated),
        (
            partial.as_slice(),
            0,
            95,
            Error::Limit {
                requested: 96,
                maximum: 95,
            },
        ),
    ] {
        let mut ts = Transport::new();
        ts.tables(&[(0x03, PID, &[])], 0);
        ts.pes(PID, 0xc0, payload, None, None);
        let limits = ReaderLimits {
            maximum_audio_frame_bytes: maximum,
            ..ReaderLimits::default()
        };
        let mut reader = Drive::new(&ts.bytes, limits, 1, 1);
        let mut count = 0;
        loop {
            match reader.next() {
                Ok(Some(Event::Packet(_))) => count += 1,
                Ok(Some(_)) => (),
                Err(actual) => {
                    assert_eq!(actual, error);
                    assert_eq!(count, want_packets);
                    break;
                }
                Ok(None) => panic!("expected incomplete or overlong frame"),
            }
        }
    }
}
