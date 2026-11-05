use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::dvd::{Canvas, ConfigJob, ConfigLimits, ConfigStatus, VobSubConfig};

fn palette() -> &'static [u8] {
    b"palette: 000000,112233,223344,334455,445566,556677,667788,778899,8899aa,99aabb,aabbcc,bbccdd,ccddee,ddeeff,eeff00,ff0011"
}

fn parse(input: &[u8], quantum: u64) -> (VobSubConfig, Budget, usize) {
    let budget = Budget::new(1024 * 1024).unwrap();
    let bytes = Bytes::copy_from_slice(&budget, input).unwrap();
    let pointer = bytes.as_ptr();
    let mut job = ConfigJob::new(bytes, &budget, ConfigLimits::default()).unwrap();
    let cancel = AtomicBool::new(false);
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(quantum);
        let status = job.poll(&mut work, &cancel).unwrap();
        assert!(work.spent() <= quantum);
        if status == ConfigStatus::Ready {
            assert_eq!(job.bytes_consumed(), input.len());
            let spent = job.work_spent();
            let result = job.try_finish(&cancel).unwrap();
            drop(job);
            assert_eq!(result.raw().as_ptr(), pointer);
            assert_eq!(result.raw().as_ref(), input);
            assert_eq!(
                budget.used(),
                Budget::control_storage_bytes() + result.raw().retained_storage_bytes()
            );
            return (result, budget, spent);
        }
    }
    panic!("bounded configuration did not finish");
}

#[test]
fn canonical_fields_and_source_retention_are_quantum_independent() {
    let mut input = b"# authored control\r\nsize: 720x576\r\n".to_vec();
    input.extend(palette());
    input.extend(b"\r\ncustom colors: OFF, tridx: 0000, colors: 0,0,0,0\r\n");
    for quantum in [1, 2, 17, 65536] {
        let (config, budget, work) = parse(&input, quantum);
        assert_eq!(
            config.canvas,
            Some(Canvas {
                width: 720,
                height: 576
            })
        );
        assert_eq!(config.custom_colours, Some(false));
        assert_eq!(
            (
                config.size_assignments,
                config.palette_assignments,
                config.custom_assignments
            ),
            (1, 1, 1)
        );
        assert_eq!(config.palette.unwrap()[15], 0xff0011);
        assert_eq!(work, input.len() + 1);
        drop(config);
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn packed_word_view_has_exact_alpha_and_preserves_transparent_rgb() {
    let (config, _, _) = parse(palette(), 1);
    assert_eq!(
        config.rgba_lsb(0x3210, 0x8421).unwrap(),
        [
            [0, 0, 0, 17],
            [0x11, 0x22, 0x33, 34],
            [0x22, 0x33, 0x44, 68],
            [0x33, 0x44, 0x55, 136],
        ]
    );
    assert_eq!(
        config.rgba_lsb(0x0123, 0).unwrap(),
        [
            [0x33, 0x44, 0x55, 0],
            [0x22, 0x33, 0x44, 0],
            [0x11, 0x22, 0x33, 0],
            [0, 0, 0, 0],
        ]
    );
}

#[test]
fn explicit_hex_prefix_and_continuations_preserve_the_same_words() {
    let input=b" palette: 0x000000,0X112233,223344,334455,445566,556677,667788,\\\r\n  778899,8899AA,99AABB,AABBCC,BBCCDD,CCDDEE,DDEEFF,EEFF00,FF0011\r\n";
    let (config, _, work) = parse(input, 1);
    let (plain, _, _) = parse(palette(), 65536);
    assert_eq!(config.palette, plain.palette);
    assert_eq!(work, input.len() + 1);
}

#[test]
fn later_assignments_and_all_original_lines_remain_observable() {
    let mut input =
        b"size:1x2\nsize: 1920 x 1080\ncustom colors: ON, original remainder\ncustom colors: off\n"
            .to_vec();
    input.extend(palette());
    input.push(b'\n');
    input.extend(palette());
    let (config, _, _) = parse(&input, 3);
    assert_eq!(
        config.canvas,
        Some(Canvas {
            width: 1920,
            height: 1080
        })
    );
    assert_eq!(config.custom_colours, Some(false));
    assert_eq!(
        (
            config.size_assignments,
            config.palette_assignments,
            config.custom_assignments
        ),
        (2, 2, 2)
    );
    assert_eq!(config.raw().as_ref(), input);
}

#[test]
fn unrelated_settings_and_codecprivate_timestamps_are_not_applied() {
    let input=b"timestamp: 99:00:00:000, filepos: ffff\ntime offset: -999999\nalpha: 3%\n\xffopaque\0source\n";
    let (config, _, _) = parse(input, 1);
    assert_eq!(config.canvas, None);
    assert_eq!(config.palette, None);
    assert_eq!(config.custom_colours, None);
    assert_eq!(config.rgba_lsb(0xffff, 0xffff), None);
    assert_eq!(config.raw().as_ref(), input);
}

#[test]
fn empty_and_unterminated_unknown_lines_are_valid_missing_facts() {
    for input in [&b""[..], &b"# no palette"[..], &b"palett"[..], &b"size"[..]] {
        let (config, budget, work) = parse(input, 1);
        assert_eq!((config.canvas, config.palette), (None, None));
        assert_eq!(work, input.len() + 1);
        drop(config);
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn zero_work_does_not_read_or_change_allocations() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let bytes = Bytes::copy_from_slice(&budget, b"size:1x1").unwrap();
    let used = budget.used();
    let mut job = ConfigJob::new(bytes, &budget, ConfigLimits::default()).unwrap();
    for _ in 0..4 {
        assert_eq!(
            job.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
            ConfigStatus::Yield
        );
        assert_eq!(
            (job.bytes_consumed(), job.work_spent(), budget.used()),
            (0, 0, used)
        );
    }
}

#[test]
fn cancellation_is_sticky_and_releases_input_before_and_after_ready() {
    for quantum in [1, 65536] {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut job = ConfigJob::new(
            Bytes::copy_from_slice(&budget, b"size:1x1").unwrap(),
            &budget,
            ConfigLimits::default(),
        )
        .unwrap();
        job.poll(&mut WorkBudget::new(quantum), &cancel).unwrap();
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(job.try_finish(&cancel).unwrap_err(), Error::Cancelled);
        assert_eq!(budget.used(), Budget::control_storage_bytes());
        cancel.store(false, Ordering::Relaxed);
        assert_eq!(
            job.poll(&mut WorkBudget::new(100), &cancel).unwrap_err(),
            Error::Cancelled
        );
    }
}

#[test]
fn malformed_recognized_fields_fail_stickily_without_partial_publication() {
    let mut inputs = vec![
        b"size:1".to_vec(),
        b"size:1x".to_vec(),
        b"size:1x2x3".to_vec(),
        b"size:1x2 3".to_vec(),
        b"palette:".to_vec(),
        b"custom colors: MAYBE".to_vec(),
    ];
    let mut too_few = palette().to_vec();
    too_few.truncate(too_few.len() - 7);
    inputs.push(too_few);
    let mut extra = palette().to_vec();
    extra.extend(b",ffffff");
    inputs.push(extra);
    let mut comma = palette().to_vec();
    comma.push(b',');
    inputs.push(comma);
    let mut continuation = palette().to_vec();
    continuation.extend(b",\\");
    inputs.push(continuation);
    for input in inputs {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut job = ConfigJob::new(
            Bytes::copy_from_slice(&budget, &input).unwrap(),
            &budget,
            ConfigLimits::default(),
        )
        .unwrap();
        let error = loop {
            match job.poll(&mut WorkBudget::new(1), &cancel) {
                Ok(ConfigStatus::Yield) => (),
                Err(e) => break e,
                other => panic!("unexpected {other:?}"),
            }
        };
        assert_eq!(job.try_finish(&cancel).unwrap_err(), error);
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn pool_byte_canvas_and_work_limits_remain_explicit() {
    let budget = Budget::new(4096).unwrap();
    let other = Budget::new(4096).unwrap();
    assert_eq!(
        ConfigJob::new(
            Bytes::copy_from_slice(&other, b"").unwrap(),
            &budget,
            ConfigLimits::default()
        )
        .unwrap_err(),
        Error::ForeignPool
    );
    let limits = ConfigLimits {
        max_bytes: 1,
        ..Default::default()
    };
    assert!(matches!(
        ConfigJob::new(
            Bytes::copy_from_slice(&budget, b"xx").unwrap(),
            &budget,
            limits
        ),
        Err(Error::Limit {
            requested: 2,
            maximum: 1
        })
    ));
    for quantum in [1, 65536] {
        let limits = ConfigLimits {
            max_work: 6,
            ..Default::default()
        };
        let cancel = AtomicBool::new(false);
        let mut job = ConfigJob::new(
            Bytes::copy_from_slice(&budget, b"size:1x1").unwrap(),
            &budget,
            limits,
        )
        .unwrap();
        loop {
            match job.poll(&mut WorkBudget::new(quantum), &cancel) {
                Ok(ConfigStatus::Yield) => (),
                Err(e) => {
                    assert_eq!(
                        e,
                        Error::Limit {
                            requested: 7,
                            maximum: 6
                        }
                    );
                    break;
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!((job.bytes_consumed(), job.work_spent()), (6, 6));
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
    let limits = ConfigLimits {
        max_canvas_axis: 10,
        ..Default::default()
    };
    let cancel = AtomicBool::new(false);
    let mut job = ConfigJob::new(
        Bytes::copy_from_slice(&budget, b"size:11x1").unwrap(),
        &budget,
        limits,
    )
    .unwrap();
    assert!(matches!(
        job.poll(&mut WorkBudget::new(100), &cancel),
        Err(Error::Limit {
            requested: 11,
            maximum: 10
        })
    ));
}

#[test]
fn long_whitespace_is_charged_without_line_materialization() {
    let mut input = b"size:".to_vec();
    input.resize(20000, b' ');
    input.extend(b"1x2");
    let (config, budget, work) = parse(&input, 1);
    assert_eq!(
        config.canvas,
        Some(Canvas {
            width: 1,
            height: 2
        })
    );
    assert_eq!(work, input.len() + 1);
    assert_eq!(budget.peak(), budget.used());
}
