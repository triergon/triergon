use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::dvd::{
    DvdArea, DvdBitmap, DvdRasterJob, DvdRasterLimits, DvdRasterStatus, FieldOffsets,
    NativeCropMode, NativeProjectionJob, NativeSelection, ProjectedBitmap, ProjectionLimits,
    ProjectionStatus,
};

// Authored ARGB words, including nonzero RGB under zero alpha. These tests
// isolate projection; a caller-supplied palette does not prove colour decoding.
const PALETTE: [u32; 4] = [0x0012_3456, 0xffa1_b2c3, 0x0102_0304, 0x0005_0607];

fn area(width: u16, height: u16) -> DvdArea {
    DvdArea {
        left: 10,
        right: 9 + width,
        top: 20,
        bottom: 19 + height,
    }
}

fn image(budget: &Budget, width: u16, height: u16, pixels: &[u8]) -> DvdBitmap {
    DvdBitmap::from_indices(
        area(width, height),
        Bytes::copy_from_slice(budget, pixels).unwrap(),
        budget,
        1024 * 1024,
    )
    .unwrap()
}

fn ready<const N: usize>(job: &mut NativeProjectionJob<N>, quantum: u64) {
    let cancel = AtomicBool::new(false);
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(quantum);
        let result = job.poll(&mut work, &cancel).unwrap();
        assert!(work.spent() <= quantum);
        if result == ProjectionStatus::Ready {
            return;
        }
    }
    panic!("bounded projection did not finish");
}

fn run<const N: usize>(
    source: DvdBitmap,
    palette: [u32; N],
    mode: NativeCropMode,
    budget: &Budget,
    quantum: u64,
) -> (ProjectedBitmap<N>, usize, usize) {
    let mut job =
        NativeProjectionJob::new(source, palette, mode, budget, ProjectionLimits::default())
            .unwrap();
    ready(&mut job, quantum);
    let spent = job.work_spent();
    let scanned = job.pixels_scanned();
    (
        job.try_finish(&AtomicBool::new(false)).unwrap(),
        spent,
        scanned,
    )
}

#[test]
fn transparent_leading_column_is_a_borrowed_crop_without_source_loss() {
    let mut plane = Vec::new();
    for _ in 0..2 {
        plane.extend_from_slice(&[0, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1]);
    }
    for quantum in [1, 2, 7, 65536] {
        let budget = Budget::new(4096).unwrap();
        let source = image(&budget, 18, 2, &plane);
        let address = source.pixels().as_ptr();
        let charge = budget.used();
        let (result, spent, scanned) =
            run(source, PALETTE, NativeCropMode::Ordinary, &budget, quantum);
        assert_eq!((spent, scanned), (37, 36));
        assert_eq!(result.source().pixels().as_ref(), plane.as_slice());
        assert_eq!(result.source().pixels().as_ptr(), address);
        assert_eq!(budget.used(), charge);
        let view = result.native_view().unwrap();
        assert_eq!(
            (view.crop().area, view.crop().width, view.crop().height),
            (
                DvdArea {
                    left: 11,
                    right: 27,
                    top: 20,
                    bottom: 21
                },
                17,
                2
            )
        );
        assert_eq!((view.crop().source_x, view.crop().source_y), (1, 0));
        for row in 0..2 {
            let start = usize::from(row) * 18 + 1;
            assert_eq!(view.indices_row(row).unwrap(), &plane[start..start + 17]);
            assert_eq!(
                view.indices_row(row).unwrap().as_ptr(),
                result.source().pixels()[start..].as_ptr()
            );
        }
        assert_eq!(result.palette(), &PALETTE);
    }
}

#[test]
fn all_four_borders_crop_but_transparent_interior_pixels_remain() {
    let plane = [0, 0, 0, 0, 0, 0, 1, 0, 2, 0, 0, 2, 3, 1, 0, 0, 0, 0, 0, 0];
    let budget = Budget::new(4096).unwrap();
    let (result, spent, _) = run(
        image(&budget, 5, 4, &plane),
        PALETTE,
        NativeCropMode::Ordinary,
        &budget,
        1,
    );
    assert_eq!(spent, 21);
    let view = result.native_view().unwrap();
    assert_eq!(
        view.crop().area,
        DvdArea {
            left: 11,
            right: 13,
            top: 21,
            bottom: 22
        }
    );
    assert_eq!(view.indices_row(0).unwrap(), &[1, 0, 2]);
    assert_eq!(view.indices_row(1).unwrap(), &[2, 3, 1]);
    assert_eq!(view.argb_at(1, 0).unwrap(), 0x0012_3456);
    assert_eq!(view.argb_at(0, 1).unwrap(), 0x0102_0304);
    assert_eq!(view.argb_at(1, 1).unwrap(), 0x0005_0607);
}

#[test]
fn any_nonzero_alpha_keeps_the_pixel_regardless_of_rgb() {
    let budget = Budget::new(4096).unwrap();
    let (result, _, _) = run(
        image(&budget, 3, 3, &[0, 0, 0, 0, 2, 0, 0, 0, 0]),
        PALETTE,
        NativeCropMode::Ordinary,
        &budget,
        1,
    );
    let view = result.native_view().unwrap();
    assert_eq!((view.crop().width, view.crop().height), (1, 1));
    assert_eq!(
        view.crop().area,
        DvdArea {
            left: 11,
            right: 11,
            top: 21,
            bottom: 21
        }
    );
    assert_eq!(view.argb_at(0, 0).unwrap(), PALETTE[2]);
    assert_eq!(result.selection(), NativeSelection::Rendered);
}

#[test]
fn fully_transparent_ordinary_omission_retains_the_entire_source() {
    let budget = Budget::new(4096).unwrap();
    let plane = [0, 3, 0, 3, 3, 0];
    let (result, spent, scanned) = run(
        image(&budget, 3, 2, &plane),
        PALETTE,
        NativeCropMode::Ordinary,
        &budget,
        2,
    );
    assert_eq!((spent, scanned), (7, 6));
    assert_eq!(
        result.selection(),
        NativeSelection::ReceiverTransparentOmission
    );
    assert!(result.native_view().is_none());
    assert_eq!(result.source().pixels().as_ref(), plane);
    assert_eq!(result.palette(), &PALETTE);
}

#[test]
fn uniform_alpha_charges_all_pixels_of_a_validated_raster() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let raw = [0, 11, 0, 6, 0x11, 0x12, 0, 0, 0, 6, 0xff];
    for (palette, expected) in [
        ([0xff12_3456; 4], NativeSelection::Rendered),
        (
            [0x0012_3456; 4],
            NativeSelection::ReceiverTransparentOmission,
        ),
    ] {
        let mut raster = DvdRasterJob::new(
            Bytes::copy_from_slice(&budget, &raw).unwrap(),
            area(4, 2),
            FieldOffsets { top: 4, bottom: 5 },
            &budget,
            DvdRasterLimits::default(),
        )
        .unwrap();
        assert_eq!(
            raster.poll(&mut WorkBudget::new(128), &cancel).unwrap(),
            DvdRasterStatus::Ready
        );
        let mut job = NativeProjectionJob::new(
            raster.try_finish(&cancel).unwrap(),
            palette,
            NativeCropMode::Ordinary,
            &budget,
            ProjectionLimits::default(),
        )
        .unwrap();
        assert_eq!(
            job.poll(&mut WorkBudget::new(3), &cancel).unwrap(),
            ProjectionStatus::Yield
        );
        assert_eq!((job.work_spent(), job.pixels_scanned()), (3, 2));
        ready(&mut job, 7);
        assert_eq!((job.work_spent(), job.pixels_scanned()), (9, 8));
        let out = job.try_finish(&cancel).unwrap();
        assert_eq!(out.selection(), expected);
        assert_eq!(out.source().pixels().as_ref(), &[1, 1, 1, 1, 2, 2, 2, 2]);
        if let Some(view) = out.native_view() {
            assert_eq!(view.crop().area, area(4, 2));
        }
    }
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn menu_bypasses_cropping_and_keeps_transparent_rgb() {
    let budget = Budget::new(4096).unwrap();
    let plane = [0, 3, 0, 3, 3, 0];
    let (result, spent, scanned) = run(
        image(&budget, 3, 2, &plane),
        PALETTE,
        NativeCropMode::Menu,
        &budget,
        1,
    );
    // Supplied indices need validation even though menu selection needs no crop.
    assert_eq!((spent, scanned), (7, 6));
    let view = result.native_view().unwrap();
    assert_eq!(view.crop().area, area(3, 2));
    assert_eq!(view.argb_at(0, 0).unwrap(), PALETTE[0]);
    assert_eq!(view.argb_at(1, 0).unwrap(), PALETTE[3]);
    assert_eq!(result.selection(), NativeSelection::Rendered);
}

#[test]
fn receiver_one_row_omission_does_not_reject_or_erase_source_pixels() {
    for mode in [NativeCropMode::Ordinary, NativeCropMode::Menu] {
        let budget = Budget::new(4096).unwrap();
        let plane = [1, 2, 1];
        let (result, spent, scanned) = run(image(&budget, 3, 1, &plane), PALETTE, mode, &budget, 1);
        assert_eq!((spent, scanned), (4, 3));
        assert_eq!(result.selection(), NativeSelection::ReceiverOneRowOmission);
        assert!(result.native_view().is_none());
        assert_eq!(result.source().pixels().as_ref(), plane);
    }
}

#[test]
fn full_byte_palette_validates_all_indices_without_narrowing() {
    let budget = Budget::new(4096).unwrap();
    let mut palette = [0x0012_3456; 256];
    palette[255] = 0x0100_0001;
    palette[254] = 0xffab_cdef;
    let plane = [0, 255, 0, 0, 254, 0];
    let (result, spent, scanned) = run(
        image(&budget, 3, 2, &plane),
        palette,
        NativeCropMode::Ordinary,
        &budget,
        1,
    );
    assert_eq!((spent, scanned), (7, 6));
    let view = result.native_view().unwrap();
    assert_eq!(
        view.crop().area,
        DvdArea {
            left: 11,
            right: 11,
            top: 20,
            bottom: 21
        }
    );
    assert_eq!(view.indices_row(0).unwrap(), &[255]);
    assert_eq!(view.argb_at(0, 0).unwrap(), palette[255]);
    assert_eq!(view.argb_at(0, 1).unwrap(), palette[254]);
}

#[test]
fn a_complete_byte_palette_menu_needs_only_setup_work() {
    let budget = Budget::new(4096).unwrap();
    let mut palette = [0_u32; 256];
    palette[255] = 0xff12_3456;
    let plane = [0, 255, 0, 255, 0, 255];
    let source = image(&budget, 3, 2, &plane);
    let charge = budget.used();
    let (result, spent, scanned) = run(source, palette, NativeCropMode::Menu, &budget, 1);
    assert_eq!((spent, scanned), (1, 0));
    assert_eq!(budget.used(), charge);
    assert_eq!(
        result.native_view().unwrap().indices_row(0).unwrap(),
        &[0, 255, 0]
    );
}

#[test]
fn out_of_range_index_is_rejected_even_for_menu_or_one_row() {
    for (mode, height) in [
        (NativeCropMode::Ordinary, 2),
        (NativeCropMode::Menu, 2),
        (NativeCropMode::Ordinary, 1),
        (NativeCropMode::Menu, 1),
    ] {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let pixels = if height == 1 {
            &[1, 255][..]
        } else {
            &[1, 2, 3, 255][..]
        };
        let mut job = NativeProjectionJob::new(
            image(&budget, 2, height, pixels),
            PALETTE,
            mode,
            &budget,
            ProjectionLimits::default(),
        )
        .unwrap();
        let error = job.poll(&mut WorkBudget::new(65536), &cancel).unwrap_err();
        assert_eq!(error, Error::Invalid("DVD palette index"));
        assert_eq!(job.try_finish(&cancel).unwrap_err(), error);
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn actual_two_bit_raster_menu_uses_its_proven_index_range() {
    // Independent literal SPU: two four-pixel field rows, one stop control.
    let raw = [0, 14, 0, 8, 0x45, 0x67, 0x76, 0x54, 0, 90, 0, 8, 2, 0xff];
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut raster = DvdRasterJob::new(
        Bytes::copy_from_slice(&budget, &raw).unwrap(),
        area(4, 2),
        FieldOffsets { top: 4, bottom: 6 },
        &budget,
        DvdRasterLimits::default(),
    )
    .unwrap();
    while raster.poll(&mut WorkBudget::new(1), &cancel).unwrap() != DvdRasterStatus::Ready {}
    let source = raster.try_finish(&cancel).unwrap();
    assert_eq!(source.pixels().as_ref(), &[0, 1, 2, 3, 3, 2, 1, 0]);
    let (result, spent, scanned) = run(source, PALETTE, NativeCropMode::Menu, &budget, 1);
    assert_eq!((spent, scanned), (1, 0));
    assert_eq!(
        result.native_view().unwrap().indices_row(1).unwrap(),
        &[3, 2, 1, 0]
    );
}

#[test]
fn zero_work_does_not_scan_allocate_or_change_storage() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = NativeProjectionJob::new(
        image(&budget, 2, 2, &[1, 2, 3, 0]),
        PALETTE,
        NativeCropMode::Ordinary,
        &budget,
        ProjectionLimits::default(),
    )
    .unwrap();
    let charge = budget.used();
    for _ in 0..3 {
        assert_eq!(
            job.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
            ProjectionStatus::Yield
        );
        assert_eq!(
            (job.work_spent(), job.pixels_scanned(), budget.used()),
            (0, 0, charge)
        );
    }
    assert_eq!(
        job.poll(&mut WorkBudget::new(1), &cancel).unwrap(),
        ProjectionStatus::Yield
    );
    assert_eq!((job.work_spent(), job.pixels_scanned()), (1, 0));
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
        ProjectionStatus::Yield
    );
    assert_eq!(
        (job.work_spent(), job.pixels_scanned(), budget.used()),
        (1, 0, charge)
    );
}

#[test]
fn work_limit_is_exact_sticky_and_releases_private_storage() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let limits = ProjectionLimits {
        max_work: 4,
        ..ProjectionLimits::default()
    };
    let mut job = NativeProjectionJob::new(
        image(&budget, 2, 2, &[1, 2, 3, 0]),
        PALETTE,
        NativeCropMode::Ordinary,
        &budget,
        limits,
    )
    .unwrap();
    let error = job.poll(&mut WorkBudget::new(65536), &cancel).unwrap_err();
    assert_eq!(
        error,
        Error::Limit {
            requested: 5,
            maximum: 4
        }
    );
    assert_eq!((job.work_spent(), job.pixels_scanned()), (4, 3));
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &cancel).unwrap_err(),
        error
    );
    assert_eq!(budget.used(), Budget::control_storage_bytes());
    let mut exact = NativeProjectionJob::new(
        image(&budget, 2, 2, &[1, 2, 3, 0]),
        PALETTE,
        NativeCropMode::Ordinary,
        &budget,
        ProjectionLimits {
            max_work: 5,
            ..limits
        },
    )
    .unwrap();
    ready(&mut exact, 1);
    assert_eq!(exact.work_spent(), 5);
    assert!(exact.try_finish(&cancel).unwrap().native_view().is_some());
}

#[test]
fn cancellation_before_during_and_after_ready_is_sticky() {
    for stage in 0..3 {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut job = NativeProjectionJob::new(
            image(&budget, 2, 2, &[1, 2, 3, 0]),
            PALETTE,
            NativeCropMode::Ordinary,
            &budget,
            ProjectionLimits::default(),
        )
        .unwrap();
        if stage == 1 {
            job.poll(&mut WorkBudget::new(3), &cancel).unwrap();
        }
        if stage == 2 {
            ready(&mut job, 1);
        }
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            job.poll(&mut WorkBudget::new(0), &cancel).unwrap_err(),
            Error::Cancelled
        );
        cancel.store(false, Ordering::Relaxed);
        assert_eq!(job.try_finish(&cancel).unwrap_err(), Error::Cancelled);
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn returned_source_and_borrowed_rows_outlive_the_job_without_extra_charge() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let source = image(&budget, 2, 2, &[0, 1, 0, 2]);
    let charge = budget.used();
    let retained = source.pixels().clone();
    let mut job = NativeProjectionJob::new(
        source,
        PALETTE,
        NativeCropMode::Ordinary,
        &budget,
        ProjectionLimits::default(),
    )
    .unwrap();
    ready(&mut job, 1);
    let result = job.try_finish(&cancel).unwrap();
    drop(job);
    assert_eq!(budget.used(), charge);
    assert_eq!(result.native_view().unwrap().indices_row(1).unwrap(), &[2]);
    drop(result);
    assert_eq!(budget.used(), charge);
    assert_eq!(retained.as_ref(), &[0, 1, 0, 2]);
    drop(retained);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn supplied_geometry_length_pool_and_pixel_ceiling_are_checked() {
    let budget = Budget::new(4096).unwrap();
    let other = Budget::new(4096).unwrap();
    let bytes = |data: &[u8]| Bytes::copy_from_slice(&budget, data).unwrap();
    assert_eq!(
        DvdBitmap::from_indices(area(2, 2), bytes(&[0; 3]), &budget, 4).unwrap_err(),
        Error::Invalid("DVD indexed plane length")
    );
    assert_eq!(
        DvdBitmap::from_indices(area(2, 2), bytes(&[0; 4]), &budget, 3).unwrap_err(),
        Error::Limit {
            requested: 4,
            maximum: 3
        }
    );
    assert_eq!(
        DvdBitmap::from_indices(area(2, 2), bytes(&[0; 4]), &other, 4).unwrap_err(),
        Error::ForeignPool
    );
    assert!(
        DvdBitmap::from_indices(
            DvdArea {
                left: 1,
                right: 0,
                top: 0,
                bottom: 0
            },
            bytes(&[]),
            &budget,
            4
        )
        .is_err()
    );
    assert_eq!(
        DvdBitmap::from_indices(
            DvdArea {
                left: 0,
                right: u16::MAX,
                top: 0,
                bottom: 0
            },
            bytes(&[]),
            &budget,
            usize::MAX
        )
        .unwrap_err(),
        Error::Invalid("DVD reversed or overflowing display area")
    );
    assert_eq!(
        NativeProjectionJob::new(
            image(&budget, 2, 2, &[0; 4]),
            PALETTE,
            NativeCropMode::Menu,
            &other,
            ProjectionLimits::default()
        )
        .unwrap_err(),
        Error::ForeignPool
    );
    assert_eq!(
        NativeProjectionJob::new(
            image(&budget, 2, 2, &[0; 4]),
            PALETTE,
            NativeCropMode::Menu,
            &budget,
            ProjectionLimits {
                max_pixels: 3,
                ..ProjectionLimits::default()
            }
        )
        .unwrap_err(),
        Error::Limit {
            requested: 4,
            maximum: 3
        }
    );
    assert_eq!(
        NativeProjectionJob::new(
            image(&budget, 2, 2, &[0; 4]),
            [0; 16],
            NativeCropMode::Menu,
            &budget,
            ProjectionLimits::default()
        )
        .unwrap_err(),
        Error::Unsupported("DVD native palette size")
    );
    let mut changed = image(&budget, 2, 2, &[0; 4]);
    changed.width = 1;
    assert_eq!(
        NativeProjectionJob::new(
            changed,
            PALETTE,
            NativeCropMode::Menu,
            &budget,
            ProjectionLimits::default()
        )
        .unwrap_err(),
        Error::Invalid("DVD bitmap geometry mismatch")
    );
}

#[test]
fn native_view_bounds_are_explicit_and_do_not_change_the_projection() {
    let budget = Budget::new(4096).unwrap();
    let (result, _, _) = run(
        image(&budget, 2, 2, &[0, 1, 0, 2]),
        PALETTE,
        NativeCropMode::Ordinary,
        &budget,
        1,
    );
    let view = result.native_view().unwrap();
    assert_eq!(
        view.indices_row(2).unwrap_err(),
        Error::Invalid("DVD native row index")
    );
    assert_eq!(
        view.argb_at(1, 0).unwrap_err(),
        Error::Invalid("DVD native column index")
    );
    assert_eq!(
        view.argb_at(0, 2).unwrap_err(),
        Error::Invalid("DVD native row index")
    );
    assert_eq!(view.indices_row(0).unwrap(), &[1]);
    assert_eq!(view.argb_at(0, 1).unwrap(), PALETTE[2]);
}

#[test]
fn ready_poll_is_free_but_finish_and_post_finish_are_single_use() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = NativeProjectionJob::new(
        image(&budget, 2, 2, &[1; 4]),
        PALETTE,
        NativeCropMode::Ordinary,
        &budget,
        ProjectionLimits::default(),
    )
    .unwrap();
    ready(&mut job, 1);
    let before = job.work_spent();
    for _ in 0..3 {
        assert_eq!(
            job.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
            ProjectionStatus::Ready
        );
    }
    assert_eq!(job.work_spent(), before);
    let result = job.try_finish(&cancel).unwrap();
    let error = job.try_finish(&cancel).unwrap_err();
    assert_eq!(error, Error::Invalid("DVD projection not ready"));
    assert_eq!(
        job.poll(&mut WorkBudget::new(65536), &cancel).unwrap_err(),
        error
    );
    assert_eq!(
        result.native_view().unwrap().argb_at(0, 0).unwrap(),
        PALETTE[1]
    );
}

#[test]
fn premature_finish_is_sticky_and_releases_the_unpublished_plane() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = NativeProjectionJob::new(
        image(&budget, 2, 2, &[1; 4]),
        PALETTE,
        NativeCropMode::Ordinary,
        &budget,
        ProjectionLimits::default(),
    )
    .unwrap();
    let error = job.try_finish(&cancel).unwrap_err();
    assert_eq!(error, Error::Invalid("DVD projection not ready"));
    assert_eq!(
        job.poll(&mut WorkBudget::new(65536), &cancel).unwrap_err(),
        error
    );
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}
