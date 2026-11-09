use super::{
    Budget, Drive, Error, Event, ID, OFF, PID, PacketInfo, Reader, ReaderEvent, ReaderLimits,
    SourceCapabilities, Step, TraceEvent, Transport, WorkBudget, adts, timestamp,
};
use triergon_core::{InputRequest, SourceLocation};
use triergon_mpegts::compat::adts::PesPolicy;
use triergon_mpegts::{FaultKind, SourceRange, TransportPacket};

const CARRIER: usize = 188;
const HEADER_BYTES: usize = 65;
const HEADER_END: usize = (2 + HEADER_BYTES) * CARRIER;
const REPLACEMENT: usize = HEADER_BYTES * size_of::<SourceRange>();

fn source(policy: PesPolicy) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    // 9 fixed bytes, PTS/DTS, the private-data extension and 29 stuffing bytes.
    let mut header = vec![0, 0, 1, 0xc0, 0, 0, 0x80, 0xc1, 56];
    timestamp(&mut header, 3, 90_000);
    timestamp(&mut header, 1, 89_000);
    header.push(0x8e);
    header.extend_from_slice(&[0x5a; 16]);
    header.resize(HEADER_BYTES, 0xff);
    let frame = adts(&[0x31; 20]);
    if policy == PesPolicy::Strict {
        let length = u16::try_from(header.len() - 6 + frame.len()).unwrap();
        header[4..6].copy_from_slice(&length.to_be_bytes());
    }
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    for (index, byte) in header.iter().enumerate() {
        let offset = ts.bytes.len();
        ts.payload(PID, std::slice::from_ref(byte));
        if index != 0 {
            ts.bytes[offset + 1] &= !0x40;
        }
    }
    assert_eq!(ts.bytes.len(), HEADER_END);
    ts.payload(PID, &frame);
    ts.bytes[HEADER_END + 1] &= !0x40;
    (ts.bytes, header, frame)
}

fn before_growth(
    raw: &[u8],
    policy: PesPolicy,
    maximum: usize,
    retain_output: bool,
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
            ReaderLimits {
                adts_pes_policy: policy,
                ..ReaderLimits::default()
            },
            CARRIER,
        )
        .unwrap(),
        pool,
        raw,
        chunk: CARRIER,
        work: 1,
        catalogue: Vec::new(),
        catalogue_epoch_override: None,
    };
    let mut held = None;
    let mut last = false;
    while let Some(event) = drive.next().unwrap() {
        match event {
            Event::Other(ReaderEvent::Source(source)) => match source.event() {
                TraceEvent::Transport(packet) => {
                    if retain_output && packet.source_offset == 0 {
                        held = Some(packet.clone());
                    } else if packet.source_offset == (HEADER_END - CARRIER) as u64 {
                        assert_eq!(packet.pid, PID);
                        last = true;
                    }
                }
                TraceEvent::Continuity { pid, .. } if last => {
                    assert_eq!(*pid, PID);
                    return (drive, held);
                }
                TraceEvent::Diagnostic(fault) => panic!("unexpected diagnostic: {fault:?}"),
                _ => (),
            },
            other => panic!("unexpected output before header growth: {other:?}"),
        }
    }
    panic!("did not reach the final header carrier");
}

fn grow(reader: &mut Reader) -> Result<InputRequest, Error> {
    for _ in 0..10_000 {
        let mut work = WorkBudget::new(1);
        let step = reader.step(&mut work, &OFF).unwrap();
        assert!(work.spent() <= 1);
        match step {
            Step::Yield => (),
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
                assert_eq!(
                    fault.location,
                    SourceLocation {
                        source: ID,
                        offset: (HEADER_END - CARRIER) as u64
                    }
                );
                assert!(fault.rejected.is_none());
                return Err(fault.error);
            }
            other => panic!("unexpected header growth step: {other:?}"),
        }
    }
    panic!("header growth did not settle with unit grants");
}

fn charges(raw: &[u8], policy: PesPolicy) -> (usize, usize) {
    let (mut drive, _) = before_growth(raw, policy, 1 << 20, false);
    let pinned = drive.pool.used();
    let minimum = pinned + REPLACEMENT;
    assert!(drive.pool.peak() < minimum);
    assert_eq!(grow(&mut drive.reader).unwrap().offset, HEADER_END as u64);
    assert_eq!(drive.pool.peak(), minimum);

    let (drive, held) = before_growth(raw, policy, 1 << 20, true);
    let with_output = drive.pool.used();
    drop(held.unwrap());
    assert_eq!(drive.pool.used(), pinned);
    (minimum, with_output - pinned)
}

#[test]
fn split_adts_header_growth_refuses_a_permanent_limit() {
    for policy in [PesPolicy::ReportUnbounded, PesPolicy::Strict] {
        let (raw, _, _) = source(policy);
        let (minimum, _) = charges(&raw, policy);
        let maximum = minimum - 1;
        let (mut drive, _) = before_growth(&raw, policy, maximum, false);
        let result = grow(&mut drive.reader).unwrap_err();
        if matches!(result, Error::Backpressure { .. }) {
            assert_eq!(grow(&mut drive.reader).unwrap_err(), result);
        }
        assert_eq!(
            result,
            Error::Limit {
                requested: minimum,
                maximum
            },
            "{policy:?}"
        );
    }
}

#[test]
fn split_adts_header_growth_retries_with_the_original_ranges() {
    for policy in [PesPolicy::ReportUnbounded, PesPolicy::Strict] {
        retry_header_growth(policy);
    }
}

fn retry_header_growth(policy: PesPolicy) {
    let (raw, header, frame) = source(policy);
    let (minimum, caller_charge) = charges(&raw, policy);
    assert!(caller_charge > 0);
    let (mut drive, held) = before_growth(&raw, policy, minimum + caller_charge - 1, true);
    let before = drive.reader.request().unwrap();
    let expected = Error::Backpressure {
        requested: REPLACEMENT,
        available: REPLACEMENT - 1,
    };
    assert_eq!(grow(&mut drive.reader).unwrap_err(), expected);
    assert_eq!(grow(&mut drive.reader).unwrap_err(), expected);
    drop(held.unwrap());
    let request = grow(&mut drive.reader).unwrap();
    assert_eq!(
        (request.source, request.token),
        (before.source, before.token)
    );
    assert_eq!(request.offset, HEADER_END as u64);

    let mut saw_source = false;
    let mut saw_packet = false;
    let unbounded = policy == PesPolicy::ReportUnbounded;
    while let Some(event) = drive.next().unwrap() {
        let packet = match event {
            Event::Other(ReaderEvent::Source(source)) => {
                let event = source.event();
                if let TraceEvent::Diagnostic(fault) = event {
                    panic!("unexpected fault after retry: {fault:?}");
                }
                if let Some(pes) = event.pes_source() {
                    assert!(!saw_source);
                    assert_eq!(matches!(event, TraceEvent::UnboundedAdtsPes(_)), unbounded);
                    assert_eq!(pes.header.as_bytes(), header);
                    assert_eq!(pes.header.pts_raw33, Some(90_000));
                    assert_eq!(pes.header.dts_raw33, Some(89_000));
                    let ranges: Vec<_> = (0..HEADER_BYTES)
                        .map(|i| SourceRange {
                            entity: ID,
                            offset: ((i + 3) * CARRIER - 1) as u64,
                            bytes: 1,
                        })
                        .collect();
                    assert_eq!(pes.header_source_ranges.as_slice(), ranges);
                    assert_eq!(pes.payload().collect::<Vec<_>>(), [frame.as_slice()]);
                    saw_source = true;
                }
                continue;
            }
            Event::Packet(packet) if !unbounded => packet,
            Event::Other(ReaderEvent::RecoveredAdts(packet)) if unbounded => packet,
            Event::Packet(_) | Event::Other(ReaderEvent::RecoveredAdts(_)) => {
                panic!("unexpected packet class for {policy:?}")
            }
            _ => continue,
        };
        assert!(saw_source && !saw_packet);
        assert_eq!(packet.payload(), frame);
        assert_eq!(packet.pts(), Some(90_000));
        assert_eq!(packet.dts(), Some(89_000));
        saw_packet = true;
    }
    assert!(saw_packet);
}
