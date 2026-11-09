use std::sync::atomic::AtomicBool;
use triergon_core::container::{ContainerReader, Event};
use triergon_core::{
    Budget, Bytes, Error, InputRequest, InputWindow, SourceCapabilities, SourceFailure, SourceId,
    Step, WorkBudget,
};
use triergon_mpegts::compat::annex_b::{ContinuityEvent, ContinuityReader};
use triergon_mpegts::{FaultKind, ReaderEvent, ReaderLimits, TraceEvent};

const ID: SourceId = SourceId(9);
const THIRD: &[u8] = include_bytes!("data/cc-third-copy.ts");
const TIMELINE: &[u8] = include_bytes!("data/pcr-timeline-discontinuity.ts");
static OFF: AtomicBool = AtomicBool::new(false);

fn reader(pool: &Budget) -> ContinuityReader {
    ContinuityReader::new(
        pool,
        ID,
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        ReaderLimits::default(),
        37,
    )
    .unwrap()
}

fn provide(reader: &mut ContinuityReader, pool: &Budget, raw: &[u8], request: InputRequest) {
    let start = usize::try_from(request.offset).unwrap();
    let end = (start + request.maximum).min(raw.len());
    assert!(start < end);
    let bytes = Bytes::copy_from_slice(pool, &raw[start..end]).unwrap();
    reader
        .provide(&InputWindow::new(request.source, request.token, request.offset, bytes).unwrap())
        .unwrap();
}

#[test]
fn continuity_residue_keeps_original_carriers_and_events_with_unit_work() {
    for (raw, strict_bytes, residue_bytes) in [(THIRD, 27, 81), (TIMELINE, 30, 30)] {
        let pool = Budget::new(8 << 20).unwrap();
        let mut reader = reader(&pool);
        let mut original = Vec::new();
        let mut reported = None;
        let mut eof = None;
        let mut strict_pes_bytes = 0;
        let mut pending = 0;
        let mut ended = false;
        for _ in 0..500_000 {
            match reader.step(&mut WorkBudget::new(1), &OFF).unwrap() {
                Step::NeedInput(request) if request.offset == raw.len() as u64 => {
                    assert!(reported.is_none() && eof.is_none());
                    let Step::NeedInput(again) =
                        reader.step(&mut WorkBudget::new(1), &OFF).unwrap()
                    else {
                        panic!("starvation became EOF");
                    };
                    assert_eq!(again.token, request.token);
                    assert_eq!(again.offset, request.offset);
                    let cancelled = AtomicBool::new(true);
                    assert_eq!(
                        reader
                            .step(&mut WorkBudget::new(1), &cancelled)
                            .unwrap_err(),
                        Error::Cancelled
                    );
                    eof = Some(request.token.clone());
                    reader.finish(request.source, request.token).unwrap();
                }
                Step::NeedInput(request) => provide(&mut reader, &pool, raw, request),
                Step::Ready(Event::Other(event)) => match event {
                    ContinuityEvent::Residue(residue) => {
                        assert!(eof.is_some());
                        assert!(matches!(
                            original.last().map(triergon_mpegts::SourceEvent::event),
                            Some(TraceEvent::Finished)
                        ));
                        assert!(reported.replace(residue).is_none());
                    }
                    ContinuityEvent::Source(event) | ContinuityEvent::Pending(event) => match event
                    {
                        ReaderEvent::Source(source) => {
                            if let TraceEvent::Pes(pes) = source.event() {
                                strict_pes_bytes += pes.payload_len;
                            }
                            original.push(source);
                        }
                        ReaderEvent::Interrupted { .. } => pending += 1,
                        ReaderEvent::IncompleteVideo(_) => (),
                        other => panic!("unexpected source event {other:?}"),
                    },
                },
                Step::End(end) => {
                    assert_eq!((end.source, end.offset), (ID, raw.len() as u64));
                    ended = true;
                    break;
                }
                Step::Yield => (),
                other => panic!("unexpected step {other:?}"),
            }
        }
        assert!(ended);
        assert_eq!(strict_pes_bytes, strict_bytes);
        assert_eq!(pending, 1);
        reader.reset().unwrap();
        assert!(matches!(
            reader.finish(ID, eof.unwrap()),
            Err(Error::StaleGeneration { .. })
        ));
        reader.close();
        drop(reader);

        let residue = reported.unwrap();
        assert_eq!(residue.source_end().offset, raw.len() as u64);
        assert_eq!(residue.payload_len(), residue_bytes);
        assert_eq!(
            residue.payload().map(<[u8]>::len).sum::<usize>(),
            residue_bytes
        );
        assert_eq!(residue.clock().pts90, Some(90_000));
        assert_eq!(residue.clock().source_dts_raw33, None);
        for chunk in residue.chunks() {
            for span in chunk.spans.as_slice() {
                assert_eq!(span.source.entity, ID);
                let start = usize::try_from(span.source.offset).unwrap();
                assert_eq!(span.as_ref(), &raw[start..start + span.source.bytes]);
            }
        }
        let mut boundary_seen = false;
        for fact in residue.evidence() {
            let carrier_index = usize::try_from(fact.carrier_context.source_event_ordinal).unwrap();
            let event_index = usize::try_from(fact.context.source_event_ordinal).unwrap();
            assert!(carrier_index < event_index);
            let TraceEvent::Transport(carrier) = original[carrier_index].event() else {
                panic!("evidence carrier has no original transport event");
            };
            assert_eq!(carrier.source_offset, fact.carrier.source_offset);
            assert_eq!(carrier.source.as_ref(), fact.carrier.source.as_ref());
            assert!(std::ptr::eq(
                fact.event.event(),
                original[event_index].event()
            ));
            match fact.event.event() {
                TraceEvent::Diagnostic(fault) => {
                    assert_eq!(fault.kind, FaultKind::Continuity);
                    assert_eq!(fault.error, Error::Invalid("TS excess duplicate"));
                    assert_eq!(fault.location.offset, 752);
                    boundary_seen = true;
                }
                TraceEvent::Timeline { clock_epoch: 1, .. } => {
                    assert_eq!(fact.carrier_context.epochs.clock, 0);
                    assert_eq!(fact.context.epochs.clock, 1);
                    boundary_seen = true;
                }
                _ => (),
            }
        }
        assert!(boundary_seen);
        drop(residue);
        drop(original);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn source_failure_does_not_publish_a_continuity_residue() {
    let pool = Budget::new(8 << 20).unwrap();
    let mut reader = reader(&pool);
    for _ in 0..500_000 {
        match reader.step(&mut WorkBudget::new(1), &OFF).unwrap() {
            Step::NeedInput(request) if request.offset == THIRD.len() as u64 => {
                reader
                    .fail(request.source, request.token, SourceFailure::Transport)
                    .unwrap();
                assert_eq!(
                    reader.step(&mut WorkBudget::new(1), &OFF).unwrap_err(),
                    Error::Transport
                );
                reader.close();
                assert_eq!(pool.used(), Budget::control_storage_bytes());
                return;
            }
            Step::NeedInput(request) => provide(&mut reader, &pool, THIRD, request),
            Step::Ready(Event::Other(ContinuityEvent::Source(_) | ContinuityEvent::Pending(_)))
            | Step::Yield => (),
            other => panic!("source ended without an EOF signal: {other:?}"),
        }
    }
    panic!("source did not reach its final input request");
}
