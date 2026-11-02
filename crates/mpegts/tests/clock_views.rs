//! Red-first clock regressions. Parent executes before any clock implementation.
//! Source facts and archived native expectations are asserted separately.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, Step, Time,
    WorkBudget,
};
use triergon_mpegts::clock::{
    ClockKey, NativeClockProfile, NativePesClockV1, NativeTimestampOrigin, SourceClockAnchor,
    SourceClockSample, SourceTimePolicy, SourceTimeStatus, source_timestamp,
};
use triergon_mpegts::{
    AssemblyStep, PacketFormat, ParseLimits, Pes, PesAssembler, SourceEpochs, TraceEvent,
    TransportStream,
};

const M: i128 = 8_589_934_592;
const KEY: ClockKey = ClockKey {
    tsid: 1,
    program: 1,
    pcr_pid: 256,
    epoch: 0,
};
const BOUND: u64 = 1_288_490_188_799; // strictly below half a PTS period, in 27MHz ticks

fn anchored(sample: SourceClockSample) -> SourceTimePolicy {
    SourceTimePolicy::Bounded(SourceClockAnchor {
        key: KEY,
        sample,
        maximum_distance27: BOUND,
    })
}

fn collect(source: &[u8]) -> Vec<TraceEvent> {
    let pool = Budget::new(1 << 20).unwrap();
    let mut input = InputCursor::new(
        &pool,
        SourceId(91),
        SourceCapabilities {
            length: Some(source.len() as u64),
            random_access: false,
        },
        4096,
    )
    .unwrap();
    let mut demux =
        TransportStream::new(pool.clone(), PacketFormat::Ts188, ParseLimits::default(), 1).unwrap();
    let mut events = Vec::new();
    for _ in 0..200_000 {
        match demux
            .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(TraceEvent::Finished) => return events,
            Step::Ready(event) => events.push(event),
            Step::Yield => {}
            Step::End(_) => demux.end_of_source().unwrap(),
            Step::NeedInput(request) => {
                let offset = request.offset as usize;
                if offset == source.len() {
                    input.finish(request.source, request.token).unwrap();
                } else {
                    let end = source.len().min(offset + request.maximum.min(7));
                    let bytes = Bytes::copy_from_slice(&pool, &source[offset..end]).unwrap();
                    input
                        .provide(
                            &InputWindow::new(request.source, request.token, request.offset, bytes)
                                .unwrap(),
                        )
                        .unwrap();
                }
            }
        }
    }
    panic!("clock source operation failed to progress");
}

fn packets(events: &[TraceEvent]) -> Vec<&Pes> {
    events
        .iter()
        .filter_map(|event| match event {
            TraceEvent::Pes(value) => Some(value),
            _ => None,
        })
        .collect()
}

fn check_native(
    source: &[u8],
    raw_pts: &[Option<u64>],
    raw_dts: &[Option<u64>],
    native_pts: &[Option<i128>],
    native_dts: &[Option<i128>],
) {
    let events = collect(source);
    let pes = packets(&events);
    assert_eq!(pes.len(), raw_pts.len());
    let mut context = NativePesClockV1::new(1).unwrap();
    let profile = NativeClockProfile::ArchivedPesV1;
    assert_eq!(
        profile.source_pin(),
        "d32b387f2b0a484599d4587d651891f0c63c4238"
    );
    assert_eq!(
        profile.worker_sha256(),
        "19c06e53c62852b409eec9771cb11d9d7f5164b4618545a449d7e993f99346df"
    );
    assert_eq!(profile.operation(), "mpegts-native-pes-observation-v1");
    assert_eq!(profile.flags(), 48);
    assert!(profile.default_overflow_correction());
    assert!(!profile.find_stream_info());
    for (index, source) in pes.iter().enumerate() {
        let header = source.header.as_bytes().to_vec();
        let payload = source.payload().flatten().copied().collect::<Vec<_>>();
        assert_eq!(source.header.pts_raw33, raw_pts[index]);
        assert_eq!(source.header.dts_raw33, raw_dts[index]);
        let view = context
            .project(source, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .expect("red until separately named default-native view is implemented")
            .unwrap();
        assert_eq!(view.profile, profile);
        assert_eq!(view.program, 1);
        assert_eq!(view.pid, source.pid);
        assert_eq!(view.source_epochs, source.epochs);
        assert_eq!(view.source_pts_raw33, raw_pts[index]);
        assert_eq!(view.source_dts_raw33, raw_dts[index]);
        assert_eq!(view.pts90, native_pts[index]);
        assert_eq!(view.dts90, native_dts[index]);
        assert_eq!(
            view.pts_origin,
            if raw_pts[index].is_some() {
                NativeTimestampOrigin::SourcePts
            } else {
                NativeTimestampOrigin::Absent
            }
        );
        assert_eq!(
            view.dts_origin,
            if raw_dts[index].is_some() {
                NativeTimestampOrigin::SourceDts
            } else if raw_pts[index].is_some() {
                NativeTimestampOrigin::PtsOnlyCompatibility
            } else {
                NativeTimestampOrigin::Absent
            }
        );
        assert_eq!(source.header.pts_raw33, raw_pts[index]);
        assert_eq!(source.header.dts_raw33, raw_dts[index]);
        assert_eq!(source.header.as_bytes(), header);
        assert_eq!(
            source.payload().flatten().copied().collect::<Vec<_>>(),
            payload
        );
    }
}

#[test]
fn source_absence_and_missing_anchor_are_not_zero_or_native_inference() {
    let absent = source_timestamp(None, KEY, SourceTimePolicy::Unanchored).unwrap();
    assert_eq!(absent.status, SourceTimeStatus::Absent);
    assert_eq!(absent.ticks90, None);
    assert_eq!(absent.exact, None);
    let unknown = source_timestamp(Some(90000), KEY, SourceTimePolicy::Unanchored).unwrap();
    assert_eq!(unknown.raw33, Some(90000));
    assert_eq!(unknown.status, SourceTimeStatus::AnchorBoundMissing);
    assert_eq!(unknown.ticks90, None);
}

#[test]
fn source_period_zero_is_an_explicit_relative_choice() {
    let view = source_timestamp(
        Some((M - 7) as u64),
        KEY,
        SourceTimePolicy::RelativeEpochZero,
    )
    .unwrap();
    assert_eq!(view.ticks90, Some(M - 7));
    assert_eq!(view.exact, Some(Time::new(M - 7, 90000).unwrap()));
    assert_eq!(view.status, SourceTimeStatus::RelativeEpochChoice);
}

#[test]
fn source_forward_wrap_uses_an_admitted_source_anchor_not_native_minus_seven() {
    let view = source_timestamp(Some(4), KEY, anchored(SourceClockSample::Pts90(M - 7))).unwrap();
    assert_eq!(view.raw33, Some(4));
    assert_eq!(view.ticks90, Some(M + 4));
    assert_eq!(view.status, SourceTimeStatus::Anchored);
    assert_eq!(view.exact, Some(Time::new(M + 4, 90000).unwrap()));
}

#[test]
fn source_pair_uses_resolved_dts_as_same_pes_pts_anchor_without_changing_raw33() {
    let events = collect(include_bytes!("data/pts-dts-common-epoch.ts"));
    let pes = packets(&events);
    let mut previous = M - 6;
    for (packet, dts, pts) in [(pes[0], M - 6, M + 2), (pes[1], M + 3, M + 7)] {
        let source_dts = source_timestamp(
            packet.header.dts_raw33,
            KEY,
            anchored(SourceClockSample::Pts90(previous)),
        )
        .unwrap();
        assert_eq!(source_dts.ticks90, Some(dts));
        let source_pts = source_timestamp(
            packet.header.pts_raw33,
            KEY,
            anchored(SourceClockSample::Pts90(dts)),
        )
        .unwrap();
        assert_eq!(source_pts.ticks90, Some(pts));
        assert_eq!(packet.header.pts_raw33, Some((pts % M) as u64));
        previous = dts;
    }
}

#[test]
fn source_negative_pts_and_native_positive_pts_are_distinct_interpretations() {
    let view = source_timestamp(
        Some((M - 2) as u64),
        KEY,
        anchored(SourceClockSample::Pts90(3)),
    )
    .unwrap();
    assert_eq!(view.raw33, Some((M - 2) as u64));
    assert_eq!(view.ticks90, Some(-2));
    assert_eq!(view.exact, Some(Time::new(-2, 90000).unwrap()));
}

#[test]
fn source_half_period_is_ambiguous_and_keeps_raw_value() {
    let view = source_timestamp(
        Some((M / 2) as u64),
        KEY,
        anchored(SourceClockSample::Pts90(0)),
    )
    .unwrap();
    assert_eq!(view.status, SourceTimeStatus::AmbiguousHalfPeriod);
    assert_eq!(view.raw33, Some((M / 2) as u64));
    assert_eq!(view.ticks90, None);
}

#[test]
fn pcr_fraction_is_not_rounded_before_selecting_a_source_wrap() {
    let view = source_timestamp(
        Some((M / 2) as u64),
        KEY,
        anchored(SourceClockSample::Pcr27(1)),
    )
    .unwrap();
    assert_eq!(view.ticks90, Some(M / 2));
    assert_eq!(view.status, SourceTimeStatus::Anchored);
    let view = source_timestamp(
        Some((M / 2) as u64),
        KEY,
        anchored(SourceClockSample::Pcr27(-1)),
    )
    .unwrap();
    assert_eq!(view.ticks90, Some(-M / 2));
}

#[test]
fn anchor_program_pcr_and_epoch_identity_must_match_and_distance_is_explicit() {
    for foreign in [
        ClockKey { program: 2, ..KEY },
        ClockKey { tsid: 2, ..KEY },
        ClockKey {
            pcr_pid: 300,
            ..KEY
        },
        ClockKey { epoch: 1, ..KEY },
    ] {
        let policy = SourceTimePolicy::Bounded(SourceClockAnchor {
            key: foreign,
            sample: SourceClockSample::Pts90(0),
            maximum_distance27: BOUND,
        });
        let view = source_timestamp(Some(1), KEY, policy).unwrap();
        assert_eq!(view.status, SourceTimeStatus::AnchorIdentityMismatch);
        assert_eq!(view.ticks90, None);
    }
    let policy = SourceTimePolicy::Bounded(SourceClockAnchor {
        key: KEY,
        sample: SourceClockSample::Pts90(0),
        maximum_distance27: 299,
    });
    assert_eq!(
        source_timestamp(Some(1), KEY, policy).unwrap().status,
        SourceTimeStatus::OutsideAnchorBound
    );
}

#[test]
fn extreme_source_anchor_does_not_overflow_an_unnecessary_times_300_intermediate() {
    for anchor in [i128::MIN + 10, i128::MAX - 10] {
        let raw = anchor.rem_euclid(M) as u64;
        assert_eq!(
            source_timestamp(Some(raw), KEY, anchored(SourceClockSample::Pts90(anchor)))
                .unwrap()
                .ticks90,
            Some(anchor)
        );
    }
    let raw = (i128::MAX.rem_euclid(M) + 1).rem_euclid(M) as u64;
    assert_eq!(
        source_timestamp(
            Some(raw),
            KEY,
            anchored(SourceClockSample::Pts90(i128::MAX))
        ),
        Err(Error::Overflow)
    );
}

#[test]
fn out_of_range_raw33_is_rejected_instead_of_masked() {
    assert_eq!(
        source_timestamp(Some(M as u64), KEY, SourceTimePolicy::RelativeEpochZero),
        Err(Error::Invalid("timestamp raw33"))
    );
}

// Exact original native values: source/stdout identities are in the lane receipt.
#[test]
fn archived_native_forward_wrap_keeps_source_pts_raw_and_dts_absent() {
    check_native(
        include_bytes!("data/pts-wrap-forward.ts"),
        &[Some((M - 7) as u64), Some(4)],
        &[None, None],
        &[Some(-7), Some(4)],
        &[Some(-7), Some(4)],
    );
}

#[test]
fn archived_native_common_epoch_pair_is_not_a_raw33_rewrite() {
    check_native(
        include_bytes!("data/pts-dts-common-epoch.ts"),
        &[Some(2), Some(7)],
        &[Some((M - 6) as u64), Some(3)],
        &[Some(2), Some(7)],
        &[Some(-6), Some(3)],
    );
}

#[test]
fn archived_native_small_dts_does_not_sign_extend_near_end_pts() {
    check_native(
        include_bytes!("data/pts-negative-against-dts.ts"),
        &[Some((M - 2) as u64)],
        &[Some(3)],
        &[Some(M - 2)],
        &[Some(3)],
    );
}

#[test]
fn archived_native_half_period_value_does_not_resolve_source_ambiguity() {
    check_native(
        include_bytes!("data/pts-half-wrap-ambiguous.ts"),
        &[Some(0), Some((M / 2) as u64)],
        &[None, None],
        &[Some(0), Some(M / 2)],
        &[Some(0), Some(M / 2)],
    );
}

#[test]
fn archived_native_pcr_discontinuity_keeps_source_epoch_and_native_context_separate() {
    let source = include_bytes!("data/pcr-timeline-discontinuity.ts");
    let events = collect(source);
    let pes = packets(&events);
    assert_eq!((pes[0].epochs.clock, pes[1].epochs.clock), (0, 1));
    check_native(
        source,
        &[Some(90000), Some(10)],
        &[None, None],
        &[Some(90000), Some(10)],
        &[Some(90000), Some(10)],
    );
}

fn no_timestamps() -> Pes {
    // Original finite PES, flags00, one-byte payload. No fake sentinel timestamp.
    let pool = Budget::new(1 << 20).unwrap();
    let mut assembler = PesAssembler::new(
        256,
        true,
        pool.clone(),
        ParseLimits::default(),
        SourceEpochs {
            clock: 0,
            configuration: 2,
        },
    )
    .unwrap();
    assembler
        .push(
            Bytes::copy_from_slice(&pool, &[0, 0, 1, 0xe0, 0, 4, 0x80, 0, 0, b'x']).unwrap(),
            true,
            SourceId(92),
            17,
        )
        .unwrap();
    for _ in 0..100 {
        match assembler
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            AssemblyStep::Output(pes) => return pes,
            AssemblyStep::Yield => {}
            other => panic!("PTS-absent PES incomplete: {other:?}"),
        }
    }
    panic!("PTS-absent source made no progress");
}

#[test]
fn missing_timestamp_packet_does_not_choose_native_initial_wrap_origin() {
    let missing = no_timestamps();
    let events = collect(include_bytes!("data/pts-wrap-forward.ts"));
    let pes = packets(&events);
    let mut context = NativePesClockV1::new(1).unwrap();
    let empty = context
        .project(&missing, &mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
        .unwrap();
    assert_eq!((empty.pts90, empty.dts90), (None, None));
    assert_eq!(
        (empty.pts_origin, empty.dts_origin),
        (NativeTimestampOrigin::Absent, NativeTimestampOrigin::Absent)
    );
    let first = context
        .project(pes[0], &mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
        .unwrap();
    assert_eq!((first.pts90, first.dts90), (Some(-7), Some(-7)));
    assert_eq!(missing.header.dts_raw33, None);
}

#[test]
fn native_view_yield_and_cancel_do_not_seed_history_or_mutate_source() {
    let events = collect(include_bytes!("data/pts-wrap-forward.ts"));
    let pes = packets(&events);
    let mut context = NativePesClockV1::new(1).unwrap();
    let cancel = AtomicBool::new(false);
    assert!(
        context
            .project(pes[1], &mut WorkBudget::new(0), &cancel)
            .unwrap()
            .is_none()
    );
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        context.project(pes[1], &mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    );
    cancel.store(false, Ordering::Relaxed);
    let view = context
        .project(pes[0], &mut WorkBudget::new(1), &cancel)
        .unwrap()
        .unwrap();
    assert_eq!(view.pts90, Some(-7));
    assert_eq!(pes[0].header.pts_raw33, Some((M - 7) as u64));
    assert_eq!(pes[0].header.dts_raw33, None);
}

// Source signal fixtures below are authored from H.222.0 2.4.3.5. They are
// original test inputs, not native observations and not decoder/AU fixtures.
fn adaptation(pid: u16, signalled: bool, pcr: Option<u64>) -> Vec<u8> {
    let mut packet = vec![0xff; 188];
    packet[..6].copy_from_slice(&[
        0x47,
        (pid >> 8) as u8,
        pid as u8,
        0x20,
        183,
        if signalled { 0x80 } else { 0 } | if pcr.is_some() { 0x10 } else { 0 },
    ]);
    if let Some(base) = pcr {
        packet[6..12].copy_from_slice(&[
            (base >> 25) as u8,
            (base >> 17) as u8,
            (base >> 9) as u8,
            (base >> 1) as u8,
            ((base & 1) << 7) as u8 | 0x7e,
            0,
        ]);
    }
    packet
}

fn signal_source(events: &[(bool, Option<u64>)]) -> Vec<u8> {
    let mut source = include_bytes!("data/ts188-basic.ts")[..376].to_vec();
    for (signalled, pcr) in events {
        source.extend(adaptation(256, *signalled, *pcr));
    }
    source
}

fn clock_fault(events: &[TraceEvent], reason: &'static str, offset: u64) {
    assert!(events.iter().any(|event| matches!(event, TraceEvent::Diagnostic(fault) if fault.error == Error::Invalid(reason) && fault.location.offset == offset)), "missing source clock diagnostic {reason} at {offset}");
}

#[test]
fn valid_pending_pcr_signal_persists_through_new_clock_sample() {
    let events = collect(&signal_source(&[
        (true, None),
        (true, None),
        (true, Some(10)),
    ]));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, TraceEvent::Timeline { clock_epoch: 1, .. }))
            .count(),
        1
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, TraceEvent::Diagnostic(_)))
    );
}

#[test]
fn cleared_pcr_signal_is_a_diagnostic_not_a_trusted_new_clock() {
    let events = collect(&signal_source(&[(true, None), (false, Some(10))]));
    clock_fault(&events, "PCR discontinuity signal dropped", 564);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, TraceEvent::Timeline { .. }))
    );
    assert!(events.iter().any(|e| matches!(e, TraceEvent::Transport(p) if p.source_offset == 564 && p.adaptation.as_ref().and_then(|a| a.pcr).is_some_and(|pcr| pcr.base == 10))));
}

#[test]
fn another_clock_boundary_requires_two_new_pcr_samples() {
    let events = collect(&signal_source(&[
        (true, Some(90000)),
        (false, None),
        (true, Some(10)),
    ]));
    clock_fault(&events, "clock discontinuity before two PCR samples", 752);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, TraceEvent::Timeline { .. }))
            .count(),
        1
    );
}

#[test]
fn next_valid_clock_boundary_after_two_samples_preserves_both_epochs() {
    let events = collect(&signal_source(&[
        (true, Some(90000)),
        (false, Some(90010)),
        (true, Some(10)),
    ]));
    let epochs: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            TraceEvent::Timeline { clock_epoch, .. } => Some(*clock_epoch),
            _ => None,
        })
        .collect();
    assert_eq!(epochs, [1, 2]);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, TraceEvent::Diagnostic(_)))
    );
}

fn crc(data: &[u8]) -> u32 {
    let mut value = 0xffff_ffff_u64;
    for byte in data {
        value = (value << 8) ^ (u64::from(*byte) << 32);
        for shift in (0..8).rev() {
            if value & (1_u64 << (32 + shift)) != 0 {
                value ^= 0x1_04c1_1db7_u64 << shift;
            }
        }
    }
    value as u32
}

#[test]
fn pending_old_pcr_signal_is_not_consumed_after_pmt_rebinds_clock_pid() {
    let original = include_bytes!("data/ts188-basic.ts");
    let mut source = original[..376].to_vec();
    source.extend(adaptation(256, true, None));
    let mut pmt = original[188..376].to_vec();
    pmt[3] = (pmt[3] & 0xf0) | 1;
    let payload = if pmt[3] & 0x20 != 0 {
        5 + usize::from(pmt[4])
    } else {
        4
    };
    let start = payload + 1 + usize::from(pmt[payload]);
    let end = start + 3 + ((usize::from(pmt[start + 1] & 15) << 8) | usize::from(pmt[start + 2]));
    pmt[start + 5] = 0xc3;
    pmt[start + 8..start + 10].copy_from_slice(&[0xe1, 0x2c]);
    let checksum = crc(&pmt[start..end - 4]);
    pmt[end - 4..end].copy_from_slice(&checksum.to_be_bytes());
    source.extend(pmt);
    source.extend(adaptation(300, false, Some(10)));
    let events = collect(&source);
    clock_fault(&events, "pending clock signal changed PCR binding", 564);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, TraceEvent::Timeline { .. }))
    );
    assert!(events.iter().any(
        |e| matches!(e, TraceEvent::ProgramMap { map, active: true, .. } if map.pcr_pid == 300)
    ));
}
