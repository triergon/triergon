use super::{
    AccessUnit, Drive, Error, Event, PID, PacketInfo, ReaderEvent, ReaderLimits, Transport, adts,
};
use triergon_framing::AacAdtsCarrierBoundary;
use triergon_mpegts::compat::adts::PacketPolicy;

fn fragment_policy() -> ReaderLimits {
    ReaderLimits {
        adts_packet_policy: PacketPolicy::ReportFragments,
        ..ReaderLimits::default()
    }
}

#[test]
fn fragments_keep_source_ranges_and_each_pes_clock_is_used_once() {
    let prefix = b"opaque prefix";
    let suffix = b"opaque suffix";
    let frame = adts(b"coded payload");
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    ts.pes(
        PID,
        0xc0,
        &[&prefix[..], &frame].concat(),
        Some(90_000),
        None,
    );
    ts.pes(PID, 0xc0, suffix, Some(180_000), None);

    let mut strict = Drive::new(&ts.bytes, ReaderLimits::default(), 1, 1);
    loop {
        match strict.next() {
            Err(error) => {
                assert_eq!(error, Error::Invalid("ADTS sync/layer"));
                break;
            }
            Ok(Some(Event::Other(_))) => (),
            Ok(_) => panic!("strict reader accepted an opaque prefix"),
        }
    }

    let mut reader = Drive::new(&ts.bytes, fragment_policy(), 1, 1);
    let mut found = Vec::new();
    while let Some(event) = reader.next().unwrap() {
        match event {
            Event::Other(ReaderEvent::AdtsFragment(packet)) => {
                let AccessUnit::AdtsFragment(fragment) = packet.access_unit() else {
                    panic!("fragment event lost its carrier");
                };
                assert!(fragment.selected_error().is_some());
                assert_eq!(packet.duration(), None);
                found.push((packet, true));
            }
            Event::Packet(packet) => {
                assert!(matches!(packet.access_unit(), AccessUnit::Adts(_)));
                found.push((packet, false));
            }
            _ => (),
        }
    }
    reader.reader.close();
    assert_eq!(found.len(), 3);
    let [(first, true), (coded, false), (last, true)] = found.as_slice() else {
        panic!("fragment and packet order changed");
    };
    assert_eq!(first.payload(), prefix);
    assert_eq!(first.pts(), Some(90_000));
    assert_eq!(first.dts(), None);
    assert_eq!(first.sources()[0].payload_range, 0..prefix.len());
    assert_eq!(coded.payload(), frame);
    assert_eq!(coded.pts(), None);
    assert_eq!(coded.dts(), None);
    assert_eq!(
        coded.sources()[0].payload_range,
        prefix.len()..prefix.len() + frame.len()
    );
    assert_eq!(last.payload(), suffix);
    assert_eq!(last.pts(), Some(180_000));
    assert_eq!(last.sources()[0].payload_range, 0..suffix.len());
    for (packet, boundary) in [
        (first, AacAdtsCarrierBoundary::FollowingEnvelope),
        (last, AacAdtsCarrierBoundary::ExplicitEofFragment),
    ] {
        let AccessUnit::AdtsFragment(fragment) = packet.access_unit() else {
            unreachable!()
        };
        assert_eq!(fragment.boundary(), boundary);
    }
}

#[test]
fn discontinuity_does_not_publish_a_compatibility_eof_tail() {
    let frame = adts(b"coded after the clock transition");
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    ts.pes(
        PID,
        0xc0,
        b"opaque before the clock transition",
        Some(90_000),
        None,
    );
    ts.pcr(90_000, true);
    ts.pes(PID, 0xc0, &frame, Some(180_000), None);

    let mut reader = Drive::new(&ts.bytes, fragment_policy(), 7, 11);
    let mut found_frame = false;
    let mut found_interruption = false;
    while let Some(event) = reader.next().unwrap() {
        match event {
            Event::Packet(packet) => {
                assert_eq!(packet.payload(), frame);
                found_frame = true;
            }
            Event::Other(ReaderEvent::Interrupted { .. }) => found_interruption = true,
            Event::Other(ReaderEvent::AdtsFragment(_)) => panic!("interruption became EOF"),
            _ => (),
        }
    }
    assert!(found_frame && found_interruption);
}
