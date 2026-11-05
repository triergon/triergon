//! Original source/actual-reference regressions. No codec frame decoding or AU
//! claims. Original four input bytes and archived19c native golds are immutable.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, SourceLocation,
    Step, WorkBudget,
};
use triergon_mpegts::clock::{NativeClockProfile, NativePesClockV1, NativeTimestampOrigin};
use triergon_mpegts::native_pes::{
    NativePesBoundary, NativePesObserver, NativePesPacket, NativePesPayloadKind, NativePesScope,
    NativePrivateAudioCarriage, NativePrivateAudioDescriptors, NativeSourceContext,
};
use triergon_mpegts::native_recovery::{
    NativePesObservation, NativePesRecoveryObserver, NativeRejectedPolicy,
};
use triergon_mpegts::psi::{CodecIdentification, DescriptorNamespace};
use triergon_mpegts::{
    AssemblyStep, PacketFormat, ParseLimits, SourceEpochs, SourceRange, TraceEvent,
    TransportInspection, TransportInspector, TransportPacket, TransportStream, parse_transport,
};

const PAYLOAD: &[u8] = b"lane-ts-original-payload\x00\x47\xff";
const EPOCHS: SourceEpochs = SourceEpochs {
    clock: 0,
    configuration: 2,
};
const UNSUPPORTED: Error =
    Error::Unsupported("native private audio finite PES operation not implemented");

#[derive(Clone, Copy)]
struct Case {
    carriage: NativePrivateAudioCarriage,
    stream_type: u8,
    descriptors: &'static [u8],
    bytes: &'static [u8],
}
const AC3_DVB: Case = Case {
    carriage: NativePrivateAudioCarriage::DvbAc3Tag,
    stream_type: 6,
    descriptors: &[0x6a, 1, 0],
    bytes: include_bytes!("data/carriage-ac3-dvb.ts"),
};
const EAC3_DVB: Case = Case {
    carriage: NativePrivateAudioCarriage::DvbEac3Tag,
    stream_type: 6,
    descriptors: &[0x7a, 1, 0],
    bytes: include_bytes!("data/carriage-eac3-dvb.ts"),
};
const AC3_DIRECT: Case = Case {
    carriage: NativePrivateAudioCarriage::PrivateType81Ac3,
    stream_type: 0x81,
    descriptors: &[],
    bytes: include_bytes!("data/carriage-ac3-direct.ts"),
};
const EAC3_DIRECT: Case = Case {
    carriage: NativePrivateAudioCarriage::PrivateType87Eac3,
    stream_type: 0x87,
    descriptors: &[],
    bytes: include_bytes!("data/carriage-eac3-direct.ts"),
};
const CASES: [Case; 4] = [AC3_DVB, EAC3_DVB, AC3_DIRECT, EAC3_DIRECT];

fn descriptors(case: Case) -> NativePrivateAudioDescriptors<'static> {
    NativePrivateAudioDescriptors {
        program: &[],
        elementary: case.descriptors,
    }
}
fn scope(case: Case) -> NativePesScope {
    NativePesScope {
        program: 1,
        pid: 257,
        stream_type: case.stream_type,
        stream_id: 0xbd,
        entity: SourceId(1),
        epochs: EPOCHS,
    }
}
fn context(ordinal: u64) -> NativeSourceContext {
    NativeSourceContext {
        source_event_ordinal: ordinal,
        epochs: EPOCHS,
        byte_run_epoch: None,
    }
}
fn observer(case: Case, budget: &Budget) -> NativePesObserver {
    NativePesObserver::new_private_audio_v1(
        NativeClockProfile::ArchivedPesV1,
        scope(case),
        case.carriage,
        descriptors(case),
        budget.clone(),
        ParseLimits::default(),
    )
    .unwrap_or_else(|error| {
        panic!("real finite private-audio runtime RED: {error:?}; stub {UNSUPPORTED:?}")
    })
}
fn carrier(raw: &[u8], budget: &Budget, offset: u64) -> TransportPacket {
    let bytes = Bytes::copy_from_slice(budget, raw).unwrap();
    let mut packet = parse_transport(bytes, PacketFormat::Ts188, offset).unwrap();
    packet.source_id = Some(SourceId(1));
    packet
}
fn original(case: Case, budget: &Budget) -> TransportPacket {
    assert_eq!(case.bytes.len(), 564);
    carrier(&case.bytes[376..564], budget, 376)
}
fn drain(observer: &mut NativePesObserver) -> Vec<NativePesPacket> {
    let mut out = Vec::new();
    for _ in 0..16_384 {
        let mut work = WorkBudget::new(1);
        match observer.poll(&mut work, &AtomicBool::new(false)).unwrap() {
            AssemblyStep::Output(packet) => out.push(packet),
            AssemblyStep::Yield => (),
            AssemblyStep::NeedPayload => return out,
            AssemblyStep::Finished => panic!("input starvation is not EOF"),
        }
        assert!(work.spent() <= 1);
    }
    panic!("bounded actual field/byte work did not finish");
}
fn check_packet(packet: &NativePesPacket, case: Case) {
    assert_eq!((packet.program, packet.pid), (1, 257));
    assert_eq!(packet.source_epochs, EPOCHS);
    assert_eq!(
        (packet.source_origin.source, packet.source_origin.offset),
        (SourceId(1), 376)
    );
    assert_eq!(
        (
            packet.native_position,
            packet.native_flags,
            packet.native_duration90_raw
        ),
        (376, 0, 0)
    );
    assert_eq!(packet.native_stream_id, 0xbd); // public side-data type19 byte bd
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
    assert_eq!(packet.source_header.as_bytes(), &case.bytes[523..537]);
    assert_eq!(packet.source_header.pts_raw33, Some(90_000));
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
    assert_eq!(spans[0].context, context(8));
    assert_eq!(spans[0].as_ref(), PAYLOAD);
    assert!(spans[0].backing_bytes().retained_capacity() >= 188);
    let mut clock = NativePesClockV1::new(1).unwrap();
    let view = clock
        .project_native_header(
            257,
            EPOCHS,
            packet.time_header(),
            &mut WorkBudget::new(1),
            &AtomicBool::new(false),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        (view.source_pts_raw33, view.source_dts_raw33),
        (Some(90_000), None)
    );
    assert_eq!((view.pts90, view.dts90), (Some(90_000), Some(90_000)));
    assert_eq!(view.dts_origin, NativeTimestampOrigin::PtsOnlyCompatibility);
}
fn finite(case: Case) {
    let budget = Budget::new(16_384).unwrap();
    let mut observer = observer(case, &budget);
    observer.push(original(case, &budget), context(8)).unwrap();
    let packets = drain(&mut observer);
    assert_eq!(packets.len(), 1, "one real finite PES, no AU splitting");
    check_packet(&packets[0], case);
    observer.end_of_source().unwrap();
    assert!(matches!(
        observer.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Ok(AssemblyStep::Finished)
    ));
}
fn constructor(case: Case) {
    let budget = Budget::new(4096).unwrap();
    let used = budget.used();
    let value = observer(case, &budget);
    assert_eq!(value.scope(), scope(case));
    assert_eq!(budget.used(), used, "no allocation in bounded admission");
}

#[test]
fn audio_ac3_dvb_constructor() {
    constructor(AC3_DVB);
}
#[test]
fn audio_eac3_dvb_constructor() {
    constructor(EAC3_DVB);
}
#[test]
fn audio_ac3_private_type81_constructor() {
    constructor(AC3_DIRECT);
}
#[test]
fn audio_eac3_private_type87_constructor() {
    constructor(EAC3_DIRECT);
}
#[test]
fn audio_ac3_dvb_actual_finite_packet_all_fields() {
    finite(AC3_DVB);
}
#[test]
fn audio_eac3_dvb_actual_finite_packet_all_fields() {
    finite(EAC3_DVB);
}
#[test]
fn audio_ac3_private_type81_actual_finite_packet_all_fields() {
    finite(AC3_DIRECT);
}
#[test]
fn audio_eac3_private_type87_actual_finite_packet_all_fields() {
    finite(EAC3_DIRECT);
}

#[test]
fn audio_original_source_operation_retains_unspecified_namespace_unknown_dts_and_epochs() {
    for case in CASES {
        let budget = Budget::new(65_536).unwrap();
        let mut input = InputCursor::new(
            &budget,
            SourceId(1),
            SourceCapabilities {
                length: Some(case.bytes.len() as u64),
                random_access: false,
            },
            188,
        )
        .unwrap();
        let mut stream = TransportStream::new(
            budget.clone(),
            PacketFormat::Ts188,
            ParseLimits::default(),
            1,
        )
        .unwrap();
        let mut map_seen = 0;
        let mut pes_seen = 0;
        let mut ended = false;
        let mut finished = false;
        for _ in 0..50_000 {
            match stream
                .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                Step::Yield => (),
                Step::NeedInput(request) => {
                    let offset = usize::try_from(request.offset).unwrap();
                    if offset == case.bytes.len() {
                        input.finish(request.source, request.token).unwrap();
                    } else {
                        let data = Bytes::copy_from_slice(&budget, &case.bytes[offset..offset + 1])
                            .unwrap();
                        input
                            .provide(
                                &InputWindow::new(
                                    request.source,
                                    request.token,
                                    request.offset,
                                    data,
                                )
                                .unwrap(),
                            )
                            .unwrap();
                    }
                }
                Step::End(_) => {
                    assert!(!ended);
                    stream.end_of_source().unwrap();
                    ended = true;
                }
                Step::Ready(TraceEvent::ProgramMap {
                    map,
                    active: true,
                    configuration_epoch,
                }) => {
                    map_seen += 1;
                    assert_eq!(configuration_epoch, 2);
                    assert_eq!(map.descriptor_namespace, DescriptorNamespace::Unspecified);
                    assert_eq!(map.streams.len(), 1);
                    let es = &map.streams.as_slice()[0];
                    assert_eq!((es.stream_type, es.pid), (case.stream_type, 257));
                    assert_eq!(es.descriptor_bytes.as_ref(), case.descriptors);
                    assert_eq!(es.descriptor_namespace(), DescriptorNamespace::Unspecified);
                    assert_eq!(es.codec_identification(), CodecIdentification::Unknown);
                }
                Step::Ready(TraceEvent::Pes(pes)) => {
                    pes_seen += 1;
                    assert_eq!(
                        (pes.header.pts_raw33, pes.header.dts_raw33),
                        (Some(90_000), None)
                    );
                    assert!(pes.payload().flatten().copied().eq(PAYLOAD.iter().copied()));
                }
                Step::Ready(TraceEvent::Diagnostic(error)) => {
                    panic!("original valid source changed: {error:?}")
                }
                Step::Ready(TraceEvent::Finished) => {
                    finished = true;
                    break;
                }
                Step::Ready(_) => (),
            }
        }
        assert!(ended && finished);
        assert_eq!((map_seen, pes_seen), (1, 1));
    }
}

#[test]
fn audio_legacy_new_remains_h264_only() {
    for case in CASES {
        let budget = Budget::new(4096).unwrap();
        let used = budget.used();
        assert_eq!(
            NativePesObserver::new(
                NativeClockProfile::ArchivedPesV1,
                scope(case),
                budget.clone(),
                ParseLimits::default()
            )
            .unwrap_err(),
            Error::Unsupported("native PES stream context unobserved")
        );
        assert_eq!(budget.used(), used);
    }
}

#[test]
fn audio_bad_scope_kind_id_and_limits_precede_feature_stub_without_charge() {
    for case in CASES {
        let budget = Budget::new(4096).unwrap();
        let used = budget.used();
        let call = |value, kind, limits| {
            NativePesObserver::new_private_audio_v1(
                NativeClockProfile::ArchivedPesV1,
                value,
                kind,
                descriptors(case),
                budget.clone(),
                limits,
            )
            .unwrap_err()
        };
        let mut bad = scope(case);
        bad.program = 0;
        assert_eq!(
            call(bad, case.carriage, ParseLimits::default()),
            Error::Invalid("native PES selected program")
        );
        bad = scope(case);
        bad.pid = 0x1fff;
        assert_eq!(
            call(bad, case.carriage, ParseLimits::default()),
            Error::Invalid("assembly PID")
        );
        bad = scope(case);
        bad.stream_type = 0x1b;
        assert_eq!(
            call(bad, case.carriage, ParseLimits::default()),
            Error::Invalid("native private audio carriage mismatch")
        );
        bad = scope(case);
        bad.stream_id = 0xc0;
        assert_eq!(
            call(bad, case.carriage, ParseLimits::default()),
            Error::Unsupported("native private audio stream ID unobserved")
        );
        assert_eq!(
            call(
                scope(case),
                case.carriage,
                ParseLimits {
                    pes_bytes: 5,
                    ..ParseLimits::default()
                }
            ),
            Error::Invalid("TS parser limits")
        );
        assert_eq!(budget.used(), used);
    }
}

#[test]
fn audio_private_tag_conflicts_and_missing_evidence_never_choose_first_codec() {
    let limits = ParseLimits::default();
    for raw in [&[0x6a, 1, 0, 0x7a, 1, 0][..], &[0x59, 0, 0x6a, 1, 0][..]] {
        assert_eq!(
            NativePrivateAudioCarriage::from_observed_pmt(
                6,
                NativePrivateAudioDescriptors {
                    program: &[],
                    elementary: raw
                },
                limits
            )
            .unwrap_err(),
            Error::Unsupported("native private audio conflicting descriptor context")
        );
    }
    assert_eq!(
        NativePrivateAudioCarriage::from_observed_pmt(
            6,
            NativePrivateAudioDescriptors::default(),
            limits
        )
        .unwrap_err(),
        Error::Unsupported("native private audio descriptor evidence absent")
    );
}

#[test]
fn audio_registration_specifier_and_program_tags_are_explicit_nonpasses_not_namespace_evidence() {
    for raw in [
        &[5, 4, b'H', b'D', b'M', b'V'][..],
        &[0x5f, 4, 0, 0, 0, 1][..],
    ] {
        assert_eq!(
            NativePrivateAudioCarriage::from_observed_pmt(
                0x81,
                NativePrivateAudioDescriptors {
                    program: raw,
                    elementary: &[]
                },
                ParseLimits::default()
            )
            .unwrap_err(),
            Error::Unsupported("native private audio registration/specifier context unobserved")
        );
    }
    assert_eq!(
        NativePrivateAudioCarriage::from_observed_pmt(
            6,
            NativePrivateAudioDescriptors {
                program: AC3_DVB.descriptors,
                elementary: &[]
            },
            ParseLimits::default()
        )
        .unwrap_err(),
        Error::Unsupported("native private audio program-level tag context unobserved")
    );
}

#[test]
fn hls_audio_registration_confirms_the_existing_carriage_rules() {
    let ac3 = b"\x05\x04AC-3";
    let eac3 = b"\x05\x04EAC3";
    let descriptors = |elementary| NativePrivateAudioDescriptors {
        program: &[],
        elementary,
    };
    let limits = ParseLimits::default();
    for (stream_type, elementary, carriage) in [
        (0x81, &ac3[..], NativePrivateAudioCarriage::PrivateType81Ac3),
        (
            0x87,
            &eac3[..],
            NativePrivateAudioCarriage::PrivateType87Eac3,
        ),
        (
            6,
            &b"\x05\x04AC-3\x6a\x03\xc0\x42\x08"[..],
            NativePrivateAudioCarriage::DvbAc3Tag,
        ),
        (
            6,
            &b"\x05\x04EAC3\x7a\x01\x00"[..],
            NativePrivateAudioCarriage::DvbEac3Tag,
        ),
    ] {
        assert_eq!(
            NativePrivateAudioCarriage::from_hls_pmt(stream_type, descriptors(elementary), limits),
            Ok(carriage)
        );
        assert!(
            NativePrivateAudioCarriage::from_observed_pmt(
                stream_type,
                descriptors(elementary),
                limits
            )
            .is_err()
        );
    }
    for (stream_type, elementary) in [
        (0x87, &ac3[..]),
        (0x81, &eac3[..]),
        (6, &ac3[..]),
        (6, &b"\x6a\x03\xc0\x42\x08"[..]),
        (6, &b"\x7a\x01\x00"[..]),
        (6, &b"\x05\x04AC-3\x7a\x01\x00"[..]),
        (0x81, &b"\x05\x04AC-3\x05\x04EAC3"[..]),
    ] {
        assert!(
            NativePrivateAudioCarriage::from_hls_pmt(stream_type, descriptors(elementary), limits)
                .is_err()
        );
    }
}

#[test]
fn audio_descriptor_optional_fields_duplicates_languages_and_opaque_tails_stay_raw() {
    let raw = [
        0x6a, 3, 0x80, 0x11, 0xaa, 0x0a, 4, b'e', b'n', b'g', 0x80, 0x90, 1, 0xff, 0x6a, 1, 0,
    ];
    let original = raw;
    assert_eq!(
        NativePrivateAudioCarriage::from_observed_pmt(
            6,
            NativePrivateAudioDescriptors {
                program: &[],
                elementary: &raw
            },
            ParseLimits::default()
        )
        .unwrap(),
        NativePrivateAudioCarriage::DvbAc3Tag
    );
    assert_eq!(raw, original);
    assert_eq!(
        NativePrivateAudioCarriage::from_observed_pmt(
            6,
            NativePrivateAudioDescriptors {
                program: &[],
                elementary: &[0x7a, 1, 8]
            },
            ParseLimits::default()
        )
        .unwrap(),
        NativePrivateAudioCarriage::DvbEac3Tag
    ); // mix-info flag adds no byte
}

#[test]
fn audio_bad_tlv_and_real_descriptor_limits_remain_checked() {
    assert_eq!(
        NativePrivateAudioCarriage::from_observed_pmt(
            6,
            NativePrivateAudioDescriptors {
                program: &[],
                elementary: &[0x6a, 2, 0]
            },
            ParseLimits::default()
        )
        .unwrap_err(),
        Error::Invalid("descriptor length")
    );
    assert_eq!(
        NativePrivateAudioCarriage::from_observed_pmt(
            6,
            NativePrivateAudioDescriptors {
                program: &[],
                elementary: &[0x6a, 1, 0x80]
            },
            ParseLimits::default()
        )
        .unwrap_err(),
        Error::Invalid("audio descriptor optional fields")
    );
    assert_eq!(
        NativePrivateAudioCarriage::from_observed_pmt(
            6,
            NativePrivateAudioDescriptors {
                program: &[],
                elementary: &[0x6a, 1, 0, 0x90, 0, 0x90, 0]
            },
            ParseLimits {
                descriptors: 2,
                ..ParseLimits::default()
            }
        )
        .unwrap_err(),
        Error::Limit {
            requested: 3,
            maximum: 2
        }
    );
}

#[test]
fn audio_work_zero_and_one_advance_actual_input_without_duplicate_output() {
    for case in CASES {
        let budget = Budget::new(16_384).unwrap();
        let mut observer = observer(case, &budget);
        observer.push(original(case, &budget), context(8)).unwrap();
        let used = budget.used();
        let mut zero = WorkBudget::new(0);
        assert!(matches!(
            observer.poll(&mut zero, &AtomicBool::new(false)),
            Ok(AssemblyStep::Yield)
        ));
        assert_eq!(zero.spent(), 0);
        assert_eq!(budget.used(), used);
        let out = drain(&mut observer);
        assert_eq!(out.len(), 1);
        check_packet(&out[0], case);
        assert!(drain(&mut observer).is_empty());
    }
}

#[test]
fn audio_cancel_after_real_first_unit_resumes_without_losing_carrier() {
    for case in CASES {
        let budget = Budget::new(16_384).unwrap();
        let mut observer = observer(case, &budget);
        observer.push(original(case, &budget), context(8)).unwrap();
        assert!(matches!(
            observer.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
            Ok(AssemblyStep::Yield)
        ));
        let used = budget.used();
        let cancel = AtomicBool::new(true);
        let mut work = WorkBudget::new(19);
        assert!(matches!(
            observer.poll(&mut work, &cancel),
            Err(Error::Cancelled)
        ));
        assert_eq!(work.spent(), 0);
        assert_eq!(budget.used(), used);
        cancel.store(false, Ordering::Relaxed);
        let out = drain(&mut observer);
        assert_eq!(out.len(), 1);
        check_packet(&out[0], case);
    }
}

#[test]
fn audio_external_pressure_retries_same_input_then_owned_packet_survives_observer() {
    for case in CASES {
        let budget = Budget::new(16_384).unwrap();
        let before = budget.used();
        let mut observer = observer(case, &budget);
        observer.push(original(case, &budget), context(8)).unwrap();
        let hold = budget.reserve(budget.available()).unwrap();
        assert!(matches!(
            observer.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
            Err(Error::Backpressure { .. })
        ));
        drop(hold);
        let out = drain(&mut observer);
        assert_eq!(out.len(), 1);
        check_packet(&out[0], case);
        drop(observer);
        check_packet(&out[0], case);
        assert!(budget.used() > before);
        drop(out);
        assert_eq!(budget.used(), before);
    }
}

#[test]
fn audio_hidden_owner_is_fully_charged_and_not_replaced_by_visible_length() {
    let budget = Budget::new(16_384).unwrap();
    let before = budget.used();
    let mut observer = observer(AC3_DVB, &budget);
    let mut source = vec![0xaa; 13];
    source.extend_from_slice(&AC3_DVB.bytes[376..564]);
    source.extend_from_slice(&[0xbb; 37]);
    let owner = Bytes::copy_from_slice(&budget, &source).unwrap();
    let capacity = owner.retained_capacity();
    let mut packet =
        parse_transport(owner.slice(13..201).unwrap(), PacketFormat::Ts188, 376).unwrap();
    packet.source_id = Some(SourceId(1));
    observer.push(packet, context(8)).unwrap();
    let out = drain(&mut observer);
    assert_eq!(out.len(), 1);
    check_packet(&out[0], AC3_DVB);
    assert_eq!(
        out[0].spans.as_slice()[0]
            .backing_bytes()
            .retained_capacity(),
        capacity
    );
    assert!(
        out[0].spans.as_slice()[0]
            .backing_bytes()
            .shares_storage_with(&owner)
    );
    drop(owner);
    drop(observer);
    check_packet(&out[0], AC3_DVB);
    assert!(budget.used() > before);
    drop(out);
    assert_eq!(budget.used(), before);
}

// Authored H.222 transport/PES byte encodings; execution is deferred to A.
fn time(value: u64) -> [u8; 5] {
    assert!(value < (1_u64 << 33));
    [
        0x21 | (((value >> 30) as u8 & 7) << 1),
        (value >> 22) as u8,
        (((value >> 15) as u8 & 0x7f) << 1) | 1,
        (value >> 7) as u8,
        ((value as u8 & 0x7f) << 1) | 1,
    ]
}
fn pes(body: &[u8], pts: u64) -> Vec<u8> {
    let length = u16::try_from(body.len() + 8).unwrap();
    let mut result = vec![
        0,
        0,
        1,
        0xbd,
        (length >> 8) as u8,
        length as u8,
        0x80,
        0x80,
        5,
    ];
    result.extend_from_slice(&time(pts));
    result.extend_from_slice(body);
    result
}
fn transport(payload: &[u8], start: bool, cc: u8) -> [u8; 188] {
    assert!(!payload.is_empty() && payload.len() <= 182);
    let mut out = [0xff; 188];
    out[..4].copy_from_slice(&[0x47, 0x01 | if start { 0x40 } else { 0 }, 1, 0x30 | cc]);
    out[4] = (183 - payload.len()) as u8;
    out[5] = 0;
    out[188 - payload.len()..].copy_from_slice(payload);
    out
}

#[test]
fn audio_two_real_pes_keep_order_raw_timestamps_context_and_no_after_eof_repetition() {
    let budget = Budget::new(16_384).unwrap();
    let mut observer = observer(EAC3_DIRECT, &budget);
    observer
        .push(original(EAC3_DIRECT, &budget), context(8))
        .unwrap();
    let first = drain(&mut observer);
    assert_eq!(first.len(), 1);
    check_packet(&first[0], EAC3_DIRECT);
    let next = transport(&pes(b"second\x00\x47\xff", 180_000), true, 1);
    observer
        .push(carrier(&next, &budget, 564), context(9))
        .unwrap();
    let second = drain(&mut observer);
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].source_header.pts_raw33, Some(180_000));
    assert_eq!(second[0].source_header.dts_raw33, None);
    assert_eq!(second[0].native_position, 564);
    assert_eq!(second[0].source_epochs, EPOCHS);
    assert_eq!(second[0].spans.as_slice()[0].context, context(9));
    assert!(
        second[0]
            .payload()
            .flatten()
            .copied()
            .eq(b"second\x00\x47\xff".iter().copied())
    );
    check_packet(&first[0], EAC3_DIRECT);
    observer.end_of_source().unwrap();
    for _ in 0..2 {
        assert!(matches!(
            observer.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
            Ok(AssemblyStep::Finished)
        ));
    }
}

#[test]
fn audio_mailbox_rejects_second_input_and_returns_exact_charged_owner() {
    let budget = Budget::new(16_384).unwrap();
    let mut observer = observer(AC3_DIRECT, &budget);
    observer
        .push(original(AC3_DIRECT, &budget), context(8))
        .unwrap();
    let raw = transport(&pes(PAYLOAD, 180_000), true, 1);
    let second = carrier(&raw, &budget, 564);
    let same = second.source.clone();
    let error = observer.push(second, context(9)).unwrap_err();
    assert_eq!(error.error, Error::Invalid("native PES input not drained"));
    assert!(error.value.source.shares_storage_with(&same));
    assert_eq!(error.value.source.as_ref(), raw);
    let out = drain(&mut observer);
    assert_eq!(out.len(), 1);
    check_packet(&out[0], AC3_DIRECT);
    observer.push(error.value, context(9)).unwrap();
    assert_eq!(drain(&mut observer).len(), 1);
}

#[test]
fn audio_source_entity_epoch_and_foreign_pool_rejection_do_not_poison_valid_owner() {
    for case in CASES {
        let budget = Budget::new(16_384).unwrap();
        let mut observer = observer(case, &budget);
        let foreign = Budget::new(4096).unwrap();
        let packet = original(case, &foreign);
        let same = packet.source.clone();
        let rejected = observer.push(packet, context(8)).unwrap_err();
        assert_eq!(
            rejected.error,
            Error::Invalid("foreign native PES byte pool")
        );
        assert!(rejected.value.source.shares_storage_with(&same));
        let mut packet = original(case, &budget);
        packet.source_id = Some(SourceId(2));
        assert!(matches!(
            observer.push(packet, context(8)).unwrap_err().error,
            Error::Unsupported(_)
        ));
        let mut wrong = context(8);
        wrong.epochs.configuration += 1;
        assert!(matches!(
            observer
                .push(original(case, &budget), wrong)
                .unwrap_err()
                .error,
            Error::Unsupported(_)
        ));
        observer.push(original(case, &budget), context(8)).unwrap();
        let out = drain(&mut observer);
        assert_eq!(out.len(), 1);
        check_packet(&out[0], case);
    }
}

#[test]
fn audio_finite_pes_policy_limit_is_real_and_has_no_partial_output() {
    let budget = Budget::new(16_384).unwrap();
    let mut observer = NativePesObserver::new_private_audio_v1(
        NativeClockProfile::ArchivedPesV1,
        scope(AC3_DVB),
        AC3_DVB.carriage,
        descriptors(AC3_DVB),
        budget.clone(),
        ParseLimits {
            pes_bytes: 40,
            ..ParseLimits::default()
        },
    )
    .unwrap();
    observer
        .push(original(AC3_DVB, &budget), context(8))
        .unwrap();
    let mut failed = false;
    for _ in 0..4096 {
        match observer.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(AssemblyStep::Yield) => (),
            Err(Error::Limit {
                requested: 41,
                maximum: 40,
            }) => {
                failed = true;
                break;
            }
            other => panic!("limit hidden or partial output: {other:?}"),
        }
    }
    assert!(failed);
}

#[test]
fn audio_fragmented_valid_header_and_body_preserve_full_real_span_ownership() {
    for cut in [5, 20] {
        let budget = Budget::new(16_384).unwrap();
        let mut observer = observer(EAC3_DVB, &budget);
        let bytes = pes(PAYLOAD, 90_000);
        let a = transport(&bytes[..cut], true, 0);
        let b = transport(&bytes[cut..], false, 1);
        observer
            .push(carrier(&a, &budget, 376), context(8))
            .unwrap();
        assert!(drain(&mut observer).is_empty());
        observer
            .push(carrier(&b, &budget, 564), context(9))
            .unwrap();
        let out = drain(&mut observer);
        assert_eq!(out.len(), 1);
        let packet = &out[0];
        assert_eq!(packet.payload_len, 27);
        assert_eq!(packet.native_position, 376);
        assert_eq!(
            (
                packet.source_header.pts_raw33,
                packet.source_header.dts_raw33
            ),
            (Some(90_000), None)
        );
        assert!(
            packet
                .payload()
                .flatten()
                .copied()
                .eq(PAYLOAD.iter().copied())
        );
        assert_eq!(
            packet.header_source_ranges.len(),
            if cut < 14 { 2 } else { 1 }
        );
        assert_eq!(packet.spans.len(), if cut < 14 { 1 } else { 2 });
        for span in packet.spans.as_slice() {
            assert!(span.backing_bytes().retained_capacity() >= 188);
        }
        if cut == 20 {
            assert_eq!(packet.spans.as_slice()[0].source.offset, 558);
            assert_eq!(packet.spans.as_slice()[1].source.offset, 731);
        }
    }
}

fn require_nonpass(observer: &mut NativePesObserver) {
    for _ in 0..16_384 {
        match observer.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(AssemblyStep::Yield) => (),
            Err(Error::Unsupported(_)) => return,
            other => panic!("unproven audio context was emitted/silently accepted: {other:?}"),
        }
    }
    panic!("nonpass not reached with bounded work");
}
#[test]
fn audio_incomplete_eof_and_next_pusi_are_explicit_nonpasses_not_partial_native_packets() {
    for next_pusi in [false, true] {
        let budget = Budget::new(16_384).unwrap();
        let mut observer = observer(AC3_DVB, &budget);
        let data = pes(PAYLOAD, 90_000);
        let raw = transport(&data[..20], true, 0);
        observer
            .push(carrier(&raw, &budget, 376), context(8))
            .unwrap();
        assert!(drain(&mut observer).is_empty());
        if next_pusi {
            let next = transport(&data, true, 1);
            observer
                .push(carrier(&next, &budget, 564), context(9))
                .unwrap();
        } else {
            observer.end_of_source().unwrap();
        }
        require_nonpass(&mut observer);
    }
}
#[test]
fn audio_counter_damage_unbounded_length_and_finite_overrun_are_not_h264_recovery() {
    for mode in 0..3 {
        let budget = Budget::new(16_384).unwrap();
        let mut observer = observer(EAC3_DIRECT, &budget);
        let mut data = pes(PAYLOAD, 90_000);
        if mode == 0 {
            let raw = transport(&data[..20], true, 0);
            observer
                .push(carrier(&raw, &budget, 376), context(8))
                .unwrap();
            assert!(drain(&mut observer).is_empty());
            let raw = transport(&data[20..], false, 2);
            observer
                .push(carrier(&raw, &budget, 564), context(9))
                .unwrap();
        } else {
            if mode == 1 {
                data[4] = 0;
                data[5] = 0;
            } else {
                data.push(0xcc);
            }
            let raw = transport(&data, true, 0);
            observer
                .push(carrier(&raw, &budget, 376), context(8))
                .unwrap();
        }
        require_nonpass(&mut observer);
    }
}

#[test]
fn audio_composed_wrapper_explicit_policy_uses_real_accepted_packets_only() {
    let budget = Budget::new(16_384).unwrap();
    let mut observer = NativePesRecoveryObserver::new_private_audio_v1(
        NativeClockProfile::ArchivedPesV1,
        scope(AC3_DVB),
        AC3_DVB.carriage,
        descriptors(AC3_DVB),
        budget.clone(),
        ParseLimits::default(),
    )
    .unwrap();
    assert_eq!(observer.policy(), NativeRejectedPolicy::NoRejectedCarrierV1);
    observer
        .push_accepted(original(AC3_DVB, &budget), context(8))
        .unwrap();
    let mut count = 0;
    for _ in 0..16_384 {
        let mut work = WorkBudget::new(1);
        match observer.poll(&mut work, &AtomicBool::new(false)).unwrap() {
            AssemblyStep::Yield => (),
            AssemblyStep::Output(NativePesObservation::Accepted(packet)) => {
                check_packet(&packet, AC3_DVB);
                count += 1;
            }
            AssemblyStep::NeedPayload => break,
            other => panic!("private audio was represented as rejected recovery: {other:?}"),
        }
        assert!(work.spent() <= 1);
    }
    assert_eq!(count, 1);
}

#[test]
fn audio_rejected_carrier_stays_original_invalid_source_and_does_not_advance_native_input() {
    let budget = Budget::new(16_384).unwrap();
    let mut observer = NativePesRecoveryObserver::new_private_audio_v1(
        NativeClockProfile::ArchivedPesV1,
        scope(AC3_DVB),
        AC3_DVB.carriage,
        descriptors(AC3_DVB),
        budget.clone(),
        ParseLimits::default(),
    )
    .unwrap();
    let raw = include_bytes!("data/native-pes-v1/opcr-without-pcr-invalid.ts");
    let owner = Bytes::copy_from_slice(&budget, &raw[376..564]).unwrap();
    let mut inspector = TransportInspector::new(
        owner.clone(),
        PacketFormat::Ts188,
        SourceLocation {
            source: SourceId(1),
            offset: 376,
        },
        &budget,
    )
    .unwrap();
    let rejection = loop {
        match inspector
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            AssemblyStep::Yield => (),
            AssemblyStep::Output(TransportInspection::Rejected(value)) => break value,
            other => panic!("original OPCR fault changed: {other:?}"),
        }
    };
    let error = observer.push_rejected(rejection, context(8)).unwrap_err();
    assert_eq!(
        error.error,
        Error::Unsupported("native private audio rejected carrier unobserved")
    );
    assert_eq!(error.value.error(), Error::Invalid("OPCR without PCR"));
    assert_eq!(error.value.source_offset(), 376);
    assert!(error.value.bytes().shares_storage_with(&owner));
    assert_eq!(error.value.bytes().as_ref(), &raw[376..564]);
    // The unsupported opt-in observation does not poison/reseed its real source.
    observer
        .push_accepted(original(AC3_DVB, &budget), context(8))
        .unwrap();
    let mut packets = 0;
    for _ in 0..16_384 {
        match observer
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            AssemblyStep::Yield => (),
            AssemblyStep::Output(NativePesObservation::Accepted(packet)) => {
                check_packet(&packet, AC3_DVB);
                packets += 1;
            }
            AssemblyStep::NeedPayload => break,
            other => panic!("forged or extra recovery output: {other:?}"),
        }
    }
    assert_eq!(packets, 1);
}
