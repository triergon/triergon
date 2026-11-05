use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::dvd::{
    ControlState, DvdArea, FieldOffsets, ReceiverDisplayTimes, TraditionalLimits,
    TraditionalSpuJob, TraditionalStatus, TraditionalSubpicture,
};

const A: &[u8] = &[
    1, 3, 0x32, 0x10, 4, 0xff, 0xff, 5, 0, 0xa0, 0x0d, 1, 0x40, 0x15, 6, 0, 4, 0, 5, 0xff,
];
const B: &[u8] = &[
    1, 3, 0x01, 0x23, 4, 0xff, 0xff, 5, 0, 0xf0, 0x12, 1, 0xf0, 0x20, 6, 0, 6, 0, 7, 0xff,
];
const GEOMETRY_FIELDS: &[u8] = &[5, 0, 0xa0, 0x0d, 1, 0x40, 0x15, 6, 0, 4, 0, 5, 0xff];

fn rgb() -> [u32; 16] {
    std::array::from_fn(|i| 0x0011_2233 + (i as u32 * 0x0001_0203))
}

fn packet_with_pixels(blocks: &[(u16, &[u8])], pixels: [u8; 4]) -> Vec<u8> {
    let mut offsets = Vec::new();
    let mut size = 8;
    for (_, commands) in blocks {
        offsets.push(size);
        size += 4 + commands.len();
    }
    let mut raw = Vec::new();
    raw.extend_from_slice(&u16::try_from(size).unwrap().to_be_bytes());
    raw.extend_from_slice(&8_u16.to_be_bytes());
    raw.extend_from_slice(&pixels);
    for (i, &(date, commands)) in blocks.iter().enumerate() {
        raw.extend_from_slice(&date.to_be_bytes());
        raw.extend_from_slice(
            &u16::try_from(*offsets.get(i + 1).unwrap_or(&offsets[i]))
                .unwrap()
                .to_be_bytes(),
        );
        raw.extend_from_slice(commands);
    }
    assert_eq!(raw.len(), size);
    raw
}

fn packet(blocks: &[(u16, &[u8])]) -> Vec<u8> {
    packet_with_pixels(blocks, [0x11, 0x12, 0x13, 0x11])
}
fn canonical() -> Vec<u8> {
    packet(&[(0, A), (90, &[2, 0xff])])
}

fn ready(job: &mut TraditionalSpuJob, quantum: u64) -> u64 {
    let cancel = AtomicBool::new(false);
    let mut spent = 0;
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(quantum);
        let result = job.poll(&mut work, &cancel).unwrap();
        assert!(work.spent() <= quantum);
        spent += work.spent();
        if result == TraditionalStatus::Ready {
            return spent;
        }
    }
    panic!("bounded traditional SPU did not finish");
}

fn run(
    raw: &[u8],
    initial: ControlState,
    palette: Option<[u32; 16]>,
    quantum: u64,
) -> (TraditionalSubpicture, u64) {
    let budget = Budget::new(1024 * 1024).unwrap();
    let mut job = TraditionalSpuJob::new(
        Bytes::copy_from_slice(&budget, raw).unwrap(),
        initial,
        palette,
        &budget,
        TraditionalLimits::default(),
    )
    .unwrap();
    let spent = ready(&mut job, quantum);
    (job.try_finish(&AtomicBool::new(false)).unwrap(), spent)
}

#[test]
fn whole_spu_output_and_work_are_independent_of_poll_quantum() {
    let raw = canonical();
    let (large, total) = run(&raw, ControlState::default(), Some(rgb()), 4096);
    for quantum in [1, 2, 3, 7, 4096] {
        let (result, spent) = run(&raw, ControlState::default(), Some(rgb()), quantum);
        assert_eq!(spent, total);
        assert_eq!(result.program().raw().as_ref(), raw);
        assert_eq!(result.program().controls(), large.program().controls());
        assert_eq!(result.selected_control(), Some(0));
        assert_eq!(
            result.receiver_display_times(),
            ReceiverDisplayTimes {
                start_ms: 0,
                end_ms: 1024
            }
        );
        assert!(!result.receiver_menu());
        assert!(!result.receiver_forced_flag());
        let view = result.bitmap().unwrap().native_view().unwrap();
        assert_eq!(
            view.crop().area,
            DvdArea {
                left: 10,
                right: 13,
                top: 20,
                bottom: 21
            }
        );
        assert_eq!(view.indices_row(0).unwrap(), &[1; 4]);
        assert_eq!(view.indices_row(1).unwrap(), &[2; 4]);
        assert_eq!(view.argb_at(0, 0).unwrap(), 0xff12_2436);
        assert_eq!(result.palette().unwrap().source_rgb(), Some(&rgb()));
    }
}

#[test]
fn last_eligible_raster_replaces_pixels_but_later_palette_only_words_stay_source() {
    let raw = packet(&[
        (0, A),
        (17, B),
        (20, &[3, 0xab, 0xcd, 4, 0, 0, 0xff]),
        (90, &[2, 0xff]),
    ]);
    let (result, _) = run(&raw, ControlState::default(), Some(rgb()), 1);
    assert_eq!(result.selected_control(), Some(1));
    let view = result.bitmap().unwrap().native_view().unwrap();
    assert_eq!(
        view.crop().area,
        DvdArea {
            left: 15,
            right: 18,
            top: 31,
            bottom: 32
        }
    );
    assert_eq!(view.indices_row(0).unwrap(), &[3; 4]);
    assert_eq!(view.indices_row(1).unwrap(), &[1; 4]);
    assert_eq!(result.palette().unwrap().palette_word(), 0x0123);
    assert_eq!(result.palette().unwrap().alpha_word(), 0xffff);
    assert_eq!(result.program().state_after().palette_word, Some(0xabcd));
    assert_eq!(result.program().state_after().alpha_word, Some(0));
    assert_eq!(
        result.receiver_display_times(),
        ReceiverDisplayTimes {
            start_ms: 193,
            end_ms: 1024
        }
    );
    assert_eq!(result.program().raw().as_ref(), raw);
}

#[test]
fn late_forced_command_changes_menu_selection_without_relabelling_earlier_rectangle() {
    let mut transparent = A.to_vec();
    transparent[5] = 0;
    transparent[6] = 0;
    let raw = packet(&[(0, &transparent), (17, &[0, 0xff]), (90, &[2, 0xff])]);
    let (result, _) = run(&raw, ControlState::default(), Some(rgb()), 1);
    assert!(result.receiver_menu());
    assert!(!result.receiver_forced_flag());
    assert!(result.receiver_emits_bitmap());
    assert_eq!(
        result
            .bitmap()
            .unwrap()
            .native_view()
            .unwrap()
            .argb_at(0, 0)
            .unwrap(),
        0x0012_2436
    );
    assert_eq!(
        result.receiver_display_times(),
        ReceiverDisplayTimes {
            start_ms: 0,
            end_ms: 1024
        }
    );
}

#[test]
fn forced_and_ordinary_dates_and_last_ordinary_start_remain_separate() {
    let mut forced = A.to_vec();
    forced[0] = 0;
    let raw = packet(&[(1, &forced), (17, &[1, 0xff]), (90, &[2, 0xff])]);
    let (result, _) = run(&raw, ControlState::default(), Some(rgb()), 1);
    assert!(result.receiver_menu());
    assert!(result.receiver_forced_flag());
    assert_eq!(
        result.receiver_display_times(),
        ReceiverDisplayTimes {
            start_ms: 193,
            end_ms: 1024
        }
    );
    assert_eq!(result.program().controls()[0].date.0, 1);
    assert_eq!(result.program().last_start().unwrap().0, 17);
}

#[test]
fn missing_source_commands_do_not_become_known_zero_fields() {
    let (result, _) = run(
        &packet(&[(17, GEOMETRY_FIELDS)]),
        ControlState::default(),
        Some(rgb()),
        1,
    );
    assert_eq!(
        (result.program().last_start(), result.program().last_stop()),
        (None, None)
    );
    assert_eq!(
        (
            result.program().state_after().palette_word,
            result.program().state_after().alpha_word
        ),
        (None, None)
    );
    assert_eq!(
        result.receiver_display_times(),
        ReceiverDisplayTimes {
            start_ms: 0,
            end_ms: 0
        }
    );
    assert!(!result.receiver_emits_bitmap());
    assert_eq!(
        result.bitmap().unwrap().source().pixels().as_ref(),
        &[1, 1, 1, 1, 2, 2, 2, 2]
    );
    assert_eq!(result.palette().unwrap().alpha_word(), 0);
}

#[test]
fn a_receiver_omitted_one_row_does_not_replace_an_earlier_eligible_bitmap() {
    let mut one_row = B.to_vec();
    one_row[13] = 0x1f;
    let (only, _) = run(
        &packet(&[(0, &one_row)]),
        ControlState::default(),
        Some(rgb()),
        1,
    );
    assert!(only.bitmap().is_none());
    assert!(!only.receiver_emits_bitmap());
    assert_eq!(
        only.program().controls()[0]
            .local_area
            .unwrap()
            .dimensions()
            .unwrap(),
        (4, 1)
    );
    let (later, _) = run(
        &packet(&[(0, A), (17, &one_row), (90, &[2, 0xff])]),
        ControlState::default(),
        Some(rgb()),
        1,
    );
    assert_eq!(later.selected_control(), Some(0));
    assert_eq!(
        later
            .bitmap()
            .unwrap()
            .native_view()
            .unwrap()
            .indices_row(0)
            .unwrap(),
        &[1; 4]
    );
    assert_eq!(later.program().controls().len(), 3);
}

#[test]
fn invalid_intermediate_raster_cannot_be_hidden_by_a_later_replacement() {
    let raw = packet_with_pixels(&[(0, A), (17, B)], [0x21, 0x12, 0x13, 0x11]);
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = TraditionalSpuJob::new(
        Bytes::copy_from_slice(&budget, &raw).unwrap(),
        ControlState::default(),
        Some(rgb()),
        &budget,
        TraditionalLimits::default(),
    )
    .unwrap();
    let error = job.poll(&mut WorkBudget::new(65536), &cancel).unwrap_err();
    assert_eq!(error, Error::Invalid("DVD run exceeds display row"));
    assert_eq!(job.try_finish(&cancel).unwrap_err(), error);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn aggregate_pixel_and_intermediate_bitmap_limits_apply_before_new_raster_allocation() {
    let raw = packet(&[(0, A), (17, B)]);
    let variants = [
        (
            TraditionalLimits {
                max_total_pixels: 8,
                ..TraditionalLimits::default()
            },
            Error::Limit {
                requested: 16,
                maximum: 8,
            },
        ),
        (
            TraditionalLimits {
                max_bitmaps: 1,
                ..TraditionalLimits::default()
            },
            Error::Limit {
                requested: 2,
                maximum: 1,
            },
        ),
    ];
    for (limits, expected) in variants {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut job = TraditionalSpuJob::new(
            Bytes::copy_from_slice(&budget, &raw).unwrap(),
            ControlState::default(),
            Some(rgb()),
            &budget,
            limits,
        )
        .unwrap();
        assert_eq!(
            job.poll(&mut WorkBudget::new(65536), &cancel).unwrap_err(),
            expected
        );
        assert_eq!((job.bitmaps_started(), job.pixels_planned()), (1, 8));
        assert_eq!(job.try_finish(&cancel).unwrap_err(), expected);
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn zero_work_preserves_every_allocation_boundary_and_paid_progress() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = TraditionalSpuJob::new(
        Bytes::copy_from_slice(&budget, &canonical()).unwrap(),
        ControlState::default(),
        Some(rgb()),
        &budget,
        TraditionalLimits::default(),
    )
    .unwrap();
    let mut complete = false;
    for _ in 0..256 {
        let before = budget.used();
        let counters = (job.bitmaps_started(), job.pixels_planned());
        let mut zero = WorkBudget::new(0);
        job.poll(&mut zero, &cancel).unwrap();
        assert_eq!((zero.spent(), budget.used()), (0, before));
        assert_eq!((job.bitmaps_started(), job.pixels_planned()), counters);
        if job.poll(&mut WorkBudget::new(1), &cancel).unwrap() == TraditionalStatus::Ready {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert!(job.try_finish(&cancel).unwrap().receiver_emits_bitmap());
}

#[test]
fn repeated_allocation_pressure_does_not_recharge_work_or_lose_source_state() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = TraditionalSpuJob::new(
        Bytes::copy_from_slice(&budget, &canonical()).unwrap(),
        ControlState::default(),
        Some(rgb()),
        &budget,
        TraditionalLimits::default(),
    )
    .unwrap();
    let mut pressure = 0;
    let mut complete = false;
    for _ in 0..256 {
        let blocker = budget.reserve(budget.available()).unwrap();
        match job.poll(&mut WorkBudget::new(1), &cancel) {
            Err(Error::Backpressure { .. }) => {
                pressure += 1;
                let mut retry = WorkBudget::new(64);
                assert!(matches!(
                    job.poll(&mut retry, &cancel),
                    Err(Error::Backpressure { .. })
                ));
                assert_eq!(retry.spent(), 0);
                let charge = budget.used();
                assert_eq!(
                    job.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
                    TraditionalStatus::Yield
                );
                assert_eq!(budget.used(), charge);
            }
            Ok(TraditionalStatus::Ready) => {
                complete = true;
                drop(blocker);
                break;
            }
            Ok(TraditionalStatus::Yield) => (),
            Err(error) => panic!("unexpected pressure outcome {error:?}"),
        }
        drop(blocker);
        if job.poll(&mut WorkBudget::new(1), &cancel).unwrap() == TraditionalStatus::Ready {
            complete = true;
            break;
        }
    }
    assert!(complete && pressure > 0);
    let result = job.try_finish(&cancel).unwrap();
    assert_eq!(result.program().raw().as_ref(), canonical());
    assert_eq!(
        result
            .bitmap()
            .unwrap()
            .native_view()
            .unwrap()
            .indices_row(1)
            .unwrap(),
        &[2; 4]
    );
}

#[test]
fn cancellation_during_each_stage_and_after_ready_is_sticky_and_releases_storage() {
    for steps in [0, 3, 18, 100] {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut job = TraditionalSpuJob::new(
            Bytes::copy_from_slice(&budget, &canonical()).unwrap(),
            ControlState::default(),
            Some(rgb()),
            &budget,
            TraditionalLimits::default(),
        )
        .unwrap();
        for _ in 0..steps {
            job.poll(&mut WorkBudget::new(1), &cancel).unwrap();
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
fn explicit_cross_packet_palette_carry_preserves_old_outputs_and_local_geometry_rule() {
    let (first, _) = run(&packet(&[(0, B)]), ControlState::default(), Some(rgb()), 1);
    let initial = first.program().state_after();
    let (second, _) = run(&packet(&[(17, GEOMETRY_FIELDS)]), initial, Some(rgb()), 1);
    assert_eq!(second.palette().unwrap().palette_word(), 0x0123);
    assert_eq!(second.program().controls()[0].local_palette_word, None);
    assert_eq!(second.program().initial_state().palette_word, Some(0x0123));
    assert_eq!(
        second
            .bitmap()
            .unwrap()
            .native_view()
            .unwrap()
            .indices_row(0)
            .unwrap(),
        &[1; 4]
    );
    assert_eq!(
        first
            .bitmap()
            .unwrap()
            .native_view()
            .unwrap()
            .indices_row(0)
            .unwrap(),
        &[3; 4]
    );
    let (no_local, _) = run(&packet(&[(17, &[1, 0xff])]), initial, Some(rgb()), 1);
    assert!(no_local.bitmap().is_none());
    assert_eq!(no_local.program().state_after().area, initial.area);
    assert_eq!(no_local.program().controls()[0].local_area, None);
}

#[test]
fn receiver_times_do_not_depend_on_the_raster_control_date_or_lose_raw_maximum() {
    let (result, _) = run(
        &packet(&[(17, A), (u16::MAX, &[2, 0xff])]),
        ControlState::default(),
        None,
        1,
    );
    assert_eq!(
        result.receiver_display_times(),
        ReceiverDisplayTimes {
            start_ms: 193,
            end_ms: 745642
        }
    );
    assert_eq!(result.program().last_stop().unwrap().0, u16::MAX);
    assert!(result.palette().unwrap().source_rgb().is_none());
    assert_eq!(
        result.palette().unwrap().argb(),
        &[0xff00_0000, 0xff54_5454, 0xffa9_a9a9, 0xfffe_fefe]
    );
}

#[test]
fn source_owner_and_retained_palette_survive_job_teardown() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let raw = Bytes::copy_from_slice(&budget, &canonical()).unwrap();
    let retained = raw.clone();
    let mut job = TraditionalSpuJob::new(
        raw,
        ControlState::default(),
        Some(rgb()),
        &budget,
        TraditionalLimits::default(),
    )
    .unwrap();
    ready(&mut job, 1);
    let result = job.try_finish(&cancel).unwrap();
    drop(job);
    assert_eq!(result.program().raw().as_ptr(), retained.as_ptr());
    assert_eq!(result.palette().unwrap().source_rgb(), Some(&rgb()));
    assert!(budget.used() > Budget::control_storage_bytes() + retained.retained_storage_bytes());
    drop(result);
    assert_eq!(
        budget.used(),
        Budget::control_storage_bytes() + retained.retained_storage_bytes()
    );
    drop(retained);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn foreign_pool_opaque_operation_and_premature_finish_remain_explicit() {
    let budget = Budget::new(4096).unwrap();
    let other = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        TraditionalSpuJob::new(
            Bytes::copy_from_slice(&other, &canonical()).unwrap(),
            ControlState::default(),
            None,
            &budget,
            TraditionalLimits::default()
        )
        .unwrap_err(),
        Error::ForeignPool
    );
    let mut opaque = TraditionalSpuJob::new(
        Bytes::copy_from_slice(&budget, &packet(&[(0, &[7, 1, 2, 3])])).unwrap(),
        ControlState::default(),
        None,
        &budget,
        TraditionalLimits::default(),
    )
    .unwrap();
    assert_eq!(
        opaque
            .poll(&mut WorkBudget::new(65536), &cancel)
            .unwrap_err(),
        Error::Unsupported("DVD receiver opaque control")
    );
    assert_eq!(
        opaque.try_finish(&cancel).unwrap_err(),
        Error::Unsupported("DVD receiver opaque control")
    );
    let mut early = TraditionalSpuJob::new(
        Bytes::copy_from_slice(&budget, &canonical()).unwrap(),
        ControlState {
            fields: Some(FieldOffsets { top: 4, bottom: 5 }),
            ..ControlState::default()
        },
        None,
        &budget,
        TraditionalLimits::default(),
    )
    .unwrap();
    assert_eq!(
        early.try_finish(&cancel).unwrap_err(),
        Error::Invalid("DVD subpicture not ready")
    );
    assert_eq!(
        early
            .poll(&mut WorkBudget::new(65536), &cancel)
            .unwrap_err(),
        Error::Invalid("DVD subpicture not ready")
    );
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn source_display_commands_keep_wire_order_and_exact_dates() {
    use triergon_subtitles::dvd::{DisplayAction, DisplayControlStep};

    let raw = packet(&[(7, A), (7, &[2, 0, 1, 0xff]), (9, &[2, 0xff])]);
    let (picture, _) = run(&raw, ControlState::default(), Some(rgb()), 1);
    let expected = [
        (0, 7 * 1024, DisplayAction::Show),
        (1, 7 * 1024, DisplayAction::Clear),
        (1, 7 * 1024, DisplayAction::ForcedShow),
        (1, 7 * 1024, DisplayAction::Show),
        (2, 9 * 1024, DisplayAction::Clear),
    ];
    for quantum in [1, 64] {
        let mut controls = picture.program().display_controls().unwrap();
        let cancel = AtomicBool::new(true);
        let mut work = WorkBudget::new(quantum);
        assert!(matches!(
            controls.step(&mut work, &cancel),
            Err(Error::Cancelled)
        ));
        assert_eq!(work.spent(), 0);
        cancel.store(false, Ordering::Relaxed);
        assert_eq!(
            controls.step(&mut WorkBudget::new(0), &cancel).unwrap(),
            DisplayControlStep::Yield
        );
        let mut actual = Vec::new();
        for _ in 0..64 {
            let mut work = WorkBudget::new(quantum);
            match controls.step(&mut work, &cancel).unwrap() {
                DisplayControlStep::Command(event) => {
                    assert!(work.spent() != 0);
                    actual.push((event.index, event.date.ticks_90k(), event.action));
                }
                DisplayControlStep::Yield => assert_eq!(work.spent(), quantum),
                DisplayControlStep::End => break,
            }
        }
        assert_eq!(actual, expected);
        assert_eq!(
            controls.step(&mut WorkBudget::new(0), &cancel).unwrap(),
            DisplayControlStep::End
        );
    }
}
