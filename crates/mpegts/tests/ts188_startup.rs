use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, InputCursor, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_mpegts::compat::input::Ts188Startup;

fn startup(bytes: &[u8], chunk: usize, grant: u64) -> (InputCursor, Budget, u64) {
    let pool = Budget::new(1 << 20).unwrap();
    let mut input = InputCursor::new(
        &pool,
        SourceId(1),
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: true,
        },
        32768,
    )
    .unwrap();
    let mut startup = Ts188Startup::new(&input).unwrap();
    let cancel = AtomicBool::new(false);
    let mut spent = 0;
    for _ in 0..10000 {
        let mut work = WorkBudget::new(grant);
        let step = startup.poll(&mut input, &mut work, &cancel).unwrap();
        spent += work.spent();
        match step {
            Step::NeedInput(request) => {
                let at = request.offset as usize;
                let end = bytes.len().min(at + chunk.min(request.maximum));
                input
                    .provide(
                        &InputWindow::new(
                            request.source,
                            request.token,
                            request.offset,
                            Bytes::copy_from_slice(&pool, &bytes[at..end]).unwrap(),
                        )
                        .unwrap(),
                    )
                    .unwrap();
            }
            Step::Yield => {}
            Step::Ready(offset) => {
                assert_eq!(input.position(), offset);
                assert!(
                    matches!(startup.poll(&mut input, &mut WorkBudget::new(0), &cancel).unwrap(), Step::Ready(same) if same == offset)
                );
                assert!((1504 + 188 + 6..=1504 + 7 * 188).contains(&spent));
                return (input, pool, offset);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    panic!("startup did not finish");
}

#[test]
fn phases_keep_original_coordinates_and_charged_windows() {
    for phase in [0, 1, 187] {
        let mut bytes = vec![0; phase + 9 * 188];
        for packet in bytes[phase..].chunks_exact_mut(188) {
            packet[..4].copy_from_slice(&[0x47, 0, 0, 0x10]);
        }
        for (chunk, grant) in [(32768, 4096), (1, 1), (187, 64)] {
            let (mut input, pool, offset) = startup(&bytes, chunk, grant);
            assert_eq!(offset, phase as u64);
            if input.buffered().is_empty() {
                let request = input
                    .pending_request(&AtomicBool::new(false))
                    .unwrap()
                    .unwrap();
                assert_eq!(request.offset, offset);
                let window = InputWindow::new(
                    request.source,
                    request.token,
                    offset,
                    Bytes::copy_from_slice(&pool, &bytes[phase..]).unwrap(),
                )
                .unwrap();
                input.provide(&window).unwrap();
            }
            assert_eq!(
                input.buffered(),
                &bytes[phase..phase + input.buffered().len()]
            );
            assert_eq!(bytes.len(), phase + 9 * 188);
        }
    }
}

#[test]
fn refuses_nonseekable_or_moved_cursor_and_cancellation_is_retryable() {
    let pool = Budget::new(8192).unwrap();
    let mut input = InputCursor::new(
        &pool,
        SourceId(1),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        188,
    )
    .unwrap();
    assert!(Ts188Startup::new(&input).is_err());
    input = InputCursor::new(
        &pool,
        SourceId(1),
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        188,
    )
    .unwrap();
    let mut startup = Ts188Startup::new(&input).unwrap();
    assert!(
        startup
            .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(true))
            .is_err()
    );
    assert!(matches!(
        startup
            .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        Step::NeedInput(_)
    ));
    input.request_range(1).unwrap();
    assert!(
        startup
            .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .is_err()
    );
}

fn trace(bytes: &[u8], phase: usize, opt_in: bool) -> Vec<String> {
    use triergon_mpegts::{PacketFormat, ParseLimits, TransportStream};
    let (mut input, pool) = if opt_in {
        let (input, pool, offset) = startup(bytes, 187, 64);
        assert_eq!(offset, phase as u64);
        (input, pool)
    } else {
        let pool = Budget::new(1 << 20).unwrap();
        let mut input = InputCursor::new(
            &pool,
            SourceId(1),
            SourceCapabilities {
                length: Some(bytes.len() as u64),
                random_access: true,
            },
            32768,
        )
        .unwrap();
        input.request_range(phase as u64).unwrap();
        (input, pool)
    };
    let mut stream =
        TransportStream::new(pool.clone(), PacketFormat::Ts188, ParseLimits::default(), 1).unwrap();
    let mut events = Vec::new();
    for _ in 0..10000 {
        match stream
            .poll(
                &mut input,
                &mut WorkBudget::new(64),
                &AtomicBool::new(false),
            )
            .unwrap()
        {
            Step::NeedInput(request) => {
                let at = request.offset as usize;
                let end = bytes.len().min(at + request.maximum.min(187));
                if at == end {
                    input.finish(request.source, request.token).unwrap();
                } else {
                    input
                        .provide(
                            &InputWindow::new(
                                request.source,
                                request.token,
                                request.offset,
                                Bytes::copy_from_slice(&pool, &bytes[at..end]).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                }
            }
            Step::Ready(event) => events.push(source_trace(event)),
            Step::Yield => {}
            Step::End(_) => return events,
            Step::Backpressure(never) => match never {},
        }
    }
    panic!("transport did not finish");
}

#[test]
fn complete_transport_traces_match_explicit_phase_traversal() {
    // Established PAT/PMT/HEVC carriers, repeated to fill the startup window.
    // Repeated counters deliberately retain continuity diagnostics too.
    let fixture = include_bytes!("data/carriage-hevc-direct.ts");
    for phase in [0, 1] {
        let mut bytes = vec![0; phase];
        for _ in 0..3 {
            bytes.extend_from_slice(fixture);
        }
        bytes.push(0x47); // preserve the partial-tail diagnostic
        let old = trace(&bytes, phase, false);
        let new = trace(&bytes, phase, true);
        assert!(!old.is_empty());
        println!("phase {phase}: {} source trace records equal", old.len());
        assert_eq!(old, new);
    }
}

// Record source bytes and coordinates, excluding allocation-owner Debug state.
fn source_trace(event: triergon_mpegts::TraceEvent) -> String {
    use triergon_mpegts::TraceEvent;
    match event {
        TraceEvent::Transport(packet) => format!(
            "transport {:?} {} {:?}",
            packet.source_id,
            packet.source_offset,
            packet.source.as_ref()
        ),
        TraceEvent::Section(section) => format!(
            "section {} {:?} {:?} {:?}",
            section.pid(),
            section.bytes().as_ref(),
            section.source_ranges(),
            section.crc_remainder()
        ),
        TraceEvent::Pat(table) => format!(
            "pat {:?} {:?} {:?} {:?}",
            table.header,
            table.programs.as_slice(),
            table
                .source_sections
                .as_slice()
                .iter()
                .map(Bytes::as_ref)
                .collect::<Vec<_>>(),
            table.source_ranges.as_slice()
        ),
        TraceEvent::ProgramMap {
            map,
            active,
            configuration_epoch,
        } => format!(
            "pmt {:?} {:?} {:?} {:?} {} {}",
            map.header,
            map.source.as_ref(),
            map.source_ranges.as_slice(),
            map.descriptor_namespace,
            active,
            configuration_epoch
        ),
        TraceEvent::Pes(pes) => format!(
            "pes {} {:?} {:?} {:?} {:?} {:?}",
            pes.pid,
            pes.header,
            pes.epochs,
            pes.header_source_ranges.as_slice(),
            pes.source_ranges().collect::<Vec<_>>(),
            pes.payload().collect::<Vec<_>>()
        ),
        TraceEvent::Diagnostic(fault) => {
            assert!(fault.rejected.is_none());
            format!(
                "fault {:?} {:?} {:?} {:?}",
                fault.kind, fault.pid, fault.location, fault.error
            )
        }
        event @ (TraceEvent::Continuity { .. }
        | TraceEvent::Timeline { .. }
        | TraceEvent::Discontinuity { .. }
        | TraceEvent::Finished) => format!("{event:?}"),
        TraceEvent::UnboundedAdtsPes(_) => panic!("unexpected unbounded ADTS"),
    }
}

#[test]
fn rejects_short_absent_and_competing_grids_without_repositioning() {
    use triergon_core::Error;
    let absent = vec![0; 1504];
    let mut competing = absent.clone();
    for phase in [0, 12] {
        for carrier in competing[phase..].chunks_exact_mut(188) {
            carrier[..4].copy_from_slice(&[0x47, 0, 0, 0x10]);
        }
    }
    for (bytes, expected) in [
        (&absent[..100], Error::Truncated),
        (
            &absent[..],
            Error::Unsupported("TS188 startup without a sync grid"),
        ),
        (
            &competing[..],
            Error::Unsupported("TS188 startup with competing sync grids"),
        ),
    ] {
        let pool = Budget::new(8192).unwrap();
        let mut input = InputCursor::new(
            &pool,
            SourceId(1),
            SourceCapabilities {
                length: Some(bytes.len() as u64),
                random_access: true,
            },
            2048,
        )
        .unwrap();
        let mut startup = Ts188Startup::new(&input).unwrap();
        let request = input
            .pending_request(&AtomicBool::new(false))
            .unwrap()
            .unwrap();
        input
            .provide(
                &InputWindow::new(
                    request.source,
                    request.token.clone(),
                    0,
                    Bytes::copy_from_slice(&pool, bytes).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        let mut result = startup.poll(
            &mut input,
            &mut WorkBudget::new(10000),
            &AtomicBool::new(false),
        );
        if let Ok(Step::NeedInput(request)) = result {
            assert_eq!(request.offset, bytes.len() as u64);
            input.finish(request.source, request.token).unwrap();
            result = startup.poll(
                &mut input,
                &mut WorkBudget::new(10000),
                &AtomicBool::new(false),
            );
        }
        assert_eq!(result.unwrap_err(), expected);
        assert_eq!(input.position(), bytes.len() as u64);
        assert_eq!(input.token(), request.token);
    }
}
