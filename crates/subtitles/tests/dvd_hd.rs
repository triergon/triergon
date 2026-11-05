use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::dvd::{HdCommand, HdLimits, HdPacket, HdSpuJob, HdStatus, native_hd_argb};
const RAW: &[u8] = include_bytes!("hd-source-fixtures/hd8-all-256-indices.spu");
fn ready(job: &mut HdSpuJob, q: u64, cancel: &AtomicBool) {
    for _ in 0..100000 {
        if job.poll(&mut WorkBudget::new(q), cancel).unwrap() == HdStatus::Ready {
            return;
        }
    }
    panic!("bounded job did not finish");
}
fn job(raw: &[u8], budget: &Budget, limits: HdLimits) -> HdSpuJob {
    HdSpuJob::new(Bytes::copy_from_slice(budget, raw).unwrap(), budget, limits).unwrap()
}

#[test]
fn zero_work_does_not_allocate_or_advance_source_or_raster() {
    let b = Budget::new(100000).unwrap();
    let c = AtomicBool::new(false);
    let mut j = job(RAW, &b, HdLimits::default());
    let before = b.used();
    for _ in 0..3 {
        assert_eq!(
            j.poll(&mut WorkBudget::new(0), &c).unwrap(),
            HdStatus::Yield
        );
        assert_eq!(j.work_spent(), 0);
        assert_eq!(b.used(), before);
    }
}
#[test]
fn pressure_retry_and_zero_work_preserve_paid_scan() {
    let b = Budget::new(100000).unwrap();
    let c = AtomicBool::new(false);
    let mut j = job(RAW, &b, HdLimits::default());
    let blocker = b.reserve(b.available()).unwrap();
    assert!(matches!(
        j.poll(&mut WorkBudget::new(100000), &c),
        Err(Error::Backpressure { .. })
    ));
    let spent = j.work_spent();
    assert!(spent > 1000);
    let used = b.used();
    assert_eq!(
        j.poll(&mut WorkBudget::new(0), &c).unwrap(),
        HdStatus::Yield
    );
    assert_eq!((j.work_spent(), b.used()), (spent, used));
    drop(blocker);
    ready(&mut j, 1, &c);
    assert_eq!(j.try_finish(&c).unwrap().bitmap().pixels().len(), 512);
}
#[test]
fn freeze_pressure_keeps_exact_buffer_and_work() {
    let b = Budget::new(100000).unwrap();
    let c = AtomicBool::new(false);
    let mut j = job(RAW, &b, HdLimits::default());
    ready(&mut j, 7, &c);
    let spent = j.work_spent();
    let blocker = b.reserve(b.available()).unwrap();
    assert!(matches!(j.try_finish(&c), Err(Error::Backpressure { .. })));
    drop(blocker);
    let result = j.try_finish(&c).unwrap();
    assert_eq!(j.work_spent(), spent);
    assert_eq!(
        &result.bitmap().pixels()[..256],
        &(0..=255).collect::<Vec<u8>>()
    );
}
#[test]
fn cancel_is_sticky_before_scan_after_yield_and_at_ready() {
    for q in [0, 1, 100000] {
        let b = Budget::new(100000).unwrap();
        let c = AtomicBool::new(false);
        let mut j = job(RAW, &b, HdLimits::default());
        j.poll(&mut WorkBudget::new(q), &c).unwrap();
        c.store(true, Ordering::Relaxed);
        assert_eq!(j.try_finish(&c).unwrap_err(), Error::Cancelled);
        assert!(j.source().is_none());
        assert_eq!(b.used(), Budget::control_storage_bytes());
        c.store(false, Ordering::Relaxed);
        assert_eq!(
            j.poll(&mut WorkBudget::new(100000), &c).unwrap_err(),
            Error::Cancelled
        );
    }
}
#[test]
fn returned_output_remains_valid_after_a_later_cancelled_job() {
    let b = Budget::new(100000).unwrap();
    let c = AtomicBool::new(false);
    let mut j = job(RAW, &b, HdLimits::default());
    ready(&mut j, 31, &c);
    let p = j.try_finish(&c).unwrap();
    drop(j);
    let mut next = job(RAW, &b, HdLimits::default());
    c.store(true, Ordering::Relaxed);
    assert_eq!(
        next.poll(&mut WorkBudget::new(100), &c).unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(p.bitmap().pixels()[255], 255);
    assert_eq!(p.packet().as_ref(), RAW);
}
#[test]
fn work_limit_is_exact_quantum_independent_and_retains_error_source() {
    for q in [1, 3, 4096] {
        let b = Budget::new(100000).unwrap();
        let c = AtomicBool::new(false);
        let mut j = job(
            RAW,
            &b,
            HdLimits {
                max_work: 19,
                ..Default::default()
            },
        );
        loop {
            match j.poll(&mut WorkBudget::new(q), &c) {
                Ok(HdStatus::Yield) => (),
                Err(e) => {
                    assert_eq!(
                        e,
                        Error::Limit {
                            requested: 20,
                            maximum: 19
                        }
                    );
                    break;
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(j.work_spent(), 19);
        assert_eq!(j.source().unwrap().as_ref(), RAW);
        assert_eq!(
            j.poll(&mut WorkBudget::new(999), &c).unwrap_err(),
            Error::Limit {
                requested: 20,
                maximum: 19
            }
        );
        drop(j);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}
#[test]
fn foreign_pool_rejected_before_any_parse() {
    let b = Budget::new(4096).unwrap();
    let other = Budget::new(4096).unwrap();
    assert_eq!(
        HdSpuJob::new(
            Bytes::copy_from_slice(&other, &[]).unwrap(),
            &b,
            HdLimits::default()
        )
        .unwrap_err(),
        Error::ForeignPool
    );
}
#[test]
fn headers_are_exact_and_every_truncated_prefix_is_refused() {
    for end in 0..RAW.len() {
        assert!(HdPacket::parse(&RAW[..end]).is_err());
    }
    let mut extra = RAW.to_vec();
    extra.push(0);
    assert!(HdPacket::parse(&extra).is_err());
    let packet = HdPacket::parse(RAW).unwrap();
    assert!(packet.wide_header());
    assert_eq!(packet.header_bytes(), 10);
    assert_eq!(packet.raw().as_ptr(), RAW.as_ptr());
}
#[test]
fn raw_command_facts_and_offsets_are_zero_copy() {
    let p = HdPacket::parse(RAW).unwrap();
    let blocks = p.controls(4, 32).collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[1].date.0, 90);
    let cmds = blocks[0].commands().collect::<Result<Vec<_>, _>>().unwrap();
    assert!(matches!(cmds[0], HdCommand::Start));
    if let HdCommand::YCrCb(v) = cmds[1] {
        assert_eq!(v.len(), 768);
        assert!(v.as_ptr() >= RAW.as_ptr());
    } else {
        panic!();
    }
    if let HdCommand::Contrast(v) = cmds[2] {
        assert_eq!(v[0], 255);
        assert_eq!(v[255], 0);
    } else {
        panic!();
    }
    assert!(matches!(
        cmds[3],
        HdCommand::Area {
            eight_bit_signal: true,
            ..
        }
    ));
    assert!(matches!(cmds[4],HdCommand::Fields(v) if v.operand_bits==32));
}
#[test]
fn unknown_operand_tail_is_not_reparsed_as_known_commands() {
    let mut raw = RAW.to_vec();
    let p = HdPacket::parse(&raw).unwrap();
    let at = p.control_offset() + 6;
    raw[at] = 0x7e;
    let block = HdPacket::parse(&raw)
        .unwrap()
        .controls(2, 32)
        .next()
        .unwrap()
        .unwrap();
    let mut cmds = block.commands();
    assert!(matches!(
        cmds.next().unwrap().unwrap(),
        HdCommand::Opaque { opcode: 0x7e, .. }
    ));
    assert!(cmds.next().is_none());
    let b = Budget::new(100000).unwrap();
    let c = AtomicBool::new(false);
    let mut j = job(&raw, &b, HdLimits::default());
    assert_eq!(
        j.poll(&mut WorkBudget::new(100000), &c).unwrap_err(),
        Error::Unsupported("HD opaque control")
    );
    assert_eq!(j.source().unwrap().as_ref(), raw);
}
#[test]
fn dimensions_control_command_and_packet_limits_are_typed() {
    for limits in [
        HdLimits {
            max_controls: 0,
            ..Default::default()
        },
        HdLimits {
            max_commands: 0,
            ..Default::default()
        },
        HdLimits {
            max_axis: 1,
            ..Default::default()
        },
        HdLimits {
            max_pixels: 1,
            ..Default::default()
        },
    ] {
        let b = Budget::new(100000).unwrap();
        let c = AtomicBool::new(false);
        let mut j = job(RAW, &b, limits);
        assert!(matches!(
            j.poll(&mut WorkBudget::new(100000), &c),
            Err(Error::Limit { .. })
        ));
        assert_eq!(j.source().unwrap().as_ref(), RAW);
    }
    let b = Budget::new(100000).unwrap();
    assert!(matches!(
        HdSpuJob::new(
            Bytes::copy_from_slice(&b, RAW).unwrap(),
            &b,
            HdLimits {
                max_packet: 1,
                ..Default::default()
            }
        ),
        Err(Error::Limit { .. })
    ));
}
#[test]
fn malformed_field_offsets_links_and_reversed_geometry_fail_without_pixel_allocation() {
    let parsed = HdPacket::parse(RAW).unwrap();
    let control = parsed.control_offset();
    for (at, data) in [
        (control + 2, vec![255; 4]),
        (control + 6 + 1 + 769 + 257 + 7 + 1, vec![255; 8]),
        (
            control + 6 + 1 + 769 + 257 + 1,
            vec![0xff, 0xf0, 0, 0xff, 0xf0, 0],
        ),
    ] {
        let mut raw = RAW.to_vec();
        raw[at..at + data.len()].copy_from_slice(&data);
        let b = Budget::new(100000).unwrap();
        let c = AtomicBool::new(false);
        let mut j = job(&raw, &b, HdLimits::default());
        assert!(j.poll(&mut WorkBudget::new(100000), &c).is_err());
        assert_eq!(
            b.used(),
            Budget::control_storage_bytes() + j.source().unwrap().retained_storage_bytes()
        );
    }
}
#[test]
fn absent_palette_or_contrast_is_source_unknown_not_fabricated() {
    for raw in [
        include_bytes!("hd-source-fixtures/hd8-missing-YCrCb-palette.spu").as_slice(),
        include_bytes!("hd-source-fixtures/hd8-missing-contrast.spu").as_slice(),
    ] {
        let b = Budget::new(100000).unwrap();
        let c = AtomicBool::new(false);
        let mut j = job(raw, &b, HdLimits::default());
        ready(&mut j, 1, &c);
        let p = j.try_finish(&c).unwrap();
        assert!(p.ycrcb().is_none() || p.contrast().is_none());
        assert!(matches!(p.palette_job(), Err(Error::Unsupported(_))));
        assert_eq!(p.packet().as_ref(), raw);
    }
}
#[test]
fn palette_work_is_quantum_bounded_and_raw_contrast_is_unchanged() {
    let b = Budget::new(100000).unwrap();
    let c = AtomicBool::new(false);
    let mut j = job(RAW, &b, HdLimits::default());
    ready(&mut j, 4096, &c);
    let p = j.try_finish(&c).unwrap();
    for q in [1, 3, 4096] {
        let mut palette = p.palette_job().unwrap();
        assert_eq!(
            palette.poll(&mut WorkBudget::new(0), &c).unwrap(),
            HdStatus::Yield
        );
        assert_eq!(palette.entries_done(), 0);
        while palette.poll(&mut WorkBudget::new(q), &c).unwrap() != HdStatus::Ready {}
        let words = palette.try_finish(&c).unwrap();
        assert_eq!(words[0] >> 24, 0);
        assert_eq!(words[255] >> 24, 255);
        assert_eq!(p.contrast().unwrap()[0], 255);
    }
}
#[test]
fn palette_cancel_is_sticky_at_partial_and_ready() {
    let b = Budget::new(100000).unwrap();
    let c = AtomicBool::new(false);
    let mut j = job(RAW, &b, HdLimits::default());
    ready(&mut j, 4096, &c);
    let p = j.try_finish(&c).unwrap();
    for q in [1, 4096] {
        let mut palette = p.palette_job().unwrap();
        palette.poll(&mut WorkBudget::new(q), &c).unwrap();
        c.store(true, Ordering::Relaxed);
        assert_eq!(palette.try_finish(&c).unwrap_err(), Error::Cancelled);
        c.store(false, Ordering::Relaxed);
        assert_eq!(
            palette.poll(&mut WorkBudget::new(4096), &c).unwrap_err(),
            Error::Cancelled
        );
    }
}
#[test]
fn contrast_alpha_endpoints_and_neutral_luma_are_separate_source_math() {
    assert_eq!(native_hd_argb([16, 128, 128], 255), 0);
    assert_eq!(native_hd_argb([235, 128, 128], 0), 0xffff_ffff);
    assert_eq!(native_hd_argb([0, 128, 128], 0), 0xff00_0000);
    assert_eq!(native_hd_argb([255, 128, 128], 0), 0xffff_ffff);
}
#[test]
fn wide_header_does_not_imply_eight_bit_mode() {
    let raw = include_bytes!("hd-source-fixtures/hd-header-two-bit-pixels.spu");
    let b = Budget::new(100000).unwrap();
    let c = AtomicBool::new(false);
    let mut j = job(raw, &b, HdLimits::default());
    assert!(matches!(
        j.poll(&mut WorkBudget::new(100000), &c),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn all_four_extended_operand_extents_reject_truncation_stickily_at_every_quantum() {
    let cases: &[&[u8]] = &[
        include_bytes!("hd-additive-fixtures/truncated-83-0.spu"),
        include_bytes!("hd-additive-fixtures/truncated-83-767.spu"),
        include_bytes!("hd-additive-fixtures/truncated-84-0.spu"),
        include_bytes!("hd-additive-fixtures/truncated-84-255.spu"),
        include_bytes!("hd-additive-fixtures/truncated-85-0.spu"),
        include_bytes!("hd-additive-fixtures/truncated-85-5.spu"),
        include_bytes!("hd-additive-fixtures/truncated-86-0.spu"),
        include_bytes!("hd-additive-fixtures/truncated-86-7.spu"),
    ];
    for raw in cases {
        for q in [1, 2, 3, 7, 4096] {
            let b = Budget::new(100000).unwrap();
            let c = AtomicBool::new(false);
            let mut j = job(raw, &b, HdLimits::default());
            loop {
                match j.poll(&mut WorkBudget::new(q), &c) {
                    Ok(HdStatus::Yield) => (),
                    Err(e) => {
                        assert_eq!(e, Error::Truncated);
                        break;
                    }
                    v => panic!("{v:?}"),
                }
            }
            assert_eq!(
                j.poll(&mut WorkBudget::new(4096), &c).unwrap_err(),
                Error::Truncated
            );
            assert_eq!(j.source().unwrap().as_ref(), *raw);
        }
    }
}
#[test]
fn header_width_field_operand_width_and_mode_are_independent() {
    for (raw, wide, bits) in [
        (
            include_bytes!("hd-additive-fixtures/traditional-header-extended-commands.spu")
                .as_slice(),
            false,
            32,
        ),
        (
            include_bytes!("hd-additive-fixtures/wide-header-short-field-command.spu").as_slice(),
            true,
            16,
        ),
    ] {
        assert_eq!(HdPacket::parse(raw).unwrap().wide_header(), wide);
        let b = Budget::new(100000).unwrap();
        let c = AtomicBool::new(false);
        let mut j = job(raw, &b, HdLimits::default());
        ready(&mut j, 1, &c);
        let p = j.try_finish(&c).unwrap();
        assert_eq!(p.facts().fields.unwrap().operand_bits, bits);
        assert_eq!(p.bitmap().pixels().as_ref(), [3, 3, 3, 3]);
    }
}
#[test]
fn mode_is_sticky_after_a_later_non_signalling_area_command() {
    let raw = include_bytes!("hd-additive-fixtures/mode-sticky-after-05.spu");
    let b = Budget::new(100000).unwrap();
    let c = AtomicBool::new(false);
    let mut j = job(raw, &b, HdLimits::default());
    ready(&mut j, 1, &c);
    let p = j.try_finish(&c).unwrap();
    assert!(p.facts().eight_bit);
    assert_eq!(p.bitmap().pixels().as_ref(), [3, 3, 3, 3]);
}
#[test]
fn palette_commands_alone_do_not_enable_eight_bit_mode() {
    let raw = include_bytes!("hd-additive-fixtures/palette-not-mode.spu");
    let b = Budget::new(100000).unwrap();
    let c = AtomicBool::new(false);
    let mut j = job(raw, &b, HdLimits::default());
    assert_eq!(
        j.poll(&mut WorkBudget::new(100000), &c).unwrap_err(),
        Error::Unsupported("HD two-bit source raster")
    );
}
#[test]
fn strict_overshoot_rejection_keeps_source_and_releases_partial_pixels() {
    let raw = include_bytes!("hd-additive-fixtures/wide-rle-short-overshoot.spu");
    for q in [1, 3, 4096] {
        let b = Budget::new(100000).unwrap();
        let c = AtomicBool::new(false);
        let mut j = job(raw, &b, HdLimits::default());
        loop {
            match j.poll(&mut WorkBudget::new(q), &c) {
                Ok(HdStatus::Yield) => (),
                Err(e) => {
                    assert_eq!(e, Error::Invalid("HD run exceeds source row"));
                    break;
                }
                v => panic!("{v:?}"),
            }
        }
        assert_eq!(
            b.used(),
            Budget::control_storage_bytes() + j.source().unwrap().retained_storage_bytes()
        );
    }
}
#[test]
fn odd_fields_align_each_row_and_fill_stops_at_remainder() {
    for (raw, expected) in [
        (
            include_bytes!("hd-additive-fixtures/wide-odd-fields-five-rows.spu").as_slice(),
            vec![3; 5],
        ),
        (
            include_bytes!("hd-additive-fixtures/wide-fill-after-prefix.spu").as_slice(),
            vec![255, 3, 3, 3],
        ),
    ] {
        for q in 1..=16 {
            let b = Budget::new(100000).unwrap();
            let c = AtomicBool::new(false);
            let mut j = job(raw, &b, HdLimits::default());
            ready(&mut j, q, &c);
            assert_eq!(
                j.try_finish(&c).unwrap().bitmap().pixels().as_ref(),
                expected
            );
        }
    }
}
#[test]
fn palette_and_contrast_replacement_select_new_span_without_losing_earlier_raw_facts() {
    for raw in [
        include_bytes!("hd-additive-fixtures/palette-replacement.spu").as_slice(),
        include_bytes!("hd-additive-fixtures/contrast-replacement.spu").as_slice(),
    ] {
        let b = Budget::new(100000).unwrap();
        let c = AtomicBool::new(false);
        let mut j = job(raw, &b, HdLimits::default());
        ready(&mut j, 1, &c);
        let p = j.try_finish(&c).unwrap();
        assert_eq!(p.ycrcb().unwrap()[0], 16);
        assert_eq!(p.contrast().unwrap()[1], 254);
        assert_eq!(p.packet().as_ref(), raw);
        let commands = HdPacket::parse(raw)
            .unwrap()
            .controls(4, 32)
            .next()
            .unwrap()
            .unwrap()
            .commands()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            commands
                .iter()
                .filter(|c| matches!(c, HdCommand::YCrCb(_) | HdCommand::Contrast(_)))
                .count(),
            3
        );
    }
}

#[test]
fn actual_wide_sizes_control_links_and_field_addresses_exceed_u16_without_narrowing() {
    for (raw, height) in [
        (
            include_bytes!("hd-additive-fixtures/wide-actual-field65552-height1.spu").as_slice(),
            1,
        ),
        (
            include_bytes!("hd-additive-fixtures/wide-actual-field65552-height2.spu").as_slice(),
            2,
        ),
    ] {
        let packet = HdPacket::parse(raw).unwrap();
        assert!(packet.control_offset() > 65535);
        assert_eq!(packet.raw().len(), raw.len());
        for q in [1, 7, 4096] {
            let b = Budget::new(100000).unwrap();
            let c = AtomicBool::new(false);
            let mut j = job(raw, &b, HdLimits::default());
            ready(&mut j, q, &c);
            let p = j.try_finish(&c).unwrap();
            assert_eq!(p.facts().fields.unwrap().top, 65552);
            assert_eq!(p.facts().fields.unwrap().bottom, 65553);
            assert_eq!(p.bitmap().height, height);
            assert_eq!(
                p.bitmap().pixels().as_ref(),
                vec![3; 2 * usize::from(height)]
            );
        }
    }
}
