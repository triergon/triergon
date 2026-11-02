//! Owned strict rejection/native recovery guards. No native worker is invoked.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, SourceLocation,
    Step, WorkBudget,
};
use triergon_mpegts::clock::{NativeClockProfile, NativePesClockV1};
use triergon_mpegts::native_pes::{NativePesScope, NativeRegisteredCarriage, NativeSourceContext};
use triergon_mpegts::native_recovery::{
    NativePesObservation, NativePesRecoveryObserver, NativeRecoveredPesPacket, NativeRejectedPolicy,
};
use triergon_mpegts::{
    AssemblyStep, PacketFormat, ParseLimits, RejectedTransport, SourceEpochs, TraceEvent,
    TransportInspection, TransportInspector, TransportStream,
};

const INPUT: &[u8] = include_bytes!("data/native-pes-v1/opcr-without-pcr-invalid.ts");
const EPOCHS: SourceEpochs = SourceEpochs {
    clock: 0,
    configuration: 2,
};
fn context(ordinal: u64) -> NativeSourceContext {
    NativeSourceContext {
        source_event_ordinal: ordinal,
        epochs: EPOCHS,
        byte_run_epoch: None,
    }
}
fn observer(budget: &Budget) -> NativePesRecoveryObserver {
    NativePesRecoveryObserver::new(
        NativeClockProfile::ArchivedPesV1,
        NativePesScope {
            program: 1,
            pid: 256,
            stream_type: 0x1b,
            stream_id: 0xe0,
            entity: SourceId(1),
            epochs: EPOCHS,
        },
        NativeRegisteredCarriage::Avc,
        NativeRejectedPolicy::OpcrWithoutPcrV1,
        budget.clone(),
        ParseLimits::default(),
    )
    .unwrap()
}
fn inspect(bytes: Bytes, budget: &Budget, source: SourceId) -> RejectedTransport {
    let mut inspector = TransportInspector::new(
        bytes,
        PacketFormat::Ts188,
        SourceLocation {
            source,
            offset: 376,
        },
        budget,
    )
    .unwrap();
    for _ in 0..1024 {
        let mut work = WorkBudget::new(1);
        let step = inspector.poll(&mut work, &AtomicBool::new(false)).unwrap();
        assert_eq!(work.spent(), 1);
        match step {
            AssemblyStep::Yield => {}
            AssemblyStep::Output(TransportInspection::Rejected(rejection)) => return rejection,
            other => panic!("strict real rejection required, got {other:?}"),
        }
    }
    panic!("incremental carrier inspection did not progress");
}
fn original(budget: &Budget) -> RejectedTransport {
    inspect(
        Bytes::copy_from_slice(budget, &INPUT[376..564]).unwrap(),
        budget,
        SourceId(1),
    )
}
fn recovered(observer: &mut NativePesRecoveryObserver) -> NativeRecoveredPesPacket {
    let mut result = None;
    for _ in 0..1024 {
        match observer
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            AssemblyStep::Yield => {}
            AssemblyStep::Output(NativePesObservation::FromRejectedCarrier(packet)) => {
                assert!(result.replace(packet).is_none())
            }
            AssemblyStep::NeedPayload => {
                return result.expect("real rejected carrier must produce its finite packet");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    panic!("rejected native work did not progress");
}

#[test]
fn inspector_zero_work_cancel_and_one_unit_resume_keep_original_owner() {
    let budget = Budget::new(16384).unwrap();
    let owner = Bytes::copy_from_slice(&budget, INPUT).unwrap();
    let mut inspector = TransportInspector::new(
        owner.slice(376..564).unwrap(),
        PacketFormat::Ts188,
        SourceLocation {
            source: SourceId(1),
            offset: 376,
        },
        &budget,
    )
    .unwrap();
    let before = budget.used();
    let mut zero = WorkBudget::new(0);
    assert!(matches!(
        inspector.poll(&mut zero, &AtomicBool::new(false)),
        Ok(AssemblyStep::Yield)
    ));
    assert_eq!(zero.spent(), 0);
    assert_eq!(budget.used(), before);
    assert!(matches!(
        inspector.poll(&mut WorkBudget::new(1), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(
        inspector
            .backing_bytes()
            .unwrap()
            .shares_storage_with(&owner)
    );
    let mut fields = 0;
    loop {
        let mut work = WorkBudget::new(1);
        let step = inspector.poll(&mut work, &AtomicBool::new(false)).unwrap();
        fields += 1;
        assert_eq!(work.spent(), 1);
        match step {
            AssemblyStep::Yield => {}
            AssemblyStep::Output(TransportInspection::Rejected(rejected)) => {
                assert_eq!(rejected.error(), Error::Invalid("OPCR without PCR"));
                assert!(rejected.fully_checked());
                assert_eq!(rejected.secondary_error(), None);
                assert_eq!(rejected.source_range().unwrap().offset, 376);
                assert!(rejected.backing_bytes().shares_storage_with(&owner));
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(fields < 1024);
    }
    assert!(
        fields > 10,
        "remaining real fields/stuffing must be checked after the first semantic error"
    );
    assert!(matches!(
        inspector.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(AssemblyStep::Finished)
    ));
}

#[test]
fn rejection_and_native_outputs_keep_full_hidden_owner_until_last_alias_drops() {
    let budget = Budget::new(32768).unwrap();
    let initial = budget.used();
    let mut data = Vec::with_capacity(4096);
    data.extend_from_slice(INPUT);
    let owner = Bytes::try_from_vec(&budget, data).unwrap();
    let retained = owner.retained_storage_bytes();
    let rejection = inspect(owner.slice(376..564).unwrap(), &budget, SourceId(1));
    assert_eq!(
        rejection.backing_bytes().retained_capacity(),
        owner.retained_capacity()
    );
    assert_eq!(
        budget.used(),
        initial + retained,
        "aliases must not charge the same storage twice"
    );
    let alias = rejection.clone();
    let mut parser = observer(&budget);
    parser.push_rejected(rejection, context(8)).unwrap();
    let packet = recovered(&mut parser);
    assert!(packet.rejection.backing_bytes().shares_storage_with(&owner));
    assert!(
        packet
            .encoded_header
            .backing_bytes()
            .shares_storage_with(&owner)
    );
    assert!(
        packet.spans.as_slice()[0]
            .backing_bytes()
            .shares_storage_with(&owner)
    );
    drop(owner);
    drop(alias);
    drop(parser);
    assert!(budget.used() >= initial + retained);
    assert!(
        packet
            .payload()
            .flatten()
            .copied()
            .eq(INPUT[537..564].iter().copied())
    );
    drop(packet);
    assert_eq!(budget.used(), initial);
}

#[test]
fn exact_primary_error_does_not_mask_bad_opcr_encoding() {
    let budget = Budget::new(16384).unwrap();
    let mut raw = INPUT[376..564].to_vec();
    raw[10] &= !0x7e;
    let rejection = inspect(
        Bytes::copy_from_slice(&budget, &raw).unwrap(),
        &budget,
        SourceId(1),
    );
    assert_eq!(rejection.error(), Error::Invalid("OPCR without PCR"));
    assert_eq!(
        rejection.secondary_error(),
        Some(Error::Invalid("PCR reserved bits/extension"))
    );
    let mut parser = observer(&budget);
    let error = parser.push_rejected(rejection, context(8)).unwrap_err();
    assert!(matches!(error.error, Error::Unsupported(_)));
    assert_eq!(error.value.bytes().as_ref(), raw);
}

#[test]
fn exact_primary_error_does_not_mask_bad_stuffing() {
    let budget = Budget::new(16384).unwrap();
    let mut raw = INPUT[376..564].to_vec();
    assert!(5 + usize::from(raw[4]) > 12);
    raw[12] = 0;
    let rejection = inspect(
        Bytes::copy_from_slice(&budget, &raw).unwrap(),
        &budget,
        SourceId(1),
    );
    assert_eq!(rejection.error(), Error::Invalid("OPCR without PCR"));
    assert_eq!(
        rejection.secondary_error(),
        Some(Error::Invalid("adaptation stuffing"))
    );
    assert!(!rejection.fully_checked());
    assert!(
        observer(&budget)
            .push_rejected(rejection, context(8))
            .is_err()
    );
}

#[test]
fn invalid_marker_never_becomes_a_validated_source_or_native_header() {
    let budget = Budget::new(16384).unwrap();
    let mut raw = INPUT[376..564].to_vec();
    let start = 5 + usize::from(raw[4]);
    raw[start + 9] &= !1;
    let rejection = inspect(
        Bytes::copy_from_slice(&budget, &raw).unwrap(),
        &budget,
        SourceId(1),
    );
    assert!(rejection.fully_checked());
    let mut parser = observer(&budget);
    parser.push_rejected(rejection, context(8)).unwrap();
    let mut failed = false;
    for _ in 0..32 {
        match parser.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(AssemblyStep::Yield) => {}
            Err(Error::Invalid("native rejected timestamp markers")) => {
                failed = true;
                break;
            }
            other => panic!("marker fault must not produce header/output: {other:?}"),
        }
    }
    assert!(failed);
    let mut work = WorkBudget::new(0);
    assert!(matches!(
        parser.poll(&mut work, &AtomicBool::new(false)),
        Err(Error::Invalid("native rejected timestamp markers"))
    ));
    assert_eq!(work.spent(), 0);
    assert_eq!(parser.pending_rejection().unwrap().bytes().as_ref(), raw);
}

#[test]
fn rejected_header_flags_are_an_explicit_unproven_context() {
    let budget = Budget::new(16384).unwrap();
    let mut raw = INPUT[376..564].to_vec();
    let start = 5 + usize::from(raw[4]);
    raw[start + 7] = 0x40;
    let mut parser = observer(&budget);
    parser
        .push_rejected(
            inspect(
                Bytes::copy_from_slice(&budget, &raw).unwrap(),
                &budget,
                SourceId(1),
            ),
            context(8),
        )
        .unwrap();
    let mut failed = false;
    for _ in 0..32 {
        match parser.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(AssemblyStep::Yield) => {}
            Err(Error::Unsupported("native rejected header flags unobserved")) => {
                failed = true;
                break;
            }
            other => panic!("invalid flags must not produce output: {other:?}"),
        }
    }
    assert!(failed);
}

#[test]
fn native_rejected_zero_work_cancel_and_resume_preserve_boundary() {
    let budget = Budget::new(16384).unwrap();
    let rejection = original(&budget);
    let alias = rejection.backing_bytes().clone();
    let mut parser = observer(&budget);
    parser.push_rejected(rejection, context(8)).unwrap();
    assert!(matches!(
        parser.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(AssemblyStep::Yield)
    ));
    let cancel = AtomicBool::new(true);
    let mut work = WorkBudget::new(5);
    assert!(matches!(
        parser.poll(&mut work, &cancel),
        Err(Error::Cancelled)
    ));
    assert_eq!(work.spent(), 0);
    assert!(
        parser
            .pending_rejection()
            .unwrap()
            .backing_bytes()
            .shares_storage_with(&alias)
    );
    cancel.store(false, Ordering::Relaxed);
    let packet = recovered(&mut parser);
    assert_eq!(packet.context.source_event_ordinal, 8);
    assert_eq!(packet.native_position, 376);
    let mut clock = NativePesClockV1::new(1).unwrap();
    assert!(
        clock
            .project_encoded_header(
                256,
                &packet.encoded_header,
                &mut WorkBudget::new(0),
                &cancel
            )
            .unwrap()
            .is_none()
    );
    let time = clock
        .project_encoded_header(
            256,
            &packet.encoded_header,
            &mut WorkBudget::new(1),
            &cancel,
        )
        .unwrap()
        .unwrap();
    assert_eq!((time.pts90, time.dts90), (90000, 90000));
    assert_eq!(time.encoded_dts_raw33, None);
}

#[test]
fn native_rejected_shared_pressure_retries_without_replaying_input() {
    let budget = Budget::new(16384).unwrap();
    let mut parser = observer(&budget);
    parser.push_rejected(original(&budget), context(8)).unwrap();
    let hold = budget.reserve(budget.available()).unwrap();
    let mut blocked = false;
    for _ in 0..32 {
        match parser.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(AssemblyStep::Yield) => {}
            Err(Error::Backpressure { .. }) => {
                blocked = true;
                break;
            }
            other => panic!("unexpected pressure result {other:?}"),
        }
    }
    assert!(blocked);
    assert_eq!(parser.pending_rejection().unwrap().source_offset(), 376);
    drop(hold);
    assert_eq!(recovered(&mut parser).payload_len, 27);
    assert!(matches!(
        parser.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Ok(AssemblyStep::NeedPayload)
    ));
}

#[test]
fn wrong_pool_recovers_exact_rejection_and_hidden_owner() {
    let budget = Budget::new(16384).unwrap();
    let foreign = Budget::new(16384).unwrap();
    let rejection = original(&foreign);
    let alias = rejection.backing_bytes().clone();
    let error = observer(&budget)
        .push_rejected(rejection, context(8))
        .unwrap_err();
    assert_eq!(error.error, Error::ForeignPool);
    assert!(error.value.backing_bytes().shares_storage_with(&alias));
    assert_eq!(error.value.error(), Error::Invalid("OPCR without PCR"));
}

#[test]
fn source_epoch_and_byte_run_are_not_inferred_or_reset() {
    let budget = Budget::new(16384).unwrap();
    let mut parser = observer(&budget);
    let mut wrong = context(8);
    wrong.epochs.clock = 1;
    assert!(parser.push_rejected(original(&budget), wrong).is_err());
    wrong = context(8);
    wrong.byte_run_epoch = Some(0);
    assert!(parser.push_rejected(original(&budget), wrong).is_err());
    parser.push_rejected(original(&budget), context(8)).unwrap();
    assert_eq!(recovered(&mut parser).source_epochs, EPOCHS);
}

#[test]
fn a_rejected_mailbox_does_not_accept_duplicate_or_logical_eof() {
    let budget = Budget::new(16384).unwrap();
    let rejection = original(&budget);
    let mut parser = observer(&budget);
    parser.push_rejected(rejection.clone(), context(8)).unwrap();
    assert!(parser.push_rejected(rejection.clone(), context(8)).is_err());
    assert!(parser.end_of_source().is_err());
    drop(recovered(&mut parser));
    assert!(parser.push_rejected(rejection, context(8)).is_err());
    parser.end_of_source().unwrap();
    assert!(matches!(
        parser.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Ok(AssemblyStep::Finished)
    ));
}

#[test]
fn inspector_precondition_failure_returns_the_original_input() {
    let budget = Budget::new(16384).unwrap();
    let foreign = Budget::new(16384).unwrap();
    let owner = Bytes::copy_from_slice(&foreign, &INPUT[376..564]).unwrap();
    let error = TransportInspector::new(
        owner.clone(),
        PacketFormat::Ts188,
        SourceLocation {
            source: SourceId(1),
            offset: 376,
        },
        &budget,
    )
    .unwrap_err();
    assert_eq!(error.error, Error::ForeignPool);
    assert!(error.value.shares_storage_with(&owner));
    let error = TransportInspector::new(
        owner.clone(),
        PacketFormat::Ts188,
        SourceLocation {
            source: SourceId(1),
            offset: u64::MAX,
        },
        &foreign,
    )
    .unwrap_err();
    assert_eq!(error.error, Error::Overflow);
    assert!(error.value.shares_storage_with(&owner));
}

#[test]
fn opaque_m2ts_rejection_is_not_automatically_admitted_by_188_policy() {
    let budget = Budget::new(16384).unwrap();
    let mut raw = vec![1, 2, 3, 4];
    raw.extend_from_slice(&INPUT[376..564]);
    let owner = Bytes::copy_from_slice(&budget, &raw).unwrap();
    let mut inspection = TransportInspector::new(
        owner.clone(),
        PacketFormat::M2ts192,
        SourceLocation {
            source: SourceId(1),
            offset: 376,
        },
        &budget,
    )
    .unwrap();
    let rejection = loop {
        match inspection
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            AssemblyStep::Yield => {}
            AssemblyStep::Output(TransportInspection::Rejected(value)) => break value,
            other => panic!("strict OPCR rejection required, got {other:?}"),
        }
    };
    assert_eq!(rejection.format(), PacketFormat::M2ts192);
    let error = observer(&budget)
        .push_rejected(rejection, context(8))
        .unwrap_err();
    assert!(matches!(error.error, Error::Unsupported(_)));
    assert!(error.value.backing_bytes().shares_storage_with(&owner));
    assert_eq!(error.value.bytes().as_ref(), raw);
}

#[test]
fn rejected_entity_mismatch_does_not_become_a_clock_reset() {
    let budget = Budget::new(16384).unwrap();
    let rejection = inspect(
        Bytes::copy_from_slice(&budget, &INPUT[376..564]).unwrap(),
        &budget,
        SourceId(2),
    );
    let error = observer(&budget)
        .push_rejected(rejection, context(8))
        .unwrap_err();
    assert!(matches!(error.error, Error::Unsupported(_)));
    assert_eq!(error.value.source_id(), Some(SourceId(2)));
}

#[test]
fn framer_freeze_pressure_preserves_final_chunk_and_real_rejection() {
    use triergon_mpegts::{FrameState, TransportFramer};
    let budget = Budget::new(4096).unwrap();
    let mut framer = TransportFramer::new(budget.clone(), PacketFormat::Ts188);
    let raw = &INPUT[376..564];
    let step = framer.push(raw, 376, &mut WorkBudget::new(189), &AtomicBool::new(false));
    assert_eq!(
        step.consumed, 0,
        "final chunk acknowledgement waits for real inspection"
    );
    assert!(matches!(step.state, FrameState::Yield));
    let hold = budget.reserve(budget.available()).unwrap();
    let step = framer.push(raw, 376, &mut WorkBudget::new(1), &AtomicBool::new(false));
    assert_eq!(step.consumed, 0);
    assert!(matches!(
        step.state,
        FrameState::Error(Error::Backpressure { .. })
    ));
    drop(hold);
    let mut rejected = false;
    for _ in 0..1024 {
        let step = framer.push(raw, 376, &mut WorkBudget::new(1), &AtomicBool::new(false));
        match step.state {
            FrameState::Yield => assert_eq!(step.consumed, 0),
            FrameState::Error(Error::Invalid("OPCR without PCR")) => {
                assert_eq!(step.consumed, 188);
                rejected = true;
                break;
            }
            other => panic!("unexpected freeze/inspection result: {other:?}"),
        }
    }
    assert!(rejected);
    let evidence = framer.take_rejection().unwrap();
    assert_eq!(evidence.bytes().as_ref(), raw);
    assert_eq!(evidence.source_offset(), 376);
    assert!(framer.take_rejection().is_none());
    framer.finish().unwrap();
}

#[test]
fn composed_source_emits_real_diagnostics_8_9_without_video_or_epoch_reset() {
    let budget = Budget::new(1 << 20).unwrap();
    let owner = Bytes::copy_from_slice(&budget, INPUT).unwrap();
    let mut input = InputCursor::new(
        &budget,
        SourceId(1),
        SourceCapabilities {
            length: Some(752),
            random_access: false,
        },
        1024,
    )
    .unwrap();
    let mut stream =
        TransportStream::new(budget, PacketFormat::Ts188, ParseLimits::default(), 1).unwrap();
    let mut ordinal = 0;
    let mut faults = 0;
    let mut finished = false;
    for _ in 0..50000 {
        match stream
            .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(event) => {
                match event {
                    TraceEvent::Transport(packet) => assert_ne!(packet.pid, 256),
                    TraceEvent::Pes(_)
                    | TraceEvent::Timeline { .. }
                    | TraceEvent::Discontinuity { .. } => {
                        panic!("rejected source must not fabricate packet/clock events")
                    }
                    TraceEvent::Diagnostic(fault) => {
                        assert_eq!(ordinal, 8 + faults);
                        assert_eq!(fault.location.offset, 376 + faults * 188);
                        assert_eq!(fault.error, Error::Invalid("OPCR without PCR"));
                        assert_eq!(fault.pid, None);
                        let rejected = fault.rejected.unwrap();
                        assert_eq!(rejected.source_id(), Some(SourceId(1)));
                        assert_eq!(rejected.source_offset(), fault.location.offset);
                        faults += 1;
                    }
                    TraceEvent::Finished => {
                        finished = true;
                        break;
                    }
                    _ => {}
                }
                ordinal += 1;
            }
            Step::Yield => {}
            Step::NeedInput(request) => {
                let start = request.offset as usize;
                if start == INPUT.len() {
                    input.finish(request.source, request.token).unwrap();
                } else {
                    let end = (start + request.maximum).min(INPUT.len());
                    input
                        .provide(
                            &InputWindow::new(
                                request.source,
                                request.token,
                                request.offset,
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
    assert!(finished);
    assert_eq!(faults, 2);
    assert_eq!(stream.source_epochs(), EPOCHS);
    assert_eq!(input.position(), 752);
}
