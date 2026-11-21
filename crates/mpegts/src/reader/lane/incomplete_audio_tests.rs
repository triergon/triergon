use super::*;
use crate::{Reader, ReaderEvent};
use triergon_core::container::{Event, PacketInfo, TrackId};

const HEADER: [u8; 7] = [0xff, 0xf1, 0x4c, 0x80, 3, 0x7f, 0xfc];
const ADTS_PID: TrackId = TrackId(257);

fn transport() -> Vec<u8> {
    // Keep the authored fixture's PAT, PMT and PES header. Only the finite
    // PES payload is replaced with an ADTS header declaring 27 bytes.
    let source = include_bytes!("../../../tests/data/carriage-aac-adts-direct.ts");
    let mut raw = source[..376].to_vec();
    let mut packet = [0xff; 188];
    packet[..4].copy_from_slice(&source[376..380]);
    packet[4] = 162;
    packet[5] = 0;
    packet[167..181].copy_from_slice(&source[523..537]);
    packet[171..173].copy_from_slice(&15_u16.to_be_bytes());
    packet[181..].copy_from_slice(&HEADER);
    raw.extend_from_slice(&packet);
    raw
}

fn split_transport() -> Vec<u8> {
    let source = include_bytes!("../../../tests/data/carriage-aac-adts-direct.ts");
    let mut raw = source[..376].to_vec();
    for (counter, byte) in HEADER.iter().enumerate() {
        let mut packet = [0xff; 188];
        packet[..4].copy_from_slice(&source[376..380]);
        packet[3] = 0x30 | u8::try_from(counter).unwrap();
        packet[4] = 168;
        packet[5] = 0;
        packet[173..187].copy_from_slice(&source[523..537]);
        packet[177..179].copy_from_slice(&9_u16.to_be_bytes());
        packet[187] = *byte;
        raw.extend_from_slice(&packet);
    }
    raw
}

fn reader(pool: &Budget, selected: bool) -> Reader {
    let source = SourceId(309);
    let caps = SourceCapabilities {
        length: None,
        random_access: false,
    };
    let limits = ReaderLimits {
        audio_eof_policy: AudioEofPolicy::ReportIncomplete,
        ..ReaderLimits::default()
    };
    if selected {
        Reader::new_with_packet_tracks(pool, source, caps, limits, 4096, &[ADTS_PID])
    } else {
        Reader::new(pool, source, caps, limits, 4096)
    }
    .unwrap()
}

fn checkpoint(reader: &Reader) -> Option<usize> {
    if reader.storage.is_some() {
        return Some(4);
    }
    reader.lanes.as_slice().iter().find_map(|lane| {
        if let Some(copy) = &lane.incomplete {
            if !copy.prepared && copy.piece == lane.pieces.len() {
                return Some(0);
            }
            if copy.prepared && copy.piece == lane.pieces.len() && copy.bytes.is_none() {
                return Some(1);
            }
        }
        let build = lane.build.as_ref()?;
        if !matches!(
            build.unit,
            BuildUnit::Packet(AccessUnit::IncompleteAudio { .. })
        ) {
            return None;
        }
        if matches!(build.origins, BuildOrigins::ReserveTable(_)) {
            return Some(2);
        }
        (build.origin == lane.pieces.len()).then_some(3)
    })
}

fn provide(reader: &mut Reader, pool: &Budget, raw: &[u8], request: InputRequest) {
    let at = usize::try_from(request.offset).unwrap();
    if at == raw.len() {
        reader.finish(request.source, request.token).unwrap();
    } else {
        let end = raw.len().min(at + request.maximum.min(7));
        let bytes = Bytes::copy_from_slice(pool, &raw[at..end]).unwrap();
        reader
            .provide(
                &InputWindow::new(request.source, request.token, request.offset, bytes).unwrap(),
            )
            .unwrap();
    }
}

#[test]
fn fragment_copy_freeze_origins_and_publication_retry_transactionally() {
    let raw = transport();
    let pool = Budget::new(32 << 20).unwrap();
    let mut reader = reader(&pool, false);
    let cancel = AtomicBool::new(false);
    let mut reached = 0;
    let mut fragment = None;
    let mut ended = false;
    for _ in 0..1_000_000 {
        if reached < 4 && checkpoint(&reader) == Some(reached) {
            let pressure = pool.reserve(pool.available()).unwrap();
            let mut refused = false;
            for _ in 0..10_000 {
                match reader.step(&mut WorkBudget::new(1), &cancel).unwrap() {
                    Step::Yield => (),
                    Step::Backpressure(_) => {
                        refused = true;
                        break;
                    }
                    other => panic!("fragment advanced under pressure: {other:?}"),
                }
            }
            assert!(refused, "checkpoint {reached}");
            drop(pressure);
            reached += 1;
        }
        match reader.step(&mut WorkBudget::new(1), &cancel).unwrap() {
            Step::NeedInput(request) => provide(&mut reader, &pool, &raw, request),
            Step::Ready(Event::Other(ReaderEvent::IncompleteAudio(packet))) => {
                assert!(fragment.is_none());
                assert_eq!(reached, 4);
                assert_eq!(packet.payload(), HEADER);
                assert_eq!(packet.elementary_range(), 0..7);
                assert_eq!(packet.sources().len(), 1);
                assert_eq!(packet.sources()[0].payload_range, 0..7);
                assert_eq!(packet.pts(), Some(90_000));
                fragment = Some(packet);
            }
            Step::Ready(Event::Packet(_)) => panic!("unfinished audio is not a packet"),
            Step::Ready(_) | Step::Yield => (),
            Step::End(_) => {
                ended = true;
                break;
            }
            other @ Step::Backpressure(_) => panic!("fragment retry failed: {other:?}"),
        }
    }
    assert!(ended);
    reader.close();
    assert_eq!(fragment.as_ref().unwrap().payload(), HEADER);
    drop(fragment);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn cancellation_at_each_fragment_allocation_precedes_zero_work_and_releases_storage() {
    let raw = transport();
    for target in 0..5 {
        let pool = Budget::new(32 << 20).unwrap();
        let mut reader = reader(&pool, false);
        let cancel = AtomicBool::new(false);
        let mut reached = false;
        for _ in 0..1_000_000 {
            if checkpoint(&reader) == Some(target) {
                cancel.store(true, Ordering::Relaxed);
                assert_eq!(
                    reader.step(&mut WorkBudget::new(0), &cancel).unwrap_err(),
                    Error::Cancelled
                );
                assert_eq!(pool.used(), Budget::control_storage_bytes());
                reader.reset().unwrap();
                reader.close();
                assert_eq!(pool.used(), Budget::control_storage_bytes());
                reached = true;
                break;
            }
            match reader.step(&mut WorkBudget::new(1), &cancel).unwrap() {
                Step::NeedInput(request) => provide(&mut reader, &pool, &raw, request),
                Step::Ready(Event::Packet(_) | Event::Other(ReaderEvent::IncompleteAudio(_)))
                | Step::End(_) => panic!("fragment cancellation checkpoint not reached"),
                Step::Ready(_) | Step::Yield => (),
                other @ Step::Backpressure(_) => {
                    panic!("fragment cancellation failed: {other:?}")
                }
            }
        }
        assert!(reached, "checkpoint {target}");
    }
}

#[test]
fn fragment_storage_audit_counts_all_reader_owners_and_excludes_external_reservations() {
    let raw = split_transport();
    for chunk in [7, 4096] {
        let pool = Budget::new(32 << 20).unwrap();
        let mut reader = reader(&pool, false);
        let cancel = AtomicBool::new(false);
        let external = pool.reserve(19_937).unwrap();
        let mut reached = false;
        for _ in 0..1_000_000 {
            if let Some(lane) = reader.lanes.as_slice().iter().find(|lane| {
                lane.incomplete
                    .as_ref()
                    .is_some_and(|copy| copy.reader_storage.is_some())
            }) {
                let bytes = lane.incomplete.as_ref().unwrap().reader_storage.unwrap();
                assert_eq!(bytes, pool.used() - external.bytes());
                assert_eq!(lane.pieces.len(), 7);
                if chunk == 7 {
                    let backings: Vec<_> = lane
                        .pieces
                        .as_slice()
                        .iter()
                        .map(|piece| piece.source.pes().spans()[0].backing_bytes())
                        .collect();
                    assert!(backings.iter().enumerate().all(|(i, backing)| {
                        backings[..i]
                            .iter()
                            .all(|prior| !prior.shares_storage_with(backing))
                    }));
                }
                reached = true;
                break;
            }
            match reader.step(&mut WorkBudget::new(1), &cancel).unwrap() {
                Step::NeedInput(request) => {
                    let at = usize::try_from(request.offset).unwrap();
                    if at == raw.len() {
                        reader.finish(request.source, request.token).unwrap();
                    } else {
                        let end = raw.len().min(at + request.maximum.min(chunk));
                        let bytes = Bytes::copy_from_slice(&pool, &raw[at..end]).unwrap();
                        reader
                            .provide(
                                &InputWindow::new(
                                    request.source,
                                    request.token,
                                    request.offset,
                                    bytes,
                                )
                                .unwrap(),
                            )
                            .unwrap();
                    }
                }
                Step::Ready(Event::Other(ReaderEvent::IncompleteAudio(_))) | Step::End(_) => {
                    panic!("audit not observed")
                }
                Step::Ready(_) | Step::Yield => (),
                other @ Step::Backpressure(_) => panic!("audit refused: {other:?}"),
            }
        }
        assert!(reached);
        reader.close();
        assert_eq!(
            pool.used(),
            Budget::control_storage_bytes() + external.bytes()
        );
        drop(external);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

fn tight_split_run(
    limit: usize,
    profile: bool,
    external_pressure: bool,
    selected: bool,
) -> (usize, usize) {
    let raw = split_transport();
    let pool = Budget::new(limit).unwrap();
    let mut reader = reader(&pool, selected);
    let cancel = AtomicBool::new(false);
    let mut baseline = None;
    let mut minimum = 0;
    let mut sources = 0;
    let mut fragments = 0;
    let mut pressure = None;
    let mut injected = false;
    let mut retried = false;
    for _ in 0..1_000_000 {
        if baseline.is_none()
            && let Some(storage) = reader
                .lanes
                .as_slice()
                .iter()
                .find_map(|lane| lane.incomplete.as_ref()?.reader_storage)
        {
            baseline = Some(pool.peak());
            minimum = storage
                + HEADER.len()
                + Bytes::owner_overhead_bytes()
                + 7 * size_of::<PayloadSource>()
                + Shared::<PacketData>::allocation_bytes().unwrap();
            if profile {
                assert_eq!(sources, 7);
                return (baseline.unwrap(), minimum);
            }
        }
        if external_pressure && baseline.is_some() && !injected {
            pressure = Some(pool.reserve(pool.available()).unwrap());
            injected = true;
        }
        let mut work = WorkBudget::new(1);
        let step = reader.step(&mut work, &cancel);
        assert!(work.spent() <= 1);
        match step {
            Ok(Step::NeedInput(request)) => provide(&mut reader, &pool, &raw, request),
            Ok(Step::Ready(Event::Other(ReaderEvent::Source(source)))) => {
                if matches!(source.event(), crate::stream::TraceEvent::Pes(_)) {
                    sources += 1;
                }
                // Every original source event is discarded here.
            }
            Ok(Step::Ready(Event::Other(ReaderEvent::IncompleteAudio(packet)))) => {
                assert_eq!(packet.payload(), HEADER);
                assert_eq!(packet.sources().len(), 7);
                fragments += 1;
                // No returned packet or fragment remains held between steps.
            }
            Ok(Step::Backpressure(_)) if pressure.is_some() => {
                drop(pressure.take());
                retried = true;
            }
            Ok(Step::Backpressure(value)) => {
                panic!("permanent EOF pressure with no held output at {limit}: {value:?}")
            }
            Ok(Step::Ready(Event::Packet(_))) => panic!("fragment is not an AU"),
            Ok(Step::Ready(_) | Step::Yield) => (),
            Ok(Step::End(_)) => {
                assert_eq!(sources, 7);
                assert_eq!(fragments, 1);
                assert!(limit >= minimum);
                assert_eq!(retried, external_pressure);
                return (baseline.unwrap(), minimum);
            }
            Err(Error::Limit { requested, maximum }) => {
                assert_eq!(sources, 7);
                assert_eq!(fragments, 0);
                assert_eq!(requested, minimum);
                assert_eq!(maximum, limit);
                assert!(minimum > limit);
                return (baseline.unwrap(), minimum);
            }
            Err(error) => panic!("tight EOF failed at {limit}: {error:?}"),
        }
    }
    panic!("tight EOF did not terminate at {limit}");
}

#[test]
fn seven_byte_adts_header_in_seven_validated_pes_terminates_across_tight_budgets() {
    for selected in [false, true] {
        let (prefix_peak, minimum) = tight_split_run(32 << 20, true, false, selected);
        for limit in prefix_peak..=prefix_peak.max(minimum) + 32 {
            assert_eq!(
                tight_split_run(limit, false, false, selected),
                (prefix_peak, minimum)
            );
        }
        tight_split_run(prefix_peak.max(minimum) + 1, false, true, selected);
    }
}
