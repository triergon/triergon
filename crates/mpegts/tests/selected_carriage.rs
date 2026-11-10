//! Full-object inputs and storage/format guards for the explicit selected
//! demux view. Source syntax and the other entry points stay strict.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_mpegts::compat::pes::SourceContext;
use triergon_mpegts::psi::DescriptorNamespace;
use triergon_mpegts::reference_trace::catalogue::{
    Catalogue, CatalogueCodec, PsiCompatibilityPolicies,
};
use triergon_mpegts::reference_trace::packet_completion::PacketCompletion;
use triergon_mpegts::{
    MediaEntityEnd, ObservedTrace, PacketFormat, ParseLimits, TraceEvent, TransportStream,
};

const BASIC: &[u8] = include_bytes!("data/ts188-basic.ts");
const M2TS: &[u8] = include_bytes!("data/m2ts192-opaque-prefix.m2ts");
struct Run {
    events: Vec<(ObservedTrace<TraceEvent>, SourceContext)>,
    end: MediaEntityEnd,
    input: InputCursor,
}
impl Run {
    fn events(&self) -> impl Iterator<Item = (&TraceEvent, &SourceContext)> {
        self.events
            .iter()
            .map(|(event, context)| (event.event(), context))
    }
}
fn parsed(data: &[u8], b: &Budget, format: PacketFormat, chunk: usize) -> Run {
    let mut input = InputCursor::new(
        b,
        SourceId(1),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        4096,
    )
    .unwrap();
    let mut demux = TransportStream::new(b.clone(), format, ParseLimits::default(), 1).unwrap();
    let mut events = Vec::new();
    let cancel = AtomicBool::new(false);
    for _ in 0..1_000_000 {
        match demux
            .poll_observed(&mut input, &mut WorkBudget::new(7), &cancel)
            .unwrap()
        {
            Step::Ready(e) => {
                let c = SourceContext {
                    source_event_ordinal: events.len() as u64,
                    epochs: demux.source_epochs(),
                    byte_run_epoch: None,
                };
                events.push((e, c));
            }
            Step::Yield => (),
            Step::NeedInput(r) => {
                let at = usize::try_from(r.offset).unwrap();
                if at == data.len() {
                    input.finish(r.source, r.token).unwrap();
                } else {
                    let end = data.len().min(at + r.maximum.min(chunk));
                    input
                        .provide(
                            &InputWindow::new(
                                r.source,
                                r.token,
                                r.offset,
                                Bytes::copy_from_slice(b, &data[at..end]).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                }
            }
            Step::End(_) => {
                let end = demux
                    .media_entity_end(&mut input, &mut WorkBudget::new(1), &cancel)
                    .unwrap()
                    .unwrap();
                return Run { events, end, input };
            }
        }
    }
    panic!("bounded authored source did not reach actual EOF")
}
fn catalogue(b: &Budget, format: PacketFormat) -> Catalogue {
    Catalogue::new_selected_carriage(b.clone(), ParseLimits::default(), 1, format).unwrap()
}
fn audio(data: &[u8], kind: u8, codec: CatalogueCodec) {
    for chunk in [1, 7, 4096] {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = parsed(data, &b, PacketFormat::Ts188, chunk);
        let mut v = catalogue(&b, PacketFormat::Ts188);
        let mut seen = false;
        for (e, _) in run.events() {
            if let TraceEvent::ProgramMap {
                map, active: true, ..
            } = e
            {
                assert_eq!(map.descriptor_namespace, DescriptorNamespace::Unspecified);
                assert_eq!(map.streams.as_slice()[0].stream_type, kind);
                assert!(!map.source.is_empty());
                seen = true;
            }
            let n = Catalogue::required_work(e).unwrap();
            assert!(
                v.observe(e, &mut WorkBudget::new(n), &AtomicBool::new(false))
                    .unwrap()
            );
        }
        assert!(seen);
        assert_eq!(v.streams().len(), 1);
        let stream = &v.streams()[0];
        assert_eq!(
            (stream.pid, stream.stream_type, stream.codec()),
            (257, kind, codec)
        );
        assert_eq!(codec.media_type(), 1);
        assert_eq!(
            v.evidence()[stream.evidence].bytes.as_slice()[0].as_ref(),
            run.events()
                .find_map(|(e, _)| {
                    if let TraceEvent::ProgramMap {
                        map, active: true, ..
                    } = e
                    {
                        Some(map.source.as_ref())
                    } else {
                        None
                    }
                })
                .unwrap()
        );
        v.finish(&run.end, &run.input).unwrap();
    }
}
macro_rules! audio_guard {
    ($name:ident,$path:literal,$kind:expr,$codec:ident) => {
        #[test]
        fn $name() {
            audio(include_bytes!($path), $kind, CatalogueCodec::$codec);
        }
    };
}
audio_guard!(
    aac_adts_complete_source_identity,
    "data/carriage-aac-adts-direct.ts",
    0x0f,
    AacAdts
);
audio_guard!(
    aac_latm_complete_source_identity,
    "data/carriage-aac-latm-direct.ts",
    0x11,
    AacLatm
);
audio_guard!(
    ac3_direct_complete_source_identity,
    "data/carriage-ac3-direct.ts",
    0x81,
    Ac3
);
audio_guard!(
    eac3_direct_complete_source_identity,
    "data/carriage-eac3-direct.ts",
    0x87,
    Eac3
);
audio_guard!(
    ac3_dvb_keeps_unspecified_source_namespace,
    "data/carriage-ac3-dvb.ts",
    6,
    Ac3
);
audio_guard!(
    eac3_dvb_keeps_unspecified_source_namespace,
    "data/carriage-eac3-dvb.ts",
    6,
    Eac3
);

#[test]
fn old_constructor_keeps_audio_refusal() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = parsed(
        include_bytes!("data/carriage-aac-adts-direct.ts"),
        &b,
        PacketFormat::Ts188,
        7,
    );
    let mut v = Catalogue::new(b, ParseLimits::default(), 1).unwrap();
    let failure = run.events().find_map(|(e, _)| {
        v.observe(e, &mut WorkBudget::new(1_000_000), &AtomicBool::new(false))
            .err()
    });
    assert_eq!(
        failure,
        Some(Error::Unsupported(
            "native catalogue selected video types only"
        ))
    );
}
#[test]
fn selected_m2ts_keeps_prefixes_and_real_eof() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = parsed(M2TS, &b, PacketFormat::M2ts192, 1);
    let mut v = catalogue(&b, PacketFormat::M2ts192);
    v.enable_psi_compatibility(PsiCompatibilityPolicies::ALL_OBSERVED)
        .unwrap();
    for (e, c) in run.events() {
        if let TraceEvent::Transport(p) = e {
            assert_eq!(p.source.len(), 192);
            assert_eq!(
                p.prefix.as_ref().unwrap().as_ref(),
                &M2TS[p.source_offset as usize..p.source_offset as usize + 4]
            );
        }
        let n = v.required_psi_compatibility_work(e).unwrap();
        assert!(
            v.observe_psi_compatibility(
                e,
                SourceId(1),
                *c,
                &mut WorkBudget::new(n),
                &AtomicBool::new(false)
            )
            .unwrap()
        );
    }
    let n = v.required_psi_compatibility_finish_work().unwrap();
    assert!(
        v.finish_psi_compatibility(
            &run.end,
            &run.input,
            &mut WorkBudget::new(n),
            &AtomicBool::new(false)
        )
        .unwrap()
    );
    assert!(v.is_ended());
    assert_eq!(v.streams().len(), 1);
}
#[test]
fn plain_catalogue_rejects_m2ts_end_witness() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = parsed(M2TS, &b, PacketFormat::M2ts192, 7);
    let mut v = Catalogue::new(b, ParseLimits::default(), 1).unwrap();
    for (e, _) in run.events() {
        v.observe(e, &mut WorkBudget::new(1_000_000), &AtomicBool::new(false))
            .unwrap();
    }
    assert!(v.finish(&run.end, &run.input).is_err());
    assert!(!v.is_ended());
}
#[test]
fn explicit_packet_format_charges_physical_192_and_retains_prefix() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = parsed(M2TS, &b, PacketFormat::M2ts192, 7);
    let (e, c) = &run.events[0];
    let mut v =
        PacketCompletion::new_for_format(b, ParseLimits::default(), 1, PacketFormat::M2ts192)
            .unwrap();
    let n = v.required_work(e.event()).unwrap();
    assert_eq!(n, 192 * 512 + 16384);
    assert!(
        v.observe_issued(e, *c, &mut WorkBudget::new(n), &AtomicBool::new(false))
            .unwrap()
    );
    assert_eq!(
        v.physical_carriers()[0].packet().source.as_ref(),
        &M2TS[..192]
    );
}
#[test]
fn old_packet_completion_rejects_192_carrier() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = parsed(M2TS, &b, PacketFormat::M2ts192, 7);
    let (e, c) = &run.events[0];
    let mut v = PacketCompletion::new(b, ParseLimits::default(), 1).unwrap();
    let n = v.required_work(e.event()).unwrap();
    assert!(
        v.observe_issued(e, *c, &mut WorkBudget::new(n), &AtomicBool::new(false))
            .is_err()
    );
    assert!(v.physical_carriers().is_empty());
}
#[test]
fn work_pressure_and_cancel_leave_carrier_retryable() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = parsed(M2TS, &b, PacketFormat::M2ts192, 7);
    let (e, c) = &run.events[0];
    let mut v =
        PacketCompletion::new_for_format(b, ParseLimits::default(), 1, PacketFormat::M2ts192)
            .unwrap();
    let n = v.required_work(e.event()).unwrap();
    let cancel = AtomicBool::new(false);
    for grant in [0, 1, n - 1] {
        assert!(
            !v.observe_issued(e, *c, &mut WorkBudget::new(grant), &cancel)
                .unwrap()
        );
        assert!(v.physical_carriers().is_empty());
        assert!(v.fault().is_none());
    }
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        v.observe_issued(e, *c, &mut WorkBudget::new(n), &cancel),
        Err(Error::Cancelled)
    );
    cancel.store(false, Ordering::Relaxed);
    assert!(
        v.observe_issued(e, *c, &mut WorkBudget::new(n), &cancel)
            .unwrap()
    );
}
#[test]
fn foreign_pool_carrier_is_not_retained() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = parsed(M2TS, &b, PacketFormat::M2ts192, 7);
    let (e, c) = &run.events[0];
    let other = Budget::new(8 * 1024 * 1024).unwrap();
    let mut v = PacketCompletion::new_for_format(
        other.clone(),
        ParseLimits::default(),
        1,
        PacketFormat::M2ts192,
    )
    .unwrap();
    let n = v.required_work(e.event()).unwrap();
    assert_eq!(
        v.observe_issued(e, *c, &mut WorkBudget::new(n), &AtomicBool::new(false)),
        Err(Error::ForeignPool)
    );
    assert!(v.physical_carriers().is_empty());
}
#[test]
fn caller_alias_keeps_full_charge_after_view_and_source_drop() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let baseline = b.used();
    let run = parsed(M2TS, &b, PacketFormat::M2ts192, 7);
    let (e, c) = &run.events[0];
    let mut v = PacketCompletion::new_for_format(
        b.clone(),
        ParseLimits::default(),
        1,
        PacketFormat::M2ts192,
    )
    .unwrap();
    let n = v.required_work(e.event()).unwrap();
    v.observe_issued(e, *c, &mut WorkBudget::new(n), &AtomicBool::new(false))
        .unwrap();
    let held = v.physical_carriers()[0].packet().source.clone();
    drop(v);
    drop(run);
    assert!(b.used() >= baseline + held.retained_capacity());
    assert_eq!(held.as_ref(), &M2TS[..192]);
    drop(held);
    assert_eq!(b.used(), baseline);
}
#[test]
fn resource_pressure_drops_failed_growth_without_budget_escape() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let baseline = b.used();
    let run = parsed(BASIC, &b, PacketFormat::Ts188, 7);
    let (e, c) = &run.events[0];
    let held = b.reserve(b.available()).unwrap();
    let mut v =
        PacketCompletion::new_for_format(b.clone(), ParseLimits::default(), 1, PacketFormat::Ts188)
            .unwrap();
    let n = v.required_work(e.event()).unwrap();
    assert!(
        v.observe_issued(e, *c, &mut WorkBudget::new(n), &AtomicBool::new(false))
            .is_err()
    );
    assert!(v.fault().is_some());
    drop(v);
    drop(held);
    drop(run);
    assert_eq!(b.used(), baseline);
}

fn repeated_pat_source() -> Vec<u8> {
    let mut data = BASIC.to_vec();
    let mut pat = BASIC[..188].to_vec();
    pat[3] = (pat[3] & 0xf0) | 1;
    data.extend_from_slice(&pat);
    data
}
#[test]
fn complete_pat_repetition_preserves_pcr_and_source_table_events() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let data = repeated_pat_source();
    let run = parsed(&data, &b, PacketFormat::Ts188, 1);
    assert_eq!(
        run.events()
            .filter(|(e, _)| matches!(e, TraceEvent::Pat(_)))
            .count(),
        1
    );
    let mut v = catalogue(&b, PacketFormat::Ts188);
    v.enable_psi_compatibility(PsiCompatibilityPolicies::ALL_OBSERVED)
        .unwrap();
    for (e, c) in run.events() {
        let n = v.required_psi_compatibility_work(e).unwrap();
        assert!(
            v.observe_psi_compatibility(
                e,
                SourceId(1),
                *c,
                &mut WorkBudget::new(n),
                &AtomicBool::new(false)
            )
            .unwrap()
        );
    }
    let n = v.required_psi_compatibility_finish_work().unwrap();
    assert!(
        v.finish_psi_compatibility(
            &run.end,
            &run.input,
            &mut WorkBudget::new(n),
            &AtomicBool::new(false)
        )
        .unwrap()
    );
    assert_eq!(
        v.programs()
            .iter()
            .find(|p| p.program == 1)
            .unwrap()
            .pcr_pid,
        256
    );
    assert_eq!(v.streams().len(), 1);
}
#[test]
fn plain_repeated_pat_end_refusal_remains_literal() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let data = repeated_pat_source();
    let run = parsed(&data, &b, PacketFormat::Ts188, 7);
    let mut v = Catalogue::new(b, ParseLimits::default(), 1).unwrap();
    v.enable_psi_compatibility(PsiCompatibilityPolicies::ALL_OBSERVED)
        .unwrap();
    for (e, c) in run.events() {
        let n = v.required_psi_compatibility_work(e).unwrap();
        v.observe_psi_compatibility(
            e,
            SourceId(1),
            *c,
            &mut WorkBudget::new(n),
            &AtomicBool::new(false),
        )
        .unwrap();
    }
    let n = v.required_psi_compatibility_finish_work().unwrap();
    assert_eq!(
        v.finish_psi_compatibility(
            &run.end,
            &run.input,
            &mut WorkBudget::new(n),
            &AtomicBool::new(false)
        ),
        Err(Error::Unsupported(
            "PSI compatibility pending PAT has no actual section coverage"
        ))
    );
}
