use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, WorkBudget};
use triergon_subtitles::dvd::{HdLimits, HdSpuJob, HdStatus};
fn check(raw: &[u8], expected: &[u8], width: u16, height: u16, forced: bool) {
    let mut previous = None;
    for quantum in [1, 2, 3, 4, 5, 7, 8, 9, 16, 31, 64, 4096] {
        let budget = Budget::new(1024 * 1024).unwrap();
        let cancel = AtomicBool::new(false);
        let mut job = HdSpuJob::new(
            Bytes::copy_from_slice(&budget, raw).unwrap(),
            &budget,
            HdLimits::default(),
        )
        .unwrap();
        loop {
            let mut work = WorkBudget::new(quantum);
            let status = job.poll(&mut work, &cancel).unwrap();
            assert!(work.spent() <= quantum);
            if status == HdStatus::Ready {
                break;
            }
        }
        let spent = job.work_spent();
        if let Some(old) = previous {
            assert_eq!(spent, old);
        }
        previous = Some(spent);
        let picture = job.try_finish(&cancel).unwrap();
        assert_eq!(picture.packet().as_ref(), raw);
        assert_eq!(
            (picture.bitmap().width, picture.bitmap().height),
            (width, height)
        );
        assert_eq!(picture.bitmap().pixels().as_ref(), expected);
        assert_eq!(picture.facts().forced, forced);
        assert_eq!(picture.ycrcb().unwrap().len(), 768);
        assert_eq!(picture.contrast().unwrap().len(), 256);
        assert_eq!(picture.facts().stop_date.unwrap().ticks_90k(), 92160);
        let kept = picture.bitmap().pixels().clone();
        let source = picture.packet().clone();
        drop(picture);
        drop(job);
        assert_eq!(
            budget.used(),
            Budget::control_storage_bytes()
                + kept.retained_storage_bytes()
                + source.retained_storage_bytes()
        );
        drop(kept);
        drop(source);
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}
#[test]
fn source_00() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-000-single-001-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-000-single-001-padding-0.indices"),
        1,
        2,
        false,
    );
}
#[test]
fn source_01() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-000-single-001-padding-1.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-000-single-001-padding-1.indices"),
        1,
        2,
        false,
    );
}
#[test]
fn source_02() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-000-small-002-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-000-small-002-padding-0.indices"),
        2,
        2,
        false,
    );
}
#[test]
fn source_03() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-000-small-009-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-000-small-009-padding-0.indices"),
        9,
        2,
        false,
    );
}
#[test]
fn source_04() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-000-large-010-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-000-large-010-padding-0.indices"),
        10,
        2,
        false,
    );
}
#[test]
fn source_05() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-000-large-136-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-000-large-136-padding-0.indices"),
        136,
        2,
        false,
    );
}
#[test]
fn source_06() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-000-fill-257-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-000-fill-257-padding-0.indices"),
        257,
        2,
        false,
    );
}
#[test]
fn source_07() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-003-single-001-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-003-single-001-padding-0.indices"),
        1,
        2,
        false,
    );
}
#[test]
fn source_08() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-003-single-001-padding-1.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-003-single-001-padding-1.indices"),
        1,
        2,
        false,
    );
}
#[test]
fn source_09() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-003-small-002-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-003-small-002-padding-0.indices"),
        2,
        2,
        false,
    );
}
#[test]
fn source_10() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-003-small-009-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-003-small-009-padding-0.indices"),
        9,
        2,
        false,
    );
}
#[test]
fn source_11() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-003-large-010-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-003-large-010-padding-0.indices"),
        10,
        2,
        false,
    );
}
#[test]
fn source_12() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-003-large-136-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-003-large-136-padding-0.indices"),
        136,
        2,
        false,
    );
}
#[test]
fn source_13() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-003-fill-257-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-003-fill-257-padding-0.indices"),
        257,
        2,
        false,
    );
}
#[test]
fn source_14() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-004-single-001-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-004-single-001-padding-0.indices"),
        1,
        2,
        false,
    );
}
#[test]
fn source_15() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-004-single-001-padding-1.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-004-single-001-padding-1.indices"),
        1,
        2,
        false,
    );
}
#[test]
fn source_16() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-004-small-002-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-004-small-002-padding-0.indices"),
        2,
        2,
        false,
    );
}
#[test]
fn source_17() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-004-small-009-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-004-small-009-padding-0.indices"),
        9,
        2,
        false,
    );
}
#[test]
fn source_18() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-004-large-010-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-004-large-010-padding-0.indices"),
        10,
        2,
        false,
    );
}
#[test]
fn source_19() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-004-large-136-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-004-large-136-padding-0.indices"),
        136,
        2,
        false,
    );
}
#[test]
fn source_20() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-004-fill-257-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-004-fill-257-padding-0.indices"),
        257,
        2,
        false,
    );
}
#[test]
fn source_21() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-017-single-001-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-017-single-001-padding-0.indices"),
        1,
        2,
        false,
    );
}
#[test]
fn source_22() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-017-single-001-padding-1.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-017-single-001-padding-1.indices"),
        1,
        2,
        false,
    );
}
#[test]
fn source_23() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-017-small-002-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-017-small-002-padding-0.indices"),
        2,
        2,
        false,
    );
}
#[test]
fn source_24() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-017-small-009-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-017-small-009-padding-0.indices"),
        9,
        2,
        false,
    );
}
#[test]
fn source_25() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-017-large-010-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-017-large-010-padding-0.indices"),
        10,
        2,
        false,
    );
}
#[test]
fn source_26() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-017-large-136-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-017-large-136-padding-0.indices"),
        136,
        2,
        false,
    );
}
#[test]
fn source_27() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-017-fill-257-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-017-fill-257-padding-0.indices"),
        257,
        2,
        false,
    );
}
#[test]
fn source_28() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-255-single-001-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-255-single-001-padding-0.indices"),
        1,
        2,
        false,
    );
}
#[test]
fn source_29() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-255-single-001-padding-1.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-255-single-001-padding-1.indices"),
        1,
        2,
        false,
    );
}
#[test]
fn source_30() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-255-small-002-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-255-small-002-padding-0.indices"),
        2,
        2,
        false,
    );
}
#[test]
fn source_31() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-255-small-009-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-255-small-009-padding-0.indices"),
        9,
        2,
        false,
    );
}
#[test]
fn source_32() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-255-large-010-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-255-large-010-padding-0.indices"),
        10,
        2,
        false,
    );
}
#[test]
fn source_33() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-255-large-136-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-255-large-136-padding-0.indices"),
        136,
        2,
        false,
    );
}
#[test]
fn source_34() {
    check(
        include_bytes!("hd-source-fixtures/hd8-index-255-fill-257-padding-0.spu"),
        include_bytes!("hd-source-fixtures/hd8-index-255-fill-257-padding-0.indices"),
        257,
        2,
        false,
    );
}
#[test]
fn source_35() {
    check(
        include_bytes!("hd-source-fixtures/hd8-all-256-indices.spu"),
        include_bytes!("hd-source-fixtures/hd8-all-256-indices.indices"),
        256,
        2,
        false,
    );
}
#[test]
fn source_36() {
    check(
        include_bytes!("hd-source-fixtures/hd8-mixed-token-forms.spu"),
        include_bytes!("hd-source-fixtures/hd8-mixed-token-forms.indices"),
        18,
        2,
        false,
    );
}
#[test]
fn source_37() {
    check(
        include_bytes!("hd-source-fixtures/hd8-forced.spu"),
        include_bytes!("hd-source-fixtures/hd8-forced.indices"),
        18,
        2,
        true,
    );
}
#[test]
fn source_38() {
    check(
        include_bytes!("hd-source-fixtures/extended-opcodes-traditional-header.spu"),
        include_bytes!("hd-source-fixtures/extended-opcodes-traditional-header.indices"),
        18,
        2,
        false,
    );
}
