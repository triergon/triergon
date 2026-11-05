use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::dvd::{HdLimits, HdReceiverJob, HdReceiverStatus};
const RAW: &[u8] = include_bytes!("hd-source-fixtures/hd8-all-256-indices.spu");
fn make(raw: &[u8], b: &Budget, limits: HdLimits) -> HdReceiverJob {
    HdReceiverJob::new(Bytes::copy_from_slice(b, raw).unwrap(), b, limits).unwrap()
}
fn ready(j: &mut HdReceiverJob, q: u64, c: &AtomicBool) {
    for _ in 0..100000 {
        if j.poll(&mut WorkBudget::new(q), c).unwrap() == HdReceiverStatus::Ready {
            return;
        }
    }
    panic!("bounded case did not finish");
}
#[test]
fn zero_work_never_allocates_or_interprets_a_source_rejection() {
    for raw in [RAW, &[0_u8][..]] {
        let b = Budget::new(100000).unwrap();
        let c = AtomicBool::new(false);
        let mut j = make(raw, &b, HdLimits::default());
        let used = b.used();
        assert_eq!(
            j.poll(&mut WorkBudget::new(0), &c).unwrap(),
            HdReceiverStatus::Yield
        );
        assert_eq!(j.work_spent(), 0);
        assert_eq!(b.used(), used);
    }
}
#[test]
fn incomplete_input_needs_explicit_end_and_is_never_a_valid_picture() {
    for len in 0..10 {
        let raw = &RAW[..len];
        let b = Budget::new(100000).unwrap();
        let c = AtomicBool::new(false);
        let mut j = make(raw, &b, HdLimits::default());
        assert_eq!(
            j.poll(&mut WorkBudget::new(1), &c).unwrap(),
            HdReceiverStatus::NeedInputEnd
        );
        let spent = j.work_spent();
        assert_eq!(
            j.poll(&mut WorkBudget::new(0), &c).unwrap(),
            HdReceiverStatus::NeedInputEnd
        );
        assert_eq!(j.work_spent(), spent);
        assert!(j.try_finish(&c).is_err());
        j.finish_input();
        assert_eq!(
            j.poll(&mut WorkBudget::new(0), &c).unwrap(),
            HdReceiverStatus::Ready
        );
        let output = j.try_finish(&c).unwrap();
        assert_eq!(output.packet().as_ref(), raw);
        assert_eq!(output.source_error(), Some(Error::Truncated));
        assert!(output.picture().is_none());
    }
}
#[test]
fn malformed_fields_retain_source_values_and_require_end_without_fake_output() {
    let raw = include_bytes!("hd-source-fixtures/hd8-fields-at-ffffffff.spu");
    for q in [1, 3, 4096] {
        let b = Budget::new(100000).unwrap();
        let c = AtomicBool::new(false);
        let mut j = make(raw, &b, HdLimits::default());
        loop {
            if j.poll(&mut WorkBudget::new(q), &c).unwrap() == HdReceiverStatus::NeedInputEnd {
                break;
            }
        }
        j.finish_input();
        ready(&mut j, q, &c);
        let output = j.try_finish(&c).unwrap();
        assert!(output.picture().is_none());
        assert_eq!(output.source_facts().fields.unwrap().top, u32::MAX);
        assert_eq!(
            output.source_error(),
            Some(Error::Invalid("HD field outside pixel data"))
        );
        assert_eq!(output.packet().as_ref(), raw);
    }
}
#[test]
fn source_rejection_does_not_hide_work_pixel_or_foreign_pool_limits() {
    for limits in [
        HdLimits {
            max_work: 0,
            ..Default::default()
        },
        HdLimits {
            max_pixels: 0,
            ..Default::default()
        },
    ] {
        let b = Budget::new(100000).unwrap();
        let c = AtomicBool::new(false);
        let mut j = make(RAW, &b, limits);
        j.finish_input();
        assert!(matches!(
            j.poll(&mut WorkBudget::new(100000), &c),
            Err(Error::Limit { .. })
        ));
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
    let b = Budget::new(100000).unwrap();
    let other = Budget::new(100000).unwrap();
    assert_eq!(
        HdReceiverJob::new(
            Bytes::copy_from_slice(&other, RAW).unwrap(),
            &b,
            HdLimits::default()
        )
        .unwrap_err(),
        Error::ForeignPool
    );
}
#[test]
fn pressure_and_freeze_retries_keep_paid_work_and_zero_poll_does_not_allocate() {
    let b = Budget::new(100000).unwrap();
    let c = AtomicBool::new(false);
    let mut j = make(RAW, &b, HdLimits::default());
    let blocker = b.reserve(b.available()).unwrap();
    assert!(matches!(
        j.poll(&mut WorkBudget::new(100000), &c),
        Err(Error::Backpressure { .. })
    ));
    let spent = j.work_spent();
    assert_eq!(
        j.poll(&mut WorkBudget::new(0), &c).unwrap(),
        HdReceiverStatus::Yield
    );
    assert_eq!(j.work_spent(), spent);
    drop(blocker);
    ready(&mut j, 1, &c);
    let o = j.try_finish(&c).unwrap();
    assert_eq!(o.picture().unwrap().bitmap().pixels().len(), 512);
}
#[test]
fn retained_source_and_picture_share_original_charge_and_outlive_job() {
    let b = Budget::new(100000).unwrap();
    let c = AtomicBool::new(false);
    let mut j = make(RAW, &b, HdLimits::default());
    ready(&mut j, 7, &c);
    let o = j.try_finish(&c).unwrap();
    assert!(
        o.packet()
            .shares_storage_with(o.picture().unwrap().packet())
    );
    drop(j);
    assert_eq!(
        b.used(),
        Budget::control_storage_bytes()
            + o.packet().retained_storage_bytes()
            + o.picture()
                .unwrap()
                .bitmap()
                .pixels()
                .retained_storage_bytes()
    );
    drop(o);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn cancellation_is_sticky_while_pending_ready_or_rejected_and_frees_private_owners() {
    for raw in [RAW, &[0_u8][..]] {
        for q in [0, 1, 100000] {
            let b = Budget::new(100000).unwrap();
            let c = AtomicBool::new(false);
            let mut j = make(raw, &b, HdLimits::default());
            j.poll(&mut WorkBudget::new(q), &c).unwrap();
            c.store(true, Ordering::Relaxed);
            assert_eq!(j.try_finish(&c).unwrap_err(), Error::Cancelled);
            assert_eq!(b.used(), Budget::control_storage_bytes());
            c.store(false, Ordering::Relaxed);
            assert_eq!(
                j.poll(&mut WorkBudget::new(100000), &c).unwrap_err(),
                Error::Cancelled
            );
        }
    }
}
#[test]
fn missing_colour_stays_unknown_beside_a_real_decoded_source_plane() {
    let raw = include_bytes!("hd-source-fixtures/hd8-missing-YCrCb-palette.spu");
    let b = Budget::new(100000).unwrap();
    let c = AtomicBool::new(false);
    let mut j = make(raw, &b, HdLimits::default());
    ready(&mut j, 1, &c);
    let o = j.try_finish(&c).unwrap();
    assert!(o.source_error().is_none());
    let p = o.picture().unwrap();
    assert!(p.ycrcb().is_none());
    assert!(p.native_palette_job().is_err());
    assert_eq!(p.bitmap().pixels().len(), 36);
}
#[test]
fn freeze_backpressure_is_retryable_and_zero_poll_keeps_the_paid_plane() {
    let b = Budget::new(100000).unwrap();
    let c = AtomicBool::new(false);
    let mut j = make(RAW, &b, HdLimits::default());
    // Only the exact512 pixel bytes fit; shared-storage metadata must wait.
    let blocker = b.reserve(b.available() - 512).unwrap();
    assert!(matches!(
        j.poll(&mut WorkBudget::new(100000), &c),
        Err(Error::Backpressure { .. })
    ));
    let spent = j.work_spent();
    let used = b.used();
    assert_eq!(
        j.poll(&mut WorkBudget::new(0), &c).unwrap(),
        HdReceiverStatus::Yield
    );
    assert_eq!(j.work_spent(), spent);
    assert_eq!(b.used(), used);
    drop(blocker);
    ready(&mut j, 1, &c);
    assert_eq!(j.work_spent(), spent);
    let output = j.try_finish(&c).unwrap();
    assert_eq!(output.picture().unwrap().bitmap().pixels().len(), 512);
}
#[test]
fn every_eligible_multirow_raster_is_validated_before_later_selection() {
    let raw = include_bytes!("hd-multi-fixtures/early-rle-overshoot-later-valid.spu");
    for q in 1..=16 {
        let b = Budget::new(100000).unwrap();
        let c = AtomicBool::new(false);
        let mut j = make(raw, &b, HdLimits::default());
        j.finish_input();
        ready(&mut j, q, &c);
        let o = j.try_finish(&c).unwrap();
        assert!(o.picture().is_none());
        assert_eq!(
            o.source_error(),
            Some(Error::Invalid("HD run exceeds source row"))
        );
        assert_eq!(o.packet().as_ref(), raw);
    }
}
#[test]
fn later_one_row_omission_does_not_replace_a_previous_bitmap() {
    let raw = include_bytes!("hd-multi-fixtures/early-valid-later-one-row.spu");
    for q in [1, 2, 3, 7, 4096] {
        let b = Budget::new(100000).unwrap();
        let c = AtomicBool::new(false);
        let mut j = make(raw, &b, HdLimits::default());
        ready(&mut j, q, &c);
        let o = j.try_finish(&c).unwrap();
        let p = o.picture().unwrap();
        assert_eq!((p.bitmap().width, p.bitmap().height), (4, 2));
        assert_eq!(p.bitmap().pixels().as_ref(), [3; 8]);
        assert_eq!(p.facts().start_date.unwrap().0, 17);
        assert_eq!(
            p.facts().selected_control,
            Some(
                triergon_subtitles::dvd::HdPacket::parse(raw)
                    .unwrap()
                    .control_offset()
            )
        );
    }
}
#[test]
fn one_row_unselected_asset_is_not_decoded_as_a_native_multirow_candidate() {
    let raw = include_bytes!("hd-multi-fixtures/early-one-row-bad-rle-later-valid.spu");
    for q in [1, 3, 4096] {
        let b = Budget::new(100000).unwrap();
        let c = AtomicBool::new(false);
        let mut j = make(raw, &b, HdLimits::default());
        ready(&mut j, q, &c);
        let o = j.try_finish(&c).unwrap();
        assert!(o.source_error().is_none());
        assert_eq!(o.picture().unwrap().bitmap().pixels().as_ref(), [17; 8]);
        assert_eq!(o.packet().as_ref(), raw);
    }
}
#[test]
fn later_eligible_raster_and_palette_snapshot_remain_distinct_from_later_metadata() {
    let c = AtomicBool::new(false);
    for (raw, index) in [
        (
            include_bytes!("hd-multi-fixtures/two-valid-rasters-last-selected.spu").as_slice(),
            17,
        ),
        (
            include_bytes!("hd-multi-fixtures/early-valid-later-palette-only.spu").as_slice(),
            3,
        ),
    ] {
        let b = Budget::new(100000).unwrap();
        let mut j = make(raw, &b, HdLimits::default());
        ready(&mut j, 1, &c);
        let o = j.try_finish(&c).unwrap();
        let p = o.picture().unwrap();
        assert_eq!(p.bitmap().pixels().as_ref(), [index; 8]);
        let mut palette = p.native_palette_job().unwrap();
        while palette.poll(&mut WorkBudget::new(1), &c).unwrap()
            != triergon_subtitles::dvd::HdStatus::Ready
        {}
        assert_eq!(palette.try_finish(&c).unwrap()[3] >> 24, 3);
    }
}
#[test]
fn late_forced_menu_changes_crop_policy_without_inventing_rectangle_force_or_start() {
    let raw = include_bytes!("hd-multi-fixtures/early-valid-later-forced-only.spu");
    let b = Budget::new(100000).unwrap();
    let c = AtomicBool::new(false);
    let mut j = make(raw, &b, HdLimits::default());
    ready(&mut j, 1, &c);
    let o = j.try_finish(&c).unwrap();
    let f = o.picture().unwrap().facts();
    assert!(f.menu);
    assert!(!f.forced);
    assert_eq!(f.forced_date.unwrap().0, 17);
    assert_eq!(f.start_date.unwrap().0, 0);
}
