mod common;
use common::{child, children, roots, table};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{
    Budget, Error, InputCursor, SourceCapabilities, SourceFailure, SourceId, TimeBase, WorkBudget,
};
use triergon_isobmff::{
    EditList, Limits, Mov9CursorStatus, Mov9DependedOnV4, Mov9DependencyCauseV5,
    Mov9DependencyObservationV5, Mov9EncodedEditContextV2, Mov9EncodedEditJobV2,
    Mov9EncodedEditLimitsV2, Mov9EncodedEditPlanV2, Mov9EncodedLimitsV1, Mov9EncodedSelectionV4,
    Mov9EncodedStreamJobV1, Mov9EncodedStreamV1, Mov9NativeIndexCursor, Mov9NativeIndexLimits,
    Mov9PacketCursor, Mov9PacketLimits, Mov9Track, PinnedMovD32EncodedEditsV2,
    PinnedMovD32EncodedRecoveryV5, PinnedMovD32EncodedRegularV4, RecoveryKind, RecoveryLimits,
    RecoveryLookup, SampleTable, TrackHeader,
};
const BASE: &[u8] = include_bytes!("fixtures/encoded-recovery-v5/roll-negative-v0.mp4");
struct Source {
    pool: Budget,
    header: TrackHeader,
    table: SampleTable,
    edits: EditList,
    media: Vec<Range<u64>>,
    length: u64,
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
        let edits = EditList::parse(
            child(&child(&track, b"edts").unwrap(), b"elst").unwrap(),
            Limits::default(),
        )
        .unwrap();
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
    fn configs(&self, i: &mut InputCursor) -> Vec<Mov9EncodedStreamV1> {
        let mut j = Mov9EncodedStreamJobV1::new_description_v4(
            &self.header,
            &self.table,
            i,
            Mov9EncodedSelectionV4 {
                major_brand: Some(*b"isom"),
                description: 1,
                limits: Mov9EncodedLimitsV1::default(),
            },
            PinnedMovD32EncodedRegularV4,
        )
        .unwrap();
        for _ in 0..20000 {
            if j.poll(i, &mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
                == Mov9CursorStatus::Ready
            {
                return vec![j.take(i, &AtomicBool::new(false)).unwrap()];
            }
        }
        panic!("config progress");
    }
    fn context<'a>(&'a self, s: &'a Mov9EncodedStreamV1) -> Mov9EncodedEditContextV2<'a> {
        Mov9EncodedEditContextV2 {
            table: &self.table,
            edits: &self.edits,
            stream: s,
            media_base: self.header.time_base().unwrap(),
            movie_base: TimeBase::new(1, 1000).unwrap(),
        }
    }
    fn job<'a>(
        &'a self,
        configs: &'a [Mov9EncodedStreamV1],
        i: &InputCursor,
    ) -> Mov9EncodedEditJobV2<'a> {
        Mov9EncodedEditJobV2::new_v5(
            self.context(&configs[0]),
            configs,
            i,
            Mov9EncodedEditLimitsV2::default(),
            PinnedMovD32EncodedRecoveryV5,
        )
        .unwrap()
    }
    fn plan(&self, q: u64) -> Result<Mov9EncodedEditPlanV2, Error> {
        let mut i = self.input();
        let configs = self.configs(&mut i);
        finish(&mut self.job(&configs, &i), &mut i, q)
    }
    fn track(&self) -> Mov9Track<'_> {
        Mov9Track {
            track_id: self.header.track_id,
            table: &self.table,
            fragments: &[],
            media_base: self.header.time_base().unwrap(),
            edits: Some((&self.edits, TimeBase::new(1, 1000).unwrap())),
        }
    }
}
fn finish(
    j: &mut Mov9EncodedEditJobV2<'_>,
    i: &mut InputCursor,
    q: u64,
) -> Result<Mov9EncodedEditPlanV2, Error> {
    for _ in 0..20000 {
        let mut w = WorkBudget::new(q);
        let state = j.poll(i, &mut w, &AtomicBool::new(false))?;
        assert!(w.spent() <= q);
        if state == Mov9CursorStatus::Ready {
            return j.take(i, &AtomicBool::new(false));
        }
    }
    panic!("group/sample actual-work progress");
}
fn rows(s: &Source, plan: Mov9EncodedEditPlanV2) -> Vec<triergon_isobmff::Mov9PacketOccurrence> {
    let plans = [Some(plan)];
    let mut c = Mov9PacketCursor::new_encoded_regular_v2(
        &s.pool,
        &[s.track()],
        &s.media,
        Mov9PacketLimits::default(),
        &plans,
        PinnedMovD32EncodedEditsV2,
    )
    .unwrap();
    let mut rows = Vec::new();
    for _ in 0..20000 {
        let mut w = WorkBudget::new(1);
        match c.poll(&mut w, &AtomicBool::new(false)).unwrap() {
            Mov9CursorStatus::Yield => {}
            Mov9CursorStatus::End => return rows,
            Mov9CursorStatus::Ready => rows.push(c.take_occurrence().unwrap()),
        }
        assert!(w.spent() <= 1);
    }
    panic!("packet work1 progress");
}
fn fixture(name: &str) -> &'static [u8] {
    match name {
        "coverage-under" => include_bytes!("fixtures/encoded-recovery-v5/coverage-under.mp4"),
        "empty-run" => include_bytes!("fixtures/encoded-recovery-v5/empty-run.mp4"),
        "mapping-before-description" => {
            include_bytes!("fixtures/encoded-recovery-v5/mapping-before-description.mp4")
        }
        "description-without-map" => {
            include_bytes!("fixtures/encoded-recovery-v5/description-without-map.mp4")
        }
        "roll-zero-association" => {
            include_bytes!("fixtures/encoded-recovery-v5/roll-zero-association.mp4")
        }
        "duplicate-description" => {
            include_bytes!("fixtures/encoded-recovery-v5/duplicate-description.mp4")
        }
        "prol-negative-v1" => include_bytes!("fixtures/encoded-recovery-v5/prol-negative-v1.mp4"),
        "roll-negative-v0" => include_bytes!("fixtures/encoded-recovery-v5/roll-negative-v0.mp4"),
        "roll-positive-v1" => include_bytes!("fixtures/encoded-recovery-v5/roll-positive-v1.mp4"),
        "grouping-parameter" => {
            include_bytes!("fixtures/encoded-recovery-v5/grouping-parameter.mp4")
        }
        "dangling-description" => {
            include_bytes!("fixtures/encoded-recovery-v5/dangling-description.mp4")
        }
        "fragment-namespace" => {
            include_bytes!("fixtures/encoded-recovery-v5/fragment-namespace.mp4")
        }
        "roll-prol-independent" => {
            include_bytes!("fixtures/encoded-recovery-v5/roll-prol-independent.mp4")
        }
        "coverage-over" => include_bytes!("fixtures/encoded-recovery-v5/coverage-over.mp4"),
        "roll-multiple-descriptions" => {
            include_bytes!("fixtures/encoded-recovery-v5/roll-multiple-descriptions.mp4")
        }
        "variable-length-unadmitted" => {
            include_bytes!("fixtures/encoded-recovery-v5/variable-length-unadmitted.mp4")
        }
        "unknown-group-unadmitted" => {
            include_bytes!("fixtures/encoded-recovery-v5/unknown-group-unadmitted.mp4")
        }
        "reserved-source-retained" => {
            include_bytes!("fixtures/encoded-recovery-v5/reserved-source-retained.mp4")
        }
        _ => panic!("unknown authored fixture"),
    }
}
fn selection(
    plan: &Mov9EncodedEditPlanV2,
    kind: RecoveryKind,
    ordinal: u64,
) -> triergon_isobmff::RecoverySelection {
    let facts = plan
        .recovery_source_v5()
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
        (result[0].native.pts, result[1].native.pts),
        (Some(0), Some(40))
    );
    assert_eq!(
        (result[0].native.dts, result[1].native.dts),
        (Some(0), Some(40))
    );
    assert_eq!(
        (result[0].native.duration, result[1].native.duration),
        (40, 40)
    );
    assert_eq!((result[0].native.flags, result[1].native.flags), (1, 0));
    assert_eq!(
        (result[0].source.dependency, result[1].source.dependency),
        (Some(0x26), Some(0x16))
    );
    assert!(result[0].source.is_container_sync());
    assert!(!result[1].source.is_container_sync());
    for item in result {
        assert_eq!(item.native.source_range, item.source.range);
    }
}
#[test]
fn reserved_dependency_stays_reserved_while_receiver_adds_no_hint() {
    let s = Source::new(fixture("reserved-source-retained"));
    let result = rows(&s, s.plan(1).unwrap());
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].source.dependency, Some(0x2e));
    assert_eq!((result[0].native.flags, result[1].native.flags), (1, 16));
    let observation = Mov9DependencyObservationV5::from_source(0x2e, PinnedMovD32EncodedRecoveryV5);
    assert_eq!(observation.source.raw, 0x2e);
    assert_eq!(observation.source.depended_on, Mov9DependedOnV4::Reserved);
    assert_eq!(
        observation.cause,
        Mov9DependencyCauseV5::SourceReservedNotAppliedByReceiver
    );
    assert!(!observation.native_disposable_hint());
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
        plan.recovery_source_v5()
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
        Error::Invalid("encoded V5 unresolved recovery description")
    );
}
#[test]
fn zero_count_run_is_rejected_before_any_packet() {
    assert_eq!(
        Source::new(fixture("empty-run")).plan(1).unwrap_err(),
        Error::Invalid("encoded V5 empty recovery run")
    );
}
#[test]
fn too_many_source_samples_in_map_is_rejected() {
    assert_eq!(
        Source::new(fixture("coverage-over")).plan(1).unwrap_err(),
        Error::Invalid("encoded V5 recovery exceeds samples")
    );
}
#[test]
fn incomplete_source_coverage_remains_explicitly_unadmitted() {
    assert_eq!(
        Source::new(fixture("coverage-under")).plan(1).unwrap_err(),
        Error::Unsupported("encoded V5 incomplete recovery coverage")
    );
}
#[test]
fn fragment_namespace_cannot_resolve_as_regular_track_description() {
    assert_eq!(
        Source::new(fixture("fragment-namespace"))
            .plan(1)
            .unwrap_err(),
        Error::Unsupported("encoded V5 fragment recovery namespace")
    );
}
#[test]
fn unknown_grouping_parameter_stays_visible_and_unadmitted() {
    assert_eq!(
        Source::new(fixture("grouping-parameter"))
            .plan(1)
            .unwrap_err(),
        Error::Unsupported("encoded V5 recovery grouping parameter")
    );
}
#[test]
fn variable_lengths_do_not_trigger_repeated_prefix_parsing() {
    assert_eq!(
        Source::new(fixture("variable-length-unadmitted"))
            .plan(1)
            .unwrap_err(),
        Error::Unsupported("encoded V5 variable or extended recovery entries")
    );
}
#[test]
fn duplicate_context_does_not_silently_replace_prior_owner() {
    assert_eq!(
        Source::new(fixture("duplicate-description"))
            .plan(1)
            .unwrap_err(),
        Error::Invalid("encoded V5 duplicate recovery context")
    );
}
#[test]
fn unknown_group_semantics_do_not_get_roll_fallback() {
    assert_eq!(
        Source::new(fixture("unknown-group-unadmitted"))
            .plan(1)
            .unwrap_err(),
        Error::Unsupported("encoded V5 other sample-group semantics")
    );
}
#[test]
fn old_v4_actual_membership_refusal_is_unchanged() {
    let s = Source::new(BASE);
    let mut i = s.input();
    let configs = s.configs(&mut i);
    let mut old = Mov9EncodedEditJobV2::new_v4(
        s.context(&configs[0]),
        &configs,
        &i,
        Mov9EncodedEditLimitsV2::default(),
        PinnedMovD32EncodedRegularV4,
    )
    .unwrap();
    assert_eq!(
        finish(&mut old, &mut i, 1).unwrap_err(),
        Error::Unsupported("encoded V4 nonempty recovery descriptions")
    );
}
#[test]
fn source_group_owner_survives_producer_and_pool_handles() {
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
        .recovery_source_v5()
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
        Mov9CursorStatus::Yield
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
        assert_eq!(a.native, b.native);
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
        Mov9CursorStatus::Yield
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
fn reset_invalidates_group_scan_and_ready_owner() {
    let s = Source::new(BASE);
    let mut i = s.input();
    let configs = s.configs(&mut i);
    let mut j = s.job(&configs, &i);
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        Mov9CursorStatus::Yield
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
    let group_owner = plan
        .recovery_source_v5()
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
        Mov9PacketCursor::new_encoded_regular_v2(
            &s.pool,
            &[s.track()],
            &s.media,
            Mov9PacketLimits::default(),
            &plans,
            PinnedMovD32EncodedEditsV2
        ),
        Err(Error::Backpressure { .. })
    ));
    drop(hold);
    assert_eq!(rows(&s, plan).len(), 2);
    assert!(!group_owner.is_empty());
}
#[test]
fn packet_hint_does_not_become_public_index_key() {
    let s = Source::new(fixture("reserved-source-retained"));
    let plans = [Some(s.plan(1).unwrap())];
    let mut c = Mov9NativeIndexCursor::new_encoded_regular_v2(
        &s.pool,
        s.track(),
        &s.media,
        Mov9NativeIndexLimits::default(),
        &plans,
        PinnedMovD32EncodedEditsV2,
    )
    .unwrap();
    let mut flags = Vec::new();
    for _ in 0..20000 {
        match c
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Mov9CursorStatus::Yield => {}
            Mov9CursorStatus::Ready => {
                let e = c.take_entry().unwrap();
                assert_eq!(
                    e.occurrence.source.dependency,
                    Some(if flags.is_empty() { 0x2e } else { 0x1a })
                );
                flags.push(e.flags);
            }
            Mov9CursorStatus::End => {
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
        "fixtures/encoded-dependency-v4/dep-reserved-referenced-field.mp4"
    ));
    let result = rows(&s, s.plan(1).unwrap());
    assert_eq!(result.len(), 2);
    assert_eq!(
        (result[0].source.dependency, result[1].source.dependency),
        (Some(0x2e), Some(0x16))
    );
    assert_eq!((result[0].native.flags, result[1].native.flags), (1, 0));
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
    let mut j = Mov9EncodedEditJobV2::new_v5(
        s.context(&configs[0]),
        &configs,
        &i,
        Mov9EncodedEditLimitsV2 { max_samples: 1 },
        PinnedMovD32EncodedRecoveryV5,
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
