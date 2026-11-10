use super::{
    AccessUnit, CodecId, Drive, Error, Event, PID, PacketInfo, ReaderEvent, ReaderLimits,
    TraceEvent, TrackInfo, Transport,
};
use triergon_mpegts::compat::latm::PacketPolicy;

fn envelope(payload: &[u8]) -> Vec<u8> {
    let [high, low] = (payload.len() as u16).to_be_bytes();
    let mut bytes = vec![0x56, 0xe0 | high, low];
    bytes.extend_from_slice(payload);
    bytes
}

fn transport() -> Transport {
    let mut ts = Transport::new();
    ts.tables(&[(0x11, PID, &[])], 0);
    ts
}

#[test]
fn strict_loas_assembles_split_headers_and_preserves_pes_clocks() {
    let first = envelope(&[0x24; 213]);
    let second = envelope(&[0x82; 19]);
    let mut ts = transport();
    ts.pes(PID, 0xc0, &first[..2], Some(90_000), None);
    ts.pes(
        PID,
        0xc0,
        &[&first[2..], &second].concat(),
        Some(180_000),
        Some(179_000),
    );
    for (window, work) in [(1, 1), (4096, 4096)] {
        let mut reader = Drive::new(&ts.bytes, ReaderLimits::default(), window, work);
        let mut packets = Vec::new();
        while let Some(event) = reader.next().unwrap() {
            if let Event::Packet(packet) = event {
                packets.push(packet);
            }
        }
        assert_eq!(packets.len(), 2);
        reader.reader.close();
        for (packet, bytes) in packets.iter().zip([&first, &second]) {
            let AccessUnit::Loas(frame) = packet.access_unit() else {
                panic!("LATM track did not publish a LOAS envelope");
            };
            assert_eq!(frame.bytes().as_ref(), bytes);
            assert_eq!(frame.multiplex_element(), &bytes[3..]);
            assert_eq!(packet.payload(), bytes);
            assert_eq!(packet.duration(), None);
            assert_eq!(packet.track_info().codec(), CodecId::AacLatm);
        }
        assert_eq!(packets[0].pts(), Some(90_000));
        assert_eq!(packets[0].dts(), None);
        assert_eq!(packets[0].sources().len(), 2);
        assert_eq!(packets[0].sources()[0].payload_range, 0..2);
        assert_eq!(packets[0].sources()[1].payload_range, 0..first.len() - 2);
        assert_eq!(packets[1].pts(), Some(180_000));
        assert_eq!(packets[1].dts(), Some(179_000));
        assert_eq!(
            packets[1].sources()[0].payload_range,
            first.len() - 2..first.len() - 2 + second.len()
        );
    }
}

#[test]
fn complete_buffered_loas_reuses_source_storage() {
    let frame = envelope(&[0x24; 23]);
    let mut ts = transport();
    ts.pes(PID, 0xc0, &frame.repeat(2), Some(90_000), None);
    let mut reader = Drive::new(&ts.bytes, ReaderLimits::default(), 4096, 4096);
    let mut found = 0;
    while let Some(event) = reader.next().unwrap() {
        let Event::Packet(packet) = event else {
            continue;
        };
        let AccessUnit::Loas(loas) = packet.access_unit() else {
            panic!("expected a LOAS envelope");
        };
        let TraceEvent::Pes(pes) = packet.sources()[0].pes.event() else {
            panic!("expected original PES");
        };
        assert!(
            loas.bytes()
                .shares_storage_with(pes.spans()[0].backing_bytes())
        );
        assert_eq!(loas.elementary_range(), found..found + frame.len() as u64);
        assert_eq!(packet.pts(), (found == 0).then_some(90_000));
        found += frame.len() as u64;
    }
    assert_eq!(found, 2 * frame.len() as u64);
}

#[test]
fn strict_loas_rejects_bad_prefix_empty_body_and_truncation() {
    let oversized = envelope(&[0x24; 20]);
    for (body, expected) in [
        (&[0x00, 0xe0, 1][..], Error::Invalid("LOAS syncword")),
        (
            &[0x56, 0xe0, 0][..],
            Error::Invalid("empty LOAS multiplex element"),
        ),
        (&[0x56, 0xe0][..], Error::Truncated),
        (&[0x56, 0xe0, 5, 1][..], Error::Truncated),
        (
            &oversized[..],
            Error::Limit {
                requested: 23,
                maximum: 16,
            },
        ),
    ] {
        let mut ts = transport();
        ts.pes(PID, 0xc0, body, Some(90_000), None);
        let limits = ReaderLimits {
            maximum_audio_frame_bytes: 16,
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
fn compatibility_observes_pes_boundaries_and_keeps_original_sources() {
    let frame = envelope(&[0x24; 191]);
    let payload = [b"opaque ".as_slice(), &frame, &frame[..4]].concat();
    let mut ts = transport();
    ts.pes(PID, 0xc0, &payload, Some(90_000), None);
    ts.pes(PID, 0xc0, &frame, Some(180_000), Some(179_000));
    let limits = ReaderLimits {
        latm_packet_policy: PacketPolicy::PreservePes,
        ..ReaderLimits::default()
    };
    let mut reader = Drive::new(&ts.bytes, limits, 1, 1);
    let mut sources = Vec::new();
    let mut packets = Vec::new();
    while let Some(event) = reader.next().unwrap() {
        match event {
            Event::Other(ReaderEvent::Source(source))
                if matches!(source.event(), TraceEvent::Pes(_)) =>
            {
                sources.push(source);
            }
            Event::Other(ReaderEvent::LatmPes(packet)) => packets.push(packet),
            Event::Packet(_) => panic!("PES preservation emitted a neutral packet"),
            _ => (),
        }
    }
    assert_eq!(packets.len(), 2);
    assert_eq!(sources.len(), 2);
    reader.reader.close();
    for ((packet, source), expected) in packets.iter().zip(&sources).zip([&payload, &frame]) {
        assert!(std::ptr::eq(packet.source().event(), source.event()));
        assert_eq!(packet.clock_key().program, 7);
        assert!(!packet.clock_is_quarantined());
        assert_eq!(packet.track_info().codec(), CodecId::AacLatm);
        let bytes: Vec<u8> = packet.pes().payload().flatten().copied().collect();
        assert_eq!(&bytes, expected);
    }
    assert_eq!(packets[0].pes().header.pts_raw33, Some(90_000));
    assert_eq!(packets[0].pes().header.dts_raw33, None);
    assert_eq!(packets[1].pes().header.dts_raw33, Some(179_000));
}
