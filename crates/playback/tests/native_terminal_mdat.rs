mod common;
use common::{byte_input, run_open};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, SourceCapabilities, SourceId, WorkBudget};
use triergon_playback::{
    ByteSource,
    mp4_open::{Mp4Open, OpenLimits, OpenedMp4},
    mp4_plan::{Mp4IndexJob, Mp4View, PlanLimits},
};

const REGULAR: &[u8] = include_bytes!("fixtures/open-ended/regular-terminal.mp4");
#[allow(dead_code)]
mod original_source_gold {
    include!("fixtures/mp4_golds.rs");
}

fn open(raw: &[u8], known: bool) -> (OpenedMp4<ByteSource>, Budget) {
    let pool = Budget::new(64 << 20).unwrap();
    let id = SourceId(7601);
    let capabilities = SourceCapabilities {
        length: known.then_some(raw.len() as u64),
        random_access: false,
    };
    let selected = byte_input(&pool, id, raw, 7, capabilities, 64).unwrap();
    let mut opener = Mp4Open::new(selected, OpenLimits::default()).unwrap();
    run_open(&mut opener, 1).unwrap();
    (opener.finish().unwrap(), pool)
}

fn selected_native_plan(known: bool) {
    let (opened, _pool) = open(REGULAR, known);
    let mut job = Mp4IndexJob::new_native_terminal_v1(&opened.movie, PlanLimits::default())
        .expect("explicit native terminal profile must admit actual front terminal metadata");
    for _ in 0..200_000 {
        if job
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            let plan = job.finish().unwrap();
            if known {
                assert!(plan.terminal_mdat().is_none());
                assert!(
                    matches!(plan.read_scope(), triergon_playback::mp4_open::MetadataReadScope::FrontMetadataBeforeMdat(h) if h.end().unwrap() == Some(REGULAR.len() as u64))
                );
            } else {
                assert!(plan.terminal_mdat().is_some());
            }
            let original = original_source_gold::CASES
                .iter()
                .find(|c| c.name == "r-front-stco")
                .unwrap();
            assert_eq!(plan.native_rows().len(), original.samples.len());
            for row in plan.native_rows() {
                if let Some(terminal) = plan.terminal_mdat() {
                    terminal.require_sample(&row.source).unwrap();
                }
                assert!(row.source.range.end <= REGULAR.len() as u64);
            }
            return;
        }
    }
    panic!("bounded explicit native terminal index");
}

#[test]
fn native_terminal_unknown_length_regular_index_is_complete() {
    selected_native_plan(false);
}

#[test]
fn native_terminal_known_length_regular_index_is_complete() {
    selected_native_plan(true);
}

#[test]
fn default_native_terminal_refusal_stays_explicit() {
    let (opened, _pool) = open(REGULAR, false);
    assert!(
        Mp4IndexJob::new(
            &opened.movie,
            Mp4View::PinnedMov9NoParseNoFillin,
            PlanLimits::default()
        )
        .is_err()
    );
}

#[test]
fn finite_initial_native_front_plan_cannot_use_unattached_indexed_end() {
    let raw = include_bytes!("fixtures/open-ended/pending-terminal.mp4");
    let (opened, _pool) = open(raw, false);
    let mut job =
        Mp4IndexJob::new_native_terminal_v1(&opened.movie, PlanLimits::default()).unwrap();
    while !job
        .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
    {}
    let plan = job.finish().unwrap();
    assert!(plan.terminal_mdat().is_none());
    assert_eq!(plan.rows(), 3);
    assert!(matches!(
        triergon_playback::Engine::mp4(opened.input, plan, 1 << 20),
        Err(triergon_core::Error::Unsupported(
            "native front plan requires actual source completion"
        ))
    ));
}
