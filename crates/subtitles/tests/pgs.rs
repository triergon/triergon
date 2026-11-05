use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::pgs::{
    Composition, Envelope, ObjectFragment, Palette, RasterJob, RasterLimits, RasterStatus,
    SegmentIter, Windows,
};

fn packet(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut output = vec![kind];
    output.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
    output.extend_from_slice(body);
    output
}

fn raster(encoded: &[u8], width: u16, height: u16, quantum: u64) -> (Vec<u8>, usize) {
    let budget = Budget::new(128 * 1024).unwrap();
    let bytes = Bytes::copy_from_slice(&budget, encoded).unwrap();
    let mut job = RasterJob::new(
        bytes,
        width,
        height,
        &budget,
        RasterLimits {
            max_axis: u16::MAX,
            ..RasterLimits::default()
        },
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut yields = 0;
    loop {
        match job.poll(&mut WorkBudget::new(quantum), &cancel).unwrap() {
            RasterStatus::Ready => break,
            RasterStatus::Yield => yields += 1,
        }
        assert!(yields < encoded.len() + usize::from(width) * usize::from(height) + 10);
    }
    let work = job.work_spent();
    assert_eq!(job.encoded_consumed(), encoded.len());
    let object = job.try_finish(&cancel).unwrap();
    assert_eq!((object.width, object.height), (width, height));
    drop(job);
    assert!(budget.used() > Budget::control_storage_bytes());
    let result = object.pixels().to_vec();
    drop(object);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
    (result, work)
}

#[test]
fn all_rle_forms_and_boundary_lengths_have_exact_indices_and_work() {
    for length in [1u16, 2, 63, 64, 255, 256, 16383] {
        for index in [0u8, 1, 255] {
            let mut encoded = if length < 64 {
                if index == 0 {
                    vec![0, length as u8]
                } else {
                    vec![0, 0x80 | length as u8, index]
                }
            } else {
                let mut bytes = vec![0, 0x40 | (length >> 8) as u8, length as u8];
                if index != 0 {
                    bytes[1] |= 0x80;
                    bytes.push(index);
                }
                bytes
            };
            encoded.extend_from_slice(&[0, 0]);
            let expected = vec![index; usize::from(length)];
            for quantum in [1, 3, 17, 65536] {
                let (actual, work) = raster(&encoded, length, 1, quantum);
                assert_eq!(actual, expected);
                assert_eq!(work, 1 + encoded.len() + expected.len());
            }
        }
    }
}

#[test]
fn mixed_rows_and_final_row_without_eol_are_schedule_independent() {
    let encoded = [1, 2, 0, 2, 0, 0, 0, 0x84, 9];
    for quantum in [1, 2, 3, 7, 4096] {
        assert_eq!(raster(&encoded, 4, 2, quantum).0, [1, 2, 0, 0, 9, 9, 9, 9]);
    }
}

#[test]
fn incomplete_codes_short_rows_row_overflow_and_extra_bytes_are_rejected() {
    for bytes in [
        &[0][..],
        &[0, 0xc1][..],
        &[0, 0xc1, 1][..],
        &[1, 0, 0][..],
        &[1, 2, 3][..],
        &[0, 0x80, 2][..],
        &[1, 2, 0, 0, 7][..],
    ] {
        let budget = Budget::new(4096).unwrap();
        let source = Bytes::copy_from_slice(&budget, bytes).unwrap();
        let mut job = RasterJob::new(source, 2, 1, &budget, RasterLimits::default()).unwrap();
        let cancel = AtomicBool::new(false);
        let error = job.poll(&mut WorkBudget::new(4096), &cancel).unwrap_err();
        assert!(matches!(error, Error::Invalid(_) | Error::Truncated));
        assert_eq!(
            job.poll(&mut WorkBudget::new(4096), &cancel).unwrap_err(),
            error
        );
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn zero_work_does_not_allocate_consume_or_construct_a_ready_result() {
    let budget = Budget::new(4096).unwrap();
    let source = Bytes::copy_from_slice(&budget, &[1]).unwrap();
    let mut job = RasterJob::new(source, 1, 1, &budget, RasterLimits::default()).unwrap();
    let before = budget.used();
    let cancel = AtomicBool::new(false);
    for _ in 0..5 {
        assert_eq!(
            job.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
            RasterStatus::Yield
        );
        assert_eq!(budget.used(), before);
        assert_eq!((job.encoded_consumed(), job.work_spent()), (0, 0));
    }
    assert!(job.try_finish(&cancel).is_err());
}

#[test]
fn backpressure_retry_does_not_repeat_setup_work_or_invalidate_the_source() {
    let budget = Budget::new(2048).unwrap();
    let held = Bytes::copy_from_slice(&budget, &[0; 800]).unwrap();
    let source = Bytes::copy_from_slice(&budget, &[0, 0xc5, 0xdc, 7]).unwrap();
    let mut job = RasterJob::new(source, 1500, 1, &budget, RasterLimits::default()).unwrap();
    let cancel = AtomicBool::new(false);
    for _ in 0..3 {
        assert!(matches!(
            job.poll(&mut WorkBudget::new(1), &cancel),
            Err(Error::Backpressure { .. })
        ));
        assert_eq!(job.work_spent(), 1);
        assert_eq!(job.encoded_consumed(), 0);
    }
    drop(held);
    let before = budget.used();
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
        RasterStatus::Yield
    );
    assert_eq!(budget.used(), before);
    assert_eq!(job.work_spent(), 1);
    assert_eq!(
        job.poll(&mut WorkBudget::new(4096), &cancel).unwrap(),
        RasterStatus::Ready
    );
    assert_eq!(
        job.try_finish(&cancel).unwrap().pixels().as_ref(),
        &[7; 1500]
    );
}

#[test]
fn cancellation_is_sticky_after_yield_and_after_ready() {
    for initially_ready in [false, true] {
        let budget = Budget::new(4096).unwrap();
        let source = Bytes::copy_from_slice(&budget, &[1]).unwrap();
        let mut job = RasterJob::new(source, 1, 1, &budget, RasterLimits::default()).unwrap();
        let cancel = AtomicBool::new(false);
        job.poll(
            &mut WorkBudget::new(if initially_ready { 100 } else { 1 }),
            &cancel,
        )
        .unwrap();
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(job.try_finish(&cancel).unwrap_err(), Error::Cancelled);
        cancel.store(false, Ordering::Relaxed);
        assert_eq!(
            job.poll(&mut WorkBudget::new(4096), &cancel).unwrap_err(),
            Error::Cancelled
        );
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn foreign_pools_dimensions_encoded_capacity_and_work_are_bounded() {
    let budget = Budget::new(4096).unwrap();
    let foreign = Budget::new(4096).unwrap();
    let source = Bytes::copy_from_slice(&foreign, &[1]).unwrap();
    assert_eq!(
        RasterJob::new(source, 1, 1, &budget, RasterLimits::default()).unwrap_err(),
        Error::ForeignPool
    );
    let source = Bytes::copy_from_slice(&budget, &[1]).unwrap();
    assert!(RasterJob::new(source.clone(), 0, 1, &budget, RasterLimits::default()).is_err());
    assert!(matches!(
        RasterJob::new(
            source.clone(),
            2,
            1,
            &budget,
            RasterLimits {
                max_pixels: 1,
                ..RasterLimits::default()
            }
        ),
        Err(Error::Limit { .. })
    ));
    assert!(matches!(
        RasterJob::new(
            source.clone(),
            1,
            1,
            &budget,
            RasterLimits {
                max_encoded: 0,
                ..RasterLimits::default()
            }
        ),
        Err(Error::Limit { .. })
    ));
    let mut job = RasterJob::new(
        source,
        1,
        1,
        &budget,
        RasterLimits {
            max_work: 2,
            ..RasterLimits::default()
        },
    )
    .unwrap();
    assert!(matches!(
        job.poll(&mut WorkBudget::new(99), &AtomicBool::new(false)),
        Err(Error::Limit { .. })
    ));
}

#[test]
fn packet_and_sup_headers_preserve_raw_clocks_unknown_types_and_borrowed_bytes() {
    let p = packet(0x7f, &[1, 2, 3]);
    let segment = SegmentIter::new(&p, Envelope::Packet)
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(segment.kind, 0x7f);
    assert_eq!(segment.payload, [1, 2, 3]);
    assert_eq!(segment.clock, None);
    assert!(std::ptr::eq(segment.raw.as_ptr(), p.as_ptr()));
    let mut sup = b"PG".to_vec();
    sup.extend_from_slice(&u32::MAX.to_be_bytes());
    sup.extend_from_slice(&42u32.to_be_bytes());
    sup.extend_from_slice(&p);
    let segment = SegmentIter::new(&sup, Envelope::Sup)
        .next()
        .unwrap()
        .unwrap();
    let clock = segment.clock.unwrap();
    assert_eq!((clock.pts, clock.dts), (u32::MAX, 42));
    assert_eq!(segment.payload, [1, 2, 3]);
    for end in 1..sup.len() {
        let mut iter = SegmentIter::new(&sup[..end], Envelope::Sup);
        assert!(iter.next().unwrap().is_err());
        assert!(iter.next().is_none());
    }
}

#[test]
fn palette_keeps_entry_order_partial_updates_and_exact_y_cr_cb_alpha() {
    let bytes = packet(0x14, &[3, 9, 7, 16, 128, 129, 255, 7, 17, 130, 131, 0]);
    let palette = Palette::parse(
        SegmentIter::new(&bytes, Envelope::Packet)
            .next()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!((palette.id, palette.version), (3, 9));
    let entries = palette.entries().collect::<Vec<_>>();
    assert_eq!(
        (
            entries[0].id,
            entries[0].y,
            entries[0].cr,
            entries[0].cb,
            entries[0].alpha
        ),
        (7, 16, 128, 129, 255)
    );
    assert_eq!((entries[1].id, entries[1].alpha), (7, 0));
    let malformed = packet(0x14, &[3, 9, 7]);
    assert!(
        Palette::parse(
            SegmentIter::new(&malformed, Envelope::Packet)
                .next()
                .unwrap()
                .unwrap()
        )
        .is_err()
    );
}

#[test]
fn object_fragment_lengths_dimensions_and_sequence_flags_stay_distinct() {
    let bytes = packet(0x15, &[0x12, 0x34, 7, 0xc0, 0, 0, 6, 0, 2, 0, 1, 1, 2]);
    let fragment = ObjectFragment::parse(
        SegmentIter::new(&bytes, Envelope::Packet)
            .next()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (fragment.id, fragment.version, fragment.last),
        (0x1234, 7, true)
    );
    let start = fragment.first.unwrap();
    assert_eq!((start.data_length, start.width, start.height), (6, 2, 1));
    assert_eq!(fragment.data, [1, 2]);
    let bytes = packet(0x15, &[0x12, 0x34, 7, 0x40, 0, 0]);
    let fragment = ObjectFragment::parse(
        SegmentIter::new(&bytes, Envelope::Packet)
            .next()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(fragment.first.is_none());
    assert!(fragment.last);
    assert_eq!(fragment.data, [0, 0]);
}

#[test]
fn cropping_and_forced_flags_are_independent_and_composition_records_are_exact() {
    for flags in [0u8, 0x40, 0x80, 0xc0] {
        let mut body = vec![
            7, 128, 4, 56, 0x10, 0x12, 0x34, 0x80, 0x80, 3, 1, 0, 7, 2, flags, 0, 9, 0, 10,
        ];
        if flags & 0x80 != 0 {
            body.extend_from_slice(&[0, 1, 0, 2, 0, 3, 0, 4]);
        }
        let bytes = packet(0x16, &body);
        let composition = Composition::parse(
            SegmentIter::new(&bytes, Envelope::Packet)
                .next()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            (composition.width, composition.height, composition.number),
            (1920, 1080, 0x1234)
        );
        let object = composition.objects().next().unwrap().unwrap();
        assert_eq!(
            (object.id, object.window, object.x, object.y),
            (7, 2, 9, 10)
        );
        assert_eq!(object.forced(), flags & 0x40 != 0);
        assert_eq!(object.crop.is_some(), flags & 0x80 != 0);
        if let Some(crop) = object.crop {
            assert_eq!((crop.x, crop.y, crop.width, crop.height), (1, 2, 3, 4));
        }
        body.push(0);
        let bytes = packet(0x16, &body);
        assert!(
            Composition::parse(
                SegmentIter::new(&bytes, Envelope::Packet)
                    .next()
                    .unwrap()
                    .unwrap()
            )
            .is_err()
        );
    }
}

#[test]
fn window_count_and_geometry_cannot_consume_trailing_data() {
    let bytes = packet(0x17, &[1, 7, 0, 8, 0, 9, 0, 10, 0, 11]);
    let windows = Windows::parse(
        SegmentIter::new(&bytes, Envelope::Packet)
            .next()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let window = windows.iter().next().unwrap();
    assert_eq!(
        (window.id, window.x, window.y, window.width, window.height),
        (7, 8, 9, 10, 11)
    );
    let bytes = packet(0x17, &[0, 1]);
    assert!(
        Windows::parse(
            SegmentIter::new(&bytes, Envelope::Packet)
                .next()
                .unwrap()
                .unwrap()
        )
        .is_err()
    );
}
