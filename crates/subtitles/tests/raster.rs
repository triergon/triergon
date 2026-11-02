//! Tests authored before the raster implementation. Execution belongs to the parent.
//! Expected planes are copied from an independent original source-art oracle.
#![forbid(unsafe_code)]

#[path = "support/cases.rs"]
mod cases;

use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::dvb::{
    ClipRect, Depth, FieldParity, FieldSpec, InitialPlane, MapTables, PaletteFacts, Point, Raster,
    RasterLimits, RasterSpec, Size, SourceFacts, Status,
};

fn source(budget: &Budget, case: &cases::Case) -> SourceFacts {
    SourceFacts {
        object: Some(Bytes::copy_from_slice(budget, case.object_source).unwrap()),
        palette: if case.palette.is_empty() {
            PaletteFacts::Default { depth: case.depth }
        } else {
            PaletteFacts::Encoded {
                depth: case.depth,
                page_id: u16::from_be_bytes([case.palette[2], case.palette[3]]),
                clut_id: 0,
                version: 0,
                original: Bytes::copy_from_slice(budget, case.palette).unwrap(),
            }
        },
    }
}

fn spec(case: &cases::Case) -> RasterSpec {
    RasterSpec {
        region: Size {
            width: case.width,
            height: case.height,
        },
        object_origin: Point {
            x: case.object_x,
            y: case.object_y,
        },
        depth: case.depth,
        non_modifying: case.nonmodifying,
        clip: None,
    }
}

fn field(
    raster: &mut Raster,
    parity: FieldParity,
    bytes: &[u8],
    chunk: usize,
    units: u64,
    cancel: &AtomicBool,
) {
    raster
        .start_field(FieldSpec::new(parity, bytes.len()))
        .unwrap();
    let mut at = 0_usize;
    for _ in 0..100_000 {
        let end = at.saturating_add(chunk).min(bytes.len());
        let mut work = WorkBudget::new(units);
        let progress = raster
            .step(&bytes[at..end], end == bytes.len(), &mut work, cancel)
            .expect("original source-art field must decode");
        assert!(progress.consumed <= end - at);
        at += progress.consumed;
        match progress.status {
            Status::FieldComplete => {
                assert_eq!(at, bytes.len());
                return;
            }
            Status::NeedInput => {
                assert_eq!(at, end);
                assert!(at < bytes.len());
            }
            Status::Yield => assert!(work.spent() > 0),
        }
    }
    panic!("bounded fixture did not finish");
}

fn decode_fields(raster: &mut Raster, case: &cases::Case, chunk: usize, units: u64) {
    let cancel = AtomicBool::new(false);
    field(raster, FieldParity::Top, case.top, chunk, units, &cancel);
    let bottom = if case.repeat_top {
        case.top
    } else {
        case.bottom
    };
    field(raster, FieldParity::Bottom, bottom, chunk, units, &cancel);
}

fn default_source(depth: Depth) -> SourceFacts {
    SourceFacts {
        object: None,
        palette: PaletteFacts::Default { depth },
    }
}

#[test]
fn original_planes_match_across_byte_starvation_and_single_unit_work() {
    let cancel = AtomicBool::new(false);
    for case in cases::CASES {
        for chunk in [1, 2, 7, usize::MAX] {
            for units in [1, 3, 4096] {
                let budget = Budget::new(1 << 20).unwrap();
                let facts = source(&budget, case);
                let saved = facts.clone();
                let mut raster = Raster::new(
                    &budget,
                    spec(case),
                    RasterLimits::default(),
                    InitialPlane::Fill(case.fill),
                    facts,
                    &cancel,
                )
                .unwrap();
                decode_fields(&mut raster, case, chunk, units);
                let plane = raster.try_finish(&cancel).unwrap();
                assert_eq!(plane.pixels.as_ref(), case.expected, "{}", case.name);
                assert_eq!(plane.geometry.stride, case.width as usize);
                assert_eq!(
                    plane.source, saved,
                    "palette/source facts must survive exactly"
                );
            }
        }
    }
}

#[test]
fn clipping_does_not_move_source_rows_or_skip_invalid_source_bounds() {
    let case = &cases::CASES[0];
    assert_eq!(case.name, "dvb2-literals-fields");
    let cancel = AtomicBool::new(false);
    let budget = Budget::new(4096).unwrap();
    let mut options = spec(case);
    options.clip = Some(ClipRect {
        x: -2,
        y: 1,
        width: 6,
        height: 2,
    });
    let mut raster = Raster::new(
        &budget,
        options,
        RasterLimits::default(),
        InitialPlane::Fill(0),
        default_source(Depth::Two),
        &cancel,
    )
    .unwrap();
    decode_fields(&mut raster, case, 1, 1);
    let plane = raster.try_finish(&cancel).unwrap();
    assert_eq!(
        plane.geometry.stored,
        ClipRect {
            x: 0,
            y: 1,
            width: 4,
            height: 2
        }
    );
    assert_eq!(plane.pixels.as_ref(), &[3, 2, 1, 0, 1, 1, 2, 2]);
}

#[test]
fn ragged_right_edges_and_nonmodifying_pixels_preserve_an_immutable_initial_plane() {
    let case = cases::CASES
        .iter()
        .find(|c| c.name == "dvb-nonmodifying")
        .unwrap();
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let initial = Bytes::copy_from_slice(&budget, &[2; 12]).unwrap();
    let retained = initial.clone();
    let mut raster = Raster::new(
        &budget,
        spec(case),
        RasterLimits::default(),
        InitialPlane::Copy(initial),
        default_source(Depth::Two),
        &cancel,
    )
    .unwrap();
    decode_fields(&mut raster, case, 1, 1);
    let plane = raster.try_finish(&cancel).unwrap();
    assert_eq!(plane.pixels.as_ref(), case.expected);
    assert_eq!(retained.as_ref(), &[2; 12]);

    let case = cases::CASES
        .iter()
        .find(|c| c.name == "dvb-geometry-window")
        .unwrap();
    let mut raster = Raster::new(
        &budget,
        spec(case),
        RasterLimits::default(),
        InitialPlane::Fill(3),
        default_source(Depth::Two),
        &cancel,
    )
    .unwrap();
    decode_fields(&mut raster, case, 2, 3);
    assert_eq!(
        raster.try_finish(&cancel).unwrap().pixels.as_ref(),
        &[3, 1, 2, 3, 3, 2, 1, 3]
    );
}

#[test]
fn cancellation_during_a_long_run_is_terminal_and_releases_private_plane_storage() {
    let case = cases::CASES
        .iter()
        .find(|c| c.name == "dvb2-run-branches")
        .unwrap();
    let budget = Budget::new(8192).unwrap();
    let baseline = budget.used();
    let cancel = AtomicBool::new(false);
    let mut raster = Raster::new(
        &budget,
        spec(case),
        RasterLimits::default(),
        InitialPlane::Fill(0),
        default_source(Depth::Two),
        &cancel,
    )
    .unwrap();
    raster
        .start_field(FieldSpec::new(FieldParity::Top, case.top.len()))
        .unwrap();
    let mut work = WorkBudget::new(936);
    let progress = raster.step(case.top, true, &mut work, &cancel).unwrap();
    assert_eq!(progress.status, Status::Yield);
    assert!(raster.decoded_pixels() > 84 && raster.decoded_pixels() < 368);
    cancel.store(true, Ordering::Relaxed);
    let failure = raster
        .step(
            &case.top[progress.consumed..],
            true,
            &mut WorkBudget::new(1),
            &cancel,
        )
        .unwrap_err();
    assert_eq!(failure.error, Error::Cancelled);
    assert_eq!(failure.consumed, 0);
    assert_eq!(budget.used(), baseline);
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(raster.try_finish(&cancel).unwrap_err(), Error::Cancelled);
}

#[test]
fn freeze_backpressure_keeps_the_finished_builder_for_retry_and_output_clones_keep_one_charge() {
    let case = &cases::CASES[0];
    let budget = Budget::new(4096).unwrap();
    let baseline = budget.used();
    let cancel = AtomicBool::new(false);
    let mut raster = Raster::new(
        &budget,
        spec(case),
        RasterLimits::default(),
        InitialPlane::Fill(0),
        default_source(Depth::Two),
        &cancel,
    )
    .unwrap();
    decode_fields(&mut raster, case, 1, 3);
    let held = budget.reserve(budget.available()).unwrap();
    assert!(matches!(
        raster.try_finish(&cancel),
        Err(Error::Backpressure { .. })
    ));
    drop(held);
    let plane = raster.try_finish(&cancel).unwrap();
    assert_eq!(plane.pixels.as_ref(), case.expected);
    let charged = budget.used();
    let retained = plane.pixels.clone();
    drop(raster);
    drop(plane);
    assert_eq!(budget.used(), charged);
    assert_eq!(retained.as_ref(), case.expected);
    drop(retained);
    assert_eq!(budget.used(), baseline);
}

#[test]
fn zero_work_starvation_and_a_truncated_final_field_are_distinct() {
    let case = &cases::CASES[0];
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut raster = Raster::new(
        &budget,
        spec(case),
        RasterLimits::default(),
        InitialPlane::Fill(0),
        default_source(Depth::Two),
        &cancel,
    )
    .unwrap();
    raster
        .start_field(FieldSpec::new(FieldParity::Top, case.top.len()))
        .unwrap();
    let p = raster
        .step(case.top, true, &mut WorkBudget::new(0), &cancel)
        .unwrap();
    assert_eq!(p.status, Status::Yield);
    assert_eq!(p.consumed, 0);
    let p = raster
        .step(&[], false, &mut WorkBudget::new(4096), &cancel)
        .unwrap();
    assert_eq!(p.status, Status::NeedInput);
    assert_eq!(p.consumed, 0);
    let failure = raster
        .step(&[], true, &mut WorkBudget::new(4096), &cancel)
        .unwrap_err();
    assert_eq!(failure.error, Error::Truncated);
    assert_eq!(failure.field_offset, 0);
}

#[test]
fn every_truncated_prefix_keeps_the_original_field_length_and_cannot_publish_a_plane() {
    let case = cases::CASES
        .iter()
        .find(|c| c.name == "dvb8-run-branches")
        .unwrap();
    for prefix in 0..case.top.len() {
        let budget = Budget::new(8192).unwrap();
        let cancel = AtomicBool::new(false);
        let mut raster = Raster::new(
            &budget,
            spec(case),
            RasterLimits::default(),
            InitialPlane::Fill(0),
            default_source(Depth::Eight),
            &cancel,
        )
        .unwrap();
        raster
            .start_field(FieldSpec::new(FieldParity::Top, case.top.len()))
            .unwrap();
        let error = raster
            .step(
                &case.top[..prefix],
                true,
                &mut WorkBudget::new(100_000),
                &cancel,
            )
            .unwrap_err();
        assert_eq!(error.error, Error::Truncated);
        assert!(raster.try_finish(&cancel).is_err());
    }
}

#[test]
fn an_out_of_row_run_is_invalid_even_when_the_output_clip_is_empty() {
    // Independent primary-table 2-bit code: five colour-1 pixels, string end, line end.
    let bytes = [0x10, 0x29, 0x00, 0xf0];
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let options = RasterSpec {
        region: Size {
            width: 4,
            height: 2,
        },
        object_origin: Point { x: 0, y: 0 },
        depth: Depth::Two,
        non_modifying: false,
        clip: Some(ClipRect {
            x: 100,
            y: 100,
            width: 1,
            height: 1,
        }),
    };
    let mut raster = Raster::new(
        &budget,
        options,
        RasterLimits::default(),
        InitialPlane::Fill(0),
        default_source(Depth::Two),
        &cancel,
    )
    .unwrap();
    raster
        .start_field(FieldSpec::new(FieldParity::Top, bytes.len()))
        .unwrap();
    let error = raster
        .step(&bytes, true, &mut WorkBudget::new(100), &cancel)
        .unwrap_err();
    assert_eq!(error.error, Error::Invalid("DVB run exceeds source row"));
    assert_eq!(raster.decoded_pixels(), 0);
    assert!(raster.try_finish(&cancel).is_err());
}

#[test]
fn malformed_stuffing_reserved_subblocks_and_short_explicit_eight_bit_runs_fail() {
    let case = &cases::CASES[0];
    let mut stuffing = case.top.to_vec();
    let last = stuffing.len() - 2;
    stuffing[last] |= 1;
    for (depth, bytes, expected) in [
        (
            Depth::Two,
            stuffing.as_slice(),
            Error::Invalid("DVB nonzero string padding"),
        ),
        (
            Depth::Eight,
            &[0xfe][..],
            Error::Unsupported("DVB pixel-data sub-block"),
        ),
        (
            Depth::Eight,
            &[0x12, 0, 0x82, 2, 0, 0, 0xf0][..],
            Error::Invalid("DVB 8-bit explicit run below three"),
        ),
    ] {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut options = spec(case);
        options.depth = depth;
        let mut raster = Raster::new(
            &budget,
            options,
            RasterLimits::default(),
            InitialPlane::Fill(0),
            default_source(depth),
            &cancel,
        )
        .unwrap();
        raster
            .start_field(FieldSpec::new(FieldParity::Top, bytes.len()))
            .unwrap();
        let error = raster
            .step(bytes, true, &mut WorkBudget::new(4096), &cancel)
            .unwrap_err();
        assert_eq!(error.error, expected);
    }
}

#[test]
fn source_and_work_limits_fail_explicitly_and_do_not_release_caller_owned_source() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let case = &cases::CASES[0];
    let limits = RasterLimits {
        max_pixels: 7,
        ..RasterLimits::default()
    };
    assert!(matches!(
        Raster::new(
            &budget,
            spec(case),
            limits,
            InitialPlane::Fill(0),
            default_source(Depth::Two),
            &cancel
        ),
        Err(Error::Limit { .. })
    ));
    let limits = RasterLimits {
        max_work: 1,
        ..RasterLimits::default()
    };
    let mut raster = Raster::new(
        &budget,
        spec(case),
        limits,
        InitialPlane::Fill(0),
        default_source(Depth::Two),
        &cancel,
    )
    .unwrap();
    raster
        .start_field(FieldSpec::new(FieldParity::Top, case.top.len()))
        .unwrap();
    let error = raster
        .step(case.top, true, &mut WorkBudget::new(100), &cancel)
        .unwrap_err();
    assert!(matches!(error.error, Error::Limit { .. }));
    assert!(raster.try_finish(&cancel).is_err());
}

#[test]
fn maps_are_explicit_field_state_and_raw_palette_bytes_are_not_rewritten() {
    let case = cases::CASES
        .iter()
        .find(|c| c.name == "dvb-map2to4")
        .unwrap();
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let facts = source(&budget, case);
    let retained = facts.clone();
    let mut raster = Raster::new(
        &budget,
        spec(case),
        RasterLimits::default(),
        InitialPlane::Fill(0),
        facts,
        &cancel,
    )
    .unwrap();
    field(&mut raster, FieldParity::Top, case.top, 1, 1, &cancel);
    let maps = raster.field_maps().unwrap();
    assert_eq!(maps.two_to_four, [0, 4, 5, 6]);
    assert_eq!(MapTables::default().two_to_four, [0, 7, 8, 15]);
    field(&mut raster, FieldParity::Bottom, case.bottom, 1, 1, &cancel);
    let output = raster.try_finish(&cancel).unwrap();
    assert_eq!(output.source, retained);
    assert_eq!(output.pixels.as_ref(), case.expected);
}

#[test]
fn compound_region_keeps_one_private_plane_until_all_objects_complete() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let facts = default_source(Depth::Two);
    let options = RasterSpec {
        region: Size {
            width: 4,
            height: 2,
        },
        object_origin: Point { x: 0, y: 0 },
        depth: Depth::Two,
        non_modifying: false,
        clip: None,
    };
    let mut raster = Raster::new(
        &budget,
        options,
        RasterLimits::default(),
        InitialPlane::Fill(0),
        facts,
        &cancel,
    )
    .unwrap();
    // Original primary-code two-pixel fields: [1,2] and [2,1].
    let left_top = [0x10, 0x60, 0, 0xf0];
    let left_bottom = [0x10, 0x90, 0, 0xf0];
    field(&mut raster, FieldParity::Top, &left_top, 1, 1, &cancel);
    field(
        &mut raster,
        FieldParity::Bottom,
        &left_bottom,
        1,
        1,
        &cancel,
    );
    let charged = budget.used();
    raster
        .start_object(
            Point { x: 2, y: 0 },
            false,
            default_source(Depth::Two),
            &cancel,
        )
        .unwrap();
    assert_eq!(budget.used(), charged);
    field(&mut raster, FieldParity::Top, &left_bottom, 1, 1, &cancel);
    field(&mut raster, FieldParity::Bottom, &left_top, 1, 1, &cancel);
    assert_eq!(budget.used(), charged);
    assert_eq!(
        raster.object_bounds(),
        Some(ClipRect {
            x: 2,
            y: 0,
            width: 2,
            height: 2
        })
    );
    let plane = raster.try_finish(&cancel).unwrap();
    assert_eq!(plane.pixels.as_ref(), &[1, 2, 2, 1, 2, 1, 1, 2]);
}
