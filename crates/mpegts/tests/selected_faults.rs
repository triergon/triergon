//! Original primary/source and fresh d32 public-API fault regressions.
//! Source acceptance, raw33 and unknowns never come from native observations.
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, SourceLocation,
    Step, WorkBudget,
};
use triergon_mpegts::clock::{NativeClockProfile, NativePesClockV1};
use triergon_mpegts::native_pes::{NativePesScope, NativeRegisteredCarriage, NativeSourceContext};
use triergon_mpegts::native_recovery::{
    NativeDiagnosticPesPacket, NativePesObservation, NativePesRecoveryObserver,
    NativeRejectedPolicy, NativeSelectedFault,
};
use triergon_mpegts::{
    AssemblyStep, PacketFormat, ParseLimits, SourceEpochs, TraceEvent, TraceFault,
    TransportInspection, TransportInspector, TransportStream,
};

const EPOCHS: SourceEpochs = SourceEpochs {
    clock: 0,
    configuration: 2,
};
const ORIGINAL: &[u8] = b"lane-ts-original-payload\0\x47\xff";
const AUTHORED: &[u8] = b"fault-controls\0\x47\xff";
fn bytes(id: &str) -> &'static [u8] {
    match id {
        "pcr-original" => include_bytes!("data/pcr-extension-invalid.ts"),
        "tei-original" => include_bytes!("data/transport-tei.ts"),
        "scrambled-original" => include_bytes!("data/transport-scrambled.ts"),
        "marker-original" => include_bytes!("data/pes-timestamp-marker.ts"),
        "forbidden-original" => include_bytes!("data/pes-forbidden-time-flags.ts"),
        "timeline-original" => include_bytes!("data/pcr-timeline-discontinuity.ts"),
        "marker0" => include_bytes!("data/selected-faults-v1/marker-0.ts"),
        "marker2" => include_bytes!("data/selected-faults-v1/marker-2.ts"),
        "marker4" => include_bytes!("data/selected-faults-v1/marker-4.ts"),
        "markerall" => include_bytes!("data/selected-faults-v1/marker-all.ts"),
        "pcr300" => include_bytes!("data/selected-faults-v1/pcr-extension-300.ts"),
        "pcr511" => include_bytes!("data/selected-faults-v1/pcr-extension-511.ts"),
        "pcrreserved" => include_bytes!("data/selected-faults-v1/pcr-reserved-zero.ts"),
        "pcrsecond" => include_bytes!("data/selected-faults-v1/pcr-and-stuffing.ts"),
        "scramble1" => include_bytes!("data/selected-faults-v1/scrambling-1.ts"),
        "scramble3" => include_bytes!("data/selected-faults-v1/scrambling-3.ts"),
        "forbiddenzero" => include_bytes!("data/selected-faults-v1/forbidden-zero.ts"),
        "teisecond" => include_bytes!("data/selected-faults-v1/tei-and-marker.ts"),
        "teimultiple" => include_bytes!("data/selected-faults-v1/tei-and-scrambling.ts"),
        "incomplete" => include_bytes!("data/selected-faults-v1/tei-incomplete-finite.ts"),
        "wrongprefix" => include_bytes!("data/selected-faults-v1/wrong-time-prefix.ts"),
        "tei-valid" => include_bytes!("data/selected-faults-v1/tei-then-valid.ts"),
        "timeline-three" => include_bytes!("data/selected-faults-v1/pcr-discontinuity-three.ts"),
        _ => panic!("unknown authored input"),
    }
}
fn scope() -> NativePesScope {
    NativePesScope {
        program: 1,
        pid: 256,
        stream_type: 0x1b,
        stream_id: 0xe0,
        entity: SourceId(1),
        epochs: EPOCHS,
    }
}
fn context(n: u64) -> NativeSourceContext {
    NativeSourceContext {
        source_event_ordinal: n,
        epochs: EPOCHS,
        byte_run_epoch: None,
    }
}
fn observer(b: &Budget) -> NativePesRecoveryObserver {
    NativePesRecoveryObserver::new_selected_avc_faults_v1(
        NativeClockProfile::ArchivedPesV1,
        scope(),
        b.clone(),
        ParseLimits::default(),
    )
    .expect("selected AVC source-diagnostic operation not implemented: intended runtime RED")
}
fn collect(data: &[u8], b: &Budget) -> Vec<(TraceEvent, SourceEpochs)> {
    let owner = Bytes::copy_from_slice(b, data).unwrap();
    let mut input = InputCursor::new(
        b,
        SourceId(1),
        SourceCapabilities {
            length: Some(data.len() as u64),
            random_access: false,
        },
        4096,
    )
    .unwrap();
    let mut stream =
        TransportStream::new(b.clone(), PacketFormat::Ts188, ParseLimits::default(), 1).unwrap();
    let mut result = Vec::new();
    for _ in 0..100_000 {
        match stream
            .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(event) => {
                let end = matches!(event, TraceEvent::Finished);
                result.push((event, stream.source_epochs()));
                if end {
                    return result;
                }
            }
            Step::Yield => {}
            Step::NeedInput(r) => {
                let start = r.offset as usize;
                if start == data.len() {
                    input.finish(r.source, r.token).unwrap();
                } else {
                    let end = (start + r.maximum).min(data.len());
                    input
                        .provide(
                            &InputWindow::new(
                                r.source,
                                r.token,
                                r.offset,
                                owner.slice(start..end).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                }
            }
            Step::End(_) => stream.end_of_source().unwrap(),
        }
    }
    panic!("bounded source failed to finish");
}
fn feed(o: &mut NativePesRecoveryObserver, event: &TraceEvent, c: NativeSourceContext) {
    match event {
        TraceEvent::Transport(p) if p.pid == 256 => o.push_accepted(p.clone(), c).unwrap(),
        TraceEvent::Diagnostic(f) if f.rejected.is_some() => {
            o.push_diagnostic_rejection_v1(f.clone(), c).unwrap()
        }
        TraceEvent::Diagnostic(f)
            if f.pid == Some(256)
                && matches!(
                    f.error,
                    Error::Invalid(
                        "TS transport error indicator"
                            | "PES timestamp markers"
                            | "forbidden PES time flags"
                    ) | Error::Unsupported("scrambled TS payload")
                ) =>
        {
            o.confirm_source_fault_v1(f, c).unwrap()
        }
        TraceEvent::Timeline { .. } => o.commit_source_timeline_v1(c).unwrap(),
        _ => {}
    }
}
fn drain(o: &mut NativePesRecoveryObserver) -> Vec<NativePesObservation> {
    let mut results = Vec::new();
    for _ in 0..2048 {
        let mut w = WorkBudget::new(1);
        let step = o.poll(&mut w, &AtomicBool::new(false)).unwrap();
        assert!(w.spent() <= 1);
        match step {
            AssemblyStep::Yield => {}
            AssemblyStep::Output(p) => results.push(p),
            AssemblyStep::NeedPayload => return results,
            other => panic!("unexpected {other:?}"),
        }
    }
    panic!("bounded native assembly failed to progress");
}
fn diagnostic(id: &str, expected: NativeSelectedFault, raw: Option<u64>) {
    let b = Budget::new(1 << 20).unwrap();
    let mut o = observer(&b);
    let events = collect(bytes(id), &b);
    let mut outputs = Vec::new();
    for (n, (event, epochs)) in events.iter().enumerate() {
        feed(
            &mut o,
            event,
            NativeSourceContext {
                source_event_ordinal: n as u64,
                epochs: *epochs,
                byte_run_epoch: None,
            },
        );
        outputs.extend(drain(&mut o));
    }
    assert_eq!(outputs.len(), 1);
    let NativePesObservation::FromSelectedDiagnostic(p) = outputs.pop().unwrap() else {
        panic!("source rejection needs distinct neutral header observation")
    };
    assert_eq!(p.selected_fault, expected);
    assert_eq!(p.source_fault.error, expected.source_error());
    assert_eq!(p.source_fault.kind, expected.source_layer());
    assert_eq!(
        p.source_origin,
        SourceLocation {
            source: SourceId(1),
            offset: 376
        }
    );
    assert_eq!(p.source_epochs, EPOCHS);
    assert_eq!(p.program, 1);
    assert_eq!(p.pid, 256);
    assert_eq!(p.native_stream_id, 0xe0);
    assert_eq!(p.native_position, 376);
    assert_eq!(p.native_flags, 0);
    assert_eq!(p.native_duration90_raw, 0);
    let wanted = if id.ends_with("original") {
        ORIGINAL
    } else {
        AUTHORED
    };
    assert_eq!(p.payload().flatten().copied().collect::<Vec<_>>(), wanted);
    assert_eq!(p.payload_len, wanted.len());
    assert_eq!(p.encoded_header.encoded_pts_raw33(), raw);
    assert_eq!(
        p.encoded_header.time_flags(),
        if expected == NativeSelectedFault::ForbiddenTimeFlags {
            1
        } else {
            2
        }
    );
    assert_eq!(
        p.encoded_header.markers_valid(),
        expected != NativeSelectedFault::TimestampMarkers && raw.is_some()
    );
    let input_n = 8;
    assert_eq!(p.input_context, context(input_n));
    assert_eq!(
        p.diagnostic_context,
        context(if expected == NativeSelectedFault::PcrEncoding {
            8
        } else {
            10
        })
    );
    let carrier = p.carrier.backing_bytes();
    assert_eq!(carrier.as_ref(), &bytes(id)[376..564]);
    assert!(
        p.encoded_header
            .backing_bytes()
            .shares_storage_with(carrier)
    );
    assert_eq!(p.spans.as_slice().len(), 1);
    let span = &p.spans.as_slice()[0];
    assert!(span.backing_bytes().shares_storage_with(carrier));
    assert_eq!(span.context, p.input_context);
    assert_eq!(span.transport.offset, 376);
    assert_eq!(span.transport.bytes, 188);
    assert_eq!(
        span.source.offset,
        p.header_source_ranges.as_slice()[0].offset + p.encoded_header.as_bytes().len() as u64
    );
    let mut clock = NativePesClockV1::new(1).unwrap();
    assert!(
        clock
            .project_diagnostic_header_v1(
                256,
                &p.encoded_header,
                &mut WorkBudget::new(0),
                &AtomicBool::new(false)
            )
            .unwrap()
            .is_none()
    );
    assert_eq!(
        clock.project_diagnostic_header_v1(
            256,
            &p.encoded_header,
            &mut WorkBudget::new(1),
            &AtomicBool::new(true)
        ),
        Err(Error::Cancelled)
    );
    let t = clock
        .project_diagnostic_header_v1(
            256,
            &p.encoded_header,
            &mut WorkBudget::new(1),
            &AtomicBool::new(false),
        )
        .unwrap()
        .unwrap();
    assert_eq!(t.encoded_pts_raw33, raw);
    assert_eq!(t.encoded_dts_raw33, None);
    assert_eq!(t.pts90, raw.map(i128::from));
    assert_eq!(t.dts90, t.pts90);
    assert_eq!(
        events
            .iter()
            .filter(|(e, _)| matches!(e, TraceEvent::Pes(_)))
            .count(),
        0,
        "no recovered source PES or known source time"
    );
}
macro_rules! diag {
    ($name:ident,$id:literal,$kind:ident,$raw:expr) => {
        #[test]
        fn $name() {
            diagnostic($id, NativeSelectedFault::$kind, $raw);
        }
    };
}
diag!(
    native_original_invalid_pcr,
    "pcr-original",
    PcrEncoding,
    Some(90000)
);
diag!(
    native_original_tei,
    "tei-original",
    TransportError,
    Some(90000)
);
diag!(
    native_original_scrambled,
    "scrambled-original",
    Scrambling,
    Some(90000)
);
diag!(
    native_original_marker,
    "marker-original",
    TimestampMarkers,
    Some(90000)
);
diag!(
    native_original_forbidden_unknown,
    "forbidden-original",
    ForbiddenTimeFlags,
    None
);
diag!(
    native_pcr_extension_300,
    "pcr300",
    PcrEncoding,
    Some(180000)
);
diag!(
    native_pcr_extension_511,
    "pcr511",
    PcrEncoding,
    Some(180000)
);
diag!(
    native_pcr_reserved_bits,
    "pcrreserved",
    PcrEncoding,
    Some(180000)
);
diag!(
    native_marker_first,
    "marker0",
    TimestampMarkers,
    Some(180000)
);
diag!(
    native_marker_middle,
    "marker2",
    TimestampMarkers,
    Some(180000)
);
diag!(
    native_marker_last,
    "marker4",
    TimestampMarkers,
    Some(180000)
);
diag!(
    native_marker_all,
    "markerall",
    TimestampMarkers,
    Some(180000)
);
diag!(
    native_scrambling_reserved_stays_opaque,
    "scramble1",
    Scrambling,
    Some(180000)
);
diag!(
    native_scrambling_three_stays_opaque,
    "scramble3",
    Scrambling,
    Some(180000)
);
diag!(
    native_forbidden_zero_header_unknown,
    "forbiddenzero",
    ForbiddenTimeFlags,
    None
);

fn timeline(id: &str, times: &[u64]) {
    let b = Budget::new(1 << 20).unwrap();
    let mut o = observer(&b);
    let source = collect(bytes(id), &b);
    let mut got = Vec::new();
    for (n, (event, epochs)) in source.iter().enumerate() {
        feed(
            &mut o,
            event,
            NativeSourceContext {
                source_event_ordinal: n as u64,
                epochs: *epochs,
                byte_run_epoch: None,
            },
        );
        got.extend(drain(&mut o));
    }
    assert_eq!(got.len(), times.len());
    for (p, t) in got.iter().zip(times) {
        let NativePesObservation::Accepted(p) = p else {
            panic!("valid boundary must preserve accepted header")
        };
        assert_eq!(p.source_header.pts_raw33, Some(*t));
        assert_eq!(p.source_header.dts_raw33, None);
        assert_eq!(p.native_flags, 0);
    }
    assert!(
        source
            .iter()
            .any(|(e, _)| matches!(e, TraceEvent::Timeline { clock_epoch: 1, .. }))
    );
    assert_eq!(o.scope().epochs.clock, 1);
    let last = got.last().unwrap();
    let NativePesObservation::Accepted(last) = last else {
        unreachable!()
    };
    assert_eq!(
        last.source_epochs.clock,
        if times.len() == 3 { 1 } else { 0 },
        "real transport precedes source Timeline; do not relabel its epoch"
    );
}
#[test]
fn native_original_pcr_timeline() {
    timeline("timeline-original", &[90000, 10]);
}
#[test]
fn native_following_clock_epoch_uses_real_source_context() {
    timeline("timeline-three", &[90000, 10, 90010]);
}

fn staged(b: &Budget) -> (NativePesRecoveryObserver, TraceFault) {
    let mut o = observer(b);
    let events = collect(bytes("tei-original"), b);
    let TraceEvent::Transport(p) = &events[8].0 else {
        panic!("real transport8")
    };
    o.push_accepted(p.clone(), context(8)).unwrap();
    let TraceEvent::Diagnostic(f) = &events[10].0 else {
        panic!("real diagnostic10")
    };
    (o, f.clone())
}
fn one(o: &mut NativePesRecoveryObserver) -> NativeDiagnosticPesPacket {
    let mut p = drain(o);
    assert_eq!(p.len(), 1);
    let NativePesObservation::FromSelectedDiagnostic(p) = p.pop().unwrap() else {
        panic!("distinct rejected source operation")
    };
    p
}
#[test]
fn staged_work_cancel_and_actual_diagnostic() {
    let b = Budget::new(1 << 20).unwrap();
    let (mut o, f) = staged(&b);
    let used = b.used();
    let mut w = WorkBudget::new(0);
    assert!(matches!(
        o.poll(&mut w, &AtomicBool::new(false)),
        Ok(AssemblyStep::Yield)
    ));
    assert_eq!(w.spent(), 0);
    assert_eq!(b.used(), used);
    assert!(matches!(
        o.poll(&mut WorkBudget::new(1), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(
        drain(&mut o).is_empty(),
        "no native packet before the actual strict diagnostic"
    );
    o.confirm_source_fault_v1(&f, context(10)).unwrap();
    let p = one(&mut o);
    assert_eq!(p.source_fault.error, f.error);
}
#[test]
fn pressure_retry_keeps_original_owner_and_cause() {
    let b = Budget::new(1 << 20).unwrap();
    let (mut o, f) = staged(&b);
    o.confirm_source_fault_v1(&f, context(10)).unwrap();
    let blocker = b.reserve(b.available()).unwrap();
    let mut pressured = false;
    for _ in 0..32 {
        match o.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(AssemblyStep::Yield) => {}
            Err(Error::Backpressure { .. }) => {
                pressured = true;
                break;
            }
            other => panic!("pressure may not emit/drop {other:?}"),
        }
    }
    assert!(pressured);
    drop(blocker);
    let p = one(&mut o);
    assert_eq!(p.diagnostic_context, context(10));
    assert_eq!(p.payload().flatten().copied().collect::<Vec<_>>(), ORIGINAL);
}
#[test]
fn hidden_owner_deduplicates_and_output_outlives_parser() {
    let b = Budget::new(1 << 20).unwrap();
    let initial = b.used();
    let mut data = Vec::with_capacity(4096);
    data.extend_from_slice(bytes("tei-original"));
    let owner = Bytes::try_from_vec(&b, data).unwrap();
    let retained = owner.retained_storage_bytes();
    let mut inspector = TransportInspector::new(
        owner.slice(376..564).unwrap(),
        PacketFormat::Ts188,
        SourceLocation {
            source: SourceId(1),
            offset: 376,
        },
        &b,
    )
    .unwrap();
    let TransportInspection::Accepted(p) = (match inspector
        .poll(&mut WorkBudget::new(4096), &AtomicBool::new(false))
        .unwrap()
    {
        AssemblyStep::Output(p) => p,
        other => panic!("unexpected {other:?}"),
    }) else {
        panic!("TEI is not a syntax acceptance waiver")
    };
    let mut o = observer(&b);
    o.push_accepted(p, context(8)).unwrap();
    assert_eq!(b.used(), initial + retained);
    let f = TraceFault {
        kind: triergon_mpegts::FaultKind::Transport,
        pid: Some(256),
        location: SourceLocation {
            source: SourceId(1),
            offset: 376,
        },
        error: Error::Invalid("TS transport error indicator"),
        rejected: None,
    };
    o.confirm_source_fault_v1(&f, context(10)).unwrap();
    let p = one(&mut o);
    assert!(p.carrier.backing_bytes().shares_storage_with(&owner));
    assert!(
        p.spans.as_slice()[0]
            .backing_bytes()
            .shares_storage_with(&owner)
    );
    drop(o);
    drop(inspector);
    drop(owner);
    assert!(b.used() >= initial + retained);
    drop(p);
    assert_eq!(b.used(), initial);
}
#[test]
fn stale_diagnostic_retry_does_not_replace_mailbox() {
    let b = Budget::new(1 << 20).unwrap();
    let (mut o, f) = staged(&b);
    let mut wrong = f.clone();
    wrong.location.offset += 188;
    assert!(o.confirm_source_fault_v1(&wrong, context(10)).is_err());
    let mut c = context(10);
    c.epochs.configuration += 1;
    assert!(o.confirm_source_fault_v1(&f, c).is_err());
    assert!(o.confirm_source_fault_v1(&f, context(8)).is_err());
    o.confirm_source_fault_v1(&f, context(10)).unwrap();
    assert_eq!(one(&mut o).diagnostic_context, context(10));
}
#[test]
fn foreign_stale_input_and_second_mailbox_are_recoverable() {
    let b = Budget::new(1 << 20).unwrap();
    let f = Budget::new(1 << 20).unwrap();
    let (mut o, _) = staged(&b);
    let source = collect(bytes("tei-original"), &f);
    let TraceEvent::Transport(p) = &source[8].0 else {
        panic!()
    };
    let e = o.push_accepted(p.clone(), context(11)).unwrap_err();
    assert!(e.value.source.shares_storage_with(&p.source));
    assert!(o.commit_source_timeline_v1(context(13)).is_err());
}
fn refuse(id: &str) {
    let b = Budget::new(1 << 20).unwrap();
    let mut o = observer(&b);
    let source = collect(bytes(id), &b);
    let mut refused = false;
    for (n, (e, epochs)) in source.iter().enumerate() {
        let c = NativeSourceContext {
            source_event_ordinal: n as u64,
            epochs: *epochs,
            byte_run_epoch: None,
        };
        let r = match e {
            TraceEvent::Transport(p) if p.pid == 256 => {
                o.push_accepted(p.clone(), c).map_err(|e| e.error)
            }
            TraceEvent::Diagnostic(f) if f.rejected.is_some() => o
                .push_diagnostic_rejection_v1(f.clone(), c)
                .map_err(|e| e.error),
            TraceEvent::Diagnostic(f) if f.pid == Some(256) => o.confirm_source_fault_v1(f, c),
            _ => Ok(()),
        };
        if r.is_err() {
            refused = true;
            break;
        }
        for _ in 0..1024 {
            match o.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
                Ok(AssemblyStep::Yield) => {}
                Ok(AssemblyStep::NeedPayload) => break,
                Err(_) => {
                    refused = true;
                    break;
                }
                other => panic!("unproved fault must not produce output {other:?}"),
            }
        }
        if refused {
            break;
        }
    }
    assert!(refused, "no catch-all recovery");
}
#[test]
fn multiple_transport_faults_remain_refused() {
    refuse("teimultiple");
}
#[test]
fn tei_does_not_hide_second_header_fault() {
    refuse("teisecond");
}
#[test]
fn incomplete_finite_native_corrupt2_remains_unsupported() {
    refuse("incomplete");
}
#[test]
fn wrong_time_prefix_remains_unproven() {
    refuse("wrongprefix");
}

#[test]
fn inspector_checks_remaining_fields_after_invalid_pcr() {
    let b = Budget::new(8192).unwrap();
    let data = bytes("pcr300");
    let mut i = TransportInspector::new(
        Bytes::copy_from_slice(&b, &data[376..564]).unwrap(),
        PacketFormat::Ts188,
        SourceLocation {
            source: SourceId(1),
            offset: 376,
        },
        &b,
    )
    .unwrap();
    let AssemblyStep::Output(TransportInspection::Rejected(r)) = i
        .poll(&mut WorkBudget::new(4096), &AtomicBool::new(false))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(r.error(), Error::Invalid("PCR reserved bits/extension"));
    assert!(r.fully_checked());
    assert_eq!(r.secondary_error(), None);
}
#[test]
fn inspector_invalid_pcr_preserves_second_stuffing_fault() {
    let b = Budget::new(8192).unwrap();
    let data = bytes("pcrsecond");
    let mut i = TransportInspector::new(
        Bytes::copy_from_slice(&b, &data[376..564]).unwrap(),
        PacketFormat::Ts188,
        SourceLocation {
            source: SourceId(1),
            offset: 376,
        },
        &b,
    )
    .unwrap();
    let AssemblyStep::Output(TransportInspection::Rejected(r)) = i
        .poll(&mut WorkBudget::new(4096), &AtomicBool::new(false))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(r.error(), Error::Invalid("PCR reserved bits/extension"));
    assert!(!r.fully_checked());
    assert_eq!(
        r.secondary_error(),
        Some(Error::Invalid("adaptation stuffing"))
    );
}

fn strict(id: &str, err: Error) {
    let b = Budget::new(1 << 20).unwrap();
    let events = collect(bytes(id), &b);
    let faults: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(n, (e, _))| {
            if let TraceEvent::Diagnostic(f) = e {
                Some((n, f))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(faults.len(), 1);
    assert_eq!(faults[0].1.error, err);
    assert_eq!(faults[0].0, if id == "pcr-original" { 8 } else { 10 });
    assert!(
        !events
            .iter()
            .any(|(e, _)| matches!(e, TraceEvent::Pes(_) | TraceEvent::Timeline { .. }))
    );
    assert_eq!(events.last().unwrap().1, EPOCHS);
}
#[test]
fn strict_invalid_pcr_is_not_a_clock() {
    strict(
        "pcr-original",
        Error::Invalid("PCR reserved bits/extension"),
    );
}
#[test]
fn strict_tei_stays_uncorrected() {
    strict(
        "tei-original",
        Error::Invalid("TS transport error indicator"),
    );
}
#[test]
fn strict_scrambled_stays_opaque() {
    strict(
        "scrambled-original",
        Error::Unsupported("scrambled TS payload"),
    );
}
#[test]
fn strict_marker_has_no_source_pts() {
    strict("marker-original", Error::Invalid("PES timestamp markers"));
}
#[test]
fn strict_forbidden_times_stay_unknown() {
    strict(
        "forbidden-original",
        Error::Invalid("forbidden PES time flags"),
    );
}
#[test]
fn strict_valid_timeline_keeps_raw_and_dts_unknown() {
    let b = Budget::new(1 << 20).unwrap();
    let e = collect(bytes("timeline-original"), &b);
    let p: Vec<_> = e
        .iter()
        .filter_map(|(e, _)| {
            if let TraceEvent::Pes(p) = e {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(p.len(), 2);
    assert_eq!(p[0].header.pts_raw33, Some(90000));
    assert_eq!(p[1].header.pts_raw33, Some(10));
    assert!(p.iter().all(|p| p.header.dts_raw33.is_none()));
    assert_eq!(p[1].epochs.clock, 1);
}
#[test]
fn legacy_observer_still_refuses_damaged_transport() {
    let b = Budget::new(1 << 20).unwrap();
    let e = collect(bytes("tei-original"), &b);
    let TraceEvent::Transport(p) = &e[8].0 else {
        panic!()
    };
    let mut o = NativePesRecoveryObserver::new(
        NativeClockProfile::ArchivedPesV1,
        scope(),
        NativeRegisteredCarriage::Avc,
        NativeRejectedPolicy::OpcrWithoutPcrV1,
        b,
        ParseLimits::default(),
    )
    .unwrap();
    assert!(o.push_accepted(p.clone(), context(8)).is_err());
}
#[test]
fn old_hypothesis_and_actual_corrupt_flag_have_separate_status() {
    let data = bytes("incomplete");
    let start = 376 + 5 + usize::from(data[380]);
    let declared = u16::from_be_bytes([data[start + 4], data[start + 5]]) as usize + 6;
    assert_eq!(declared, data.len() - start + 1);
    assert_eq!(data[377] & 128, 128);
    assert_eq!(data.len(), 564); /* native observed flags2 is not a source length repair */
}
