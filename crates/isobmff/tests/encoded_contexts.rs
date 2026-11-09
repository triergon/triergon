mod common;
use common::{child, children, roots, table};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{
    Budget, Error, InputCursor, SourceCapabilities, SourceId, TimeBase, WorkBudget,
};
use triergon_isobmff::{
    ConfigurationLimits, EditList, Limits, SampleTable, TrackHeader,
    compat::mov::{
        edits::{
            EncodedEditContext, EncodedEditJob, EncodedEditLimits, EncodedEditPlan,
            EncodedEditProfile,
        },
        encoded::{
            EncodedMetadataProfile, EncodedRecoveryProfile, EncodedSelection, EncodedStream,
            EncodedStreamJob,
        },
        packets::{CursorStatus, DemuxTrack, PacketCursor, PacketLimits},
    },
};

#[cfg(feature = "reference-trace")]
use triergon_isobmff::{BuildStatus, reference_trace::mov::diagnostics::PrivateDiagnosticJob};

struct Source {
    pool: Budget,
    header: TrackHeader,
    table: SampleTable,
    edits: Option<EditList>,
    media: Vec<Range<u64>>,
    length: u64,
    brand: [u8; 4],
}
impl Source {
    fn new(b: &[u8]) -> Self {
        let pool = Budget::new(1 << 22).unwrap();
        let top = roots(&pool, b).unwrap();
        let movie = top.iter().find(|b| b.header().kind == *b"moov").unwrap();
        let track = children(movie, 0)
            .unwrap()
            .into_iter()
            .find(|b| b.header().kind == *b"trak")
            .unwrap();
        let mdia = child(&track, b"mdia").unwrap();
        let header = TrackHeader::parse(
            &child(&track, b"tkhd").unwrap(),
            &child(&mdia, b"mdhd").unwrap(),
            &child(&mdia, b"hdlr").unwrap(),
        )
        .unwrap();
        let table = table(
            child(&child(&mdia, b"minf").unwrap(), b"stbl").unwrap(),
            &pool,
            1,
        )
        .unwrap();
        let edits = children(&track, 0)
            .unwrap()
            .into_iter()
            .find(|b| b.header().kind == *b"edts")
            .map(|b| EditList::parse(child(&b, b"elst").unwrap(), Limits::default()).unwrap());
        let media = top
            .iter()
            .filter(|b| b.header().kind == *b"mdat")
            .map(|b| b.header().payload_range().unwrap())
            .collect();
        Self {
            pool,
            header,
            table,
            edits,
            media,
            length: b.len() as u64,
            brand: b[8..12].try_into().unwrap(),
        }
    }
    fn input(&self) -> InputCursor {
        InputCursor::new(
            &self.pool,
            SourceId(1997),
            SourceCapabilities {
                length: Some(self.length),
                random_access: true,
            },
            64,
        )
        .unwrap()
    }
    fn configs(&self, i: &mut InputCursor) -> Vec<EncodedStream> {
        let mut j = EncodedStreamJob::new_description_with_contexts(
            &self.header,
            &self.table,
            i,
            EncodedSelection {
                major_brand: Some(self.brand),
                description: 1,
                limits: ConfigurationLimits::default(),
            },
            triergon_isobmff::compat::mov::encoded::EncodedContextProfile,
        )
        .unwrap();
        for _ in 0..20000 {
            if j.poll(i, &mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
                == CursorStatus::Ready
            {
                return vec![j.take(i, &AtomicBool::new(false)).unwrap()];
            }
        }
        panic!("config progress");
    }
    fn context<'a>(&'a self, s: &'a EncodedStream) -> EncodedEditContext<'a> {
        EncodedEditContext {
            table: &self.table,
            edits: self.edits.as_ref().unwrap(),
            stream: s,
            media_base: self.header.time_base().unwrap(),
            movie_base: TimeBase::new(1, 1000).unwrap(),
        }
    }
    fn job<'a>(&'a self, configs: &'a [EncodedStream], i: &InputCursor) -> EncodedEditJob<'a> {
        EncodedEditJob::new_with_recovery_contexts(
            self.context(&configs[0]),
            configs,
            i,
            EncodedEditLimits::default(),
            triergon_isobmff::compat::mov::encoded::EncodedContextProfile,
        )
        .unwrap()
    }
    fn plan(&self, q: u64) -> Result<EncodedEditPlan, Error> {
        let mut i = self.input();
        let configs = self.configs(&mut i);
        finish(&mut self.job(&configs, &i), &mut i, q)
    }
    fn track(&self) -> DemuxTrack<'_> {
        DemuxTrack {
            track_id: self.header.track_id,
            table: &self.table,
            fragments: &[],
            media_base: self.header.time_base().unwrap(),
            edits: self
                .edits
                .as_ref()
                .map(|e| (e, TimeBase::new(1, 1000).unwrap())),
        }
    }
}
fn finish(
    j: &mut EncodedEditJob<'_>,
    i: &mut InputCursor,
    q: u64,
) -> Result<EncodedEditPlan, Error> {
    for _ in 0..20000 {
        let mut w = WorkBudget::new(q);
        let state = j.poll(i, &mut w, &AtomicBool::new(false))?;
        assert!(w.spent() <= q);
        if state == CursorStatus::Ready {
            return j.take(i, &AtomicBool::new(false));
        }
    }
    panic!("group/sample actual-work progress");
}
fn rows(
    s: &Source,
    plan: Option<EncodedEditPlan>,
) -> Vec<triergon_isobmff::compat::mov::packets::PacketOccurrence> {
    let plans = [plan];
    let mut c = PacketCursor::new_encoded_regular(
        &s.pool,
        &[s.track()],
        &s.media,
        PacketLimits::default(),
        &plans,
        EncodedEditProfile,
    )
    .unwrap();
    let mut rows = Vec::new();
    for _ in 0..20000 {
        let mut w = WorkBudget::new(1);
        match c.poll(&mut w, &AtomicBool::new(false)).unwrap() {
            CursorStatus::Yield => {}
            CursorStatus::End => return rows,
            CursorStatus::Ready => rows.push(c.take_occurrence().unwrap()),
        }
        assert!(w.spent() <= 1);
    }
    panic!("packet work1 progress");
}

fn strict_config(s: &Source) -> Result<EncodedStream, Error> {
    let mut i = s.input();
    let job = EncodedStreamJob::new_description_with_metadata(
        &s.header,
        &s.table,
        &i,
        EncodedSelection {
            major_brand: Some(s.brand),
            description: 1,
            limits: ConfigurationLimits::default(),
        },
        EncodedMetadataProfile,
    )?;
    finish_config(job, &mut i)
}
fn range_config(s: &Source) -> Result<EncodedStream, Error> {
    let mut i = s.input();
    let job = EncodedStreamJob::new_range_playback(
        &s.header,
        &s.table,
        &i,
        EncodedSelection {
            major_brand: Some(s.brand),
            description: 1,
            limits: ConfigurationLimits::default(),
        },
    )?;
    finish_config(job, &mut i)
}
fn finish_config(
    mut job: EncodedStreamJob<'_>,
    input: &mut InputCursor,
) -> Result<EncodedStream, Error> {
    let cancel = AtomicBool::new(false);
    for _ in 0..20000 {
        if job.poll(input, &mut WorkBudget::new(1), &cancel)? == CursorStatus::Ready {
            return job.take(input, &cancel);
        }
    }
    panic!("config progress");
}
fn strict_plan(s: &Source) -> Result<EncodedEditPlan, Error> {
    let mut i = s.input();
    let config = strict_config(s)?;
    let configs = [config];
    let mut j = EncodedEditJob::new_with_recovery_groups(
        s.context(&configs[0]),
        &configs,
        &i,
        EncodedEditLimits::default(),
        EncodedRecoveryProfile,
    )?;
    finish(&mut j, &mut i, 1)
}
macro_rules! wave_case {
    ($name:ident,$file:literal) => {
        #[test]
        fn $name() {
            let s = Source::new(include_bytes!(concat!(
                "fixtures/encoded-metadata-v4/",
                $file,
                ".mov"
            )));
            assert!(
                strict_config(&s).is_err(),
                "original strict operation must stay strict"
            );
            let mut i = s.input();
            let configs = s.configs(&mut i);
            let demux = configs[0].demux();
            assert_eq!(demux.codec, "aac");
            assert_eq!(demux.extradata.as_ref(), [0x11, 0x88, 0x56, 0xe5, 0]);
            let report = configs[0].wave_source().expect("owned source wave report");
            assert_eq!(report.raw.header().kind, *b"wave");
            assert!(report.first_default_profile_fault.is_some());
            assert!(report.fault_count > 0);
            assert!(report.raw.bytes().belongs_to(&s.pool));
            let packets = rows(&s, None);
            assert_eq!(packets.len(), 2);
            assert_eq!(
                (packets[0].demux.pts, packets[1].demux.pts),
                (Some(0), Some(1024))
            );
            assert_eq!(
                (packets[0].demux.duration, packets[1].demux.duration),
                (1024, 1024)
            );
            assert_eq!((packets[0].demux.flags, packets[1].demux.flags), (1, 1));
            assert_eq!(
                (
                    packets[0].source.range.end - packets[0].source.range.start,
                    packets[1].source.range.end - packets[1].source.range.start
                ),
                (188, 235)
            );
        }
    };
}
macro_rules! recovery_case {($name:ident,$file:literal)=>{#[test]fn $name(){
 let s=Source::new(include_bytes!(concat!("fixtures/encoded-recovery-v5/",$file,".mp4")));assert!(strict_plan(&s).is_err(),"original source/default strict profile remains refused");let plan=s.plan(1).unwrap();let report=plan.recovery_observation().expect("owned source recovery result");assert!(report.ignored_group_boxes()>0);assert_eq!(report.raw_source_table().header(),s.table.raw().header());
 match report.source_result(){Ok(_facts)=>{#[cfg(feature = "reference-trace")]assert_eq!(_facts.receiver_cause(),triergon_isobmff::reference_trace::mov::encoded::RecoveryReceiverCause::RollProlNotAppliedByOpenIndex);},Err(fault)=>{assert!(matches!(fault.error,Error::Invalid(_)|Error::Unsupported(_)|Error::Truncated));assert!(matches!(&fault.raw.header().kind,b"sgpd"|b"sbgp"));assert!(fault.raw.bytes().belongs_to(&s.pool));}}
 let packets=rows(&s,Some(plan));assert_eq!(packets.len(),2);assert_eq!((packets[0].demux.pts,packets[1].demux.pts),(Some(0),Some(40)));assert_eq!((packets[0].demux.flags,packets[1].demux.flags),(1,0));for p in packets{assert_eq!(p.demux.source_range,p.source.range);}
}};}
wave_case!(wave_duplicate_frma, "qt-v1-duplicate-frma");
wave_case!(wave_missing_frma, "qt-v1-missing-frma");
wave_case!(wave_wrong_frma, "qt-v1-wrong-frma");
wave_case!(wave_missing_terminator, "qt-v1-missing-terminator");
wave_case!(wave_terminator_body, "qt-v1-terminator-body");
wave_case!(wave_after_terminator, "qt-v1-after-terminator");
wave_case!(wave_duplicate_esds, "qt-v1-duplicate-esds");
wave_case!(wave_unframed_compression, "qt-v1-unframed-compression");

#[test]
fn range_playback_refuses_malformed_quicktime_aac_wave() {
    macro_rules! refuse {
        ($name:literal) => {
            assert!(
                range_config(&Source::new(include_bytes!(concat!(
                    "fixtures/encoded-metadata-v4/qt-v1-",
                    $name,
                    ".mov"
                ))))
                .is_err(),
                "range playback admitted the malformed wave fixture {}",
                $name
            );
        };
    }
    refuse!("wrong-frma");
    refuse!("duplicate-frma");
    refuse!("missing-frma");
    refuse!("duplicate-esds");
    refuse!("after-terminator");
    refuse!("missing-terminator");
    refuse!("terminator-body");
}

#[test]
fn range_playback_keeps_valid_quicktime_aac_wave() {
    for raw in [
        include_bytes!("fixtures/encoded-metadata-v4/qt-v1-wave.mov").as_slice(),
        include_bytes!("fixtures/encoded-metadata-v4/qt-v1-unknown.mov").as_slice(),
        include_bytes!("fixtures/encoded-metadata-v4/qt-v1-unframed-compression.mov").as_slice(),
    ] {
        let s = Source::new(raw);
        let config = range_config(&s).unwrap();
        assert!(config.wave_source().is_none());
        assert_eq!(
            config.demux().extradata.as_ref(),
            [0x11, 0x88, 0x56, 0xe5, 0]
        );
    }
}

#[test]
fn range_playback_refuses_an_extra_iso_aac_wave_path() {
    let mut raw = include_bytes!("fixtures/encoded-metadata-v4/creation-v0-0.mov").to_vec();
    assert!(range_config(&Source::new(&raw)).is_ok());
    let bitrate = raw.windows(4).position(|bytes| bytes == b"btrt").unwrap();
    raw[bitrate..bitrate + 4].copy_from_slice(b"wave");
    assert!(range_config(&Source::new(&raw)).is_err());
}

#[test]
fn range_playback_keeps_the_base_config_for_dolby_vision_hevc_entries() {
    let raw = include_bytes!("fixtures/encoded-v1/control-hevc-aac-front.mp4");
    let source = Source::new(raw);
    let control = range_config(&source).unwrap();
    let entry = &source.table.descriptions()[0];
    assert!(matches!(&entry.format, b"hvc1" | b"hev1"));
    let code = usize::try_from(entry.raw().header().start).unwrap() + 4;
    for format in [b"dvh1", b"dvhe"] {
        let mut raw = raw.to_vec();
        raw[code..code + 4].copy_from_slice(format);
        let tagged = Source::new(&raw);
        let config = range_config(&tagged).unwrap();
        assert_eq!(config.source_description().format, *format);
        assert_eq!(config.kind(), control.kind());
        assert_eq!(
            config.demux().extradata.as_ref(),
            control.demux().extradata.as_ref()
        );
        assert_eq!(config.configuration_box().header().kind, *b"hvcC");
    }
}

recovery_case!(recovery_dangling_description, "dangling-description");
recovery_case!(recovery_empty_run, "empty-run");
recovery_case!(recovery_coverage_over, "coverage-over");
recovery_case!(recovery_coverage_under, "coverage-under");
recovery_case!(recovery_fragment_namespace, "fragment-namespace");
recovery_case!(recovery_grouping_parameter, "grouping-parameter");
recovery_case!(
    recovery_variable_length_unadmitted,
    "variable-length-unadmitted"
);
recovery_case!(recovery_duplicate_description, "duplicate-description");
recovery_case!(
    recovery_unknown_group_unadmitted,
    "unknown-group-unadmitted"
);

#[test]
fn wave_work_zero_cancel_and_request_generation_stay_checked() {
    let s = Source::new(include_bytes!(
        "fixtures/encoded-metadata-v4/qt-v1-wave.mov"
    ));
    let mut i = s.input();
    let mut j = EncodedStreamJob::new_description_with_contexts(
        &s.header,
        &s.table,
        &i,
        EncodedSelection {
            major_brand: Some(s.brand),
            description: 1,
            limits: ConfigurationLimits::default(),
        },
        triergon_isobmff::compat::mov::encoded::EncodedContextProfile,
    )
    .unwrap();
    let used = s.pool.used();
    let mut w = WorkBudget::new(0);
    assert_eq!(
        j.poll(&mut i, &mut w, &AtomicBool::new(false)).unwrap(),
        CursorStatus::Yield
    );
    assert_eq!(w.spent(), 0);
    assert_eq!(s.pool.used(), used);
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(0), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    let mut j = EncodedStreamJob::new_description_with_contexts(
        &s.header,
        &s.table,
        &i,
        EncodedSelection {
            major_brand: Some(s.brand),
            description: 1,
            limits: ConfigurationLimits::default(),
        },
        triergon_isobmff::compat::mov::encoded::EncodedContextProfile,
    )
    .unwrap();
    i.reset(0).unwrap();
    assert!(matches!(
        j.poll(&mut i, &mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::StaleGeneration { .. })
    ));
}
#[test]
fn foreign_pool_is_not_recovery_or_wave_policy() {
    let s = Source::new(include_bytes!(
        "fixtures/encoded-metadata-v4/qt-v1-wave.mov"
    ));
    let other = Budget::new(1 << 20).unwrap();
    let i = InputCursor::new(
        &other,
        SourceId(55),
        SourceCapabilities {
            length: Some(s.length),
            random_access: true,
        },
        64,
    )
    .unwrap();
    assert!(matches!(
        EncodedStreamJob::new_description_with_contexts(
            &s.header,
            &s.table,
            &i,
            EncodedSelection {
                major_brand: Some(s.brand),
                description: 1,
                limits: ConfigurationLimits::default()
            },
            triergon_isobmff::compat::mov::encoded::EncodedContextProfile
        ),
        Err(Error::ForeignPool)
    ));
}
#[test]
fn later_real_esds_fault_is_not_hidden_by_earlier_allowed_wave_fault() {
    let mut b = include_bytes!("fixtures/encoded-metadata-v4/qt-v1-wrong-frma.mov").to_vec();
    let at = b.windows(4).position(|w| w == b"esds").unwrap();
    b[at + 4] = 1;
    let s = Source::new(&b);
    let mut i = s.input();
    let mut j = EncodedStreamJob::new_description_with_contexts(
        &s.header,
        &s.table,
        &i,
        EncodedSelection {
            major_brand: Some(s.brand),
            description: 1,
            limits: ConfigurationLimits::default(),
        },
        triergon_isobmff::compat::mov::encoded::EncodedContextProfile,
    )
    .unwrap();
    let mut error = None;
    for _ in 0..20000 {
        match j.poll(&mut i, &mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(CursorStatus::Yield) => {}
            Err(e) => {
                error = Some(e);
                break;
            }
            Ok(_) => panic!("bad later config published"),
        }
    }
    assert!(matches!(
        error,
        Some(Error::Unsupported(_) | Error::Invalid(_))
    ));
}
#[test]
fn configuration_and_group_limits_are_never_converted_to_demux_context() {
    let s = Source::new(include_bytes!(
        "fixtures/encoded-metadata-v4/qt-v1-wave.mov"
    ));
    let mut i = s.input();
    let limits = ConfigurationLimits {
        max_config_bytes: 1,
        ..ConfigurationLimits::default()
    };
    let mut j = EncodedStreamJob::new_description_with_contexts(
        &s.header,
        &s.table,
        &i,
        EncodedSelection {
            major_brand: Some(s.brand),
            description: 1,
            limits,
        },
        triergon_isobmff::compat::mov::encoded::EncodedContextProfile,
    )
    .unwrap();
    let mut rejected = false;
    for _ in 0..20000 {
        match j.poll(&mut i, &mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Err(Error::Limit { maximum: 1, .. }) => {
                rejected = true;
                break;
            }
            Err(e) => panic!("wrong failure {e:?}"),
            _ => {}
        }
    }
    assert!(rejected);
    let s = Source::new(include_bytes!(
        "fixtures/encoded-recovery-v5/roll-negative-v0.mp4"
    ));
    let mut i = s.input();
    let configs = s.configs(&mut i);
    let mut limited = EncodedEditJob::new_with_recovery_contexts(
        s.context(&configs[0]),
        &configs,
        &i,
        EncodedEditLimits { max_samples: 1 },
        triergon_isobmff::compat::mov::encoded::EncodedContextProfile,
    )
    .unwrap();
    assert!(matches!(
        finish(&mut limited, &mut i, 1),
        Err(Error::Limit { maximum: 1, .. })
    ));
}
#[test]
fn valid_context_outputs_retain_original_whole_backing_after_parser_drop() {
    let s = Source::new(include_bytes!(
        "fixtures/encoded-metadata-v4/qt-v1-wave.mov"
    ));
    let pool = s.pool.clone();
    let mut i = s.input();
    let configs = s.configs(&mut i);
    let description = configs[0]
        .source_description()
        .raw()
        .bytes()
        .as_ref()
        .as_ptr();
    drop(i);
    drop(s);
    assert!(pool.used() > Budget::control_storage_bytes());
    assert_eq!(
        configs[0]
            .source_description()
            .raw()
            .bytes()
            .as_ref()
            .as_ptr(),
        description
    );
    assert_eq!(
        configs[0].demux().extradata.as_ref(),
        [0x11, 0x88, 0x56, 0xe5, 0]
    );
    drop(configs);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn recovery_cancel_and_logical_generation_do_not_become_source_fault_reports() {
    let s = Source::new(include_bytes!(
        "fixtures/encoded-recovery-v5/coverage-under.mp4"
    ));
    let mut i = s.input();
    let configs = s.configs(&mut i);
    let mut j = s.job(&configs, &i);
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(0), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    let mut j = s.job(&configs, &i);
    i.reset(0).unwrap();
    assert!(matches!(
        j.poll(&mut i, &mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::StaleGeneration { .. })
    ));
}
#[cfg(feature = "reference-trace")]
fn diagnostics(data: &[u8], q: u64) -> Vec<String> {
    let s = Source::new(data);
    let mut job = PrivateDiagnosticJob::new(&s.table, &s.pool, Limits::default()).unwrap();
    for _ in 0..20000 {
        let mut w = WorkBudget::new(q);
        if job.poll(&mut w, &AtomicBool::new(false)).unwrap() == BuildStatus::Ready {
            let out = job.try_finish(&AtomicBool::new(false)).unwrap();
            assert_eq!(out.source_table.header(), s.table.raw().header());
            return out
                .events
                .as_slice()
                .iter()
                .map(|e| {
                    assert!(e.raw.bytes().belongs_to(&s.pool));
                    if let Some(span) = &e.inspected_parent_span {
                        assert!(span.belongs_to(&s.pool));
                    }
                    e.kind.to_string()
                })
                .collect();
        }
        assert!(w.spent() <= q);
    }
    panic!("diagnostics actual work progress");
}
macro_rules! diagnostic_case {
    ($name:ident,$file:literal,$expected:expr) => {
        #[cfg(feature = "reference-trace")]
        #[test]
        fn $name() {
            let data = include_bytes!(concat!("fixtures/private-catalogue-v8/", $file, ".mov"));
            let expected: Vec<String> = $expected.iter().map(|s: &&str| s.to_string()).collect();
            assert_eq!(diagnostics(data, 1), expected);
            assert_eq!(diagnostics(data, 4096), expected);
        }
    };
}
diagnostic_case!(
    diagnostic_drop_dops,
    "drop-dOps",
    [
        "ignoring multiple glbl",
        "ignoring multiple glbl",
        "Unsupported VP Codec Configuration box version 0",
        "overread end of atom 'esds' by 4 bytes",
        "Unsupported DTS audio channel layout",
        "overread end of atom 'ddts' by 13 bytes"
    ]
);
diagnostic_case!(
    diagnostic_reversed_order,
    "reversed-opaque-order",
    [
        "ignoring multiple glbl",
        "ignoring multiple glbl",
        "Unsupported VP Codec Configuration box version 0",
        "overread end of atom 'esds' by 4 bytes",
        "Unsupported DTS audio channel layout",
        "overread end of atom 'ddts' by 13 bytes"
    ]
);
diagnostic_case!(
    diagnostic_one_byte_then_real,
    "one-byte-prefix-then-real",
    [
        "ignoring multiple glbl",
        "Unsupported VP Codec Configuration box version 0",
        "overread end of atom 'esds' by 4 bytes",
        "Unsupported DTS audio channel layout",
        "overread end of atom 'ddts' by 13 bytes"
    ]
);
diagnostic_case!(
    diagnostic_short_ddts7,
    "ddts-only-size-7",
    ["overread end of atom 'ddts' by 13 bytes"]
);
diagnostic_case!(
    diagnostic_short_ddts19,
    "ddts-only-size-19",
    ["overread end of atom 'ddts' by 1 bytes"]
);
diagnostic_case!(
    diagnostic_ddts_layout,
    "ddts-layout-ff00",
    ["Unsupported DTS audio channel layout"]
);
#[cfg(feature = "reference-trace")]
#[test]
fn diagnostic_backpressure_cancel_and_zero_work_keep_owned_pending_event() {
    let s = Source::new(include_bytes!(
        "fixtures/private-catalogue-v8/drop-dOps.mov"
    ));
    let mut job = PrivateDiagnosticJob::new(&s.table, &s.pool, Limits::default()).unwrap();
    let before = s.pool.used();
    let mut w = WorkBudget::new(0);
    assert_eq!(
        job.poll(&mut w, &AtomicBool::new(false)).unwrap(),
        BuildStatus::Yield
    );
    assert_eq!(s.pool.used(), before);
    let pressure = s.pool.reserve(s.pool.limit() - s.pool.used()).unwrap();
    let mut blocked = false;
    for _ in 0..10000 {
        match job.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Err(Error::Backpressure { .. }) => {
                blocked = true;
                break;
            }
            Ok(BuildStatus::Yield) => {}
            v => panic!("expected charged-event pressure {v:?}"),
        }
    }
    assert!(blocked);
    drop(pressure);
    for _ in 0..10000 {
        if job
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            == BuildStatus::Ready
        {
            let out = job.try_finish(&AtomicBool::new(false)).unwrap();
            assert_eq!(out.events.len(), 6);
            return;
        }
    }
    panic!("pressure retry lost event");
}

#[cfg(feature = "reference-trace")]
#[test]
fn diagnostic_cancel_is_sticky_and_never_publishes_partial_events() {
    let s = Source::new(include_bytes!(
        "fixtures/private-catalogue-v8/drop-dOps.mov"
    ));
    let mut job = PrivateDiagnosticJob::new(&s.table, &s.pool, Limits::default()).unwrap();
    assert_eq!(
        job.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        BuildStatus::Yield
    );
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        job.poll(&mut WorkBudget::new(100), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Cancelled
    );
    assert!(matches!(
        job.try_finish(&AtomicBool::new(false)),
        Err(Error::Cancelled)
    ));
}
