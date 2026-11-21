mod common;
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Error, InputCursor, SourceCapabilities, SourceFailure, SourceId, WorkBudget,
};
use triergon_isobmff::{
    CursorStatus,
    compat::mov::{
        edits::{EncodedEditJob, EncodedEditLimits, EncodedEditPlan, EncodedEditProfile},
        encoded::{
            DependedOn, DependencyCause, DependencyObservation, EncodedMetadataProfile,
            EncodedRecoveryProfile, EncodedStream,
        },
        packets::{PacketCursor, PacketLimits},
        seek::{IndexCursor, IndexLimits},
    },
    sample_groups::{RecoveryKind, RecoveryLimits, RecoveryLookup},
};
const BASE: &[u8] = include_bytes!("fixtures/encoded-recovery/roll-negative-v0.mp4");
#[path = "support/encoded_track.rs"]
mod encoded_track;
use encoded_track::{Source, finish, rows};

impl Source {
    fn job<'a>(&'a self, configs: &'a [EncodedStream], i: &InputCursor) -> EncodedEditJob<'a> {
        EncodedEditJob::new_with_recovery_groups(
            self.context(&configs[0]),
            configs,
            i,
            EncodedEditLimits::default(),
            EncodedRecoveryProfile,
        )
        .unwrap()
    }
    fn plan(&self, q: u64) -> Result<EncodedEditPlan, Error> {
        let mut i = self.input();
        let configs = self.configs(&mut i);
        finish(&mut self.job(&configs, &i), &mut i, q)
    }
}
fn fixture(name: &str) -> &'static [u8] {
    match name {
        "coverage-under" => include_bytes!("fixtures/encoded-recovery/coverage-under.mp4"),
        "empty-run" => include_bytes!("fixtures/encoded-recovery/empty-run.mp4"),
        "mapping-before-description" => {
            include_bytes!("fixtures/encoded-recovery/mapping-before-description.mp4")
        }
        "description-without-map" => {
            include_bytes!("fixtures/encoded-recovery/description-without-map.mp4")
        }
        "roll-zero-association" => {
            include_bytes!("fixtures/encoded-recovery/roll-zero-association.mp4")
        }
        "duplicate-description" => {
            include_bytes!("fixtures/encoded-recovery/duplicate-description.mp4")
        }
        "prol-negative-v1" => include_bytes!("fixtures/encoded-recovery/prol-negative-v1.mp4"),
        "roll-negative-v0" => include_bytes!("fixtures/encoded-recovery/roll-negative-v0.mp4"),
        "roll-positive-v1" => include_bytes!("fixtures/encoded-recovery/roll-positive-v1.mp4"),
        "grouping-parameter" => {
            include_bytes!("fixtures/encoded-recovery/grouping-parameter.mp4")
        }
        "dangling-description" => {
            include_bytes!("fixtures/encoded-recovery/dangling-description.mp4")
        }
        "fragment-namespace" => {
            include_bytes!("fixtures/encoded-recovery/fragment-namespace.mp4")
        }
        "roll-prol-independent" => {
            include_bytes!("fixtures/encoded-recovery/roll-prol-independent.mp4")
        }
        "coverage-over" => include_bytes!("fixtures/encoded-recovery/coverage-over.mp4"),
        "roll-multiple-descriptions" => {
            include_bytes!("fixtures/encoded-recovery/roll-multiple-descriptions.mp4")
        }
        "variable-length-unadmitted" => {
            include_bytes!("fixtures/encoded-recovery/variable-length-unadmitted.mp4")
        }
        "unknown-group-unadmitted" => {
            include_bytes!("fixtures/encoded-recovery/unknown-group-unadmitted.mp4")
        }
        "reserved-source-retained" => {
            include_bytes!("fixtures/encoded-recovery/reserved-source-retained.mp4")
        }
        _ => panic!("unknown authored fixture"),
    }
}
fn selection(
    plan: &EncodedEditPlan,
    kind: RecoveryKind,
    ordinal: u64,
) -> triergon_isobmff::sample_groups::RecoverySelection {
    let facts = plan
        .recovery_facts()
        .expect("validated owned source group facts");
    let mut lookup =
        RecoveryLookup::new(facts.source_scope(kind), ordinal, RecoveryLimits::default()).unwrap();
    for _ in 0..1000 {
        let mut w = WorkBudget::new(1);
        if lookup.poll(&mut w, &AtomicBool::new(false)).unwrap()
            == triergon_isobmff::BuildStatus::Ready
        {
            assert!(w.spent() <= 1);
            return lookup.finish().unwrap();
        }
        assert!(w.spent() <= 1);
    }
    panic!("source recovery lookup progress");
}
#[test]
fn actual_roll_keeps_signed_source_range_and_literal_packet_clocks() {
    let s = Source::new(BASE);
    let plan = s.plan(1).unwrap();
    let source = selection(&plan, RecoveryKind::Roll, 2);
    assert_eq!(source.raw_mapping_index, Some(1));
    let d = source.description.unwrap();
    assert_eq!(d.distance, -1);
    assert_eq!(d.raw_bits, 65535);
    assert_eq!(
        &BASE[d.source_range.start as usize..d.source_range.end as usize],
        &[255, 255]
    );
    let result = rows(&s, plan);
    assert_eq!(result.len(), 2);
    assert_eq!(
        (result[0].demux.pts, result[1].demux.pts),
        (Some(0), Some(40))
    );
    assert_eq!(
        (result[0].demux.dts, result[1].demux.dts),
        (Some(0), Some(40))
    );
    assert_eq!(
        (result[0].demux.duration, result[1].demux.duration),
        (40, 40)
    );
    assert_eq!((result[0].demux.flags, result[1].demux.flags), (1, 0));
    assert_eq!(
        (result[0].source.dependency, result[1].source.dependency),
        (Some(0x26), Some(0x16))
    );
    assert!(result[0].source.is_container_sync());
    assert!(!result[1].source.is_container_sync());
    for item in result {
        assert_eq!(item.demux.source_range, item.source.range);
    }
}
#[test]
fn reserved_dependency_stays_reserved_while_receiver_adds_no_hint() {
    let s = Source::new(fixture("reserved-source-retained"));
    let result = rows(&s, s.plan(1).unwrap());
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].source.dependency, Some(0x2e));
    assert_eq!((result[0].demux.flags, result[1].demux.flags), (1, 16));
    let observation = DependencyObservation::from_source(0x2e, EncodedRecoveryProfile);
    assert_eq!(observation.source.raw, 0x2e);
    assert_eq!(observation.source.depended_on, DependedOn::Reserved);
    assert_eq!(
        observation.cause,
        DependencyCause::SourceReservedNotAppliedByReceiver
    );
    assert!(!observation.demux_disposable_hint());
}
#[test]
fn prol_and_roll_have_separate_owned_namespaces() {
    let s = Source::new(fixture("roll-prol-independent"));
    let plan = s.plan(1).unwrap();
    assert_eq!(
        selection(&plan, RecoveryKind::Roll, 2)
            .description
            .unwrap()
            .distance,
        -1
    );
    assert_eq!(
        selection(&plan, RecoveryKind::Prol, 2)
            .description
            .unwrap()
            .distance,
        -2
    );
    assert_eq!(rows(&s, plan).len(), 2);
}
#[test]
fn mapping_before_description_resolves_against_real_later_box() {
    let s = Source::new(fixture("mapping-before-description"));
    let plan = s.plan(1).unwrap();
    assert_eq!(
        selection(&plan, RecoveryKind::Roll, 2)
            .description
            .unwrap()
            .distance,
        -1
    );
    assert_eq!(rows(&s, plan).len(), 2);
}
#[test]
fn positive_roll_stays_positive_without_invented_decoder_preroll() {
    let s = Source::new(fixture("roll-positive-v1"));
    let plan = s.plan(1).unwrap();
    let source = selection(&plan, RecoveryKind::Roll, 1);
    assert_eq!(source.description.as_ref().unwrap().distance, 1);
    assert_eq!(
        source.preceding_samples(),
        Err(Error::Unsupported(
            "positive recovery distance needs postroll"
        ))
    );
    assert_eq!(rows(&s, plan).len(), 2);
}
#[test]
fn zero_mapping_is_absent_not_description_one() {
    let s = Source::new(fixture("roll-zero-association"));
    let plan = s.plan(1).unwrap();
    assert_eq!(
        selection(&plan, RecoveryKind::Roll, 1).raw_mapping_index,
        Some(0)
    );
    assert!(
        selection(&plan, RecoveryKind::Roll, 1)
            .description
            .is_none()
    );
    assert_eq!(
        selection(&plan, RecoveryKind::Roll, 2)
            .description
            .unwrap()
            .distance,
        -1
    );
}
#[test]
fn multiple_descriptions_are_read_in_source_order() {
    let s = Source::new(fixture("roll-multiple-descriptions"));
    let plan = s.plan(1).unwrap();
    assert_eq!(
        selection(&plan, RecoveryKind::Roll, 1)
            .description
            .unwrap()
            .distance,
        -1
    );
    assert_eq!(
        selection(&plan, RecoveryKind::Roll, 2)
            .description
            .unwrap()
            .distance,
        1
    );
}
#[test]
fn description_without_map_is_retained_but_unassociated() {
    let s = Source::new(fixture("description-without-map"));
    let plan = s.plan(1).unwrap();
    assert_eq!(
        plan.recovery_facts()
            .unwrap()
            .group(RecoveryKind::Roll)
            .descriptions()
            .unwrap()
            .entry_count(),
        1
    );
    assert!(
        selection(&plan, RecoveryKind::Roll, 1)
            .description
            .is_none()
    );
}
#[test]
fn dangling_index_is_rejected_before_any_packet() {
    let s = Source::new(fixture("dangling-description"));
    assert_eq!(
        s.plan(1).unwrap_err(),
        Error::Invalid("encoded unresolved recovery description")
    );
}
#[test]
fn zero_count_run_is_rejected_before_any_packet() {
    assert_eq!(
        Source::new(fixture("empty-run")).plan(1).unwrap_err(),
        Error::Invalid("encoded empty recovery run")
    );
}
#[test]
fn too_many_source_samples_in_map_is_rejected() {
    assert_eq!(
        Source::new(fixture("coverage-over")).plan(1).unwrap_err(),
        Error::Invalid("encoded recovery exceeds samples")
    );
}
#[test]
fn incomplete_source_coverage_remains_explicitly_unadmitted() {
    assert_eq!(
        Source::new(fixture("coverage-under")).plan(1).unwrap_err(),
        Error::Unsupported("encoded incomplete recovery coverage")
    );
}
#[test]
fn fragment_namespace_cannot_resolve_as_regular_track_description() {
    assert_eq!(
        Source::new(fixture("fragment-namespace"))
            .plan(1)
            .unwrap_err(),
        Error::Unsupported("encoded fragment recovery namespace")
    );
}
#[test]
fn unknown_grouping_parameter_stays_visible_and_unadmitted() {
    assert_eq!(
        Source::new(fixture("grouping-parameter"))
            .plan(1)
            .unwrap_err(),
        Error::Unsupported("encoded recovery grouping parameter")
    );
}
#[test]
fn variable_lengths_do_not_trigger_repeated_prefix_parsing() {
    assert_eq!(
        Source::new(fixture("variable-length-unadmitted"))
            .plan(1)
            .unwrap_err(),
        Error::Unsupported("encoded variable or extended recovery entries")
    );
}
#[test]
fn duplicate_context_does_not_silently_replace_prior_backing() {
    assert_eq!(
        Source::new(fixture("duplicate-description"))
            .plan(1)
            .unwrap_err(),
        Error::Invalid("encoded duplicate recovery context")
    );
}
#[test]
fn unknown_group_semantics_do_not_get_roll_fallback() {
    assert_eq!(
        Source::new(fixture("unknown-group-unadmitted"))
            .plan(1)
            .unwrap_err(),
        Error::Unsupported("encoded other sample-group semantics")
    );
}
#[test]
fn old_actual_membership_refusal_is_unchanged() {
    let s = Source::new(BASE);
    let mut i = s.input();
    let configs = s.configs(&mut i);
    let mut old = EncodedEditJob::new_with_dependency_table(
        s.context(&configs[0]),
        &configs,
        &i,
        EncodedEditLimits::default(),
        EncodedMetadataProfile,
    )
    .unwrap();
    assert_eq!(
        finish(&mut old, &mut i, 1).unwrap_err(),
        Error::Unsupported("encoded dependency view nonempty recovery descriptions")
    );
}
#[test]
fn source_group_backing_survives_producer_and_pool_handles() {
    let (pool, plan, before) = {
        let s = Source::new(BASE);
        let plan = s.plan(1).unwrap();
        let before = s.pool.used();
        (s.pool.clone(), plan, before)
    };
    assert!(pool.used() > Budget::control_storage_bytes());
    assert!(pool.used() <= before);
    let source = selection(&plan, RecoveryKind::Roll, 2).description.unwrap();
    assert_eq!(source.distance, -1);
    let raw = plan
        .recovery_facts()
        .unwrap()
        .group(RecoveryKind::Roll)
        .descriptions()
        .unwrap()
        .raw()
        .bytes()
        .clone();
    drop(plan);
    assert!(pool.used() > Budget::control_storage_bytes());
    drop(raw);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn zero_work_does_not_charge_allocate_or_read_group_bytes() {
    let s = Source::new(BASE);
    let mut i = s.input();
    let configs = s.configs(&mut i);
    let mut job = s.job(&configs, &i);
    let used = s.pool.used();
    let position = i.position();
    let mut w = WorkBudget::new(0);
    assert_eq!(
        job.poll(&mut i, &mut w, &AtomicBool::new(false)).unwrap(),
        CursorStatus::Yield
    );
    assert_eq!(w.spent(), 0);
    assert_eq!(s.pool.used(), used);
    assert_eq!(i.position(), position);
    assert_eq!(rows(&s, finish(&mut job, &mut i, 1).unwrap()).len(), 2);
}
#[test]
fn work_one_and_bulk_preserve_all_source_fields_and_receiver_rows() {
    let s = Source::new(fixture("roll-prol-independent"));
    let a = rows(&s, s.plan(1).unwrap());
    let b = rows(&s, s.plan(4096).unwrap());
    assert_eq!(a.len(), b.len());
    for (a, b) in a.iter().zip(&b) {
        assert_eq!(a.source, b.source);
        assert_eq!(a.demux, b.demux);
    }
}
#[test]
fn cancel_during_group_scan_is_sticky_and_publishes_nothing() {
    let s = Source::new(BASE);
    let mut i = s.input();
    let configs = s.configs(&mut i);
    let mut j = s.job(&configs, &i);
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        CursorStatus::Yield
    );
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(0), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(4096), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        j.take(&mut i, &AtomicBool::new(false)).unwrap_err(),
        Error::Cancelled
    );
}
#[test]
fn failed_transport_precedes_zero_group_work() {
    let s = Source::new(BASE);
    let mut i = s.input();
    let configs = s.configs(&mut i);
    let mut j = s.job(&configs, &i);
    i.fail(i.source(), i.token(), SourceFailure::Transport)
        .unwrap();
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Transport
    );
}
#[test]
fn reset_invalidates_group_scan_and_ready_backing() {
    let s = Source::new(BASE);
    let mut i = s.input();
    let configs = s.configs(&mut i);
    let mut j = s.job(&configs, &i);
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        CursorStatus::Yield
    );
    i.reset(0).unwrap();
    assert!(matches!(
        j.poll(&mut i, &mut WorkBudget::new(0), &AtomicBool::new(false)),
        Err(Error::StaleGeneration { .. })
    ));
}
#[test]
fn foreign_pool_is_rejected_before_group_work() {
    let s = Source::new(BASE);
    let i = s.input();
    let mut other = Source::new(BASE).input();
    let mut owned = s.input();
    let configs = s.configs(&mut owned);
    let mut job = s.job(&configs, &i);
    assert_eq!(
        job.poll(&mut other, &mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap_err(),
        Error::ForeignPool
    );
}
#[test]
fn validated_source_plan_survives_output_pressure_without_extra_group_allocation() {
    let s = Source::new(BASE);
    let plan = s.plan(1).unwrap();
    let group_backing = plan
        .recovery_facts()
        .unwrap()
        .group(RecoveryKind::Roll)
        .descriptions()
        .unwrap()
        .raw()
        .bytes()
        .clone();
    let plans = [Some(plan.clone())];
    let hold = s.pool.reserve(s.pool.available()).unwrap();
    assert!(matches!(
        PacketCursor::new_encoded_regular(
            &s.pool,
            &[s.track()],
            &s.media,
            PacketLimits::default(),
            &plans,
            EncodedEditProfile
        ),
        Err(Error::Backpressure { .. })
    ));
    drop(hold);
    assert_eq!(rows(&s, plan).len(), 2);
    assert!(!group_backing.is_empty());
}
#[test]
fn packet_hint_does_not_become_public_index_key() {
    let s = Source::new(fixture("reserved-source-retained"));
    let plans = [Some(s.plan(1).unwrap())];
    let mut c = IndexCursor::new_encoded_regular(
        &s.pool,
        s.track(),
        &s.media,
        IndexLimits::default(),
        &plans,
        EncodedEditProfile,
    )
    .unwrap();
    let mut flags = Vec::new();
    for _ in 0..20000 {
        match c
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            CursorStatus::Yield => {}
            CursorStatus::Ready => {
                let e = c.take_entry().unwrap();
                assert_eq!(
                    e.occurrence.source.dependency,
                    Some(if flags.is_empty() { 0x2e } else { 0x1a })
                );
                flags.push(e.flags);
            }
            CursorStatus::End => {
                assert_eq!(flags, [1, 0]);
                return;
            }
        }
    }
    panic!("index work1 progress");
}
#[test]
fn original_reserved_without_membership_keeps_raw_and_packet_hint_absent() {
    let s = Source::new(include_bytes!(
        "fixtures/encoded-dependency/dep-reserved-referenced-field.mp4"
    ));
    let result = rows(&s, s.plan(1).unwrap());
    assert_eq!(result.len(), 2);
    assert_eq!(
        (result[0].source.dependency, result[1].source.dependency),
        (Some(0x2e), Some(0x16))
    );
    assert_eq!((result[0].demux.flags, result[1].demux.flags), (1, 0));
}
#[test]
fn same_pool_other_source_is_rejected_before_inspection() {
    let s = Source::new(BASE);
    let mut i = s.input();
    let configs = s.configs(&mut i);
    let mut j = s.job(&configs, &i);
    let mut other = InputCursor::new(
        &s.pool,
        SourceId(1998),
        SourceCapabilities {
            length: Some(s.length),
            random_access: true,
        },
        64,
    )
    .unwrap();
    assert_eq!(
        j.poll(&mut other, &mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap_err(),
        Error::SourceMismatch
    );
}
#[test]
fn configured_sample_limit_still_bounds_source_scan() {
    let s = Source::new(BASE);
    let mut i = s.input();
    let configs = s.configs(&mut i);
    let mut j = EncodedEditJob::new_with_recovery_groups(
        s.context(&configs[0]),
        &configs,
        &i,
        EncodedEditLimits { max_samples: 1 },
        EncodedRecoveryProfile,
    )
    .unwrap();
    assert_eq!(
        finish(&mut j, &mut i, 1).unwrap_err(),
        Error::Limit {
            requested: 2,
            maximum: 1
        }
    );
}
