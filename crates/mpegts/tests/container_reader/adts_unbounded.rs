use super::{
    AccessUnit, Budget, Drive, Error, Event, ID, OFF, PID, PacketInfo, Reader, ReaderEvent,
    ReaderLimits, SourceCapabilities, Step, TraceEvent, Transport, WorkBudget, adts,
};
use triergon_core::InputRequest;
use triergon_mpegts::compat::adts::{PacketPolicy, PesPolicy};
use triergon_mpegts::{FaultKind, SourceEvent};

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

const SPAN_GROWTH_FRAMES: usize = 32;

fn span_growth_source() -> (Vec<u8>, Vec<u8>) {
    let frame = adts(&[0x31; 203]);
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    unbounded(&mut ts, PID, &frame.repeat(SPAN_GROWTH_FRAMES));
    (ts.bytes, frame)
}

fn before_last_span(raw: &[u8], maximum: usize) -> (Drive<'_>, SourceEvent) {
    let pool = Budget::new(maximum).unwrap();
    let mut drive = Drive {
        reader: Reader::new(
            &pool,
            ID,
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            recovery(),
            188,
        )
        .unwrap(),
        pool,
        raw,
        chunk: 188,
        work: 1,
        catalogue: Vec::new(),
        catalogue_epoch_override: None,
    };
    let last_offset = (raw.len() - 188) as u64;
    let mut last_carrier = None;
    while let Some(event) = drive.next().unwrap() {
        let Event::Other(ReaderEvent::Source(source)) = event else {
            panic!("output before the final span: {event:?}");
        };
        match source.event() {
            TraceEvent::Transport(packet) if packet.source_offset == last_offset => {
                assert_eq!(packet.pid, PID);
                last_carrier = Some(source);
            }
            TraceEvent::Continuity { pid, .. } if last_carrier.is_some() => {
                assert_eq!(*pid, PID);
                return (drive, last_carrier.unwrap());
            }
            TraceEvent::Diagnostic(fault) => panic!("unexpected source fault: {fault:?}"),
            _ => (),
        }
    }
    panic!("did not reach the last carrier");
}

fn grow_last_span(reader: &mut Reader) -> Result<InputRequest, Error> {
    // Do not finish the source or offer another allocation while observing
    // the growth transaction. A PES resource limit may surface as a diagnostic.
    for _ in 0..10_000 {
        let mut work = WorkBudget::new(1);
        let step = reader.step(&mut work, &OFF)?;
        assert!(work.spent() <= 1);
        match step {
            Step::NeedInput(request) => return Ok(request),
            Step::Backpressure(pressure) => {
                return Err(Error::Backpressure {
                    requested: pressure.requested,
                    available: pressure.available,
                });
            }
            Step::Ready(Event::Other(ReaderEvent::Source(source))) => {
                let TraceEvent::Diagnostic(fault) = source.event() else {
                    panic!("unexpected source event: {source:?}");
                };
                assert_eq!(fault.kind, FaultKind::Pes);
                assert_eq!(fault.pid, Some(PID));
                return Err(fault.error);
            }
            Step::Yield => (),
            other => panic!("unexpected span growth step: {other:?}"),
        }
    }
    panic!("span growth did not settle with unit work grants");
}

fn span_growth_charges(raw: &[u8]) -> (usize, usize) {
    let (mut drive, carrier) = before_last_span(raw, 1 << 20);
    let with_carrier = drive.pool.used();
    drop(carrier);
    let carrier_charge = with_carrier - drive.pool.used();
    let before = drive.pool.peak();
    assert_eq!(
        grow_last_span(&mut drive.reader).unwrap().offset,
        raw.len() as u64
    );
    let minimum = drive.pool.peak();
    assert!(minimum > before + 3, "final span must set a new peak");
    (minimum, carrier_charge)
}

#[test]
fn unbounded_span_growth_counts_pinned_reader_storage() {
    let (raw, _) = span_growth_source();
    let (minimum, _) = span_growth_charges(&raw);
    let observed = [0, 1, 2, 3].map(|shortfall| {
        let maximum = minimum - shortfall;
        let (mut drive, carrier) = before_last_span(&raw, maximum);
        drop(carrier);
        let used = drive.pool.used();
        let result = grow_last_span(&mut drive.reader).map(|request| {
            assert_eq!(request.offset, raw.len() as u64);
        });
        if let Err(error @ Error::Backpressure { .. }) = result {
            assert_eq!(grow_last_span(&mut drive.reader).unwrap_err(), error);
            assert_eq!(drive.pool.used(), used);
        }
        (maximum, result)
    });
    assert_eq!(
        observed,
        [0, 1, 2, 3].map(|shortfall| {
            let maximum = minimum - shortfall;
            let result = if shortfall == 0 {
                Ok(())
            } else {
                Err(Error::Limit {
                    requested: minimum,
                    maximum,
                })
            };
            (maximum, result)
        }),
        "with every source event dropped, only a permanent limit can block the span"
    );
}

#[test]
fn unbounded_span_growth_retries_after_dropping_a_source_event() {
    let (raw, frame) = span_growth_source();
    let (minimum, carrier_charge) = span_growth_charges(&raw);
    assert!(carrier_charge > 0);
    let (mut drive, carrier) = before_last_span(&raw, minimum + carrier_charge - 1);
    let before = drive.reader.request().unwrap();
    assert!(matches!(
        grow_last_span(&mut drive.reader),
        Err(Error::Backpressure { requested, available }) if requested == available + 1
    ));
    drop(carrier);
    let resumed = grow_last_span(&mut drive.reader).unwrap();
    assert_eq!(
        (resumed.source, resumed.token),
        (before.source, before.token)
    );
    assert_eq!(resumed.offset, raw.len() as u64);

    let mut recovered = 0;
    while let Some(event) = drive.next().unwrap() {
        match event {
            Event::Other(ReaderEvent::RecoveredAdts(packet)) => {
                assert_eq!(packet.payload(), frame);
                recovered += 1;
            }
            Event::Other(ReaderEvent::Source(source)) => {
                assert!(!matches!(source.event(), TraceEvent::Diagnostic(_)));
            }
            other => panic!("unexpected output after span retry: {other:?}"),
        }
    }
    assert_eq!(recovered, SPAN_GROWTH_FRAMES);
}
