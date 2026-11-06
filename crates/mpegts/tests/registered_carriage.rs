//! Independent primary OPCR and explicit registered-carriage regressions. These
//! finite opaque payloads establish PES carriage, never codec AU conformance.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, WorkBudget};
use triergon_mpegts::clock::{NativeClockProfile, NativePesClockV1, NativeTimestampOrigin};
use triergon_mpegts::native_pes::{
    NativePesBoundary, NativePesObserver, NativePesPacket, NativePesPayloadKind, NativePesScope,
    NativeRegisteredCarriage, NativeSourceContext,
};
use triergon_mpegts::{
    AssemblyStep, PacketFormat, ParseLimits, SourceEpochs, SourceRange, TransportPacket,
    parse_transport,
};

const EPOCHS: SourceEpochs = SourceEpochs {
    clock: 0,
    configuration: 2,
};
const PAYLOAD: &[u8] = b"lane-ts-original-payload\x00\x47\xff";
const OPCR_ORIGINAL: &[u8] = include_bytes!("data/native-pes-v1/opcr-without-pcr-invalid.ts");
const OPCR_VALID: &[u8] = include_bytes!("data/continuity-v1/opcr-change-is-not-duplicate.ts");

#[derive(Clone, Copy, Debug)]
struct Case {
    carriage: NativeRegisteredCarriage,
    stream_type: u8,
    stream_id: u8,
    pid: u16,
    bytes: &'static [u8],
}
const AVC: Case = Case {
    carriage: NativeRegisteredCarriage::Avc,
    stream_type: 0x1b,
    stream_id: 0xe0,
    pid: 256,
    bytes: include_bytes!("data/ts188-basic.ts"),
};
const HEVC: Case = Case {
    carriage: NativeRegisteredCarriage::Hevc,
    stream_type: 0x24,
    stream_id: 0xe0,
    pid: 257,
    bytes: include_bytes!("data/carriage-hevc-direct.ts"),
};
const ADTS: Case = Case {
    carriage: NativeRegisteredCarriage::AacAdts,
    stream_type: 0x0f,
    stream_id: 0xc0,
    pid: 257,
    bytes: include_bytes!("data/carriage-aac-adts-direct.ts"),
};
const LATM: Case = Case {
    carriage: NativeRegisteredCarriage::AacLatm,
    stream_type: 0x11,
    stream_id: 0xc0,
    pid: 257,
    bytes: include_bytes!("data/carriage-aac-latm-direct.ts"),
};
const NEW_CASES: [Case; 3] = [HEVC, ADTS, LATM];
const ALL_CASES: [Case; 4] = [AVC, HEVC, ADTS, LATM];

fn scope(case: Case) -> NativePesScope {
    NativePesScope {
        program: 1,
        pid: case.pid,
        stream_type: case.stream_type,
        stream_id: case.stream_id,
        entity: SourceId(1),
        epochs: EPOCHS,
    }
}
fn context() -> NativeSourceContext {
    // The ordinary traces contain PAT/PMT facts before transport event8.
    // This ordinal is neither packet index2 nor a core generation/range token.
    NativeSourceContext {
        source_event_ordinal: 8,
        epochs: EPOCHS,
        byte_run_epoch: None,
    }
}
fn observer(case: Case, budget: &Budget) -> NativePesObserver {
    NativePesObserver::new_registered(
        NativeClockProfile::ArchivedPesV1,
        scope(case),
        case.carriage,
        budget.clone(),
        ParseLimits::default(),
    )
    .unwrap_or_else(|error| {
        panic!(
            "registered {:?} runtime RED until implementation: {error:?}",
            case.carriage
        )
    })
}
fn carrier(case: Case, budget: &Budget) -> TransportPacket {
    assert_eq!(case.bytes.len(), 564);
    let bytes = Bytes::copy_from_slice(budget, &case.bytes[376..564]).unwrap();
    let mut packet = parse_transport(bytes, PacketFormat::Ts188, 376).unwrap();
    packet.source_id = Some(SourceId(1));
    assert_eq!(packet.pid, case.pid);
    packet
}
fn drain_one(observer: &mut NativePesObserver) -> NativePesPacket {
    let mut result = None;
    for _ in 0..8192 {
        let mut work = WorkBudget::new(1);
        let step = observer.poll(&mut work, &AtomicBool::new(false)).unwrap();
        assert!(work.spent() <= 1);
        match step {
            AssemblyStep::Yield => {}
            AssemblyStep::Output(packet) => assert!(
                result.replace(packet).is_none(),
                "one finite PES, no AU splitting"
            ),
            AssemblyStep::NeedPayload => {
                return result.expect("one real finite carrier must emit its PES body");
            }
            AssemblyStep::Finished => panic!("input exhaustion is not logical EOF"),
        }
    }
    panic!("bounded one-carrier work did not make progress");
}
fn check_packet(packet: &NativePesPacket, case: Case) {
    assert_eq!(packet.pid, case.pid);
    assert_eq!(packet.program, 1);
    assert_eq!(packet.source_epochs, EPOCHS);
    assert_eq!(packet.source_origin.source, SourceId(1));
    assert_eq!(packet.source_origin.offset, 376);
    assert_eq!(packet.native_position, 376);
    assert_eq!(packet.native_flags, 0);
    assert_eq!(packet.native_duration90_raw, 0);
    assert_eq!(packet.native_stream_id, case.stream_id);
    assert_eq!(packet.boundary, NativePesBoundary::FiniteLength);
    assert_eq!(packet.payload_kind, NativePesPayloadKind::DeclaredComplete);
    assert_eq!(packet.payload_len, 27);
    assert!(
        packet
            .payload()
            .flatten()
            .copied()
            .eq(PAYLOAD.iter().copied())
    );
    assert_eq!(packet.source_header.as_bytes()[3], case.stream_id);
    assert_eq!(packet.source_header.pts_raw33, Some(90000));
    assert_eq!(packet.source_header.dts_raw33, None);
    assert!(packet.time_header_applies);
    assert_eq!(
        packet.header_source_ranges.as_slice(),
        &[SourceRange {
            entity: SourceId(1),
            offset: 523,
            bytes: 14
        }]
    );
    let spans = packet.spans.as_slice();
    assert_eq!(spans.len(), 1);
    assert_eq!(
        spans[0].source,
        SourceRange {
            entity: SourceId(1),
            offset: 537,
            bytes: 27
        }
    );
    assert_eq!(
        spans[0].transport,
        SourceRange {
            entity: SourceId(1),
            offset: 376,
            bytes: 188
        }
    );
    assert_eq!(spans[0].context, context());
    assert_eq!(spans[0].as_ref(), PAYLOAD);
    assert!(spans[0].backing_bytes().retained_capacity() >= 188);
    let time = NativePesClockV1::new(1)
        .unwrap()
        .project_native_header(
            case.pid,
            EPOCHS,
            packet.time_header(),
            &mut WorkBudget::new(1),
            &AtomicBool::new(false),
        )
        .unwrap()
        .unwrap();
    assert_eq!(time.source_pts_raw33, Some(90000));
    assert_eq!(time.source_dts_raw33, None);
    assert_eq!(time.pts90, Some(90000));
    assert_eq!(time.dts90, Some(90000));
    assert_eq!(time.dts_origin, NativeTimestampOrigin::PtsOnlyCompatibility);
}
fn finite(case: Case) {
    let budget = Budget::new(16384).unwrap();
    let mut observer = observer(case, &budget);
    observer.push(carrier(case, &budget), context()).unwrap();
    let packet = drain_one(&mut observer);
    check_packet(&packet, case);
    observer.end_of_source().unwrap();
    assert!(matches!(
        observer.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Ok(AssemblyStep::Finished)
    ));
}
fn constructor(case: Case) {
    let budget = Budget::new(16384).unwrap();
    let before = budget.used();
    let admitted = observer(case, &budget);
    assert_eq!(admitted.scope(), scope(case));
    assert_eq!(admitted.profile(), NativeClockProfile::ArchivedPesV1);
    assert_eq!(
        budget.used(),
        before,
        "inline admission must not allocate outside shared charged storage"
    );
}
fn zero_work(case: Case) {
    let budget = Budget::new(16384).unwrap();
    let mut observer = observer(case, &budget);
    observer.push(carrier(case, &budget), context()).unwrap();
    let before = budget.used();
    let mut work = WorkBudget::new(0);
    assert!(matches!(
        observer.poll(&mut work, &AtomicBool::new(false)),
        Ok(AssemblyStep::Yield)
    ));
    assert_eq!(work.spent(), 0);
    assert_eq!(budget.used(), before);
    check_packet(&drain_one(&mut observer), case);
}
fn cancellation(case: Case) {
    let budget = Budget::new(16384).unwrap();
    let mut observer = observer(case, &budget);
    observer.push(carrier(case, &budget), context()).unwrap();
    assert!(matches!(
        observer.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Ok(AssemblyStep::Yield)
    ));
    let before = budget.used();
    let cancel = AtomicBool::new(true);
    let mut work = WorkBudget::new(17);
    assert!(matches!(
        observer.poll(&mut work, &cancel),
        Err(Error::Cancelled)
    ));
    assert_eq!(work.spent(), 0);
    assert_eq!(budget.used(), before);
    cancel.store(false, Ordering::Relaxed);
    check_packet(&drain_one(&mut observer), case);
}
fn resource(case: Case) {
    let budget = Budget::new(16384).unwrap();
    let mut observer = observer(case, &budget);
    observer.push(carrier(case, &budget), context()).unwrap();
    let hold = budget.reserve(budget.available()).unwrap();
    assert!(matches!(
        observer.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    drop(hold);
    check_packet(&drain_one(&mut observer), case);
}

#[test]
fn original_opcr_without_pcr_at_376_requires_primary_rejection() {
    assert_eq!(OPCR_ORIGINAL.len(), 752);
    let raw = &OPCR_ORIGINAL[376..564];
    assert_eq!(
        raw[5] & 0x18,
        0x08,
        "original OPCR-only flags, no invented PCR"
    );
    let budget = Budget::new(4096).unwrap();
    let result = parse_transport(
        Bytes::copy_from_slice(&budget, raw).unwrap(),
        PacketFormat::Ts188,
        376,
    );
    assert!(
        matches!(result, Err(Error::Invalid("OPCR without PCR"))),
        "H222 2.4.3.5 runtime RED: original carrier requires rejection, got {result:?}"
    );
}
#[test]
fn existing_pcr_and_opcr_control_keeps_both_raw_fields() {
    let budget = Budget::new(4096).unwrap();
    let raw = &OPCR_VALID[376..564];
    let packet = parse_transport(
        Bytes::copy_from_slice(&budget, raw).unwrap(),
        PacketFormat::Ts188,
        376,
    )
    .unwrap();
    let adaptation = packet.adaptation.as_ref().unwrap();
    assert!(adaptation.pcr.is_some());
    assert!(adaptation.opcr.is_some());
    assert_eq!(packet.source.as_ref(), raw);
    assert_eq!(packet.source_offset, 376);
}
#[test]
fn registered_hevc_constructor_is_an_explicit_admission() {
    constructor(HEVC);
}
#[test]
fn registered_adts_constructor_is_an_explicit_admission() {
    constructor(ADTS);
}
#[test]
fn registered_latm_constructor_is_an_explicit_admission() {
    constructor(LATM);
}
#[test]
fn registered_hevc_real_finite_carrier_matches_archived_packet() {
    finite(HEVC);
}
#[test]
fn registered_adts_real_finite_carrier_matches_archived_packet() {
    finite(ADTS);
}
#[test]
fn registered_latm_real_finite_carrier_matches_archived_packet() {
    finite(LATM);
}
#[test]
fn explicit_registered_avc_keeps_the_established_finite_operation() {
    finite(AVC);
}
#[test]
fn registered_wrong_kind_is_rejected_without_charging() {
    for case in ALL_CASES {
        let budget = Budget::new(4096).unwrap();
        let before = budget.used();
        let mut wrong = scope(case);
        wrong.stream_type ^= 1;
        let error = NativePesObserver::new_registered(
            NativeClockProfile::ArchivedPesV1,
            wrong,
            case.carriage,
            budget.clone(),
            ParseLimits::default(),
        )
        .unwrap_err();
        assert_eq!(
            error,
            Error::Invalid("native registered carriage kind mismatch")
        );
        assert_eq!(wrong.stream_type, case.stream_type ^ 1);
        assert_eq!(budget.used(), before);
    }
}
#[test]
fn registered_unobserved_stream_id_is_not_silently_substituted() {
    for case in ALL_CASES {
        let budget = Budget::new(4096).unwrap();
        let before = budget.used();
        let mut wrong = scope(case);
        wrong.stream_id ^= 1;
        let error = NativePesObserver::new_registered(
            NativeClockProfile::ArchivedPesV1,
            wrong,
            case.carriage,
            budget.clone(),
            ParseLimits::default(),
        )
        .unwrap_err();
        assert_eq!(
            error,
            Error::Unsupported("native registered PES stream ID unobserved")
        );
        assert_eq!(wrong.stream_id, case.stream_id ^ 1);
        assert_eq!(budget.used(), before);
    }
}
#[test]
fn registered_invalid_program_and_null_pid_are_rejected_before_feature_stub() {
    for case in ALL_CASES {
        let budget = Budget::new(4096).unwrap();
        let before = budget.used();
        let mut wrong = scope(case);
        wrong.program = 0;
        let error = NativePesObserver::new_registered(
            NativeClockProfile::ArchivedPesV1,
            wrong,
            case.carriage,
            budget.clone(),
            ParseLimits::default(),
        )
        .unwrap_err();
        assert_eq!(error, Error::Invalid("native PES selected program"));
        wrong = scope(case);
        wrong.pid = 0x1fff;
        let error = NativePesObserver::new_registered(
            NativeClockProfile::ArchivedPesV1,
            wrong,
            case.carriage,
            budget.clone(),
            ParseLimits::default(),
        )
        .unwrap_err();
        assert_eq!(error, Error::Invalid("assembly PID"));
        assert_eq!(budget.used(), before);
    }
}
#[test]
fn registered_bad_limits_do_not_hide_behind_missing_feature() {
    for case in ALL_CASES {
        let budget = Budget::new(4096).unwrap();
        let before = budget.used();
        let limits = ParseLimits {
            pes_bytes: 5,
            ..ParseLimits::default()
        };
        let error = NativePesObserver::new_registered(
            NativeClockProfile::ArchivedPesV1,
            scope(case),
            case.carriage,
            budget.clone(),
            limits,
        )
        .unwrap_err();
        assert_eq!(error, Error::Invalid("TS parser limits"));
        assert_eq!(budget.used(), before);
    }
}
#[test]
fn explicit_avc_core_zero_work_then_resume_control() {
    zero_work(AVC);
}
#[test]
fn explicit_avc_core_cancel_then_resume_control() {
    cancellation(AVC);
}
#[test]
fn explicit_avc_shared_resource_backpressure_then_resume_control() {
    resource(AVC);
}
#[test]
fn all_new_registered_profiles_obey_zero_work_and_resume() {
    for case in NEW_CASES {
        zero_work(case);
    }
}
#[test]
fn all_new_registered_profiles_obey_cancel_and_resume() {
    for case in NEW_CASES {
        cancellation(case);
    }
}
#[test]
fn all_new_registered_profiles_obey_shared_resource_backpressure() {
    for case in NEW_CASES {
        resource(case);
    }
}
#[test]
fn explicit_registered_avc_recovers_foreign_input_without_new_allocation() {
    let budget = Budget::new(4096).unwrap();
    let foreign = Budget::new(4096).unwrap();
    let mut observer = observer(AVC, &budget);
    let packet = carrier(AVC, &foreign);
    let before = budget.used();
    let error = observer.push(packet, context()).unwrap_err();
    assert_eq!(error.error, Error::Invalid("foreign native PES byte pool"));
    assert_eq!(error.value.source_offset, 376);
    assert!(error.value.source.belongs_to(&foreign));
    assert_eq!(budget.used(), before);
}
#[test]
fn explicit_registered_output_retains_charge_after_parser_drop() {
    let budget = Budget::new(16384).unwrap();
    let initial = budget.used();
    let mut observer = observer(AVC, &budget);
    observer.push(carrier(AVC, &budget), context()).unwrap();
    let packet = drain_one(&mut observer);
    drop(observer);
    assert!(budget.used() > initial);
    check_packet(&packet, AVC);
    drop(packet);
    assert_eq!(budget.used(), initial);
}
