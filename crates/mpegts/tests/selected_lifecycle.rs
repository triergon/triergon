//! Source-first lifecycle guards. No native worker is invoked by these tests.
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_mpegts::clock::{NativeClockProfile, NativePesClockV1};
use triergon_mpegts::native_pes::{NativePesScope, NativeSourceContext};
use triergon_mpegts::native_recovery::{
    NativePatRoute, NativePatSelection, NativePesObservation, NativePesRecoveryObserver,
    NativePmtBinding,
};
use triergon_mpegts::psi::{DescriptorNamespace, PatTable, ProgramMap, TableStep};
use triergon_mpegts::{
    AssemblyStep, PacketFormat, ParseLimits, SourceEpochs, TraceEvent, TransportStream,
};
#[path = "support/selected_lifecycle_fixture.rs"]
mod fixtures;
use fixtures::Case;

type Events = Vec<(TraceEvent, SourceEpochs)>;
fn context(n: usize, e: SourceEpochs) -> NativeSourceContext {
    NativeSourceContext {
        source_event_ordinal: n as u64,
        epochs: e,
        byte_run_epoch: None,
    }
}
fn collect_entity(case: Case, b: &Budget, entity: SourceId) -> Events {
    let format = if fixtures::width(case) == 192 {
        PacketFormat::M2ts192
    } else {
        PacketFormat::Ts188
    };
    let source = TransportStream::new(
        b.clone(),
        format,
        ParseLimits::default(),
        fixtures::selected(case),
    )
    .unwrap();
    collect_stream(&fixtures::bytes(case), b, entity, source, 1).0
}

fn collect_stream(
    data: &[u8],
    b: &Budget,
    entity: SourceId,
    mut source: TransportStream,
    quantum: u64,
) -> (Events, Vec<u64>) {
    let mut capacity = Vec::with_capacity(data.len().max(4096));
    capacity.extend_from_slice(data);
    let owner = Bytes::try_from_vec(b, capacity).unwrap();
    let mut input = InputCursor::new(
        b,
        entity,
        SourceCapabilities {
            length: Some(data.len() as u64),
            random_access: false,
        },
        4096,
    )
    .unwrap();
    let mut out = Vec::new();
    let mut ordinals = Vec::new();
    let mut ordinal = 0;
    for _ in 0..100_000 {
        let step = source
            .poll(
                &mut input,
                &mut WorkBudget::new(quantum),
                &AtomicBool::new(false),
            )
            .unwrap();
        ordinal += source.take_feed_omissions();
        match step {
            Step::Ready(e) => {
                let done = matches!(e, TraceEvent::Finished);
                out.push((e, source.source_epochs()));
                ordinals.push(ordinal);
                ordinal += 1;
                if done {
                    return (out, ordinals);
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
            Step::End(_) => source.end_of_source().unwrap(),
        }
    }
    panic!("source did not finish within authored bound");
}

#[test]
fn segment_feed_preserves_source_faults_epochs_and_retained_event_ordinals() {
    let program_changes = [
        Case::Repeat,
        Case::BadMapCrc,
        Case::PendingBody,
        Case::PendingHeader,
    ]
    .map(fixtures::bytes);
    let carrier_cases = [
        include_bytes!("data/pes-multiple-ts.ts").as_slice(),
        include_bytes!("data/cc-loss-mid-pes.ts").as_slice(),
        include_bytes!("data/pcr-timeline-discontinuity.ts").as_slice(),
    ];
    for data in program_changes
        .iter()
        .map(Vec::as_slice)
        .chain(carrier_cases)
    {
        let b = Budget::new(1 << 20).unwrap();
        let limits = ParseLimits::default();
        let ns = DescriptorNamespace::Unspecified;
        let source = TransportStream::new_single_program_segment(b.clone(), limits, ns).unwrap();
        let feed = TransportStream::new_single_program_feed(b.clone(), limits, ns).unwrap();
        let (raw, raw_ordinals) = collect_stream(data, &b, SourceId(1), source, 1);
        let (fed, fed_ordinals) = collect_stream(data, &b, SourceId(1), feed, 4096);
        let key = |(event, epochs): &(TraceEvent, SourceEpochs)| {
            let fault = match event {
                TraceEvent::Diagnostic(fault) => {
                    Some((fault.kind, fault.pid, fault.location, fault.error))
                }
                _ => None,
            };
            (std::mem::discriminant(event), *epochs, fault)
        };
        let expected: Vec<_> = raw_ordinals
            .into_iter()
            .zip(raw.iter())
            .filter(|(_, (event, _))| {
                !matches!(
                    event,
                    TraceEvent::Continuity { .. } | TraceEvent::Pes(_) | TraceEvent::Section(_)
                )
            })
            .map(|(ordinal, event)| (ordinal, key(event)))
            .collect();
        assert_eq!(
            fed_ordinals
                .into_iter()
                .zip(fed.iter())
                .filter(|(_, (event, _))| !matches!(event, TraceEvent::Section(_)))
                .map(|(ordinal, event)| (ordinal, key(event)))
                .collect::<Vec<_>>(),
            expected
        );
    }
}
fn collect(case: Case, b: &Budget) -> Events {
    collect_entity(case, b, SourceId(1))
}
fn pat(events: &Events) -> (usize, &PatTable, SourceEpochs) {
    events
        .iter()
        .enumerate()
        .find_map(|(n, (e, c))| {
            if let TraceEvent::Pat(t) = e {
                Some((n, t, *c))
            } else {
                None
            }
        })
        .expect("complete source PAT")
}
fn maps(events: &Events) -> Vec<(usize, &ProgramMap, SourceEpochs, bool)> {
    events
        .iter()
        .enumerate()
        .filter_map(|(n, (e, c))| {
            if let TraceEvent::ProgramMap { map, active, .. } = e {
                Some((n, map, *c, *active))
            } else {
                None
            }
        })
        .collect()
}
fn pes(events: &Events) -> Vec<(usize, &triergon_mpegts::Pes)> {
    events
        .iter()
        .enumerate()
        .filter_map(|(n, (e, _))| {
            if let TraceEvent::Pes(p) = e {
                Some((n, p))
            } else {
                None
            }
        })
        .collect()
}
fn route(events: &Events, program: u16, b: &Budget) -> NativePatRoute {
    let (n, t, c) = pat(events);
    let mut selection = NativePatSelection::new(t, program, b, context(n, c)).unwrap();
    for _ in 0..4096 {
        let mut w = WorkBudget::new(1);
        match selection
            .poll(&mut w, &AtomicBool::new(false))
            .expect("lifecycle PAT feature runtime RED until implemented")
        {
            TableStep::Yield => assert!(w.spent() <= 1),
            TableStep::Output(Some(r)) => return r,
            other => panic!("selected route expected {other:?}"),
        }
    }
    panic!("bounded PAT selection did not finish");
}
fn bind(events: &Events, b: &Budget) -> (NativePesRecoveryObserver, NativePatRoute) {
    let program = pat(events)
        .1
        .programs
        .as_slice()
        .iter()
        .find(|p| p.program_number == 1)
        .map_or(2, |_| 1);
    let program = if maps(events)[0].1.header.extension == 2 {
        2
    } else {
        program
    };
    let r = route(events, program, b);
    let (n, m, c, _) = maps(events)[0];
    let stream = &m.streams.as_slice()[0];
    let scope = NativePesScope {
        program: r.program(),
        pid: stream.pid,
        stream_type: stream.stream_type,
        stream_id: 0xe0,
        entity: r.entity(),
        epochs: c,
    };
    let mut o = NativePesRecoveryObserver::new_selected_avc_faults_v1(
        NativeClockProfile::ArchivedPesV1,
        scope,
        b.clone(),
        ParseLimits::default(),
    )
    .unwrap();
    {
        let mut job = o.bind_selected_pmt_v1(r, m, context(n, c)).unwrap();
        let mut done = false;
        for _ in 0..4096 {
            match job
                .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                TableStep::Yield => {}
                TableStep::Output(v) => {
                    assert_eq!(v.version, m.header.version);
                    done = true;
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(done);
    }
    (o, r)
}
fn drain(o: &mut NativePesRecoveryObserver) -> Vec<triergon_mpegts::native_pes::NativePesPacket> {
    let mut p = Vec::new();
    for _ in 0..4096 {
        let mut w = WorkBudget::new(1);
        match o.poll(&mut w, &AtomicBool::new(false)).unwrap() {
            AssemblyStep::Yield => assert!(w.spent() <= 1),
            AssemblyStep::Output(NativePesObservation::Accepted(v)) => p.push(v),
            AssemblyStep::NeedPayload => return p,
            other => panic!("unexpected {other:?}"),
        }
    }
    panic!("native observer did not drain");
}
fn push_first(
    o: &mut NativePesRecoveryObserver,
    e: &Events,
) -> Vec<triergon_mpegts::native_pes::NativePesPacket> {
    let (n, p, c) = e
        .iter()
        .enumerate()
        .find_map(|(n, (v, c))| {
            if let TraceEvent::Transport(p) = v {
                if p.pid == o.scope().pid {
                    return Some((n, p, *c));
                }
                None
            } else {
                None
            }
        })
        .unwrap();
    o.push_accepted(p.clone(), context(n, c)).unwrap();
    drain(o)
}
fn rebind(o: &mut NativePesRecoveryObserver, r: NativePatRoute, e: &Events) -> NativePmtBinding {
    let (n, m, c, _) = maps(e)[1];
    let mut job = o
        .rebind_drained_selected_pmt_v1(r, m, context(n, c))
        .unwrap();
    for _ in 0..4096 {
        let mut w = WorkBudget::new(1);
        match job
            .poll(&mut w, &AtomicBool::new(false))
            .expect("lifecycle drained PMT rebind runtime RED until implemented")
        {
            TableStep::Yield => assert!(w.spent() <= 1),
            TableStep::Output(v) => return v,
            other => panic!("unexpected {other:?}"),
        }
    }
    panic!("rebind did not finish");
}
fn drive(
    case: Case,
) -> (
    Budget,
    Events,
    Vec<triergon_mpegts::native_pes::NativePesPacket>,
) {
    let b = Budget::new(1 << 20).unwrap();
    let e = collect(case, &b);
    let (mut o, r) = bind(&e, &b);
    let initial = maps(&e)[0].0;
    let mut result = Vec::new();
    for (n, (v, c)) in e.iter().enumerate().skip(initial + 1) {
        match v {
            TraceEvent::Transport(p) if p.pid == o.scope().pid => {
                o.push_accepted(p.clone(), context(n, *c)).unwrap();
                result.extend(drain(&mut o));
            }
            TraceEvent::ProgramMap { active: true, .. } => {
                let update = rebind(&mut o, r, &e);
                assert_eq!(update.context, context(n, *c));
            }
            TraceEvent::Diagnostic(f) => panic!("positive fixture fault {f:?}"),
            _ => {}
        }
    }
    (b, e, result)
}

#[test]
fn source_multi_program_and_network_entries_remain_ordered() {
    for (case, programs) in [
        (Case::MultiOne, vec![2, 1]),
        (Case::MultiTwo, vec![1, 2]),
        (Case::MultiNetwork, vec![0, 2, 1]),
    ] {
        let b = Budget::new(1 << 20).unwrap();
        let e = collect(case, &b);
        assert_eq!(pat(&e).0, 3);
        assert_eq!(
            pat(&e)
                .1
                .programs
                .as_slice()
                .iter()
                .map(|p| p.program_number)
                .collect::<Vec<_>>(),
            programs
        );
        assert_eq!(maps(&e)[0].0, 7);
        let p = pes(&e);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].0, 10);
        assert_eq!(p[0].1.epochs.configuration, 2);
        assert_eq!(p[0].1.header.dts_raw33, None);
    }
}
#[test]
fn source_multisection_is_atomic_and_keeps_physical_range_order() {
    let b = Budget::new(1 << 20).unwrap();
    let e = collect(Case::MultiSections, &b);
    let (n, t, _) = pat(&e);
    assert_eq!(n, 6);
    assert_eq!(
        t.programs
            .as_slice()
            .iter()
            .map(|p| p.program_number)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        t.source_ranges
            .as_slice()
            .iter()
            .map(|p| (p.section_number, p.range.offset, p.range.bytes))
            .collect::<Vec<_>>(),
        [(0, 360, 16), (1, 172, 16)]
    );
    assert_eq!(pes(&e)[0].0, 13);
}
#[test]
fn source_retype_preserves_raw_header_ranges_epochs_and_dts_unknown() {
    for (case, offsets) in [(Case::Retype188, [560, 936]), (Case::Retype192, [572, 956])] {
        let b = Budget::new(1 << 20).unwrap();
        let e = collect(case, &b);
        let m = maps(&e);
        assert_eq!([m[0].0, m[1].0], [7, 14]);
        assert_eq!([m[0].1.header.version, m[1].1.header.version], [31, 0]);
        let p = pes(&e);
        assert_eq!([p[0].0, p[1].0], [10, 17]);
        for (i, (_, p)) in p.iter().enumerate() {
            assert_eq!(
                p.epochs,
                SourceEpochs {
                    clock: 0,
                    configuration: 2 + i as u64
                }
            );
            assert_eq!(
                p.header.pts_raw33,
                Some(if i == 0 { 90000 } else { 180000 })
            );
            assert_eq!(p.header.dts_raw33, None);
            assert_eq!(p.source_ranges().next().unwrap().offset, offsets[i]);
            assert_eq!(
                p.payload().flatten().copied().collect::<Vec<_>>(),
                if i == 0 { b"AVC0" } else { b"HEV1" }
            );
        }
    }
}
#[test]
fn source_wrap_does_not_rewrite_raw33_or_source_epochs() {
    let b = Budget::new(1 << 20).unwrap();
    let e = collect(Case::RetypeWrap, &b);
    let p = pes(&e);
    assert_eq!(p[0].1.header.pts_raw33, Some((1 << 33) - 90000));
    assert_eq!(p[1].1.header.pts_raw33, Some(45000));
    assert!(
        p.iter()
            .all(|(_, p)| p.header.dts_raw33.is_none() && p.epochs.clock == 0)
    );
}
#[test]
fn source_exact_repeat_and_not_current_map_never_reconfigure() {
    for c in [Case::Repeat, Case::NextMap] {
        let b = Budget::new(1 << 20).unwrap();
        let e = collect(c, &b);
        assert_eq!(maps(&e).iter().filter(|m| m.3).count(), 1);
        let p = pes(&e);
        assert_eq!(p.len(), 2);
        assert!(p.iter().all(|(_, p)| p.epochs.configuration == 2));
        assert_eq!(
            p[1].1.payload().flatten().copied().collect::<Vec<_>>(),
            b"AVC1"
        );
    }
}
#[test]
fn source_incomplete_and_next_pat_never_activate() {
    for c in [Case::IncompletePat, Case::NextPat] {
        let b = Budget::new(1 << 20).unwrap();
        let e = collect(c, &b);
        assert!(!e.iter().any(|(e, _)| matches!(
            e,
            TraceEvent::Pat(_) | TraceEvent::ProgramMap { active: true, .. } | TraceEvent::Pes(_)
        )));
    }
}
#[test]
fn source_network_entry_is_not_a_track() {
    let b = Budget::new(1 << 20).unwrap();
    let e = collect(Case::NetworkOnly, &b);
    assert_eq!(pat(&e).1.programs.as_slice().len(), 1);
    assert!(pat(&e).1.programs.as_slice()[0].is_network_pid());
    assert!(maps(&e).is_empty() && pes(&e).is_empty());
    let (n, t, c) = pat(&e);
    let mut s = NativePatSelection::new(t, 1, &b, context(n, c)).unwrap();
    loop {
        match s
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            TableStep::Yield => {}
            TableStep::Output(v) => {
                assert_eq!(v, None);
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
#[test]
fn source_bad_new_crc_keeps_old_map_and_exact_error() {
    let b = Budget::new(1 << 20).unwrap();
    let e = collect(Case::BadMapCrc, &b);
    assert_eq!(maps(&e).iter().filter(|m| m.3).count(), 1);
    assert!(pes(&e).iter().all(|(_, p)| p.epochs.configuration == 2));
    assert!(e.iter().any(|(e,_)|matches!(e,TraceEvent::Diagnostic(f)if f.error==Error::Invalid("PSI table identity/CRC"))));
}
#[test]
fn source_pending_old_pes_does_not_forge_output_at_new_map() {
    for c in [Case::PendingBody, Case::PendingHeader] {
        let b = Budget::new(1 << 20).unwrap();
        let e = collect(c, &b);
        let p = pes(&e);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].1.epochs.configuration, 3);
        assert_eq!(
            p[0].1.payload().flatten().copied().collect::<Vec<_>>(),
            b"HEV1"
        );
    }
}
#[test]
fn source_fixture_crc_matches_prior_original_primary_sections() {
    let b = Budget::new(1 << 20).unwrap();
    let e = collect(Case::Retype188, &b);
    assert_eq!(
        pat(&e).1.source_sections.as_slice()[0].as_ref(),
        &[
            0, 0xb0, 13, 0, 1, 0xc1, 0, 0, 0, 1, 0xf0, 0, 0x2a, 0xb1, 4, 0xb2
        ]
    );
    assert_eq!(
        maps(&e)[0].1.source.as_ref(),
        &[
            2, 0xb0, 18, 0, 1, 0xff, 0, 0, 0xe1, 0, 0xf0, 0, 0x1b, 0xe1, 0, 0xf0, 0, 0xbb, 0x8e,
            0x4f, 0xd2
        ]
    );
}

#[test]
fn initial_single_pat_work_cancel_foreign_pool_and_owner() {
    let b = Budget::new(1 << 20).unwrap();
    let e = collect(Case::Retype188, &b);
    let (n, t, c) = pat(&e);
    let used = b.used();
    {
        let mut s = NativePatSelection::new(t, 1, &b, context(n, c)).unwrap();
        assert!(matches!(
            s.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
            Ok(TableStep::Yield)
        ));
        assert!(matches!(
            s.poll(&mut WorkBudget::new(1), &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
    }
    let r = route(&e, 1, &b);
    assert_eq!((r.program(), r.pmt_pid(), r.program_count()), (1, 4096, 1));
    assert_eq!(b.used(), used);
    let f = Budget::new(1 << 20).unwrap();
    let mut wrong = NativePatSelection::new(t, 1, &f, context(n, c)).unwrap();
    let mut failed = false;
    for _ in 0..64 {
        match wrong.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(TableStep::Yield) => {}
            Err(Error::ForeignPool) => {
                failed = true;
                break;
            }
            other => panic!("foreign source must be refused {other:?}"),
        }
    }
    assert!(failed);
}
#[test]
fn initial_binding_is_allocation_free_and_drop_cancel_is_atomic() {
    let b = Budget::new(1 << 20).unwrap();
    let e = collect(Case::Retype188, &b);
    let r = route(&e, 1, &b);
    let (n, m, c, _) = maps(&e)[0];
    let scope = NativePesScope {
        program: 1,
        pid: 256,
        stream_type: 0x1b,
        stream_id: 0xe0,
        entity: SourceId(1),
        epochs: c,
    };
    let mut o = NativePesRecoveryObserver::new_selected_avc_faults_v1(
        NativeClockProfile::ArchivedPesV1,
        scope,
        b.clone(),
        ParseLimits::default(),
    )
    .unwrap();
    let used = b.used();
    {
        let mut job = o.bind_selected_pmt_v1(r, m, context(n, c)).unwrap();
        assert!(matches!(
            job.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
            Ok(TableStep::Yield)
        ));
        assert!(matches!(
            job.poll(&mut WorkBudget::new(1), &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
        assert_eq!(job.current_scope(), scope);
    }
    assert!(o.pmt_binding_v1().is_none());
    assert_eq!(b.used(), used);
}

fn multi(case: Case) {
    let (_, e, p) = drive(case);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].program, fixtures::selected(case));
    assert_eq!(p[0].source_header.dts_raw33, None);
    assert_eq!(p[0].payload_len, 4);
    assert_eq!(p[0].native_flags, 0);
    assert_eq!(pes(&e).len(), 1);
}
#[test]
fn feature_selected_one_after_unrelated_program() {
    multi(Case::MultiOne);
}
#[test]
fn feature_selected_two_after_program_one() {
    multi(Case::MultiTwo);
}
#[test]
fn feature_network_entry_and_multiple_programs() {
    multi(Case::MultiNetwork);
}
#[test]
fn feature_reversed_multisection_route() {
    multi(Case::MultiSections);
}
fn check_retype(case: Case, positions: [i64; 2]) {
    let (_, e, p) = drive(case);
    assert_eq!(p.len(), 2);
    for (i, p) in p.iter().enumerate() {
        assert_eq!(p.native_position, positions[i]);
        assert_eq!(p.source_epochs.configuration, 2 + i as u64);
        assert_eq!(p.native_stream_id, 0xe0);
        assert_eq!(p.native_flags, 0);
        assert_eq!(p.native_duration90_raw, 0);
        assert_eq!(p.source_header.dts_raw33, None);
        assert_eq!(
            p.payload().flatten().copied().collect::<Vec<_>>(),
            if i == 0 { b"AVC0" } else { b"HEV1" }
        );
    }
    assert_eq!(maps(&e)[1].1.streams.as_slice()[0].stream_type, 0x24);
}
#[test]
fn feature_drained_avc_hevc_with_real_pes_188() {
    check_retype(Case::Retype188, [376, 752]);
}
#[test]
fn feature_drained_avc_hevc_with_opaque_prefix_192() {
    check_retype(Case::Retype192, [388, 772]);
}
#[test]
fn feature_rebind_does_not_reseed_native_clock_or_fill_source_dts() {
    let (_, _, p) = drive(Case::RetypeWrap);
    let mut clock = NativePesClockV1::new(1).unwrap();
    let mut views = Vec::new();
    for p in &p {
        views.push(
            clock
                .project_native_header(
                    p.pid,
                    p.source_epochs,
                    p.time_header(),
                    &mut WorkBudget::new(1),
                    &AtomicBool::new(false),
                )
                .unwrap()
                .unwrap(),
        );
    }
    assert_eq!(views[0].pts90, Some(-90000));
    assert_eq!(views[1].pts90, Some(45000));
    assert_eq!(views[0].origin, views[1].origin);
    assert!(views.iter().all(|v| v.source_dts_raw33.is_none()));
}
#[test]
fn feature_retained_old_output_keeps_its_full_owner_and_epoch() {
    let b = Budget::new(1 << 20).unwrap();
    let initial = b.used();
    let e = collect(Case::Retype188, &b);
    let (mut o, r) = bind(&e, &b);
    let mut storage = Vec::with_capacity(4096);
    storage.extend_from_slice(&fixtures::bytes(Case::Retype188));
    let carrier_owner = Bytes::try_from_vec(&b, storage).unwrap();
    let mut input = triergon_mpegts::parse_transport(
        carrier_owner.slice(376..564).unwrap(),
        PacketFormat::Ts188,
        376,
    )
    .unwrap();
    input.source_id = Some(SourceId(1));
    let TraceEvent::Transport(actual) = &e[8].0 else {
        panic!()
    };
    assert_eq!(input.source.as_ref(), actual.source.as_ref());
    o.push_accepted(input, context(8, e[8].1)).unwrap();
    let old = drain(&mut o).pop().unwrap();
    let owner = old.spans.as_slice()[0].backing_bytes().clone();
    assert!(owner.retained_capacity() >= 4096);
    let next = rebind(&mut o, r, &e);
    assert_eq!(next.scope.stream_type, 0x24);
    assert_eq!(old.source_epochs.configuration, 2);
    assert_eq!(old.source_header.pts_raw33, Some(90000));
    assert!(
        old.spans.as_slice()[0]
            .backing_bytes()
            .shares_storage_with(&owner)
    );
    drop(o);
    drop(e);
    drop(carrier_owner);
    assert!(old.payload().flatten().copied().eq(b"AVC0".iter().copied()));
    drop(owner);
    drop(old);
    assert_eq!(b.used(), initial);
}
#[test]
fn feature_rebind_work_zero_one_cancel_retry_and_scope_commit() {
    let b = Budget::new(1 << 20).unwrap();
    let e = collect(Case::Retype188, &b);
    let (mut o, r) = bind(&e, &b);
    let _old = push_first(&mut o, &e);
    let old = o.scope();
    let (n, m, c, _) = maps(&e)[1];
    {
        let mut j = o
            .rebind_drained_selected_pmt_v1(r, m, context(n, c))
            .unwrap();
        let used = b.used();
        assert!(matches!(
            j.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
            Ok(TableStep::Yield)
        ));
        assert_eq!(j.current_scope(), old);
        assert_eq!(b.used(), used);
        assert!(matches!(
            j.poll(&mut WorkBudget::new(1), &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
        let mut committed = false;
        for _ in 0..128 {
            let mut w = WorkBudget::new(1);
            match j
                .poll(&mut w, &AtomicBool::new(false))
                .expect("lifecycle drained PMT rebind runtime RED until implemented")
            {
                TableStep::Yield => {
                    assert!(w.spent() <= 1);
                    assert_eq!(j.current_scope(), old)
                }
                TableStep::Output(v) => {
                    assert_eq!(v.scope.epochs.configuration, 3);
                    committed = true;
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(committed);
    }
    assert_eq!(o.scope().stream_type, 0x24);
}
#[test]
fn feature_zero_available_budget_rebind_then_real_body_pressure_retry() {
    let b = Budget::new(1 << 20).unwrap();
    let e = collect(Case::Retype188, &b);
    let (mut o, r) = bind(&e, &b);
    let old = push_first(&mut o, &e);
    let blocker = b.reserve(b.available()).unwrap();
    let used = b.used();
    rebind(&mut o, r, &e);
    assert_eq!(b.used(), used);
    let TraceEvent::Transport(p) = &e[15].0 else {
        panic!()
    };
    o.push_accepted(p.clone(), context(15, e[15].1)).unwrap();
    let mut blocked = false;
    for _ in 0..4096 {
        match o.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(AssemblyStep::Yield) => {}
            Err(Error::Backpressure { .. }) => {
                blocked = true;
                break;
            }
            other => panic!("new PES must obey real pool pressure {other:?}"),
        }
    }
    assert!(blocked);
    drop(blocker);
    let next = drain(&mut o);
    assert_eq!(next.len(), 1);
    assert_eq!(
        next[0].payload().flatten().copied().collect::<Vec<_>>(),
        b"HEV1"
    );
    assert_eq!(old[0].source_epochs.configuration, 2);
}
#[test]
fn feature_hevc_cannot_inherit_avc_tei_recovery() {
    let b = Budget::new(1 << 20).unwrap();
    let e = collect(Case::Retype188, &b);
    let (mut o, r) = bind(&e, &b);
    push_first(&mut o, &e);
    rebind(&mut o, r, &e);
    let mut raw = fixtures::bytes(Case::Retype188)[752..940].to_vec();
    raw[1] |= 0x80;
    let mut p = triergon_mpegts::parse_transport(
        Bytes::copy_from_slice(&b, &raw).unwrap(),
        PacketFormat::Ts188,
        752,
    )
    .unwrap();
    p.source_id = Some(SourceId(1));
    let returned = o
        .push_accepted(
            p,
            context(
                15,
                SourceEpochs {
                    clock: 0,
                    configuration: 3,
                },
            ),
        )
        .unwrap_err();
    assert!(matches!(returned.error, Error::Unsupported(_)));
    assert_eq!(returned.value.source.as_ref(), raw);
}

#[test]
fn guard_pending_body_header_and_mailbox_refuse_rebind() {
    for case in [Case::PendingBody, Case::PendingHeader, Case::Retype188] {
        let b = Budget::new(1 << 20).unwrap();
        let e = collect(case, &b);
        let (mut o, r) = bind(&e, &b);
        if matches!(case, Case::Retype188) {
            let TraceEvent::Transport(p) = &e[8].0 else {
                panic!()
            };
            o.push_accepted(p.clone(), context(8, e[8].1)).unwrap();
        } else {
            assert!(push_first(&mut o, &e).is_empty());
        }
        let (n, m, c, _) = maps(&e)[1];
        assert!(matches!(
            o.rebind_drained_selected_pmt_v1(r, m, context(n, c)),
            Err(Error::Unsupported(
                "selected PMT binding requires drained observer"
            ))
        ));
        assert_eq!(o.scope().epochs.configuration, 2);
    }
}
#[test]
fn guard_eof_refuses_rebind() {
    let b = Budget::new(1 << 20).unwrap();
    let e = collect(Case::Retype188, &b);
    let (mut o, r) = bind(&e, &b);
    push_first(&mut o, &e);
    o.end_of_source().unwrap();
    let (n, m, c, _) = maps(&e)[1];
    assert!(matches!(
        o.rebind_drained_selected_pmt_v1(r, m, context(n, c)),
        Err(Error::Unsupported(
            "selected PMT binding requires drained observer"
        ))
    ));
}
fn fail_job(
    o: &mut NativePesRecoveryObserver,
    r: NativePatRoute,
    m: &ProgramMap,
    c: NativeSourceContext,
) -> Error {
    let mut j = o.rebind_drained_selected_pmt_v1(r, m, c).unwrap();
    for _ in 0..4096 {
        match j.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(TableStep::Yield) => {}
            Err(e) => return e,
            other => panic!("invalid source control must refuse {other:?}"),
        }
    }
    panic!("invalid job did not terminate");
}
#[test]
fn guard_changed_pid_and_not_current_map_are_specific_refusals() {
    for case in [Case::ChangedPid, Case::NextMap] {
        let b = Budget::new(1 << 20).unwrap();
        let e = collect(case, &b);
        let (mut o, r) = bind(&e, &b);
        push_first(&mut o, &e);
        let (n, m, c, _) = maps(&e)[1];
        let err = fail_job(&mut o, r, m, context(n, c));
        assert_eq!(
            err,
            if matches!(case, Case::ChangedPid) {
                Error::Unsupported("selected PMT PID/carriage transition unobserved")
            } else {
                Error::Invalid("selected PMT must be current single-stream source Map")
            }
        );
        assert_eq!(o.scope().epochs.configuration, 2);
    }
}
#[test]
fn guard_stale_epoch_order_and_clock_cannot_mutate_binding() {
    let b = Budget::new(1 << 20).unwrap();
    let e = collect(Case::Retype188, &b);
    let (mut o, r) = bind(&e, &b);
    push_first(&mut o, &e);
    let (n, m, c, _) = maps(&e)[1];
    for ctx in [
        context(8, c),
        context(
            n,
            SourceEpochs {
                clock: 1,
                configuration: 3,
            },
        ),
        context(
            n,
            SourceEpochs {
                clock: 0,
                configuration: 2,
            },
        ),
    ] {
        let err = fail_job(&mut o, r, m, ctx);
        assert!(matches!(
            err,
            Error::Invalid(
                "selected PMT source event context" | "selected PMT version/epoch sequence"
            )
        ));
        assert_eq!(o.scope().epochs.configuration, 2);
    }
}
#[test]
fn guard_foreign_map_and_entity_owners_are_not_admitted() {
    let b = Budget::new(1 << 20).unwrap();
    let e = collect(Case::Retype188, &b);
    let (mut o, r) = bind(&e, &b);
    push_first(&mut o, &e);
    let f = Budget::new(1 << 20).unwrap();
    let foreign = collect(Case::Retype188, &f);
    let (n, m, c, _) = maps(&foreign)[1];
    assert_eq!(fail_job(&mut o, r, m, context(n, c)), Error::ForeignPool);
    let foreign_entity = collect_entity(Case::Retype188, &b, SourceId(2));
    let (n, m, c, _) = maps(&foreign_entity)[1];
    assert_eq!(
        fail_job(&mut o, r, m, context(n, c)),
        Error::Unsupported("selected PMT entity/physical ranges unobserved")
    );
}
#[test]
fn guard_drop_and_cancel_preserve_old_binding_for_retry() {
    let b = Budget::new(1 << 20).unwrap();
    let e = collect(Case::Retype188, &b);
    let (mut o, r) = bind(&e, &b);
    push_first(&mut o, &e);
    let before = o.pmt_binding_v1();
    let (n, m, c, _) = maps(&e)[1];
    {
        let mut j = o
            .rebind_drained_selected_pmt_v1(r, m, context(n, c))
            .unwrap();
        assert!(matches!(
            j.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
            Ok(TableStep::Yield)
        ));
        assert!(matches!(
            j.poll(&mut WorkBudget::new(1), &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
    }
    assert_eq!(o.pmt_binding_v1(), before);
}
#[test]
fn guard_binding_ordinal_prevents_stale_transport_replay() {
    let b = Budget::new(1 << 20).unwrap();
    let e = collect(Case::Retype188, &b);
    let (mut o, _) = bind(&e, &b);
    let TraceEvent::Transport(p) = &e[8].0 else {
        panic!()
    };
    let returned = o.push_accepted(p.clone(), context(7, e[8].1)).unwrap_err();
    assert!(returned.value.source.shares_storage_with(&p.source));
    assert_eq!(o.scope().epochs.configuration, 2);
}

#[test]
#[ignore = "parent CPU lease only: materialize authored inputs before native observations"]
fn export_authored_inputs_for_parent_reference() {
    use std::{fs, io::Write, path::PathBuf};
    let root = PathBuf::from(
        std::env::var_os("SAFE_MEDIA_TS_LIFECYCLE_EXPORT")
            .expect("explicit ignored export directory"),
    );
    assert!(root.is_absolute() && root.components().any(|c| c.as_os_str() == ".research"));
    fs::create_dir_all(&root).unwrap();
    for (case, id) in fixtures::CASES {
        let bytes = fixtures::bytes(*case);
        assert_eq!(fixtures::id(*case), *id);
        let path = root.join(format!("{id}.ts"));
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap();
        f.write_all(&bytes).unwrap();
        println!(
            "{id} {} {} {}",
            fixtures::width(*case),
            fixtures::selected(*case),
            bytes.len()
        );
    }
}
