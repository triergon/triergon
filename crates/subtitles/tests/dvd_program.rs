use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::dvd::{
    ControlProgram, ControlState, DvdArea, DvdDate, DvdRasterLimits, DvdRasterStatus, FieldOffsets,
    ProgramJob, ProgramLimits, ProgramStatus,
};

const FIRST: &[u8] = &[
    1, 3, 0x32, 0x10, 4, 0xff, 0xff, 5, 0, 0xa0, 0x0d, 1, 0x40, 0x15, 6, 0, 4, 0, 6, 0xff,
];

fn packet(blocks: &[(u16, &[u8])]) -> Vec<u8> {
    let mut offsets = Vec::new();
    let mut end = 8;
    for (_, commands) in blocks {
        offsets.push(end);
        end += 4 + commands.len();
    }
    let mut raw = Vec::new();
    raw.extend_from_slice(&u16::try_from(end).unwrap().to_be_bytes());
    raw.extend_from_slice(&8_u16.to_be_bytes());
    raw.extend_from_slice(&[0x45, 0x67, 0x76, 0x54]);
    for (index, &(date, commands)) in blocks.iter().enumerate() {
        raw.extend_from_slice(&date.to_be_bytes());
        raw.extend_from_slice(
            &u16::try_from(*offsets.get(index + 1).unwrap_or(&offsets[index]))
                .unwrap()
                .to_be_bytes(),
        );
        raw.extend_from_slice(commands);
    }
    assert_eq!(raw.len(), end);
    raw
}

fn canonical() -> Vec<u8> {
    packet(&[(17, FIRST), (90, &[2, 0xff])])
}

fn ready(job: &mut ProgramJob, quantum: u64) {
    let cancel = AtomicBool::new(false);
    for _ in 0..200_000 {
        let mut work = WorkBudget::new(quantum);
        let status = job.poll(&mut work, &cancel).unwrap();
        assert!(work.spent() <= quantum);
        if status == ProgramStatus::Ready {
            return;
        }
    }
    panic!("bounded program did not finish");
}

fn run(raw: &[u8], initial: ControlState, quantum: u64) -> (ControlProgram, usize, usize) {
    let budget = Budget::new(1024 * 1024).unwrap();
    let mut job = ProgramJob::with_state(
        Bytes::copy_from_slice(&budget, raw).unwrap(),
        &budget,
        ProgramLimits::default(),
        initial,
    )
    .unwrap();
    ready(&mut job, quantum);
    let work = job.work_spent();
    let commands = job.commands_processed();
    (
        job.try_finish(&AtomicBool::new(false)).unwrap(),
        work,
        commands,
    )
}

#[test]
fn canonical_facts_and_raw_spans_are_quantum_independent() {
    let raw = canonical();
    let (baseline, work, commands) = run(&raw, ControlState::default(), 65536);
    assert_eq!((work, commands), (13, 8));
    assert_eq!(baseline.controls().len(), 2);
    assert_eq!(baseline.last_start(), Some(DvdDate(17)));
    assert_eq!(baseline.last_stop(), Some(DvdDate(90)));
    assert!(!baseline.has_forced_start());
    let first = baseline.controls()[0];
    assert_eq!(
        (
            first.raw_start,
            first.raw_end,
            first.commands_start,
            first.commands_end
        ),
        (8, 32, 12, 32)
    );
    assert_eq!(
        (
            first.starts,
            first.forced_starts,
            first.stops,
            first.command_count
        ),
        (1, 0, 0, 6)
    );
    assert_eq!(baseline.raw_control(0).unwrap(), &raw[8..32]);
    assert_eq!(baseline.raw_control(1).unwrap(), &raw[32..]);
    assert_eq!(baseline.state_after().palette_word, Some(0x3210));
    assert_eq!(baseline.state_after().alpha_word, Some(0xffff));
    for quantum in [1, 2, 3, 7, 65536] {
        let (result, spent, count) = run(&raw, ControlState::default(), quantum);
        assert_eq!(result.raw().as_ref(), raw.as_slice());
        assert_eq!(result.controls(), baseline.controls());
        assert_eq!((spent, count), (work, commands));
    }
}

#[test]
fn local_fields_and_last_assignments_remain_distinct() {
    let raw = packet(&[
        (0, FIRST),
        (20, &[3, 0xab, 0xcd, 4, 1, 0x23, 0xff]),
        (90, &[2, 0xff]),
    ]);
    let (result, _, _) = run(&raw, ControlState::default(), 1);
    let update = result.controls()[1];
    assert_eq!(update.local_palette_word, Some(0xabcd));
    assert_eq!(update.local_alpha_word, Some(0x0123));
    assert_eq!((update.local_area, update.local_fields), (None, None));
    assert_eq!(update.state_after.area, result.controls()[0].local_area);
    assert_eq!(update.state_after.fields, result.controls()[0].local_fields);
    assert_eq!(result.state_after(), update.state_after);
}

#[test]
fn ordered_starts_stops_forced_and_duplicate_words_remain_raw() {
    let commands: &[u8] = &[
        0, 2, 1, 1, 3, 0x12, 0x34, 3, 0xab, 0xcd, 4, 0, 0, 4, 0xff, 0xff, 0xff,
    ];
    let raw = packet(&[(1, commands), (u16::MAX, &[2, 1, 0xff])]);
    let (result, _, _) = run(&raw, ControlState::default(), 1);
    let first = result.controls()[0];
    assert_eq!((first.starts, first.forced_starts, first.stops), (2, 1, 1));
    assert_eq!(first.state_after.palette_word, Some(0xabcd));
    assert_eq!(first.state_after.alpha_word, Some(0xffff));
    assert_eq!(&result.raw_control(0).unwrap()[4..], commands);
    assert!(result.has_forced_start());
    assert_eq!(
        (result.last_start(), result.last_stop()),
        (Some(DvdDate(u16::MAX)), Some(DvdDate(u16::MAX)))
    );
}

#[test]
fn caller_initial_facts_are_not_synthesized_from_missing_commands() {
    let initial = ControlState {
        palette_word: Some(0x0123),
        alpha_word: Some(0x4567),
        area: Some(DvdArea {
            left: 10,
            right: 13,
            top: 20,
            bottom: 21,
        }),
        fields: Some(FieldOffsets { top: 4, bottom: 6 }),
    };
    let raw = packet(&[(90, &[2, 0xff])]);
    let (result, _, _) = run(&raw, initial, 1);
    assert_eq!(result.initial_state(), initial);
    assert_eq!(result.state_after(), initial);
    assert_eq!(
        (
            result.controls()[0].local_palette_word,
            result.controls()[0].local_area
        ),
        (None, None)
    );
    let (unknown, _, _) = run(&raw, ControlState::default(), 1);
    assert_eq!(unknown.state_after(), ControlState::default());
    assert_eq!(unknown.last_start(), None);
}

#[test]
fn opaque_operands_and_end_tails_are_preserved_without_fake_commands() {
    let raw = packet(&[
        (0, &[3, 0x32, 0x10, 7, 0, 0, 1, 2, 3]),
        (90, &[2, 0xff, 1, 3, 0xab, 0xcd]),
    ]);
    let (result, _, count) = run(&raw, ControlState::default(), 1);
    assert_eq!(count, 4);
    let first = result.controls()[0];
    let opaque = first.opaque.unwrap();
    assert_eq!(opaque.opcode, 7);
    assert_eq!(
        &result.raw()[usize::from(opaque.operands_start)..usize::from(opaque.operands_end)],
        &[0, 0, 1, 2, 3]
    );
    assert_eq!((first.starts, first.stops), (0, 0));
    let last = result.controls()[1];
    assert_eq!(
        (last.starts, last.stops, last.local_palette_word),
        (0, 1, None)
    );
    assert_eq!(
        &result.raw()[usize::from(last.commands_end)..usize::from(last.raw_end)],
        &[1, 3, 0xab, 0xcd]
    );
}

#[test]
fn zero_work_does_not_read_commands_or_allocate_records() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = ProgramJob::new(
        Bytes::copy_from_slice(&budget, &canonical()).unwrap(),
        &budget,
        ProgramLimits::default(),
    )
    .unwrap();
    let before = budget.used();
    for _ in 0..3 {
        assert_eq!(
            job.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
            ProgramStatus::Yield
        );
        assert_eq!(
            (
                job.work_spent(),
                job.commands_processed(),
                job.controls_processed()
            ),
            (0, 0, 0)
        );
        assert_eq!(budget.used(), before);
    }
    job.poll(&mut WorkBudget::new(2), &cancel).unwrap();
    assert_eq!(job.work_spent(), 2);
    job.poll(&mut WorkBudget::new(0), &cancel).unwrap();
    assert_eq!(
        (job.work_spent(), job.commands_processed(), budget.used()),
        (2, 0, before)
    );
}

#[test]
fn record_allocation_pressure_preserves_paid_work_and_command_progress() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = ProgramJob::new(
        Bytes::copy_from_slice(&budget, &canonical()).unwrap(),
        &budget,
        ProgramLimits::default(),
    )
    .unwrap();
    let blocker = budget.reserve(budget.available()).unwrap();
    for _ in 0..3 {
        assert!(matches!(
            job.poll(&mut WorkBudget::new(64), &cancel),
            Err(Error::Backpressure { .. })
        ));
        assert_eq!(
            (
                job.work_spent(),
                job.commands_processed(),
                job.controls_processed()
            ),
            (9, 6, 0)
        );
        assert_eq!(
            job.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
            ProgramStatus::Yield
        );
    }
    drop(blocker);
    ready(&mut job, 1);
    assert_eq!((job.work_spent(), job.commands_processed()), (13, 8));
    assert_eq!(job.try_finish(&cancel).unwrap().controls().len(), 2);
}

#[test]
fn cancellation_before_during_and_after_parsing_is_sticky() {
    for stage in 0..3 {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut job = ProgramJob::new(
            Bytes::copy_from_slice(&budget, &canonical()).unwrap(),
            &budget,
            ProgramLimits::default(),
        )
        .unwrap();
        if stage == 1 {
            job.poll(&mut WorkBudget::new(4), &cancel).unwrap();
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
fn control_command_and_work_limits_fail_without_partial_publication() {
    let variants = [
        ProgramLimits {
            max_controls: 0,
            ..ProgramLimits::default()
        },
        ProgramLimits {
            max_controls: 1,
            ..ProgramLimits::default()
        },
        ProgramLimits {
            max_commands: 0,
            ..ProgramLimits::default()
        },
        ProgramLimits {
            max_commands_per_control: 1,
            ..ProgramLimits::default()
        },
        ProgramLimits {
            max_work: 12,
            ..ProgramLimits::default()
        },
    ];
    for limits in variants {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut job = ProgramJob::new(
            Bytes::copy_from_slice(&budget, &canonical()).unwrap(),
            &budget,
            limits,
        )
        .unwrap();
        let error = job.poll(&mut WorkBudget::new(65536), &cancel).unwrap_err();
        assert!(matches!(error, Error::Limit { .. }));
        assert_eq!(job.try_finish(&cancel).unwrap_err(), error);
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn invalid_links_and_missing_end_do_not_loop_or_publish() {
    let mut backward = canonical();
    backward[34..36].copy_from_slice(&8_u16.to_be_bytes());
    let mut outside = canonical();
    outside[10..12].copy_from_slice(&u16::MAX.to_be_bytes());
    let mut pixel_link = canonical();
    pixel_link[2..4].copy_from_slice(&3_u16.to_be_bytes());
    for raw in [backward, outside, pixel_link, packet(&[(0, &[1])])] {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut job = ProgramJob::new(
            Bytes::copy_from_slice(&budget, &raw).unwrap(),
            &budget,
            ProgramLimits::default(),
        )
        .unwrap();
        let error = job.poll(&mut WorkBudget::new(65536), &cancel).unwrap_err();
        assert!(matches!(error, Error::Invalid(_) | Error::Truncated));
        assert_eq!(
            job.poll(&mut WorkBudget::new(0), &cancel).unwrap_err(),
            error
        );
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn raw_and_record_charges_outlive_the_job() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = ProgramJob::new(
        Bytes::copy_from_slice(&budget, &canonical()).unwrap(),
        &budget,
        ProgramLimits::default(),
    )
    .unwrap();
    ready(&mut job, 1);
    let result = job.try_finish(&cancel).unwrap();
    let raw = result.raw().clone();
    drop(job);
    assert!(budget.used() > Budget::control_storage_bytes() + raw.retained_storage_bytes());
    assert_eq!(result.controls().len(), 2);
    drop(result);
    assert_eq!(
        budget.used(),
        Budget::control_storage_bytes() + raw.retained_storage_bytes()
    );
    drop(raw);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn local_raster_is_real_and_absent_local_fields_stay_absent() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = ProgramJob::new(
        Bytes::copy_from_slice(&budget, &canonical()).unwrap(),
        &budget,
        ProgramLimits::default(),
    )
    .unwrap();
    ready(&mut job, 1);
    let result = job.try_finish(&cancel).unwrap();
    assert!(
        result
            .local_raster(1, &budget, DvdRasterLimits::default())
            .unwrap()
            .is_none()
    );
    let mut raster = result
        .local_raster(0, &budget, DvdRasterLimits::default())
        .unwrap()
        .unwrap();
    while raster.poll(&mut WorkBudget::new(1), &cancel).unwrap() != DvdRasterStatus::Ready {}
    assert_eq!(
        raster.try_finish(&cancel).unwrap().pixels().as_ref(),
        [0, 1, 2, 3, 3, 2, 1, 0]
    );
    assert!(result.raw_control(2).is_err());
}

#[test]
fn foreign_pool_and_packet_size_limits_are_explicit() {
    let budget = Budget::new(4096).unwrap();
    let other = Budget::new(4096).unwrap();
    assert_eq!(
        ProgramJob::new(
            Bytes::copy_from_slice(&other, &canonical()).unwrap(),
            &budget,
            ProgramLimits::default()
        )
        .unwrap_err(),
        Error::ForeignPool
    );
    assert!(matches!(
        ProgramJob::new(
            Bytes::copy_from_slice(&budget, &canonical()).unwrap(),
            &budget,
            ProgramLimits {
                max_packet_bytes: 37,
                ..ProgramLimits::default()
            }
        ),
        Err(Error::Limit {
            requested: 38,
            maximum: 37
        })
    ));
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn long_programs_remain_linear_and_bounded_by_source_commands() {
    let mut commands = Vec::new();
    for n in 0..5000_u16 {
        commands.push(3);
        commands.extend_from_slice(&n.to_be_bytes());
    }
    commands.push(0xff);
    let raw = packet(&[(0, &commands)]);
    for quantum in [1, 7, 65536] {
        let (result, work, count) = run(&raw, ControlState::default(), quantum);
        assert_eq!((work, count), (5004, 5001));
        assert_eq!(result.state_after().palette_word, Some(4999));
    }
    let blocks: Vec<(u16, &[u8])> = (0..512).map(|n| (n, &[2, 0xff][..])).collect();
    let (result, work, count) = run(&packet(&blocks), ControlState::default(), 1);
    assert_eq!((result.controls().len(), work, count), (512, 2049, 1024));
}
