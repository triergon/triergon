use super::{
    AccessUnit, AtomicBool, Budget, Bytes, Drive, Error, Event, ID, InputWindow, OFF, PID,
    PacketInfo, Reader, ReaderEvent, ReaderLimits, SourceCapabilities, Step, TraceEvent, Transport,
    WorkBudget, adts,
};
use triergon_core::InputRequest;
use triergon_mpegts::compat::adts::{PacketPolicy, PesPolicy};
use triergon_mpegts::{FaultKind, TransportPacket};

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

fn before_last_span(
    raw: &[u8],
    maximum: usize,
    window: usize,
    retain_first_carrier: bool,
) -> (Drive<'_>, Option<TransportPacket>) {
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
            window,
        )
        .unwrap(),
        pool,
        raw,
        chunk: window,
        // A full carrier needs one grant to stay a view of the input window.
        work: if window == 188 { 1 } else { 4096 },
        catalogue: Vec::new(),
        catalogue_epoch_override: None,
    };
    let last_offset = (raw.len() - 188) as u64;
    let mut first_carrier = None;
    let mut at_last_carrier = false;
    while let Some(event) = drive.next().unwrap() {
        match event {
            Event::Other(ReaderEvent::Transport(packet)) => {
                if packet.source_offset == last_offset {
                    assert_eq!(packet.pid, PID);
                    assert!(packet.source.retained_capacity() >= window);
                    at_last_carrier = true;
                }
                if retain_first_carrier && packet.source_offset == 0 {
                    assert_eq!(window, 188);
                    first_carrier = Some(packet);
                }
            }
            Event::Other(ReaderEvent::Continuity { pid, .. }) if at_last_carrier => {
                assert_eq!(pid, PID);
                assert_eq!(first_carrier.is_some(), retain_first_carrier);
                return (drive, first_carrier);
            }
            Event::Other(ReaderEvent::Source(source)) => {
                assert!(!matches!(source.event(), TraceEvent::Diagnostic(_)));
            }
            Event::Other(ReaderEvent::Continuity { .. }) => (),
            other => panic!("output before the final span: {other:?}"),
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
            Step::Ready(Event::Other(ReaderEvent::Interrupted {
                cause,
                elementary_range,
                ..
            })) => {
                assert!(matches!(
                    cause.event(),
                    TraceEvent::Diagnostic(fault)
                        if fault.kind == FaultKind::Pes
                            && fault.pid == Some(PID)
                            && matches!(fault.error, Error::Limit { .. })
                ));
                assert!(!elementary_range.is_empty());
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

fn span_growth_minimum(raw: &[u8], window: usize) -> usize {
    let (mut drive, _) = before_last_span(raw, 1 << 20, window, false);
    let before = drive.pool.peak();
    assert_eq!(
        grow_last_span(&mut drive.reader).unwrap().offset,
        raw.len() as u64
    );
    let minimum = drive.pool.peak();
    assert!(
        minimum > before + 3,
        "final span must set a new peak, window {window}, before {before}, after {minimum}"
    );
    minimum
}

fn first_carrier_charge(raw: &[u8]) -> usize {
    // Retain an earlier PAT in its own input allocation. The final carrier
    // shares the allocation the active PES already retains.
    let (drive, carrier) = before_last_span(raw, 1 << 20, 188, true);
    let with_carrier = drive.pool.used();
    drop(carrier.unwrap());
    let charge = with_carrier - drive.pool.used();
    assert!(charge > 0);
    charge
}

#[test]
fn unbounded_span_growth_counts_pinned_reader_storage() {
    let (raw, _) = span_growth_source();
    let minimum = span_growth_minimum(&raw, 188);
    let observed = [0, 1, 2, 3].map(|shortfall| {
        let maximum = minimum - shortfall;
        let (mut drive, _) = before_last_span(&raw, maximum, 188, false);
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
fn unbounded_span_growth_counts_a_pending_finite_pes() {
    let frame = adts(&[0x31; 203]);
    let input = frame.repeat(SPAN_GROWTH_FRAMES);
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    ts.pes(PID, 0xc0, &input[..3], Some(90_000), None);
    unbounded(&mut ts, PID, &input[3..]);

    let minimum = span_growth_minimum(&ts.bytes, 188);
    let maximum = minimum - 1;
    let (mut drive, _) = before_last_span(&ts.bytes, maximum, 188, false);
    assert_eq!(
        grow_last_span(&mut drive.reader).unwrap_err(),
        Error::Limit {
            requested: minimum,
            maximum,
        }
    );
}

#[test]
fn unbounded_span_growth_counts_other_active_lanes_and_deduplicates_the_window() {
    let frame = adts(&[0x31; 203]);
    let mut ts = Transport::new();
    ts.tables(
        &[
            (0x0f, PID, &[]),
            (0x0f, PID + 1, &[]),
            (0x0f, PID + 2, &[]),
            (0x24, PID + 3, &[]),
        ],
        0,
    );
    // Hold a partial ADTS body, an unfinished transport PES, and a completed
    // HEVC NAL plus a partial NAL in three other routes.
    ts.pes(PID + 1, 0xc0, &frame[..40], Some(90_000), None);
    unbounded(&mut ts, PID + 2, &frame[..23]);
    ts.pes(
        PID + 3,
        0xe0,
        &[0, 0, 1, 0x46, 1, 0x50, 0, 0, 1, 0x46, 1, 0x50],
        Some(90_000),
        None,
    );
    unbounded(&mut ts, PID, &frame.repeat(2 * SPAN_GROWTH_FRAMES));

    // The full window aliases the spans in both assemblers and both lanes.
    for window in [188, ts.bytes.len()] {
        let minimum = span_growth_minimum(&ts.bytes, window);
        let maximum = minimum - 1;
        let (mut drive, _) = before_last_span(&ts.bytes, maximum, window, false);
        assert_eq!(
            grow_last_span(&mut drive.reader).unwrap_err(),
            Error::Limit {
                requested: minimum,
                maximum
            }
        );

        let (mut drive, _) = before_last_span(&ts.bytes, minimum, window, false);
        let retained = drive.pool.reserve(1).unwrap();
        let before = drive.reader.request().unwrap();
        assert!(matches!(
            grow_last_span(&mut drive.reader),
            Err(Error::Backpressure { requested, available }) if requested == available + 1
        ));
        drop(retained);
        let resumed = grow_last_span(&mut drive.reader).unwrap();
        assert_eq!(
            (resumed.source, resumed.token),
            (before.source, before.token)
        );
        assert_eq!(resumed.offset, ts.bytes.len() as u64);
    }
}

#[test]
fn unbounded_span_growth_retries_after_dropping_a_source_event() {
    let (raw, frame) = span_growth_source();
    let minimum = span_growth_minimum(&raw, 188);
    let maximum = minimum + first_carrier_charge(&raw) - 1;
    let (mut drive, carrier) = before_last_span(&raw, maximum, 188, true);
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
            Event::Other(ReaderEvent::Transport(_) | ReaderEvent::Continuity { .. }) => (),
            other => panic!("unexpected output after span retry: {other:?}"),
        }
    }
    assert_eq!(recovered, SPAN_GROWTH_FRAMES);
}

#[test]
fn unbounded_span_growth_rechecks_a_newly_buffered_owner() {
    let (raw, _) = span_growth_source();
    let minimum = span_growth_minimum(&raw, 188);
    let retained_bytes = 256;
    let maximum = minimum + retained_bytes - 1;
    let (mut drive, _) = before_last_span(&raw, maximum, 188, false);
    let retained = drive.pool.reserve(retained_bytes).unwrap();
    assert!(matches!(
        grow_last_span(&mut drive.reader),
        Err(Error::Backpressure { .. })
    ));

    let request = drive.reader.request().unwrap();
    // Accept a next window while the current carrier is still blocked. Only
    // one carrier is visible, but the cursor pins the whole backing.
    let mut raw = [0xff; 3 * 188];
    raw[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
    let bytes = Bytes::copy_from_slice(&drive.pool, &raw).unwrap();
    let added = bytes.retained_storage_bytes();
    assert!(added > retained_bytes);
    let window = InputWindow::new(
        request.source,
        request.token,
        request.offset,
        bytes.into_slice(0..188).unwrap(),
    )
    .unwrap();
    drive.reader.provide(&window).unwrap();
    drop(window);
    drop(retained);
    assert_eq!(
        grow_last_span(&mut drive.reader).unwrap_err(),
        Error::Limit {
            requested: minimum + added,
            maximum,
        }
    );
}

#[test]
fn resuming_span_growth_does_not_pin_buffered_input() {
    let frame = adts(&[0x31; 203]);
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    unbounded(&mut ts, PID, &frame.repeat(3));

    let mut next = [0xff; 4096];
    next[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
    let scratch = Budget::new(1 << 20).unwrap();
    let input_charge = Bytes::copy_from_slice(&scratch, &next)
        .unwrap()
        .retained_storage_bytes();
    let maximum = span_growth_minimum(&ts.bytes, 188) + input_charge;

    // Resume with more work at each point of the suspended storage scan.
    for delay in 0..128 {
        let (mut drive, _) = before_last_span(&ts.bytes, maximum, 188, false);
        let request = drive.reader.request().unwrap();
        let bytes = Bytes::copy_from_slice(&drive.pool, &next).unwrap();
        let window = InputWindow::new(
            request.source,
            request.token,
            request.offset,
            bytes.into_slice(0..188).unwrap(),
        )
        .unwrap();
        drive.reader.provide(&window).unwrap();
        drop(window);
        let retained = drive.pool.reserve(1).unwrap();

        for _ in 0..delay {
            match drive.reader.step(&mut WorkBudget::new(1), &OFF).unwrap() {
                Step::Yield => (),
                Step::Backpressure(p) => assert_eq!(p.requested, p.available + 1),
                other => panic!("span advanced with the competing reservation: {other:?}"),
            }
        }
        drop(retained);

        let mut saw_null = false;
        let mut saw_next = false;
        for _ in 0..8 {
            match drive.reader.step(&mut WorkBudget::new(4096), &OFF).unwrap() {
                Step::Ready(Event::Other(ReaderEvent::Transport(packet))) => {
                    assert_eq!(packet.pid, 0x1fff);
                    saw_null = true;
                }
                Step::NeedInput(q) => {
                    assert_eq!(q.offset, request.offset + 188);
                    saw_next = true;
                    break;
                }
                Step::Ready(Event::Other(ReaderEvent::Continuity { .. })) => (),
                other => panic!("unnecessary pressure after resuming at {delay}: {other:?}"),
            }
        }
        assert!(saw_null && saw_next);
    }
}

#[test]
fn unbounded_span_growth_cancels_before_work_and_releases_the_audit() {
    let (raw, _) = span_growth_source();
    let minimum = span_growth_minimum(&raw, 188);
    let maximum = minimum + first_carrier_charge(&raw) - 1;
    let (mut drive, carrier) = before_last_span(&raw, maximum, 188, true);
    let carrier = carrier.unwrap();
    assert!(matches!(
        grow_last_span(&mut drive.reader),
        Err(Error::Backpressure { .. })
    ));
    let mut work = WorkBudget::new(0);
    assert_eq!(
        drive
            .reader
            .step(&mut work, &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(work.spent(), 0);
    assert_eq!(carrier.source_offset, 0);
    drop(carrier);
    assert_eq!(drive.pool.used(), Budget::control_storage_bytes());
}
