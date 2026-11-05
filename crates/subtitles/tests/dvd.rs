use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::dvd::{
    Command, DvdArea, DvdDate, DvdPacket, DvdRasterJob, DvdRasterLimits, DvdRasterStatus,
    FieldOffsets,
};

fn area(width: u16, height: u16) -> DvdArea {
    DvdArea {
        left: 10,
        right: 10 + width - 1,
        top: 20,
        bottom: 20 + height - 1,
    }
}

fn fixture(top: &[u8], bottom: &[u8], width: u16, height: u16) -> (Vec<u8>, FieldOffsets) {
    let fields = FieldOffsets {
        top: 4,
        bottom: 4 + top.len() as u16,
    };
    let control = 4 + top.len() + bottom.len();
    let second = control + 24;
    let end = second + 6;
    let mut raw = Vec::new();
    raw.extend((end as u16).to_be_bytes());
    raw.extend((control as u16).to_be_bytes());
    raw.extend(top);
    raw.extend(bottom);
    raw.extend([0, 0]);
    raw.extend((second as u16).to_be_bytes());
    raw.extend([1, 3, 0x32, 0x10, 4, 0xff, 0xff, 5]);
    let a = area(width, height);
    for (start, end) in [(a.left, a.right), (a.top, a.bottom)] {
        raw.extend([
            (start >> 4) as u8,
            ((start << 4) | (end >> 8)) as u8,
            end as u8,
        ]);
    }
    raw.push(6);
    raw.extend(fields.top.to_be_bytes());
    raw.extend(fields.bottom.to_be_bytes());
    raw.push(0xff);
    assert_eq!(raw.len(), second);
    raw.extend(90_u16.to_be_bytes());
    raw.extend((second as u16).to_be_bytes());
    raw.extend([2, 0xff]);
    assert_eq!(raw.len(), end);
    (raw, fields)
}

fn decode(
    raw: &[u8],
    fields: FieldOffsets,
    size: (u16, u16),
    quantum: u64,
) -> (Vec<u8>, usize, usize) {
    let b = Budget::new(1024 * 1024).unwrap();
    let input = Bytes::copy_from_slice(&b, raw).unwrap();
    let mut job = DvdRasterJob::new(
        input,
        area(size.0, size.1),
        fields,
        &b,
        DvdRasterLimits::default(),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(quantum);
        let status = job.poll(&mut work, &cancel).unwrap();
        assert!(work.spent() <= quantum);
        if status == DvdRasterStatus::Ready {
            let spent = job.work_spent();
            let nibbles = job.nibbles_read();
            let result = job.try_finish(&cancel).unwrap();
            assert_eq!((result.width, result.height), size);
            let retained = result.pixels().clone();
            drop(result);
            drop(job);
            assert_eq!(
                b.used(),
                Budget::control_storage_bytes() + retained.retained_storage_bytes()
            );
            let pixels = retained.to_vec();
            drop(retained);
            assert_eq!(b.used(), Budget::control_storage_bytes());
            return (pixels, spent, nibbles);
        }
    }
    panic!("bounded fixture did not finish");
}

#[test]
fn four_rle_code_forms_and_fill_row_are_quantum_independent() {
    // Independent literal codewords at each form boundary; ignored low padding
    // nibble deliberately differs from a zero-filled encoder's output.
    let cases: &[(u16, u8, &[u8])] = &[
        (1, 0, &[0x4d]),
        (3, 3, &[0xfd]),
        (4, 1, &[0x11]),
        (15, 2, &[0x3e]),
        (16, 0, &[0x04, 0x0d]),
        (63, 3, &[0x0f, 0xfd]),
        (64, 2, &[0x01, 0x02]),
        (255, 1, &[0x03, 0xfd]),
        (270, 3, &[0, 3]),
    ];
    for &(width, index, encoded) in cases {
        let (raw, fields) = fixture(encoded, &[], width, 1);
        let expected = vec![index; usize::from(width)];
        let base = decode(&raw, fields, (width, 1), 65536);
        assert_eq!(base.0, expected);
        for quantum in [1, 2, 3, 17, 65536] {
            assert_eq!(decode(&raw, fields, (width, 1), quantum), base);
        }
    }
}

#[test]
fn alternating_field_streams_write_directly_in_display_order() {
    let (raw, fields) = fixture(&[0x90, 0xb0, 0x45], &[0xa0, 0x80], 2, 5);
    for quantum in [1, 3, 65536] {
        assert_eq!(
            decode(&raw, fields, (2, 5), quantum).0,
            [1, 1, 2, 2, 3, 3, 0, 0, 0, 1]
        );
    }
}

#[test]
fn every_padding_nibble_is_opaque_at_the_row_boundary() {
    for padding in 0..16 {
        let (raw, fields) = fixture(&[0x40 | padding, 0x70 | padding], &[0x50 | padding], 1, 3);
        assert_eq!(decode(&raw, fields, (1, 3), 1).0, [0, 1, 3]);
    }
}

#[test]
fn zero_work_never_allocates_or_reads_and_pressure_retries_once() {
    let (raw, fields) = fixture(&[0, 2], &[], 100, 1);
    let b = Budget::new(4096).unwrap();
    let input = Bytes::copy_from_slice(&b, &raw).unwrap();
    let mut job =
        DvdRasterJob::new(input, area(100, 1), fields, &b, DvdRasterLimits::default()).unwrap();
    let cancel = AtomicBool::new(false);
    let before = b.used();
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
        DvdRasterStatus::Yield
    );
    assert_eq!(
        (b.used(), job.work_spent(), job.nibbles_read()),
        (before, 0, 0)
    );
    let blocker = b.reserve(b.available()).unwrap();
    assert!(matches!(
        job.poll(&mut WorkBudget::new(1), &cancel),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(job.work_spent(), 1);
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
        DvdRasterStatus::Yield
    );
    assert_eq!((job.work_spent(), job.nibbles_read()), (1, 0));
    drop(blocker);
    while job.poll(&mut WorkBudget::new(3), &cancel).unwrap() != DvdRasterStatus::Ready {}
    assert_eq!(job.work_spent(), 105);
    assert_eq!(
        job.try_finish(&cancel).unwrap().pixels().as_ref(),
        vec![2; 100]
    );
}

#[test]
fn freeze_pressure_preserves_pixels_and_output_outlives_the_job() {
    let (raw, fields) = fixture(&[0x90], &[0xa0], 2, 2);
    let b = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = DvdRasterJob::new(
        Bytes::copy_from_slice(&b, &raw).unwrap(),
        area(2, 2),
        fields,
        &b,
        DvdRasterLimits::default(),
    )
    .unwrap();
    assert_eq!(
        job.poll(&mut WorkBudget::new(100), &cancel).unwrap(),
        DvdRasterStatus::Ready
    );
    let spent = job.work_spent();
    let blocker = b.reserve(b.available()).unwrap();
    assert!(matches!(
        job.try_finish(&cancel),
        Err(Error::Backpressure { .. })
    ));
    drop(blocker);
    let result = job.try_finish(&cancel).unwrap();
    drop(job);
    assert_eq!(result.pixels().as_ref(), [1, 1, 2, 2]);
    assert_eq!(spent, 7);
    assert_eq!(
        b.used(),
        Budget::control_storage_bytes() + result.pixels().retained_storage_bytes()
    );
    drop(result);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn cancellation_is_sticky_at_yield_and_ready_and_releases_charges() {
    for quantum in [1, 1000] {
        let (raw, fields) = fixture(&[0, 1], &[], 64, 1);
        let b = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut job = DvdRasterJob::new(
            Bytes::copy_from_slice(&b, &raw).unwrap(),
            area(64, 1),
            fields,
            &b,
            DvdRasterLimits::default(),
        )
        .unwrap();
        job.poll(&mut WorkBudget::new(quantum), &cancel).unwrap();
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(job.try_finish(&cancel).unwrap_err(), Error::Cancelled);
        assert_eq!(b.used(), Budget::control_storage_bytes());
        cancel.store(false, Ordering::Relaxed);
        assert_eq!(
            job.poll(&mut WorkBudget::new(1000), &cancel).unwrap_err(),
            Error::Cancelled
        );
    }
}

#[test]
fn truncated_codes_and_overlong_rows_fail_without_reading_commands() {
    for encoded in [&[0_u8][..], &[0, 0x10][..], &[0xc0][..]] {
        let (raw, fields) = fixture(encoded, &[], 2, 1);
        let b = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut job = DvdRasterJob::new(
            Bytes::copy_from_slice(&b, &raw).unwrap(),
            area(2, 1),
            fields,
            &b,
            DvdRasterLimits::default(),
        )
        .unwrap();
        assert!(job.poll(&mut WorkBudget::new(1000), &cancel).is_err());
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn work_limit_error_and_consumption_are_independent_of_quantum() {
    let (raw, fields) = fixture(&[0, 3], &[], 64, 1);
    for quantum in [1, 3, 65536] {
        let b = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let limits = DvdRasterLimits {
            max_work: 10,
            ..Default::default()
        };
        let mut job = DvdRasterJob::new(
            Bytes::copy_from_slice(&b, &raw).unwrap(),
            area(64, 1),
            fields,
            &b,
            limits,
        )
        .unwrap();
        loop {
            match job.poll(&mut WorkBudget::new(quantum), &cancel) {
                Ok(DvdRasterStatus::Yield) => (),
                Err(error) => {
                    assert_eq!(
                        error,
                        Error::Limit {
                            requested: 11,
                            maximum: 10
                        }
                    );
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(
            (job.work_spent(), job.nibbles_read(), b.used()),
            (10, 4, Budget::control_storage_bytes())
        );
    }
}

#[test]
fn field_positions_dimensions_pool_and_capacity_are_checked_before_allocation() {
    let (raw, fields) = fixture(&[0x40], &[], 1, 1);
    let b = Budget::new(4096).unwrap();
    let other = Budget::new(4096).unwrap();
    let foreign = Bytes::copy_from_slice(&other, &[]).unwrap();
    assert_eq!(
        DvdRasterJob::new(foreign, area(1, 1), fields, &b, DvdRasterLimits::default()).unwrap_err(),
        Error::ForeignPool
    );
    for fields in [
        FieldOffsets { top: 3, bottom: 5 },
        FieldOffsets { top: 5, bottom: 5 },
        FieldOffsets {
            top: 4,
            bottom: u16::MAX,
        },
    ] {
        assert!(
            DvdRasterJob::new(
                Bytes::copy_from_slice(&b, &raw).unwrap(),
                area(1, 1),
                fields,
                &b,
                DvdRasterLimits::default()
            )
            .is_err()
        );
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
    for limits in [
        DvdRasterLimits {
            max_axis: 0,
            ..Default::default()
        },
        DvdRasterLimits {
            max_pixels: 0,
            ..Default::default()
        },
    ] {
        assert!(matches!(
            DvdRasterJob::new(
                Bytes::copy_from_slice(&b, &raw).unwrap(),
                area(1, 1),
                fields,
                &b,
                limits
            ),
            Err(Error::Limit { .. })
        ));
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn wire_control_words_geometry_and_raw_dates_are_retained_without_allocating() {
    let (raw, fields) = fixture(&[0x90], &[0xa0], 2, 2);
    let packet = DvdPacket::parse(&raw).unwrap();
    assert_eq!(packet.raw().as_ptr(), raw.as_ptr());
    assert_eq!(packet.pixel_data(), [0x90, 0xa0]);
    let controls = packet
        .controls(10, 10)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(controls.len(), 2);
    assert_eq!(controls[1].date, DvdDate(90));
    assert_eq!(controls[1].date.ticks_90k(), 92160);
    let commands = controls[0]
        .commands()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        commands,
        [
            Command::Start,
            Command::Palette(0x3210),
            Command::Alpha(0xffff),
            Command::Area(area(2, 2)),
            Command::Fields(fields),
            Command::End
        ]
    );
    assert_eq!(
        controls[1]
            .commands()
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        [Command::Stop, Command::End]
    );
}

#[test]
fn packet_truncation_links_and_command_limits_have_no_scan_ahead() {
    let (raw, _) = fixture(&[0x40], &[], 1, 1);
    for end in 0..raw.len() {
        assert!(DvdPacket::parse(&raw[..end]).is_err());
    }
    let mut tail = raw.clone();
    tail.push(0);
    assert!(DvdPacket::parse(&tail).is_err());
    let packet = DvdPacket::parse(&raw).unwrap();
    assert!(matches!(
        packet.controls(0, 10).next().unwrap(),
        Err(Error::Limit { .. })
    ));
    let first = packet.controls(10, 1).next().unwrap().unwrap();
    let mut commands = first.commands();
    assert_eq!(commands.next().unwrap().unwrap(), Command::Start);
    assert!(matches!(commands.next().unwrap(), Err(Error::Limit { .. })));
    assert!(commands.next().is_none());
    let mut broken = raw.clone();
    let at = packet.control_offset();
    broken[at + 2..at + 4].copy_from_slice(&4_u16.to_be_bytes());
    let mut controls = DvdPacket::parse(&broken).unwrap().controls(10, 10);
    assert!(controls.next().unwrap().is_err());
    assert!(controls.next().is_none());
}

#[test]
fn unknown_command_retains_the_unparsed_remainder_and_forced_start_is_distinct() {
    let (mut raw, _) = fixture(&[0x40], &[], 1, 1);
    let at = DvdPacket::parse(&raw).unwrap().control_offset() + 4;
    raw[at] = 0;
    assert_eq!(
        DvdPacket::parse(&raw)
            .unwrap()
            .controls(10, 10)
            .next()
            .unwrap()
            .unwrap()
            .commands()
            .next()
            .unwrap()
            .unwrap(),
        Command::ForcedStart
    );
    raw[at] = 7;
    let control = DvdPacket::parse(&raw)
        .unwrap()
        .controls(10, 10)
        .next()
        .unwrap()
        .unwrap();
    let mut commands = control.commands();
    match commands.next().unwrap().unwrap() {
        Command::Opaque {
            opcode: 7,
            remainder,
        } => assert_eq!(remainder.as_ptr(), raw[at + 1..].as_ptr()),
        other => panic!("{other:?}"),
    }
    assert!(commands.next().is_none());
}
