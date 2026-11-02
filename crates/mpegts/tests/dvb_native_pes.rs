//! Original descriptor/PES controls. Opaque DVB data fields remain complete;
//! no inner subtitle/AU parser, peer-derived oracle or source normalization.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, WorkBudget};
use triergon_core::{InputCursor, InputWindow, SourceCapabilities, Step};
use triergon_mpegts::clock::{NativeClockProfile, NativePesClockV1};
use triergon_mpegts::native_pes::{
    NativeDvbSubtitlesDescriptors, NativePesBoundary, NativePesObserver, NativePesPacket,
    NativePesPayloadKind, NativePesScope, NativeSourceContext,
};
use triergon_mpegts::native_recovery::{
    NativePesObservation, NativePesRecoveryObserver, NativeRejectedPolicy,
};
use triergon_mpegts::psi::{
    CodecIdentification, DescriptorNamespace, Descriptors, DvbNamespaceEvidence,
};
use triergon_mpegts::{
    AssemblyStep, PacketFormat, ParseLimits, SourceEpochs, TraceEvent, TransportPacket,
    TransportStream, parse_transport,
};

const EPOCH: SourceEpochs = SourceEpochs {
    clock: 0,
    configuration: 2,
};
const NS: DescriptorNamespace =
    DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile);
const DS: &[u8] = &[0x59, 8, b'e', b'n', b'g', 0x14, 0, 1, 0, 2];
const BODY: &[u8] = &[0x20, 0, 0x0f, 0x80, 0, 1, 0, 0, 0xff];
const RAW: &[u8] = include_bytes!("data/dvb/dvb2-literals-fields.ts");

fn scope() -> NativePesScope {
    NativePesScope {
        program: 1,
        pid: 256,
        stream_type: 6,
        stream_id: 0xbd,
        entity: SourceId(1),
        epochs: EPOCH,
    }
}
fn desc(raw: &[u8]) -> NativeDvbSubtitlesDescriptors<'_> {
    NativeDvbSubtitlesDescriptors {
        program: &[],
        elementary: raw,
    }
}
fn context(n: u64) -> NativeSourceContext {
    NativeSourceContext {
        source_event_ordinal: n,
        epochs: EPOCH,
        byte_run_epoch: None,
    }
}
fn make(b: &Budget, limits: ParseLimits) -> NativePesObserver {
    NativePesObserver::new_dvb_subtitles_v1(
        NativeClockProfile::ArchivedPesV1,
        scope(),
        NS,
        desc(DS),
        b.clone(),
        limits,
    )
    .expect("actual DVB finite PES feature stub must become an admitted observer")
}
fn packet(b: &Budget, raw: &[u8], offset: u64) -> TransportPacket {
    let mut p = parse_transport(
        Bytes::copy_from_slice(b, raw).unwrap(),
        PacketFormat::Ts188,
        offset,
    )
    .unwrap();
    p.source_id = Some(SourceId(1));
    p
}
fn timestamp(v: u64, prefix: u8) -> [u8; 5] {
    [
        (prefix << 4) | (((v >> 30) as u8 & 7) << 1) | 1,
        (v >> 22) as u8,
        (((v >> 15) as u8 & 0x7f) << 1) | 1,
        (v >> 7) as u8,
        ((v as u8 & 0x7f) << 1) | 1,
    ]
}
fn pes(body: &[u8], pts: Option<u64>, dts: Option<u64>) -> Vec<u8> {
    assert!(dts.is_none() || pts.is_some());
    let mut header = Vec::new();
    if let Some(p) = pts {
        header.extend_from_slice(&timestamp(p, if dts.is_some() { 3 } else { 2 }));
    }
    if let Some(d) = dts {
        header.extend_from_slice(&timestamp(d, 1));
    }
    let len = u16::try_from(3 + header.len() + body.len()).unwrap();
    let mut p = vec![
        0,
        0,
        1,
        0xbd,
        (len >> 8) as u8,
        len as u8,
        0x84,
        if dts.is_some() {
            0xc0
        } else if pts.is_some() {
            0x80
        } else {
            0
        },
        header.len() as u8,
    ];
    p.extend(header);
    p.extend_from_slice(body);
    p
}
fn ts(payload: &[u8], start: bool, cc: u8) -> [u8; 188] {
    assert!(payload.len() <= 182);
    let mut p = [0xff; 188];
    p[..4].copy_from_slice(&[0x47, 1 | if start { 0x40 } else { 0 }, 0, 0x30 | cc]);
    let n = 183 - payload.len();
    p[4] = n as u8;
    p[5] = 0;
    p[5 + n..].copy_from_slice(payload);
    p
}
fn drain(o: &mut NativePesObserver) -> Vec<NativePesPacket> {
    let mut out = Vec::new();
    for _ in 0..16384 {
        let mut w = WorkBudget::new(1);
        match o.poll(&mut w, &AtomicBool::new(false)).unwrap() {
            AssemblyStep::Output(p) => out.push(p),
            AssemblyStep::Yield => (),
            AssemblyStep::NeedPayload => return out,
            AssemblyStep::Finished => panic!("starvation is not entity EOF"),
        };
        assert!(w.spent() <= 1);
    }
    panic!("bounded field work stalled")
}
fn check(p: &NativePesPacket, body: &[u8], offset: u64, pts: Option<u64>, dts: Option<u64>) {
    assert_eq!((p.program, p.pid, p.native_stream_id), (1, 256, 0xbd));
    assert_eq!(p.source_epochs, EPOCH);
    assert_eq!(
        (p.native_position, p.native_flags, p.native_duration90_raw),
        (offset as i64, 0, 0)
    );
    assert_eq!(p.source_origin.source, SourceId(1));
    assert_eq!(p.source_origin.offset, offset);
    assert_eq!(p.boundary, NativePesBoundary::FiniteLength);
    assert_eq!(p.payload_kind, NativePesPayloadKind::DeclaredComplete);
    assert_eq!(p.payload_len, body.len());
    assert!(p.payload().flatten().copied().eq(body.iter().copied()));
    assert_eq!(
        (p.source_header.pts_raw33, p.source_header.dts_raw33),
        (pts, dts)
    );
    assert!(p.time_header_applies);
    assert_eq!(
        p.spans
            .as_slice()
            .iter()
            .map(|s| s.as_ref().len())
            .sum::<usize>(),
        body.len()
    );
    for s in p.spans.as_slice() {
        assert_eq!(s.context.epochs, EPOCH);
        assert!(s.backing_bytes().retained_capacity() >= 188);
        assert!(s.backing_bytes().retained_storage_bytes() > s.backing_bytes().retained_capacity());
    }
    let mut c = NativePesClockV1::new(1).unwrap();
    let t = c
        .project_native_header(
            256,
            EPOCH,
            p.time_header(),
            &mut WorkBudget::new(1),
            &AtomicBool::new(false),
        )
        .unwrap()
        .unwrap();
    assert_eq!((t.source_pts_raw33, t.source_dts_raw33), (pts, dts));
    assert_eq!(
        (t.pts90, t.dts90),
        (pts.map(i128::from), dts.or(pts).map(i128::from))
    );
}
fn original(raw: &[u8], body: &[u8], descriptor: &[u8]) {
    let b = Budget::new(32768).unwrap();
    let mut o = NativePesObserver::new_dvb_subtitles_v1(
        NativeClockProfile::ArchivedPesV1,
        scope(),
        NS,
        desc(descriptor),
        b.clone(),
        ParseLimits::default(),
    )
    .expect("actual original D0 feature stub");
    let mut output = Vec::new();
    for (index, bytes) in raw.chunks_exact(188).enumerate() {
        let p = packet(&b, bytes, (index * 188) as u64);
        if p.pid == 256 {
            o.push(p, context(index as u64)).unwrap();
            output.extend(drain(&mut o));
        }
    }
    assert_eq!(output.len(), 1);
    check(&output[0], body, 376, Some(90000), None);
    o.end_of_source().unwrap();
    assert!(matches!(
        o.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Ok(AssemblyStep::Finished)
    ));
}
macro_rules! original_case {
    ($name:ident,$id:literal,$descriptor:expr) => {
        #[test]
        fn $name() {
            original(
                include_bytes!(concat!("data/dvb/", $id, ".ts")),
                include_bytes!(concat!("data/dvb/", $id, ".pes-data")),
                $descriptor,
            );
        }
    };
}
original_case!(dvb_actual_literals_full_pes, "dvb2-literals-fields", DS);
original_case!(dvb_actual_ancillary_full_pes, "dvb-ancillary-pages", DS);
original_case!(
    dvb_actual_default_clut_full_pes,
    "dvb-default-clut2",
    &[0x59, 8, b'e', b'n', b'g', 0x10, 0, 1, 0, 2]
);
original_case!(dvb_actual_nonmodifying_full_pes, "dvb-nonmodifying", DS);
original_case!(
    dvb_actual_alpha_full_pes,
    "dvb-partial-alpha-projection",
    DS
);
original_case!(
    dvb_actual_truncated_inner_tail_stays_opaque,
    "dvb-truncated-segment",
    DS
);

#[test]
fn dvb_namespace_scope_preflight_before_stub() {
    let b = Budget::new(8192).unwrap();
    let n = b.used();
    assert_eq!(
        NativePesObserver::new_dvb_subtitles_v1(
            NativeClockProfile::ArchivedPesV1,
            scope(),
            DescriptorNamespace::Unspecified,
            desc(DS),
            b.clone(),
            ParseLimits::default()
        )
        .unwrap_err(),
        Error::Unsupported("native DVB namespace not declared")
    );
    for s in [
        NativePesScope {
            program: 0,
            ..scope()
        },
        NativePesScope {
            stream_type: 0x1b,
            ..scope()
        },
        NativePesScope {
            stream_id: 0xe0,
            ..scope()
        },
    ] {
        assert!(
            NativePesObserver::new_dvb_subtitles_v1(
                NativeClockProfile::ArchivedPesV1,
                s,
                NS,
                desc(DS),
                b.clone(),
                ParseLimits::default()
            )
            .is_err()
        );
    }
    assert_eq!(b.used(), n);
}
#[test]
fn dvb_malformed_tlv_primary_error() {
    let b = Budget::new(8192).unwrap();
    for raw in [&[0x59, 8, 1][..], &[0x59, 1, 0][..]] {
        assert!(matches!(
            NativePesObserver::new_dvb_subtitles_v1(
                NativeClockProfile::ArchivedPesV1,
                scope(),
                NS,
                desc(raw),
                b.clone(),
                ParseLimits::default()
            ),
            Err(Error::Invalid(_))
        ));
    }
}
#[test]
fn dvb_multiplicity_no_first_service_fallback() {
    let b = Budget::new(8192).unwrap();
    let mut two = vec![0x59, 16];
    two.extend_from_slice(&DS[2..]);
    two.extend_from_slice(&DS[2..]);
    let mut dup = DS.to_vec();
    dup.extend_from_slice(DS);
    for raw in [&[0x59, 0][..], &two, &dup] {
        assert!(matches!(
            NativePesObserver::new_dvb_subtitles_v1(
                NativeClockProfile::ArchivedPesV1,
                scope(),
                NS,
                desc(raw),
                b.clone(),
                ParseLimits::default()
            ),
            Err(Error::Unsupported(_))
        ));
    }
}
#[test]
fn dvb_program_and_extra_context_are_nonpasses() {
    let b = Budget::new(8192).unwrap();
    let mut extra = DS.to_vec();
    extra.extend_from_slice(&[0x6a, 1, 0]);
    for d in [
        NativeDvbSubtitlesDescriptors {
            program: &[0x05, 4, b'T', b'E', b'S', b'T'],
            elementary: DS,
        },
        desc(&extra),
    ] {
        assert!(matches!(
            NativePesObserver::new_dvb_subtitles_v1(
                NativeClockProfile::ArchivedPesV1,
                scope(),
                NS,
                d,
                b.clone(),
                ParseLimits::default()
            ),
            Err(Error::Unsupported(_))
        ));
    }
}
#[test]
fn dvb_real_descriptor_limit_before_feature() {
    let b = Budget::new(8192).unwrap();
    let limits = ParseLimits {
        section_bytes: 9,
        ..ParseLimits::default()
    };
    assert_eq!(
        NativePesObserver::new_dvb_subtitles_v1(
            NativeClockProfile::ArchivedPesV1,
            scope(),
            NS,
            desc(DS),
            b,
            limits
        )
        .unwrap_err(),
        Error::Limit {
            requested: 10,
            maximum: 9
        }
    );
}
#[test]
fn dvb_default_source_namespace_and_dts_stay_unknown() {
    let b = Budget::new(65536).unwrap();
    let mut demux =
        TransportStream::new(b.clone(), PacketFormat::Ts188, ParseLimits::default(), 1).unwrap();
    let mut input = InputCursor::new(
        &b,
        SourceId(1),
        SourceCapabilities {
            length: Some(RAW.len() as u64),
            random_access: false,
        },
        2048,
    )
    .unwrap();
    let mut maps = 0;
    let mut pes_count = 0;
    for _ in 0..65536 {
        match demux
            .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(TraceEvent::ProgramMap { map, .. }) => {
                maps += 1;
                assert_eq!(map.descriptor_namespace, DescriptorNamespace::Unspecified);
                assert_eq!(
                    map.streams.as_slice()[0].codec_identification(),
                    CodecIdentification::Unknown
                );
            }
            Step::Ready(TraceEvent::Pes(p)) => {
                pes_count += 1;
                assert_eq!(p.header.dts_raw33, None);
                assert_eq!(p.header.pts_raw33, Some(90000));
            }
            Step::Ready(TraceEvent::Finished) => {
                assert_eq!((maps, pes_count), (1, 1));
                return;
            }
            Step::Ready(_) | Step::Yield => (),
            Step::NeedInput(r) => {
                let at = r.offset as usize;
                if at == RAW.len() {
                    input.finish(r.source, r.token).unwrap();
                } else {
                    let end = (at + r.maximum).min(RAW.len());
                    input
                        .provide(
                            &InputWindow::new(
                                r.source,
                                r.token,
                                r.offset,
                                Bytes::copy_from_slice(&b, &RAW[at..end]).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                }
            }
            Step::End(_) => demux.end_of_source().unwrap(),
        }
    }
    panic!("source work bound");
}
#[test]
fn dvb_raw_reserved_language_type_never_canonicalized() {
    let raw = [0x59, 8, 0xff, 0, 0x80, 0xfe, 0x12, 0x34, 0xab, 0xcd];
    let d = Descriptors::new(&raw)
        .next()
        .unwrap()
        .unwrap()
        .dvb_subtitling()
        .unwrap();
    let s = d.services().next().unwrap();
    assert_eq!(s.source, &raw[2..]);
    assert_eq!(s.language_code, [0xff, 0, 0x80]);
    assert_eq!(s.subtitling_type, 0xfe);
    assert_eq!(
        (s.composition_page_id, s.ancillary_page_id),
        (0x1234, 0xabcd)
    );
}

#[test]
fn dvb_constructor_borrowed_preflight_owns_no_auxiliary_arena() {
    let b = Budget::new(8192).unwrap();
    let used = b.used();
    let o = make(&b, ParseLimits::default());
    assert_eq!(b.used(), used);
    drop(o);
    assert_eq!(b.used(), used);
}
#[test]
fn dvb_work_zero_one_real_progress() {
    let b = Budget::new(16384).unwrap();
    let mut o = make(&b, ParseLimits::default());
    o.push(
        packet(&b, &ts(&pes(BODY, Some(90000), None), true, 0), 376),
        context(8),
    )
    .unwrap();
    let used = b.used();
    let mut w = WorkBudget::new(0);
    assert!(matches!(
        o.poll(&mut w, &AtomicBool::new(false)),
        Ok(AssemblyStep::Yield)
    ));
    assert_eq!(w.spent(), 0);
    assert_eq!(b.used(), used);
    let v = drain(&mut o);
    assert_eq!(v.len(), 1);
    check(&v[0], BODY, 376, Some(90000), None);
    assert!(drain(&mut o).is_empty());
}
#[test]
fn dvb_cancel_real_first_work_resumes() {
    let b = Budget::new(16384).unwrap();
    let mut o = make(&b, ParseLimits::default());
    o.push(
        packet(&b, &ts(&pes(BODY, Some(90000), None), true, 0), 376),
        context(8),
    )
    .unwrap();
    assert!(matches!(
        o.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Ok(AssemblyStep::Yield)
    ));
    let used = b.used();
    let cancel = AtomicBool::new(true);
    let mut w = WorkBudget::new(31);
    assert!(matches!(o.poll(&mut w, &cancel), Err(Error::Cancelled)));
    assert_eq!(w.spent(), 0);
    assert_eq!(b.used(), used);
    cancel.store(false, Ordering::Relaxed);
    let v = drain(&mut o);
    assert_eq!(v.len(), 1);
    check(&v[0], BODY, 376, Some(90000), None);
}
#[test]
fn dvb_pressure_retry_and_owned_teardown() {
    let b = Budget::new(16384).unwrap();
    let before = b.used();
    let mut o = make(&b, ParseLimits::default());
    o.push(
        packet(&b, &ts(&pes(BODY, Some(90000), None), true, 0), 376),
        context(8),
    )
    .unwrap();
    let hold = b.reserve(b.available()).unwrap();
    assert!(matches!(
        o.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    drop(hold);
    let v = drain(&mut o);
    drop(o);
    check(&v[0], BODY, 376, Some(90000), None);
    assert!(b.used() > before);
    drop(v);
    assert_eq!(b.used(), before);
}
#[test]
fn dvb_hidden_owner_zero_copy_spans() {
    let b = Budget::new(16384).unwrap();
    let before = b.used();
    let raw = ts(&pes(BODY, Some(90000), None), true, 0);
    let mut bytes = vec![0xaa; 13];
    bytes.extend_from_slice(&raw);
    bytes.extend_from_slice(&[0xbb; 37]);
    let owner = Bytes::copy_from_slice(&b, &bytes).unwrap();
    let cap = owner.retained_capacity();
    let mut p = parse_transport(owner.slice(13..201).unwrap(), PacketFormat::Ts188, 376).unwrap();
    p.source_id = Some(SourceId(1));
    let mut o = make(&b, ParseLimits::default());
    o.push(p, context(8)).unwrap();
    let v = drain(&mut o);
    assert!(
        v[0].spans.as_slice()[0]
            .backing_bytes()
            .shares_storage_with(&owner)
    );
    assert_eq!(
        v[0].spans.as_slice()[0].backing_bytes().retained_capacity(),
        cap
    );
    drop((owner, o));
    check(&v[0], BODY, 376, Some(90000), None);
    drop(v);
    assert_eq!(b.used(), before);
}
#[test]
fn dvb_two_finite_pes_keep_order_no_eof_repeat() {
    let b = Budget::new(32768).unwrap();
    let mut o = make(&b, ParseLimits::default());
    for (index, pts) in [90000, 180000].into_iter().enumerate() {
        o.push(
            packet(
                &b,
                &ts(&pes(BODY, Some(pts), None), true, index as u8),
                376 + 188 * index as u64,
            ),
            context(8 + index as u64),
        )
        .unwrap();
        let v = drain(&mut o);
        assert_eq!(v.len(), 1);
        check(&v[0], BODY, 376 + 188 * index as u64, Some(pts), None);
    }
    o.end_of_source().unwrap();
    for _ in 0..2 {
        assert!(matches!(
            o.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
            Ok(AssemblyStep::Finished)
        ));
    }
}
#[test]
fn dvb_fragmented_header_and_body_preserve_span_order() {
    for cut in [5, 17] {
        let b = Budget::new(32768).unwrap();
        let mut o = make(&b, ParseLimits::default());
        let p = pes(BODY, Some(90000), None);
        o.push(packet(&b, &ts(&p[..cut], true, 0), 376), context(8))
            .unwrap();
        assert!(drain(&mut o).is_empty());
        o.push(packet(&b, &ts(&p[cut..], false, 1), 564), context(9))
            .unwrap();
        let v = drain(&mut o);
        assert_eq!(v.len(), 1);
        check(&v[0], BODY, 376, Some(90000), None);
        assert!(
            v[0].spans
                .as_slice()
                .iter()
                .all(|s| s.context.source_event_ordinal >= 8)
        );
    }
}
#[test]
fn dvb_generic_h222_optional_times_preserved_not_source_inferred() {
    for (pts, dts) in [(None, None), (Some(90000), Some(81000))] {
        let b = Budget::new(16384).unwrap();
        let mut o = make(&b, ParseLimits::default());
        o.push(
            packet(&b, &ts(&pes(BODY, pts, dts), true, 0), 376),
            context(8),
        )
        .unwrap();
        let v = drain(&mut o);
        assert_eq!(v.len(), 1);
        check(&v[0], BODY, 376, pts, dts);
    }
}
#[test]
fn dvb_foreign_entity_config_changes_return_owned_input() {
    let b = Budget::new(32768).unwrap();
    let foreign = Budget::new(4096).unwrap();
    let mut o = make(&b, ParseLimits::default());
    let raw = ts(&pes(BODY, Some(90000), None), true, 0);
    let p = packet(&foreign, &raw, 376);
    let keep = p.source.clone();
    let e = o.push(p, context(8)).unwrap_err();
    assert!(e.value.source.shares_storage_with(&keep));
    let mut p = packet(&b, &raw, 376);
    p.source_id = Some(SourceId(2));
    assert!(matches!(
        o.push(p, context(8)).unwrap_err().error,
        Error::Unsupported(_)
    ));
    let mut c = context(8);
    c.epochs.configuration += 1;
    assert!(matches!(
        o.push(packet(&b, &raw, 376), c).unwrap_err().error,
        Error::Unsupported(_)
    ));
    o.push(packet(&b, &raw, 376), context(8)).unwrap();
    assert_eq!(drain(&mut o).len(), 1);
}
fn failure(o: &mut NativePesObserver) -> Error {
    for _ in 0..16384 {
        match o.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Err(e) => return e,
            Ok(AssemblyStep::Yield | AssemblyStep::NeedPayload) => (),
            other => panic!("partial output or premature EOF: {other:?}"),
        }
    }
    panic!("fault bound")
}
#[test]
fn dvb_real_pes_limit_no_partial_output() {
    let b = Budget::new(32768).unwrap();
    let mut o = make(
        &b,
        ParseLimits {
            pes_bytes: 20,
            ..ParseLimits::default()
        },
    );
    let body = [0x20; 40];
    o.push(
        packet(&b, &ts(&pes(&body, Some(90000), None), true, 0), 376),
        context(8),
    )
    .unwrap();
    assert!(matches!(failure(&mut o), Error::Limit { .. }));
}
#[test]
fn dvb_real_header_span_limit_not_bypassed() {
    let b = Budget::new(32768).unwrap();
    let mut o = make(
        &b,
        ParseLimits {
            source_spans: 1,
            ..ParseLimits::default()
        },
    );
    let p = pes(BODY, Some(90000), None);
    o.push(packet(&b, &ts(&p[..5], true, 0), 376), context(8))
        .unwrap();
    assert!(drain(&mut o).is_empty());
    o.push(packet(&b, &ts(&p[5..], false, 1), 564), context(9))
        .unwrap();
    assert!(matches!(failure(&mut o), Error::Limit { .. }));
}
#[test]
fn dvb_wrong_sid_and_time_markers_are_bounded_errors() {
    for mode in 0..3 {
        let b = Budget::new(16384).unwrap();
        let mut o = make(&b, ParseLimits::default());
        let mut p = pes(BODY, Some(90000), None);
        match mode {
            0 => p[3] = 0xe0,
            1 => p[7] = 0x40,
            _ => p[9] &= 0xfe,
        };
        o.push(packet(&b, &ts(&p, true, 0), 376), context(8))
            .unwrap();
        assert!(matches!(
            failure(&mut o),
            Error::Invalid(_) | Error::Unsupported(_)
        ));
    }
}
#[test]
fn dvb_incomplete_overrun_counter_damage_not_repaired() {
    for mode in 0..3 {
        let b = Budget::new(32768).unwrap();
        let mut o = make(&b, ParseLimits::default());
        let p = pes(BODY, Some(90000), None);
        if mode == 0 {
            o.push(packet(&b, &ts(&p[..17], true, 0), 376), context(8))
                .unwrap();
            assert!(drain(&mut o).is_empty());
            o.end_of_source().unwrap();
        } else if mode == 1 {
            let mut p = p.clone();
            p.push(0xff);
            o.push(packet(&b, &ts(&p, true, 0), 376), context(8))
                .unwrap();
        } else {
            o.push(packet(&b, &ts(&p[..17], true, 0), 376), context(8))
                .unwrap();
            assert!(drain(&mut o).is_empty());
            o.push(packet(&b, &ts(&p[17..], false, 2), 564), context(9))
                .unwrap();
        }
        assert!(matches!(failure(&mut o), Error::Unsupported(_)));
    }
}
#[test]
fn dvb_wrapper_accepted_real_carriers_uses_no_rejected_policy() {
    let b = Budget::new(32768).unwrap();
    let mut o = NativePesRecoveryObserver::new_dvb_subtitles_v1(
        NativeClockProfile::ArchivedPesV1,
        scope(),
        NS,
        desc(DS),
        b.clone(),
        ParseLimits::default(),
    )
    .expect("DVB wrapper actual feature stub");
    assert_eq!(o.policy(), NativeRejectedPolicy::NoRejectedCarrierV1);
    o.push_accepted(
        packet(&b, &ts(&pes(BODY, Some(90000), None), true, 0), 376),
        context(8),
    )
    .unwrap();
    for _ in 0..16384 {
        match o
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            AssemblyStep::Output(NativePesObservation::Accepted(p)) => {
                check(&p, BODY, 376, Some(90000), None);
                return;
            }
            AssemblyStep::Yield => (),
            other => panic!("unexpected {other:?}"),
        }
    }
    panic!("wrapper work bound");
}
