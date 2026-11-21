mod common;
use std::sync::atomic::AtomicBool;
use triergon_core::{Error, InputCursor, SourceFailure, WorkBudget};
use triergon_isobmff::{
    CursorStatus, Next,
    compat::mov::{
        edits::{EncodedEditJob, EncodedEditLimits, EncodedEditPlan, EncodedEditProfile},
        encoded::{EncodedConfigProfile, EncodedMetadataProfile, EncodedStream},
        packets::{PacketCursor, PacketLimits},
        seek::{IndexCursor, IndexLimits},
    },
};
const BASE: &[u8] = include_bytes!("fixtures/encoded-dependency-v4/dep-unreferenced.mp4");
#[path = "support/encoded_track.rs"]
mod encoded_track;
use encoded_track::{Source, finish, rows};

impl Source {
    fn job<'a>(&'a self, configs: &'a [EncodedStream], i: &InputCursor) -> EncodedEditJob<'a> {
        EncodedEditJob::new_with_dependency_table(
            self.context(&configs[0]),
            configs,
            i,
            EncodedEditLimits::default(),
            EncodedMetadataProfile,
        )
        .unwrap()
    }
    fn plan(&self, q: u64) -> Result<EncodedEditPlan, Error> {
        let mut i = self.input();
        let configs = self.configs(&mut i);
        finish(&mut self.job(&configs, &i), &mut i, q)
    }
}
#[test]
fn disposable_flag_is_separate_from_raw_dependency_stss_and_source_clock() {
    let s = Source::new(BASE);
    let rows = rows(&s, s.plan(1).unwrap());
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter().map(|r| r.demux.flags).collect::<Vec<_>>(),
        [17, 16]
    );
    assert_eq!(rows[0].source.dependency, Some(0x2a));
    assert_eq!(rows[1].source.dependency, Some(0x1a));
    assert!(rows[0].source.is_container_sync());
    assert!(!rows[1].source.is_container_sync());
    assert_eq!(rows[1].source.dts_ticks, Some(40));
    assert_eq!(rows[1].demux.dts, Some(40));
    assert_eq!(rows[0].source.description_index, 1);
}
#[test]
fn leading_byte_bits_remain_uninterpreted_source_facts() {
    let s = Source::new(include_bytes!(
        "fixtures/encoded-dependency-v4/dep-leading-raw-preserved.mp4"
    ));
    let rows = rows(&s, s.plan(1).unwrap());
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].source.dependency, Some(0xa6));
    assert_eq!(rows[1].source.dependency, Some(0x96));
    assert_eq!(
        rows.iter().map(|r| r.demux.flags).collect::<Vec<_>>(),
        [1, 0]
    );
}
#[test]
fn stss_is_not_overridden_by_a_conflicting_dependency_hint() {
    let s = Source::new(include_bytes!(
        "fixtures/encoded-dependency-v4/dep-STSS-conflict-preserved.mp4"
    ));
    let rows = rows(&s, s.plan(1).unwrap());
    assert_eq!(
        rows.iter().map(|r| r.demux.flags).collect::<Vec<_>>(),
        [1, 0]
    );
    assert_eq!(rows[0].source.dependency, Some(0x16));
    assert!(rows[0].source.is_container_sync());
    assert_eq!(rows[1].source.dependency, Some(0x26));
    assert!(!rows[1].source.is_container_sync());
}
#[test]
fn unknown_dependency_zero_does_not_invent_a_disposable_fact() {
    let s = Source::new(include_bytes!(
        "fixtures/encoded-dependency-v4/dep-no-hint.mp4"
    ));
    assert_eq!(
        rows(&s, s.plan(1).unwrap())
            .iter()
            .map(|r| r.demux.flags)
            .collect::<Vec<_>>(),
        [1, 0]
    );
}
#[test]
fn absent_groups_and_explicit_unassociated_groups_both_keep_source_ranges() {
    let a = Source::new(include_bytes!(
        "fixtures/encoded-dependency-v4/dep-no-groups.mp4"
    ));
    let b = Source::new(include_bytes!(
        "fixtures/encoded-dependency-v4/dep-referenced.mp4"
    ));
    let aa = rows(&a, a.plan(1).unwrap());
    let bb = rows(&b, b.plan(1).unwrap());
    assert_eq!(aa.len(), 2);
    assert_eq!(bb.len(), 2);
    for (a, b) in aa.iter().zip(&bb) {
        assert_eq!(a.source.dependency, b.source.dependency);
        assert_eq!(a.demux.flags, b.demux.flags);
        assert_eq!(
            a.source.range.end - a.source.range.start,
            b.source.range.end - b.source.range.start
        );
        assert_ne!(
            a.source.range.start, b.source.range.start,
            "real box growth changes the physical range"
        );
    }
}
#[test]
fn actual_roll_membership_stays_unadmitted_not_ignored() {
    let s = Source::new(include_bytes!(
        "fixtures/encoded-dependency-v4/dep-real-roll-unadmitted.mp4"
    ));
    assert!(matches!(s.plan(1), Err(Error::Unsupported(_))));
    assert_eq!(s.table.sample_count(), 2);
}
#[test]
fn reserved_depended_on_field_is_not_a_false_or_true_repair() {
    let s = Source::new(include_bytes!(
        "fixtures/encoded-dependency-v4/dep-reserved-referenced-field.mp4"
    ));
    assert_eq!(
        s.plan(1).unwrap_err(),
        Error::Unsupported("encoded dependency view reserved depended-on value")
    );
    let mut c = s.table.cursor();
    if let Next::Item(sample) = c.poll(&mut WorkBudget::new(1)).unwrap() {
        assert_eq!(sample.dependency, Some(0x2e));
    } else {
        panic!("source sample unexpectedly absent");
    }
}
#[test]
fn old_source_profile_remains_explicitly_unsupported() {
    let s = Source::new(BASE);
    let mut i = s.input();
    let configs = s.configs(&mut i);
    let mut j = EncodedEditJob::new_with_configs(
        s.context(&configs[0]),
        &configs,
        &i,
        EncodedEditLimits::default(),
        EncodedConfigProfile,
    )
    .unwrap();
    assert_eq!(
        finish(&mut j, &mut i, 1).unwrap_err(),
        Error::Unsupported("encoded edit recovery/clock/config profile")
    );
}
#[test]
fn group_work_zero_has_no_source_or_charge_progress() {
    let s = Source::new(BASE);
    let mut i = s.input();
    let configs = s.configs(&mut i);
    let mut j = s.job(&configs, &i);
    let used = s.pool.used();
    let pos = i.position();
    let mut w = WorkBudget::new(0);
    assert_eq!(
        j.poll(&mut i, &mut w, &AtomicBool::new(false)).unwrap(),
        CursorStatus::Yield
    );
    assert_eq!(w.spent(), 0);
    assert_eq!(s.pool.used(), used);
    assert_eq!(i.position(), pos);
    assert_eq!(
        finish(&mut j, &mut i, 1).unwrap().source_entry().media_time,
        0
    );
}
#[test]
fn work_one_and_bulk_validate_identical_group_and_sample_facts() {
    let s = Source::new(BASE);
    let a = rows(&s, s.plan(1).unwrap());
    let b = rows(&s, s.plan(4096).unwrap());
    assert_eq!(a.len(), b.len());
    for (a, b) in a.iter().zip(&b) {
        assert_eq!(a.source, b.source);
        assert_eq!(a.demux, b.demux);
    }
}
#[test]
fn cancel_precedes_group_work_and_is_sticky() {
    let s = Source::new(BASE);
    let mut i = s.input();
    let configs = s.configs(&mut i);
    let mut j = s.job(&configs, &i);
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(0), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(99), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Cancelled
    );
}
#[test]
fn transport_failure_precedes_zero_group_work() {
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
fn reset_generation_is_rejected_during_group_scan() {
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
fn charged_packet_pressure_retries_without_changing_the_validated_plan() {
    let s = Source::new(BASE);
    let plans = [Some(s.plan(1).unwrap())];
    let hold = s.pool.reserve(s.pool.limit() - s.pool.used()).unwrap();
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
    assert_eq!(rows(&s, plans[0].clone().unwrap()).len(), 2);
}
#[test]
fn index_and_packet_receiver_share_the_same_source_derived_flags() {
    let s = Source::new(BASE);
    let plan = s.plan(1).unwrap();
    let plans = [Some(plan.clone())];
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
                let entry = c.take_entry().unwrap();
                assert_eq!(
                    entry.occurrence.demux.flags,
                    if flags.is_empty() { 17 } else { 16 }
                );
                flags.push(entry.flags);
            }
            CursorStatus::End => {
                assert_eq!(flags, [1, 0]);
                return;
            }
        }
    }
    panic!("index progress");
}
