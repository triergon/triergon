use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, TimeBase, WorkBudget};
use triergon_pixel::{ChannelRounding, YcbcrMatrix};
use triergon_subtitles::compat::pgs::{DecodedPacket, Decoder, Limits, Options, Refusal, Status};
use triergon_subtitles::pgs::Clock;

fn stamp() -> Clock {
    Clock {
        pts: Some(90_000),
        dts: None,
        timebase: TimeBase::new(1, 90_000).unwrap(),
    }
}
fn segment(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = vec![kind];
    v.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    v.extend_from_slice(payload);
    v
}
fn pcs(state: u8, crop: bool) -> Vec<u8> {
    let mut v = vec![
        0,
        8,
        0,
        4,
        0x10,
        0,
        1,
        state,
        0,
        0,
        1,
        0,
        7,
        0,
        if crop { 0x80 } else { 0 },
        0,
        1,
        0,
        2,
    ];
    if crop {
        v.extend_from_slice(&[0, 7, 0, 0, 0, 8, 0, 1]);
    }
    segment(0x16, &v)
}
fn palette() -> Vec<u8> {
    segment(0x14, &[0, 0, 1, 235, 128, 128, 255, 2, 16, 128, 128, 128])
}
fn ods(flags: u8, declared: u8, data: &[u8]) -> Vec<u8> {
    let mut v = vec![0, 7, 0, flags];
    if flags & 0x80 != 0 {
        v.extend_from_slice(&[0, 0, declared, 0, 2, 0, 1]);
    }
    v.extend_from_slice(data);
    segment(0x15, &v)
}
fn packet_with(data: &[u8], flags: u8, declared: u8, end: &[u8], crop: bool) -> Vec<u8> {
    [
        pcs(0x80, crop),
        palette(),
        ods(flags, declared, data),
        segment(0x80, end),
    ]
    .concat()
}
fn ready(decoder: &mut Decoder, quantum: u64) -> DecodedPacket {
    let cancel = AtomicBool::new(false);
    for _ in 0..1_000_000 {
        let mut work = WorkBudget::new(quantum);
        let status = decoder.poll(&mut work, &cancel).unwrap();
        assert!(work.spent() <= quantum);
        if status == Status::Ready {
            return decoder.take_output(&cancel).unwrap();
        }
    }
    panic!("bounded receiver did not finish");
}
fn run(raw: &[u8], quantum: u64) -> DecodedPacket {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = Decoder::new(&b, Options::default(), Limits::default()).unwrap();
    let bytes = Bytes::copy_from_slice(&b, raw).unwrap();
    d.push(bytes, &stamp(), &AtomicBool::new(false)).unwrap();
    ready(&mut d, quantum)
}

#[test]
fn an_end_segment_payload_is_ignored_and_the_raw_packet_kept() {
    let raw = packet_with(&[1, 2, 0, 0], 0xc0, 8, &[1], false);
    let out = run(&raw, 1);
    assert_eq!(out.raw_packet().as_ref(), raw);
    assert_eq!(
        out.subtitle().unwrap().rectangles()[0].pixels().as_ref(),
        [1, 2]
    );
}
#[test]
fn an_end_without_a_presentation_presents_no_clear() {
    let raw = segment(0x80, &[]);
    let out = run(&raw, 1);
    assert_eq!(out.canvas, [0, 0]);
    let event = out.subtitle().unwrap();
    assert_eq!(event.pts_microseconds, Some(0));
    assert!(event.rectangles().is_empty());
}
#[test]
fn receiver_uses_actual_complete_assembly_without_requiring_last_marker() {
    let raw = packet_with(&[1, 2], 0x80, 6, &[], false);
    let out = run(&raw, 1);
    assert_eq!(
        out.subtitle().unwrap().rectangles()[0].pixels().as_ref(),
        [1, 2]
    );
}
#[test]
fn receiver_fragment_version_tolerance_keeps_original_version_bytes() {
    let raw = [
        pcs(0x80, false),
        palette(),
        ods(0x80, 6, &[1]),
        segment(0x15, &[0, 7, 1, 0x40, 2]),
        segment(0x80, &[]),
    ]
    .concat();
    let out = run(&raw, 1);
    assert_eq!(out.raw_packet().as_ref(), raw);
    assert_eq!(
        out.subtitle().unwrap().rectangles()[0].pixels().as_ref(),
        [1, 2]
    );
}
#[test]
fn a_crop_outside_the_object_is_ignored() {
    let raw = packet_with(&[1, 2, 0, 0], 0xc0, 8, &[], true);
    let out = run(&raw, 1);
    let rect = &out.subtitle().unwrap().rectangles()[0];
    assert_eq!((rect.width, rect.height, rect.x, rect.y), (2, 1, 1, 2));
}
#[test]
fn receiver_completed_raster_ignores_encoded_tail_without_rewriting_raw() {
    let raw = packet_with(&[1, 2, 0, 0, 7], 0xc0, 9, &[], false);
    let out = run(&raw, 1);
    assert_eq!(out.raw_packet().as_ref(), raw);
    assert_eq!(
        out.subtitle().unwrap().rectangles()[0].pixels().as_ref(),
        [1, 2]
    );
}
#[test]
fn invalid_receiver_rle_is_an_empty_rectangle_with_actual_allocated_stride() {
    for bytes in [
        &[0][..],
        &[1, 0, 0][..],
        &[0, 0x83, 1][..],
        &[0, 0x80, 7][..],
    ] {
        let raw = packet_with(bytes, 0xc0, 4 + bytes.len() as u8, &[], false);
        let out = run(&raw, 1);
        let rect = &out.subtitle().unwrap().rectangles()[0];
        assert_eq!((rect.width, rect.height, rect.stride), (0, 0, 2));
        assert!(rect.pixels().is_empty());
    }
}
#[test]
fn incomplete_physical_segment_is_retained_and_has_no_fabricated_end() {
    for length in 3..14 {
        let full = pcs(0x80, false);
        let raw = &full[..length];
        let out = run(raw, 1);
        assert_eq!(out.raw_packet().as_ref(), raw);
        assert!(out.subtitle().is_none());
    }
}
#[test]
fn a_partial_palette_reads_the_following_bytes() {
    let raw = [
        pcs(0x80, false),
        segment(0x14, &[0, 0, 1]),
        segment(0x80, &[]),
    ]
    .concat();
    let out = run(&raw, 1);
    let rect = &out.subtitle().unwrap().rectangles()[0];
    assert_eq!(
        rect.palette()[1],
        YcbcrMatrix::BT601.argb(128, 0, 0, 0, ChannelRounding::Nearest)
    );
    assert_eq!((rect.width, rect.height), (0, 0));
    assert_eq!(out.raw_packet().as_ref(), raw);
}
#[test]
fn an_epoch_start_without_definitions_presents_nothing() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let c = AtomicBool::new(false);
    let mut d = Decoder::new(&b, Options::default(), Limits::default()).unwrap();
    let first = packet_with(&[1, 2, 0, 0], 0xc0, 8, &[], false);
    let bytes = Bytes::copy_from_slice(&b, &first).unwrap();
    d.push(bytes, &stamp(), &c).unwrap();
    let old = ready(&mut d, 1);
    let raw = [pcs(0x80, false), segment(0x80, &[])].concat();
    let bytes = Bytes::copy_from_slice(&b, &raw).unwrap();
    d.push(bytes, &stamp(), &c).unwrap();
    let out = ready(&mut d, 1);
    assert!(out.subtitle().is_none());
    assert_eq!(
        old.subtitle().unwrap().rectangles()[0].pixels().as_ref(),
        [1, 2]
    );
}
#[test]
fn cancellation_is_sticky_and_busy_foreign_rejections_preserve_owners() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let foreign = Budget::new(4096).unwrap();
    let c = AtomicBool::new(false);
    let mut d = Decoder::new(&b, Options::default(), Limits::default()).unwrap();
    let wrong = Bytes::copy_from_slice(&foreign, &[0x80, 0, 0]).unwrap();
    let held = wrong.clone();
    let refused = d.push(wrong, &stamp(), &c).unwrap_err();
    assert_eq!(refused.error, Error::ForeignPool);
    assert!(refused.value.shares_storage_with(&held));
    let raw = packet_with(&[1, 2, 0, 0], 0xc0, 8, &[], false);
    let bytes = Bytes::copy_from_slice(&b, &raw).unwrap();
    d.push(bytes, &stamp(), &c).unwrap();
    let other = Bytes::copy_from_slice(&b, &raw).unwrap();
    let ptr = other.as_ptr();
    let refused = d.push(other, &stamp(), &c).unwrap_err();
    assert_eq!(refused.value.as_ptr(), ptr);
    c.store(true, Ordering::Relaxed);
    assert_eq!(d.poll(&mut WorkBudget::new(0), &c), Err(Error::Cancelled));
    c.store(false, Ordering::Relaxed);
    assert_eq!(d.poll(&mut WorkBudget::new(1), &c), Err(Error::Cancelled));
}

#[test]
fn work_zero_preserves_every_observed_allocation_boundary_for_valid_and_recovery_paths() {
    for end in [&[][..], &[1][..]] {
        let b = Budget::new(4 * 1024 * 1024).unwrap();
        let c = AtomicBool::new(false);
        let mut d = Decoder::new(&b, Options::default(), Limits::default()).unwrap();
        let bytes =
            Bytes::copy_from_slice(&b, &packet_with(&[1, 2, 0, 0], 0xc0, 8, end, false)).unwrap();
        d.push(bytes, &stamp(), &c).unwrap();
        let mut complete = false;
        for _ in 0..100_000 {
            let before = (b.used(), d.work_spent());
            let mut zero = WorkBudget::new(0);
            d.poll(&mut zero, &c).unwrap();
            assert_eq!((b.used(), d.work_spent()), before);
            assert_eq!(zero.spent(), 0);
            if d.poll(&mut WorkBudget::new(1), &c).unwrap() == Status::Ready {
                complete = true;
                break;
            }
        }
        assert!(complete);
        assert!(d.take_output(&c).unwrap().subtitle().is_some());
    }
}
#[test]
fn allocation_pressure_retry_keeps_paid_work_and_owners() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let c = AtomicBool::new(false);
    let mut d = Decoder::new(&b, Options::default(), Limits::default()).unwrap();
    let raw = packet_with(&[1, 2, 0, 0], 0xc0, 8, &[1], false);
    let owner = Bytes::copy_from_slice(&b, &raw).unwrap();
    let ptr = owner.as_ptr();
    let bytes = owner.clone();
    d.push(bytes, &stamp(), &c).unwrap();
    let mut pressure = 0;
    let mut complete = false;
    for _ in 0..100_000 {
        let blocker = b.reserve(b.available()).unwrap();
        match d.poll(&mut WorkBudget::new(1), &c) {
            Err(Error::Backpressure { .. }) => {
                pressure += 1;
                let mut retry = WorkBudget::new(4096);
                assert!(matches!(
                    d.poll(&mut retry, &c),
                    Err(Error::Backpressure { .. })
                ));
                assert_eq!(retry.spent(), 0);
                let before = b.used();
                d.poll(&mut WorkBudget::new(0), &c).unwrap();
                assert_eq!(b.used(), before);
            }
            Ok(Status::Ready) => {
                drop(blocker);
                complete = true;
                break;
            }
            Ok(Status::Yield) => {}
            other => panic!("unexpected pressured status {other:?}"),
        }
        drop(blocker);
        if d.poll(&mut WorkBudget::new(1), &c).unwrap() == Status::Ready {
            complete = true;
            break;
        }
    }
    assert!(pressure > 0 && complete);
    let out = d.take_output(&c).unwrap();
    assert_eq!(out.raw_packet().as_ptr(), ptr);
    assert_eq!(out.raw_packet().as_ref(), raw);
    assert_eq!(
        out.subtitle().unwrap().rectangles()[0].pixels().as_ref(),
        [1, 2]
    );
}
#[test]
fn cancellation_before_during_and_after_ready_releases_private_graph_and_is_sticky() {
    for steps in [0, 3, 18, 1000, 10000] {
        let b = Budget::new(4 * 1024 * 1024).unwrap();
        let c = AtomicBool::new(false);
        let mut d = Decoder::new(&b, Options::default(), Limits::default()).unwrap();
        let raw = packet_with(&[1, 2, 0, 0], 0xc0, 8, &[1], false);
        let bytes = Bytes::copy_from_slice(&b, &raw).unwrap();
        d.push(bytes, &stamp(), &c).unwrap();
        for _ in 0..steps {
            if d.poll(&mut WorkBudget::new(1), &c).unwrap() == Status::Ready {
                break;
            }
        }
        c.store(true, Ordering::Relaxed);
        assert_eq!(d.poll(&mut WorkBudget::new(0), &c), Err(Error::Cancelled));
        assert_eq!(b.used(), Budget::control_storage_bytes());
        c.store(false, Ordering::Relaxed);
        assert_eq!(
            d.poll(&mut WorkBudget::new(4096), &c),
            Err(Error::Cancelled)
        );
    }
}
#[test]
fn exceeded_limits_are_errors_not_refusals() {
    for limits in [
        Limits {
            max_object_pixels: 1,
            ..Limits::default()
        },
        Limits {
            max_encoded: 1,
            ..Limits::default()
        },
        Limits {
            max_work: 1,
            ..Limits::default()
        },
    ] {
        let b = Budget::new(4 * 1024 * 1024).unwrap();
        let c = AtomicBool::new(false);
        let mut d = Decoder::new(&b, Options::default(), limits).unwrap();
        let bytes =
            Bytes::copy_from_slice(&b, &packet_with(&[1, 2, 0, 0], 0xc0, 8, &[1], false)).unwrap();
        d.push(bytes, &stamp(), &c).unwrap();
        let error = loop {
            match d.poll(&mut WorkBudget::new(4096), &c) {
                Err(e) => break e,
                Ok(Status::Yield) => {}
                other => panic!("resource error disappeared {other:?}"),
            }
        };
        assert!(matches!(error, Error::Limit { .. }));
        assert_eq!(d.poll(&mut WorkBudget::new(1), &c), Err(error));
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}
#[test]
fn packet_byte_limits_and_empty_input_reject_without_consuming_the_callers_owner() {
    let b = Budget::new(4096).unwrap();
    let c = AtomicBool::new(false);
    let mut d = Decoder::new(
        &b,
        Options::default(),
        Limits {
            max_packet_bytes: 2,
            ..Limits::default()
        },
    )
    .unwrap();
    let raw = Bytes::copy_from_slice(&b, &[0x80, 0, 0]).unwrap();
    let ptr = raw.as_ptr();
    let refused = d.push(raw, &stamp(), &c).unwrap_err();
    assert_eq!(
        refused.error,
        Error::Limit {
            requested: 3,
            maximum: 2
        }
    );
    assert_eq!(refused.value.as_ptr(), ptr);
    let empty = Bytes::copy_from_slice(&b, &[]).unwrap();
    let refused = d.push(empty, &stamp(), &c).unwrap_err();
    assert!(matches!(refused.error, Error::Invalid(_)));
    assert!(refused.value.is_empty());
}
#[test]
fn one_and_two_byte_packets_are_refused_and_decoding_continues() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let c = AtomicBool::new(false);
    let mut d = Decoder::new(&b, Options::default(), Limits::default()).unwrap();
    for raw in [&[0x16][..], &[0x16, 0][..]] {
        let bytes = Bytes::copy_from_slice(&b, raw).unwrap();
        d.push(bytes, &stamp(), &c).unwrap();
        let out = ready(&mut d, 1);
        assert_eq!(out.raw_packet().as_ref(), raw);
        assert_eq!(out.refusal(), Some(Refusal::ShortPacket));
        assert!(out.subtitle().is_none());
    }
    let raw = packet_with(&[1, 2, 0, 0], 0xc0, 8, &[], false);
    let bytes = Bytes::copy_from_slice(&b, &raw).unwrap();
    d.push(bytes, &stamp(), &c).unwrap();
    let out = ready(&mut d, 7);
    assert_eq!(out.refusal(), None);
    let rect = &out.subtitle().unwrap().rectangles()[0];
    assert_eq!(rect.pixels().as_ref(), [1, 2]);
}
#[test]
fn subtitle_survives_decoder_teardown() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let c = AtomicBool::new(false);
    let mut d = Decoder::new(&b, Options::default(), Limits::default()).unwrap();
    let bytes =
        Bytes::copy_from_slice(&b, &packet_with(&[1, 2, 0, 0], 0xc0, 8, &[], false)).unwrap();
    d.push(bytes, &stamp(), &c).unwrap();
    let out = ready(&mut d, 1);
    drop(d);
    assert_eq!(
        out.subtitle().unwrap().rectangles()[0].pixels().as_ref(),
        [1, 2]
    );
    assert!(b.used() > Budget::control_storage_bytes());
    drop(out);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn hard_reset_keeps_old_outputs() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let c = AtomicBool::new(false);
    let mut d = Decoder::new(&b, Options::default(), Limits::default()).unwrap();
    let mut retained = Vec::new();
    for generation in 0..3 {
        if generation != 0 {
            d.reset(&c).unwrap();
        }
        let bytes =
            Bytes::copy_from_slice(&b, &packet_with(&[1, 2, 0, 0], 0xc0, 8, &[], false)).unwrap();
        d.push(bytes, &stamp(), &c).unwrap();
        let out = ready(&mut d, 1);
        retained.push(out);
    }
    for out in retained {
        assert_eq!(
            out.subtitle().unwrap().rectangles()[0].pixels().as_ref(),
            [1, 2]
        );
    }
}
#[test]
fn unknown_and_sentinel_clocks_are_never_relabelled_known_zero() {
    for pts in [None, Some(i64::MIN), Some(-90_000)] {
        let b = Budget::new(4 * 1024 * 1024).unwrap();
        let c = AtomicBool::new(false);
        let mut d = Decoder::new(&b, Options::default(), Limits::default()).unwrap();
        let bytes =
            Bytes::copy_from_slice(&b, &packet_with(&[1, 2, 0, 0], 0xc0, 8, &[], false)).unwrap();
        let clock = Clock { pts, ..stamp() };
        d.push(bytes, &clock, &c).unwrap();
        let out = ready(&mut d, 1);
        assert_eq!(out.clock, clock);
        if pts == Some(i64::MIN) {
            assert_eq!(out.subtitle().unwrap().pts_microseconds, None);
        }
    }
}
#[test]
fn a_palette_entry_read_past_the_packet_adds_nothing_to_the_raw_bytes() {
    let raw = [pcs(0x80, false), segment(0x14, &[0, 0, 1])].concat();
    let out = run(&raw, 1);
    assert_eq!(out.raw_packet().as_ref(), raw);
}

#[test]
fn missing_receiver_object_has_zero_position_but_preserves_the_original_pcs_reference() {
    let raw = [pcs(0x80, false), palette(), segment(0x80, &[])].concat();
    let out = run(&raw, 1);
    let rect = &out.subtitle().unwrap().rectangles()[0];
    assert_eq!(
        (rect.x, rect.y, rect.width, rect.height, rect.stride),
        (0, 0, 0, 0, 0)
    );
    assert_eq!(
        (rect.reference.id, rect.reference.x, rect.reference.y),
        (7, 1, 2)
    );
    assert_eq!(out.raw_packet().as_ref(), raw);
}
#[test]
fn rle_shorter_than_declared_can_fill_the_picture() {
    let raw = [
        pcs(0x80, false),
        palette(),
        segment(0x15, &[0, 7, 0, 0xc0, 0, 0, 7, 0, 1, 0, 1, 1]),
        segment(0x80, &[]),
    ]
    .concat();
    let out = run(&raw, 1);
    let rect = &out.subtitle().unwrap().rectangles()[0];
    assert_eq!((rect.width, rect.height, rect.stride), (1, 1, 1));
    assert_eq!(rect.pixels().as_ref(), [1]);
}
#[test]
fn combined_work_and_outputs_are_invariant_across_quanta_for_valid_and_recovered_packets() {
    for end in [&[][..], &[1][..]] {
        let mut total = None;
        for quantum in [1, 2, 3, 7, 4096] {
            let b = Budget::new(4 * 1024 * 1024).unwrap();
            let c = AtomicBool::new(false);
            let mut d = Decoder::new(&b, Options::default(), Limits::default()).unwrap();
            let bytes =
                Bytes::copy_from_slice(&b, &packet_with(&[1, 2, 0, 0], 0xc0, 8, end, false))
                    .unwrap();
            d.push(bytes, &stamp(), &c).unwrap();
            let out = ready(&mut d, quantum);
            assert_eq!(
                out.subtitle().unwrap().rectangles()[0].pixels().as_ref(),
                [1, 2]
            );
            match total {
                None => total = Some(d.work_spent()),
                Some(expected) => assert_eq!(d.work_spent(), expected),
            }
        }
    }
}
#[test]
fn limits_still_apply_after_a_packet_without_a_display() {
    for limits in [
        Limits {
            max_object_pixels: 1,
            ..Limits::default()
        },
        Limits {
            max_references: 0,
            ..Limits::default()
        },
        Limits {
            max_display_pixels: 1,
            ..Limits::default()
        },
        Limits {
            max_fragments: 1,
            ..Limits::default()
        },
    ] {
        let b = Budget::new(4 * 1024 * 1024).unwrap();
        let c = AtomicBool::new(false);
        let mut d = Decoder::new(&b, Options::default(), limits).unwrap();
        let bytes = Bytes::copy_from_slice(&b, &segment(0x80, &[])).unwrap();
        d.push(bytes, &stamp(), &c).unwrap();
        let first = ready(&mut d, 1);
        drop(first);
        let raw = [
            pcs(0x80, false),
            palette(),
            ods(0x80, 6, &[1]),
            segment(0x15, &[0, 7, 99, 0x40, 2]),
            segment(0x80, &[]),
        ]
        .concat();
        let bytes = Bytes::copy_from_slice(&b, &raw).unwrap();
        d.push(bytes, &stamp(), &c).unwrap();
        let error = loop {
            match d.poll(&mut WorkBudget::new(4096), &c) {
                Err(e) => break e,
                Ok(Status::Yield) => {}
                other => panic!("receiver bound bypassed after strict refusal {other:?}"),
            }
        };
        assert!(matches!(error, Error::Limit { .. }));
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}
