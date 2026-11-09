use super::{
    AccessUnit, Drive, Error, Event, PID, PacketInfo, ReaderEvent, ReaderLimits, TraceEvent,
    Transport, adts,
};
use triergon_mpegts::FaultKind;
use triergon_mpegts::compat::adts::{PacketPolicy, PesPolicy};

fn recovery() -> ReaderLimits {
    ReaderLimits {
        adts_packet_policy: PacketPolicy::ReportFragments,
        adts_pes_policy: PesPolicy::ReportUnbounded,
        ..ReaderLimits::default()
    }
}

#[test]
fn zero_length_adts_stays_a_wire_error_and_reports_original_bytes_by_policy() {
    let bytes = include_bytes!("../data/pes-audio-zero-length-invalid.ts");
    let mut strict = Drive::new(bytes, ReaderLimits::default(), 1, 1);
    let mut diagnostic = false;
    while let Some(event) = strict.next().unwrap() {
        match event {
            Event::Other(ReaderEvent::Source(source)) => match source.event() {
                TraceEvent::Diagnostic(fault) => {
                    assert_eq!(fault.kind, FaultKind::Pes);
                    assert_eq!(fault.error, Error::Invalid("unbounded non-video PES"));
                    diagnostic = true;
                }
                TraceEvent::Pes(_) | TraceEvent::UnboundedAdtsPes(_) => {
                    panic!("strict transport admitted the invalid PES");
                }
                _ => (),
            },
            Event::Packet(_) | Event::Other(ReaderEvent::RecoveredAdts(_)) => {
                panic!("strict reader published rejected bytes");
            }
            _ => (),
        }
    }
    assert!(diagnostic);

    let mut reader = Drive::new(bytes, recovery(), 1, 1);
    let mut retained = None;
    let mut recovered = None;
    while let Some(event) = reader.next().unwrap() {
        match event {
            Event::Other(ReaderEvent::Source(source))
                if matches!(source.event(), TraceEvent::UnboundedAdtsPes(_)) =>
            {
                assert!(retained.replace(source).is_none());
            }
            Event::Other(ReaderEvent::RecoveredAdts(packet)) => {
                assert!(recovered.replace(packet).is_none());
            }
            Event::Packet(_) => panic!("unbounded PES bytes became a neutral packet"),
            _ => (),
        }
    }
    reader.reader.close();
    let source = retained.unwrap();
    let TraceEvent::UnboundedAdtsPes(rejected) = source.event() else {
        unreachable!()
    };
    let packet = recovered.unwrap();
    assert!(matches!(packet.access_unit(), AccessUnit::AdtsFragment(_)));
    assert_eq!(
        rejected.rejection(),
        Error::Invalid("unbounded non-video PES")
    );
    assert_eq!(rejected.pes().header.declared_packet_length, 0);
    assert_eq!(rejected.pes().payload_len, 27);
    let original: Vec<_> = rejected.pes().payload().flatten().copied().collect();
    assert_eq!(packet.payload(), original);
    assert!(std::ptr::eq(
        packet.sources()[0].pes.event(),
        source.event()
    ));
    assert_eq!(packet.pts(), Some(90_000));
    assert_eq!(packet.dts(), None);
}

fn unbounded(ts: &mut Transport, pid: u16, body: &[u8]) {
    let offset = ts.bytes.len();
    ts.pes(pid, 0xc0, body, Some(90_000), None);
    let packet = &mut ts.bytes[offset..offset + 188];
    let start = if packet[3] & 0x20 != 0 {
        5 + usize::from(packet[4])
    } else {
        4
    };
    assert_eq!(packet[start..start + 4], [0, 0, 1, 0xc0]);
    packet[start + 4..start + 6].fill(0);
}

#[test]
fn unbounded_and_finite_pes_share_the_same_frame_parser() {
    let frame = adts(&[0x31; 203]);
    let next = adts(&[0x24; 29]);
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    unbounded(&mut ts, PID, &frame[..3]);
    ts.pes(
        PID,
        0xc0,
        &[&frame[3..], &next].concat(),
        Some(180_000),
        Some(179_000),
    );
    let mut reader = Drive::new(&ts.bytes, recovery(), 1, 1);
    let mut packets = Vec::new();
    while let Some(event) = reader.next().unwrap() {
        match event {
            Event::Other(ReaderEvent::RecoveredAdts(packet)) => packets.push((true, packet)),
            Event::Packet(packet) => packets.push((false, packet)),
            _ => (),
        }
    }
    let [(true, first), (false, second)] = packets.as_slice() else {
        panic!("recovered and strict frames changed order or classification");
    };
    assert!(matches!(first.access_unit(), AccessUnit::Adts(_)));
    assert_eq!(first.payload(), frame);
    assert_eq!(first.pts(), Some(90_000));
    assert_eq!(first.sources().len(), 2);
    assert_eq!(first.sources()[0].payload_range, 0..3);
    assert_eq!(second.payload(), next);
    assert_eq!(second.pts(), Some(180_000));
    assert_eq!(second.dts(), Some(179_000));
}

#[test]
fn a_finite_pes_supersedes_an_empty_unbounded_clock() {
    let frame = adts(&[0x31; 19]);
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    unbounded(&mut ts, PID, &[]);
    ts.pes(PID, 0xc0, &frame, Some(180_000), Some(179_000));
    let mut reader = Drive::new(&ts.bytes, recovery(), 1, 1);
    let mut saw_empty = false;
    let mut saw_packet = false;
    while let Some(event) = reader.next().unwrap() {
        match event {
            Event::Other(ReaderEvent::Source(source)) => {
                if let TraceEvent::UnboundedAdtsPes(rejected) = source.event() {
                    assert_eq!(rejected.pes().payload_len, 0);
                    saw_empty = true;
                }
            }
            Event::Packet(packet) => {
                assert!(!saw_packet);
                assert_eq!(packet.payload(), frame);
                assert_eq!(packet.pts(), Some(180_000));
                assert_eq!(packet.dts(), Some(179_000));
                assert_eq!(packet.sources().len(), 1);
                assert!(matches!(
                    packet.clock().unwrap().pes.event(),
                    TraceEvent::Pes(_)
                ));
                saw_packet = true;
            }
            Event::Other(ReaderEvent::RecoveredAdts(_)) => {
                panic!("a superseded clock marked the finite frame as recovered");
            }
            _ => (),
        }
    }
    assert!(saw_empty && saw_packet);
}

#[test]
fn adts_policy_does_not_admit_other_unbounded_audio() {
    let mut ts = Transport::new();
    ts.tables(&[(0x11, PID, &[])], 0);
    unbounded(&mut ts, PID, &[0x56, 0xe0, 1, 0x24]);
    let mut reader = Drive::new(&ts.bytes, recovery(), 1, 1);
    let mut diagnostic = false;
    while let Some(event) = reader.next().unwrap() {
        match event {
            Event::Other(ReaderEvent::Source(source)) => match source.event() {
                TraceEvent::Diagnostic(fault) if fault.kind == FaultKind::Pes => {
                    assert_eq!(fault.error, Error::Invalid("unbounded non-video PES"));
                    diagnostic = true;
                }
                TraceEvent::UnboundedAdtsPes(_) => panic!("relabelled LATM as ADTS"),
                _ => (),
            },
            Event::Packet(_) => panic!("invalid LATM PES became a packet"),
            _ => (),
        }
    }
    assert!(diagnostic);
}
