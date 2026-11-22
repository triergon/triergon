//! Full-object inputs and storage/format guards for the explicit selected
//! demux view. Source syntax and the other entry points stay strict.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_mpegts::compat::pes::PacketCompletion;
use triergon_mpegts::compat::pes::SourceContext;
use triergon_mpegts::psi::{DescriptorNamespace, DvbNamespaceEvidence, PsiCrc32};
use triergon_mpegts::reference_trace::catalogue::{
    Catalogue, CatalogueCodec, PsiCompatibilityPolicies,
};
use triergon_mpegts::stream::{MediaEntityEnd, ObservedTrace, TraceEvent, TransportStream};
use triergon_mpegts::{PacketFormat, ParseLimits};

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
    parsed_with_namespace(data, b, format, chunk, DescriptorNamespace::Unspecified)
}
fn parsed_with_namespace(
    data: &[u8],
    b: &Budget,
    format: PacketFormat,
    chunk: usize,
    namespace: DescriptorNamespace,
) -> Run {
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
    let mut demux = TransportStream::new_with_namespace(
        b.clone(),
        format,
        ParseLimits::default(),
        1,
        namespace,
    )
    .unwrap();
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

const DVB: DescriptorNamespace =
    DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile);

fn pmt_carrier(version: u8, streams: &[(u8, u16, &[u8])]) -> [u8; 188] {
    let mut section = vec![
        2,
        0xb0,
        0,
        0,
        1,
        0xc1 | (version << 1),
        0,
        0,
        0xe1,
        0,
        0xf0,
        0,
    ];
    for &(stream_type, pid, descriptors) in streams {
        section.push(stream_type);
        section.extend((0xe000 | pid).to_be_bytes());
        section.extend((0xf000 | u16::try_from(descriptors.len()).unwrap()).to_be_bytes());
        section.extend_from_slice(descriptors);
    }
    section[2] = u8::try_from(section.len() + 1).unwrap();
    section.extend(PsiCrc32::of(&section).to_be_bytes());
    let mut carrier = [0xff; 188];
    carrier[..5].copy_from_slice(&[0x47, 0x50, 0, 0x10 | version, 0]);
    carrier[5..5 + section.len()].copy_from_slice(&section);
    carrier
}

fn project(run: &Run, budget: &Budget, format: PacketFormat) -> Result<Catalogue, Error> {
    let mut projection = catalogue(budget, format);
    for (event, _) in run.events() {
        let mut work = WorkBudget::new(Catalogue::required_work(event)?);
        assert!(projection.observe(event, &mut work, &AtomicBool::new(false))?);
    }
    projection.finish(&run.end, &run.input)?;
    Ok(projection)
}

#[test]
fn elementary_registrations_publish_separate_tags_in_pmt_order() {
    let hevc = b"\x05\x04HEVC";
    let eac3 = b"\x05\x04EAC3\x7a\x01\x00";
    let pmt = pmt_carrier(0, &[(0x24, 256, hevc), (6, 257, eac3)]);
    for format in [PacketFormat::Ts188, PacketFormat::M2ts192] {
        let mut data = Vec::new();
        let mut repeated_pat = BASIC[..188].to_vec();
        let mut repeated_pmt = pmt;
        repeated_pat[3] |= 1;
        repeated_pmt[3] |= 1;
        for packet in [&BASIC[..188], &pmt, &repeated_pat, &repeated_pmt] {
            if format == PacketFormat::M2ts192 {
                data.extend_from_slice(&[0x23, 0x45, 0x67, 0x89]);
            }
            data.extend_from_slice(packet);
        }
        let budget = Budget::new(8 * 1024 * 1024).unwrap();
        let run = parsed_with_namespace(&data, &budget, format, 1, DVB);
        assert_eq!(
            run.events()
                .filter(|(event, _)| matches!(event, TraceEvent::Section(_)))
                .count(),
            4
        );
        assert_eq!(
            run.events()
                .filter(|(event, _)| matches!(
                    event,
                    TraceEvent::Pat(_) | TraceEvent::ProgramMap { .. }
                ))
                .count(),
            2
        );
        let projection = project(&run, &budget, format).unwrap();
        assert_eq!(projection.evidence().len(), 2);
        let streams: Vec<_> = projection
            .streams()
            .iter()
            .map(|stream| {
                (
                    stream.pid,
                    stream.stream_type,
                    stream.codec_tag,
                    stream.codec(),
                )
            })
            .collect();
        assert_eq!(
            streams,
            [
                (
                    256,
                    0x24,
                    u32::from_le_bytes(*b"HEVC"),
                    CatalogueCodec::Hevc
                ),
                (257, 6, u32::from_le_bytes(*b"EAC3"), CatalogueCodec::Eac3),
            ]
        );
    }
}

#[test]
fn declared_atsc_admits_only_matching_private_audio_descriptors() {
    for (kind, tag, codec) in [
        (0x81, 0x81, CatalogueCodec::Ac3),
        (0x87, 0xcc, CatalogueCodec::Eac3),
    ] {
        let pmt = pmt_carrier(0, &[(kind, 256, &[tag, 3, 0, 0, 0])]);
        let data = [&BASIC[..188], &pmt].concat();
        let budget = Budget::new(8 * 1024 * 1024).unwrap();
        let run = parsed_with_namespace(
            &data,
            &budget,
            PacketFormat::Ts188,
            7,
            DescriptorNamespace::Atsc,
        );
        let projection = project(&run, &budget, PacketFormat::Ts188).unwrap();
        let stream = &projection.streams()[0];
        assert_eq!(
            (
                stream.pid,
                stream.stream_type,
                stream.codec_tag,
                stream.codec()
            ),
            (256, kind, u32::from(kind), codec)
        );

        let run = parsed(&data, &budget, PacketFormat::Ts188, 7);
        assert!(matches!(
            project(&run, &budget, PacketFormat::Ts188),
            Err(Error::Unsupported(_))
        ));
    }
}

#[test]
fn truncated_or_unrelated_registrations_do_not_publish_streams() {
    for descriptors in [b"\x05\x03HEV".as_slice(), b"\x05\x04EAC3".as_slice()] {
        let pmt = pmt_carrier(0, &[(0x24, 256, descriptors)]);
        let data = [&BASIC[..188], &pmt].concat();
        let budget = Budget::new(8 * 1024 * 1024).unwrap();
        let run = parsed(&data, &budget, PacketFormat::Ts188, 7);
        assert!(project(&run, &budget, PacketFormat::Ts188).is_err());
    }
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
            "demux video catalogue supports only AVC and HEVC stream types"
        ))
    );
}
#[test]
fn selected_m2ts_keeps_prefixes_and_real_eof() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = parsed(M2TS, &b, PacketFormat::M2ts192, 1);
    let mut v = catalogue(&b, PacketFormat::M2ts192);
    v.enable_psi_compatibility(PsiCompatibilityPolicies::ALL)
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
    v.enable_psi_compatibility(PsiCompatibilityPolicies::ALL)
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
    v.enable_psi_compatibility(PsiCompatibilityPolicies::ALL)
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
