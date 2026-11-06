//! Original receiver compatibility guards. Strict source facts remain intact.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, TimeBase, WorkBudget};
use triergon_subtitles::compat::pgs::{EofBehavior, RecoveryEof};
use triergon_subtitles::pgs::{
    DisplayConfig, DisplayDecoder, DisplayLimits, DisplaySet, DisplayStatus, Envelope, PacketClock,
    SegmentIter, SegmentStamp,
};
use triergon_subtitles::reference_trace::pgs::{
    Canvas, ProjectedDisplay, ProjectedPacket, ProjectionStatus, Projector, RasterOrigin,
};

fn seg(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut b = vec![kind];
    b.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    b.extend_from_slice(payload);
    b
}
fn pcs(state: u8, number: u16, ids: &[u16]) -> Vec<u8> {
    let mut b = vec![0, 64, 0, 32, 0x10];
    b.extend_from_slice(&number.to_be_bytes());
    b.extend_from_slice(&[state, 0, 0, ids.len() as u8]);
    for &id in ids {
        b.extend_from_slice(&id.to_be_bytes());
        b.extend_from_slice(&[0, 0, 0, 1, 0, 2]);
    }
    seg(0x16, &b)
}
fn pds(version: u8, y: u8) -> Vec<u8> {
    seg(
        0x14,
        &[0, version, 1, y, 128, 128, 255, 2, 235, 128, 128, 255],
    )
}
fn ods(id: u16, version: u8, flags: u8, encoded: usize, data: &[u8]) -> Vec<u8> {
    let mut b = id.to_be_bytes().to_vec();
    b.extend_from_slice(&[version, flags]);
    if flags & 0x80 != 0 {
        b.extend_from_slice(&((encoded + 4) as u32).to_be_bytes()[1..]);
        b.extend_from_slice(&[0, 2, 0, 1]);
    }
    b.extend_from_slice(data);
    seg(0x15, &b)
}
fn frame(state: u8, number: u16, split: Option<usize>, pixels: [u8; 2]) -> Vec<u8> {
    let mut b = pcs(state, number, &[7]);
    b.extend(pds(number as u8, 16));
    let data = [pixels[0], pixels[1], 0, 0];
    match split {
        None => b.extend(ods(7, number as u8, 0xc0, 4, &data)),
        Some(n) => {
            b.extend(ods(7, number as u8, 0x80, 4, &data[..n]));
            b.extend(ods(7, number as u8, 0x40, 4, &data[n..]));
        }
    }
    b.extend(seg(0x80, &[]));
    b
}
fn clock(pts: Option<i64>) -> PacketClock {
    PacketClock {
        pts,
        dts: Some(-23),
        timebase: TimeBase::new(1, 90000).unwrap(),
    }
}
fn source(d: &mut DisplayDecoder, b: &Budget, p: &[u8], time: PacketClock) -> Vec<DisplaySet> {
    let c = AtomicBool::new(false);
    d.push(
        Bytes::copy_from_slice(b, p).unwrap(),
        time,
        Envelope::Packet,
        &c,
    )
    .unwrap();
    let mut out = Vec::new();
    for _ in 0..100_000 {
        match d.poll(&mut WorkBudget::new(4096), &c).unwrap() {
            DisplayStatus::Yield => {}
            DisplayStatus::OutputReady => out.push(d.take_output(&c).unwrap()),
            DisplayStatus::NeedPacket => return out,
            other @ DisplayStatus::Ended => panic!("{other:?}"),
        }
    }
    panic!("source stalled")
}
fn decoder(b: &Budget) -> DisplayDecoder {
    DisplayDecoder::new(b, DisplayLimits::default(), DisplayConfig::default()).unwrap()
}
fn take_source(d: &mut DisplayDecoder, b: &Budget, p: &[u8]) -> DisplaySet {
    source(d, b, p, clock(Some(90001))).remove(0)
}
fn boundary(p: &mut Projector, q: u64, c: &AtomicBool) -> Result<ProjectionStatus, Error> {
    for _ in 0..100_000 {
        let before = p.work_spent();
        let mut w = WorkBudget::new(q);
        let result = p.poll(&mut w, c);
        assert_eq!(p.work_spent() - before, w.spent() as usize);
        match result {
            Ok(ProjectionStatus::Yield) if q != 0 => {}
            other => return other,
        }
    }
    panic!("projection stalled")
}
fn project(p: &mut Projector, set: DisplaySet, q: u64) -> ProjectedDisplay {
    let c = AtomicBool::new(false);
    let mut set = Some(set);
    p.push(&mut set, &c).unwrap();
    assert!(set.is_none());
    assert_eq!(boundary(p, q, &c).unwrap(), ProjectionStatus::Ready);
    p.take_output(&c).unwrap()
}
fn pool() -> Budget {
    Budget::new(4 * 1024 * 1024).unwrap()
}

#[test]
fn empty_first_has_empty_receiver_and_retains_complete_source_raster() {
    let b = pool();
    let mut d = decoder(&b);
    let mut p = Projector::new(&b, usize::MAX);
    let output = project(
        &mut p,
        take_source(&mut d, &b, &frame(0x80, 1, Some(0), [1, 2])),
        1,
    );
    let r = &output.rectangles()[0];
    assert_eq!((r.width, r.height, r.stride), (0, 0, 0));
    assert!(r.pixels().is_empty());
    assert!(r.argb_pixels().is_empty());
    assert_eq!(r.origin, RasterOrigin::EmptyAfterRejectedFirst);
    assert_eq!(output.source().objects()[0].pixels().as_ref(), &[1, 2]);
    assert_eq!(output.source().objects()[0].object_width, 2);
}

#[test]
fn every_nonempty_fragment_split_has_complete_literal_receiver_pixels() {
    for split in 1..=3 {
        for q in [1, 2, 3, 7, 65, 4096] {
            let b = pool();
            let mut d = decoder(&b);
            let mut p = Projector::new(&b, usize::MAX);
            let output = project(
                &mut p,
                take_source(&mut d, &b, &frame(0x80, 1, Some(split), [1, 2])),
                q,
            );
            assert_eq!(output.rectangles()[0].pixels().as_ref(), &[1, 2]);
            assert_eq!(
                output.rectangles()[0].argb_pixels().as_ref(),
                &[255, 0, 0, 0, 255, 255, 255, 255]
            );
            assert_eq!(output.rectangles()[0].origin, RasterOrigin::SourceRaster);
        }
    }
}

#[test]
fn rejected_replacement_retains_old_receiver_image_and_new_source_image() {
    let b = pool();
    let mut d = decoder(&b);
    let mut p = Projector::new(&b, usize::MAX);
    let old = project(
        &mut p,
        take_source(&mut d, &b, &frame(0x80, 1, None, [1, 2])),
        1,
    );
    let mut packet = frame(0, 2, Some(0), [2, 1]);
    let at = packet.windows(4).position(|x| x == [0, 2, 1, 16]).unwrap();
    packet[at + 3] = 235;
    let new = project(&mut p, take_source(&mut d, &b, &packet), 1);
    assert_eq!(old.rectangles()[0].pixels().as_ref(), &[1, 2]);
    assert_eq!(new.source().objects()[0].pixels().as_ref(), &[2, 1]);
    assert_eq!(new.rectangles()[0].pixels().as_ref(), &[1, 2]);
    assert_eq!(
        new.rectangles()[0].origin,
        RasterOrigin::RetainedBeforeRejectedFirst
    );
    assert_eq!(
        new.rectangles()[0].argb_pixels().as_ref(),
        &[255, 255, 255, 255, 255, 255, 255, 255]
    );
    assert_eq!(
        old.rectangles()[0].argb_pixels().as_ref(),
        &[255, 0, 0, 0, 255, 255, 255, 255]
    );
}

#[test]
fn epoch_discards_previous_receiver_image_before_rejected_first() {
    let b = pool();
    let mut d = decoder(&b);
    let mut p = Projector::new(&b, usize::MAX);
    let old = project(
        &mut p,
        take_source(&mut d, &b, &frame(0x80, 1, None, [1, 2])),
        7,
    );
    let new = project(
        &mut p,
        take_source(&mut d, &b, &frame(0x80, 2, Some(0), [2, 1])),
        7,
    );
    assert_eq!(
        new.rectangles()[0].origin,
        RasterOrigin::EmptyAfterRejectedFirst
    );
    assert!(new.rectangles()[0].pixels().is_empty());
    assert_eq!(old.rectangles()[0].pixels().as_ref(), &[1, 2]);
}

#[test]
fn hard_source_generation_change_discards_receiver_history() {
    let b = pool();
    let mut d = decoder(&b);
    let mut p = Projector::new(&b, usize::MAX);
    let old = project(
        &mut p,
        take_source(&mut d, &b, &frame(0x80, 1, None, [1, 2])),
        1,
    );
    d.reset(&AtomicBool::new(false)).unwrap();
    let new = project(
        &mut p,
        take_source(&mut d, &b, &frame(0, 1, Some(0), [2, 1])),
        1,
    );
    assert_ne!(old.source().generation, new.source().generation);
    assert_eq!(
        new.rectangles()[0].origin,
        RasterOrigin::EmptyAfterRejectedFirst
    );
}

#[test]
fn unused_preloaded_object_history_survives_clear_then_reference() {
    let b = pool();
    let mut d = decoder(&b);
    let mut p = Projector::new(&b, usize::MAX);
    let mut preload = pcs(0x80, 1, &[]);
    preload.extend(pds(1, 16));
    preload.extend(ods(7, 1, 0xc0, 4, &[1, 2, 0, 0]));
    preload.extend(seg(0x80, &[]));
    let first = project(&mut p, take_source(&mut d, &b, &preload), 1);
    assert!(first.rectangles().is_empty());
    let mut later = pcs(0, 2, &[7]);
    later.extend(seg(0x80, &[]));
    let later = project(&mut p, take_source(&mut d, &b, &later), 1);
    assert_eq!(later.rectangles()[0].pixels().as_ref(), &[1, 2]);
    assert_eq!(later.rectangles()[0].origin, RasterOrigin::SourceRaster);
}

#[test]
fn two_reference_receiver_limit_preserves_all_three_source_objects() {
    let b = pool();
    let mut d = decoder(&b);
    let mut p = Projector::new(&b, usize::MAX);
    let mut packet = pcs(0x80, 1, &[7, 8, 9]);
    packet.extend(pds(1, 16));
    for id in [7, 8, 9] {
        packet.extend(ods(id, 1, 0xc0, 4, &[1, 2, 0, 0]));
    }
    packet.extend(seg(0x80, &[]));
    let output = project(&mut p, take_source(&mut d, &b, &packet), 1);
    assert_eq!(output.rectangles().len(), 2);
    assert_eq!(output.omitted_source_references, 1);
    assert_eq!(
        output
            .source()
            .objects()
            .iter()
            .map(|o| o.composition.id)
            .collect::<Vec<_>>(),
        vec![7, 8, 9]
    );
    assert_eq!(output.source().objects()[2].pixels().as_ref(), &[1, 2]);
}

#[test]
fn one_native_packet_output_retains_all_source_sets() {
    let b = pool();
    let mut d = decoder(&b);
    let mut p = Projector::new(&b, usize::MAX);
    let mut packet = frame(0x80, 1, None, [1, 2]);
    packet.extend(frame(0, 2, None, [2, 1]));
    let sets = source(&mut d, &b, &packet, clock(None));
    assert_eq!(sets.len(), 2);
    let mut native = BudgetVec::new(&b);
    for set in sets {
        native.try_push(project(&mut p, set, 1)).unwrap();
    }
    let packet = ProjectedPacket::from_sets(native, None);
    assert_eq!(packet.omitted_receiver_events(), 1);
    assert_eq!(
        packet.receiver_output().unwrap().rectangles()[0]
            .pixels()
            .as_ref(),
        &[1, 2]
    );
    assert_eq!(
        packet.source_sets()[1].source().objects()[0]
            .pixels()
            .as_ref(),
        &[2, 1]
    );
    assert_eq!(packet.context_canvas(), None);
}

#[test]
fn final_packet_canvas_comes_from_actual_trailing_pcs_and_preserves_stamp() {
    let b = pool();
    let mut d = decoder(&b);
    let mut p = Projector::new(&b, usize::MAX);
    let set = take_source(&mut d, &b, &frame(0x80, 1, None, [1, 2]));
    let mut raw = pcs(0, 2, &[]);
    raw[3..7].copy_from_slice(&[1, 64, 0, 96]);
    let segment = SegmentIter::new(&raw, Envelope::Packet)
        .next()
        .unwrap()
        .unwrap();
    let stamp = SegmentStamp {
        packet_sequence: 9,
        packet_offset: 123,
        packet_clock: clock(Some(19)),
        sup_clock: None,
        envelope: Envelope::Packet,
    };
    let canvas = Canvas::parse(segment, stamp).unwrap();
    let mut sets = BudgetVec::new(&b);
    sets.try_push(project(&mut p, set, 1)).unwrap();
    let packet = ProjectedPacket::from_sets(sets, Some(canvas));
    assert_eq!(packet.context_canvas(), Some([320, 96]));
    assert_eq!(packet.context().unwrap().stamp, stamp);
    assert_eq!(
        packet
            .receiver_output()
            .unwrap()
            .source()
            .presentation
            .width,
        64
    );
}

#[test]
fn native_clock_sentinel_preserves_exact_source_tick_and_diagnostic() {
    let b = pool();
    let mut d = decoder(&b);
    let mut p = Projector::new(&b, usize::MAX);
    let set = source(
        &mut d,
        &b,
        &frame(0x80, 1, None, [1, 2]),
        clock(Some(i64::MIN)),
    )
    .remove(0);
    let output = project(&mut p, set, 1);
    assert_eq!(output.pts_microseconds, None);
    assert!(output.source().receiver_time_error.is_some());
    assert_eq!(
        output.source().presentation.stamp.packet_clock.pts,
        Some(i64::MIN)
    );
}

#[test]
fn native_clock_overflow_preserves_source_timebase_and_value() {
    let b = pool();
    let mut d = decoder(&b);
    let mut p = Projector::new(&b, usize::MAX);
    let clock = PacketClock {
        pts: Some(i64::MAX),
        dts: Some(-1),
        timebase: TimeBase::new(1, 1).unwrap(),
    };
    let set = source(&mut d, &b, &frame(0x80, 1, None, [1, 2]), clock).remove(0);
    let output = project(&mut p, set, 1);
    assert_eq!(output.pts_microseconds, None);
    assert_eq!(output.source().receiver_time_error, Some(Error::Overflow));
    assert_eq!(output.source().presentation.stamp.packet_clock, clock);
}

#[test]
fn eof_has_no_delayed_output_but_keeps_strict_missing_end_error() {
    let b = pool();
    let mut d = decoder(&b);
    let mut packet = frame(0x80, 1, None, [1, 2]);
    packet.truncate(packet.len() - 3);
    assert!(source(&mut d, &b, &packet, clock(None)).is_empty());
    let c = AtomicBool::new(false);
    let finalization = d.finish_input(&c);
    let error = if let Err(error) = finalization {
        error
    } else {
        loop {
            match d.poll(&mut WorkBudget::new(1), &c) {
                Ok(DisplayStatus::Yield) => {}
                Err(e) => break e,
                other => panic!("{other:?}"),
            }
        }
    };
    assert_eq!(error, Error::Truncated);
    let eof = RecoveryEof::from_source_finalization(Some(error));
    assert_eq!(eof.source_error, Some(Error::Truncated));
    assert_eq!(eof.receiver_error, None);
    assert_eq!(eof.behavior, EofBehavior::NoDelayedSubtitle);
}

#[test]
fn eof_projection_never_masks_cancellation_limits_or_packet_failures() {
    for error in [
        Error::Cancelled,
        Error::ForeignPool,
        Error::Overflow,
        Error::Limit {
            requested: 2,
            maximum: 1,
        },
        Error::Invalid("source packet failure"),
    ] {
        let eof = RecoveryEof::from_source_finalization(Some(error));
        assert_eq!(eof.source_error, Some(error));
        assert_eq!(eof.receiver_error, Some(error));
        assert_eq!(eof.behavior, EofBehavior::UnsupportedSourceFailure);
    }
    assert_eq!(
        RecoveryEof::from_source_finalization(None).source_error,
        None
    );
}

#[test]
fn zero_work_never_allocates_or_consumes_source_work() {
    let b = pool();
    let mut d = decoder(&b);
    let set = take_source(&mut d, &b, &frame(0x80, 1, None, [1, 2]));
    let mut p = Projector::new(&b, usize::MAX);
    let mut set = Some(set);
    p.push(&mut set, &AtomicBool::new(false)).unwrap();
    let before = (b.used(), p.work_spent());
    for _ in 0..100 {
        assert_eq!(
            p.poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            ProjectionStatus::Yield
        );
        assert_eq!((b.used(), p.work_spent()), before);
    }
}

#[test]
fn pressure_at_allocations_and_freeze_is_retryable_without_work_recharge() {
    let b = pool();
    let mut d = decoder(&b);
    let set = take_source(&mut d, &b, &frame(0x80, 1, None, [1, 2]));
    let mut p = Projector::new(&b, usize::MAX);
    let c = AtomicBool::new(false);
    let mut set = Some(set);
    p.push(&mut set, &c).unwrap();
    let mut pressure = 0;
    let output = loop {
        let blocker = b.reserve(b.available()).unwrap();
        match p.poll(&mut WorkBudget::new(1), &c) {
            Err(Error::Backpressure { .. }) => {
                pressure += 1;
                let before = (b.used(), p.work_spent());
                for _ in 0..2 {
                    assert!(matches!(
                        p.poll(&mut WorkBudget::new(1), &c),
                        Err(Error::Backpressure { .. })
                    ));
                    assert_eq!((b.used(), p.work_spent()), before);
                }
                drop(blocker);
                let before = (b.used(), p.work_spent());
                assert_eq!(
                    p.poll(&mut WorkBudget::new(0), &c).unwrap(),
                    ProjectionStatus::Yield
                );
                assert_eq!((b.used(), p.work_spent()), before);
                p.poll(&mut WorkBudget::new(1), &c).unwrap();
            }
            Ok(ProjectionStatus::Ready) => {
                drop(blocker);
                break p.take_output(&c).unwrap();
            }
            Ok(ProjectionStatus::Yield) => drop(blocker),
            other => panic!("{other:?}"),
        }
    };
    assert!(pressure >= 4, "allocation/freeze sites {pressure}");
    assert_eq!(
        output.rectangles()[0].argb_pixels().as_ref(),
        &[255, 0, 0, 0, 255, 255, 255, 255]
    );
}

#[test]
fn busy_push_retains_exact_caller_source_owner() {
    let b = pool();
    let mut d = decoder(&b);
    let first = take_source(&mut d, &b, &frame(0x80, 1, None, [1, 2]));
    let second = take_source(&mut d, &b, &frame(0, 2, None, [2, 1]));
    let pointer = second.objects()[0].pixels().as_ptr();
    let mut p = Projector::new(&b, usize::MAX);
    let c = AtomicBool::new(false);
    p.push(&mut Some(first), &c).unwrap();
    let mut second = Some(second);
    assert!(matches!(
        p.push(&mut second, &c),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(
        second.as_ref().unwrap().objects()[0].pixels().as_ptr(),
        pointer
    );
    assert_eq!(boundary(&mut p, 1, &c).unwrap(), ProjectionStatus::Ready);
    let first = p.take_output(&c).unwrap();
    p.push(&mut second, &c).unwrap();
    assert!(second.is_none());
    assert_eq!(first.source().objects()[0].pixels().as_ref(), &[1, 2]);
}

#[test]
fn foreign_source_is_retained_and_failure_is_sticky() {
    let b = pool();
    let foreign = pool();
    let mut d = decoder(&foreign);
    let set = take_source(&mut d, &foreign, &frame(0x80, 1, None, [1, 2]));
    let ptr = set.objects()[0].pixels().as_ptr();
    let mut set = Some(set);
    let mut p = Projector::new(&b, usize::MAX);
    let c = AtomicBool::new(false);
    assert_eq!(p.push(&mut set, &c), Err(Error::ForeignPool));
    assert_eq!(set.as_ref().unwrap().objects()[0].pixels().as_ptr(), ptr);
    assert_eq!(p.poll(&mut WorkBudget::new(0), &c), Err(Error::ForeignPool));
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn cancellation_is_sticky_at_many_phases_and_retained_outputs_survive() {
    for steps in [0, 1, 2, 64, 65, 66, 100, 150, 200, 240, 260, 350] {
        let b = pool();
        let mut d = decoder(&b);
        let mut p = Projector::new(&b, usize::MAX);
        let retained = project(
            &mut p,
            take_source(&mut d, &b, &frame(0x80, 1, None, [1, 2])),
            1,
        );
        let mut next = Some(take_source(&mut d, &b, &frame(0, 2, None, [2, 1])));
        let c = AtomicBool::new(false);
        p.push(&mut next, &c).unwrap();
        for _ in 0..steps {
            p.poll(&mut WorkBudget::new(1), &c).unwrap();
        }
        c.store(true, Ordering::Relaxed);
        assert_eq!(p.poll(&mut WorkBudget::new(0), &c), Err(Error::Cancelled));
        c.store(false, Ordering::Relaxed);
        assert_eq!(p.poll(&mut WorkBudget::new(100), &c), Err(Error::Cancelled));
        assert_eq!(retained.source().objects()[0].pixels().as_ref(), &[1, 2]);
        assert_eq!(retained.rectangles()[0].pixels().as_ref(), &[1, 2]);
        drop(p);
        drop(d);
        let retained_charge = b.used();
        assert!(retained_charge > Budget::control_storage_bytes());
        drop(retained);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn work_is_quantum_invariant_and_exact_total_limit_is_enforced() {
    let mut totals = Vec::new();
    for q in [1, 2, 3, 7, 65, 4096] {
        let b = pool();
        let mut d = decoder(&b);
        let mut p = Projector::new(&b, usize::MAX);
        let output = project(
            &mut p,
            take_source(&mut d, &b, &frame(0x80, 1, None, [1, 2])),
            q,
        );
        assert_eq!(output.rectangles()[0].pixels().as_ref(), &[1, 2]);
        totals.push(p.work_spent());
    }
    assert!(totals.iter().all(|&n| n == totals[0]));
    for maximum in [totals[0] - 1, totals[0]] {
        let b = pool();
        let mut d = decoder(&b);
        let mut p = Projector::new(&b, maximum);
        let mut set = Some(take_source(&mut d, &b, &frame(0x80, 1, None, [1, 2])));
        let c = AtomicBool::new(false);
        p.push(&mut set, &c).unwrap();
        let result = boundary(&mut p, 1, &c);
        if maximum == totals[0] {
            assert_eq!(result.unwrap(), ProjectionStatus::Ready);
        } else {
            assert!(matches!(result, Err(Error::Limit { .. })));
            assert_eq!(p.poll(&mut WorkBudget::new(0), &c), result);
        }
    }
}

#[test]
fn explicit_reset_releases_private_history_and_restarts_after_sticky_cancel() {
    let b = pool();
    let mut d = decoder(&b);
    let mut p = Projector::new(&b, usize::MAX);
    let retained = project(
        &mut p,
        take_source(&mut d, &b, &frame(0x80, 1, None, [1, 2])),
        1,
    );
    assert_eq!(
        p.poll(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    );
    p.reset(&AtomicBool::new(false)).unwrap();
    let next = project(
        &mut p,
        take_source(&mut d, &b, &frame(0, 2, None, [2, 1])),
        1,
    );
    assert_eq!(next.rectangles()[0].pixels().as_ref(), &[2, 1]);
    drop(p);
    drop(d);
    assert_eq!(retained.rectangles()[0].pixels().as_ref(), &[1, 2]);
    drop(next);
    drop(retained);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn hidden_third_reference_keeps_native_image_for_later_rejected_first() {
    let b = pool();
    let mut d = decoder(&b);
    let mut p = Projector::new(&b, usize::MAX);
    let mut packet = pcs(0x80, 1, &[8, 9, 7]);
    packet.extend(pds(1, 16));
    for id in [8, 9, 7] {
        packet.extend(ods(id, 1, 0xc0, 4, &[1, 2, 0, 0]));
    }
    packet.extend(seg(0x80, &[]));
    let first = project(&mut p, take_source(&mut d, &b, &packet), 1);
    assert_eq!(first.rectangles().len(), 2);
    assert_eq!(first.source().objects().len(), 3);
    let later = project(
        &mut p,
        take_source(&mut d, &b, &frame(0, 2, Some(0), [2, 1])),
        1,
    );
    assert_eq!(later.source().objects()[0].pixels().as_ref(), &[2, 1]);
    assert_eq!(later.rectangles()[0].pixels().as_ref(), &[1, 2]);
    assert_eq!(
        later.rectangles()[0].origin,
        RasterOrigin::RetainedBeforeRejectedFirst
    );
}

#[test]
fn unreferenced_preload_keeps_native_image_for_later_rejected_first() {
    let b = pool();
    let mut d = decoder(&b);
    let mut p = Projector::new(&b, usize::MAX);
    let mut packet = pcs(0x80, 1, &[]);
    packet.extend(pds(1, 16));
    packet.extend(ods(7, 1, 0xc0, 4, &[1, 2, 0, 0]));
    packet.extend(seg(0x80, &[]));
    assert!(
        project(&mut p, take_source(&mut d, &b, &packet), 1)
            .rectangles()
            .is_empty()
    );
    let later = project(
        &mut p,
        take_source(&mut d, &b, &frame(0, 2, Some(0), [2, 1])),
        1,
    );
    assert_eq!(later.source().objects()[0].pixels().as_ref(), &[2, 1]);
    assert_eq!(later.rectangles()[0].pixels().as_ref(), &[1, 2]);
    assert_eq!(
        later.rectangles()[0].origin,
        RasterOrigin::RetainedBeforeRejectedFirst
    );
}

#[test]
fn fragmented_unreferenced_preloads_are_quantum_invariant_across_clear_sets() {
    let mut totals = Vec::new();
    for q in [1, 2, 3, 7, 65, 4096] {
        let b = pool();
        let mut d = decoder(&b);
        let mut p = Projector::new(&b, usize::MAX);
        let mut first = pcs(0x80, 1, &[]);
        first.extend(pds(1, 16));
        first.extend(ods(7, 1, 0x80, 4, &[1]));
        first.extend(seg(0x80, &[]));
        assert!(
            project(&mut p, take_source(&mut d, &b, &first), q)
                .rectangles()
                .is_empty()
        );
        let mut next = pcs(0, 2, &[]);
        next.extend(ods(7, 1, 0x40, 4, &[2, 0, 0]));
        next.extend(seg(0x80, &[]));
        assert!(
            project(&mut p, take_source(&mut d, &b, &next), q)
                .rectangles()
                .is_empty()
        );
        let out = project(
            &mut p,
            take_source(&mut d, &b, &frame(0, 3, Some(0), [2, 1])),
            q,
        );
        assert_eq!(out.rectangles()[0].pixels().as_ref(), &[1, 2]);
        assert_eq!(out.source().objects()[0].pixels().as_ref(), &[2, 1]);
        totals.push(p.work_spent());
    }
    assert!(totals.iter().all(|&n| n == totals[0]));
}

#[test]
fn unreferenced_assembly_raster_pressure_and_freeze_restore_every_owner() {
    let b = pool();
    let mut d = decoder(&b);
    let mut p = Projector::new(&b, usize::MAX);
    let mut packet = pcs(0x80, 1, &[]);
    packet.extend(pds(1, 16));
    packet.extend(ods(7, 1, 0x80, 4, &[1]));
    packet.extend(ods(7, 1, 0x40, 4, &[2, 0, 0]));
    packet.extend(seg(0x80, &[]));
    let mut set = Some(take_source(&mut d, &b, &packet));
    let c = AtomicBool::new(false);
    p.push(&mut set, &c).unwrap();
    let mut pressure = 0;
    loop {
        let blocker = b.reserve(b.available()).unwrap();
        match p.poll(&mut WorkBudget::new(1), &c) {
            Err(Error::Backpressure { .. }) => {
                pressure += 1;
                let before = (p.work_spent(), b.used());
                for _ in 0..3 {
                    assert!(matches!(
                        p.poll(&mut WorkBudget::new(1), &c),
                        Err(Error::Backpressure { .. })
                    ));
                    assert_eq!((p.work_spent(), b.used()), before);
                }
                drop(blocker);
                p.poll(&mut WorkBudget::new(1), &c).unwrap();
            }
            Ok(ProjectionStatus::Yield) => drop(blocker),
            Ok(ProjectionStatus::Ready) => {
                drop(blocker);
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    // A released pressure retry may use its one remaining work unit for the
    // next child reserve/allocation. Count observable pressure boundaries,
    // rather than assuming every delegated allocation has its own poll.
    assert!(
        pressure >= 4,
        "observed assembly/raster pressure boundaries {pressure}"
    );
    drop(p.take_output(&c).unwrap());
    let out = project(
        &mut p,
        take_source(&mut d, &b, &frame(0, 2, Some(0), [2, 1])),
        1,
    );
    assert_eq!(out.rectangles()[0].pixels().as_ref(), &[1, 2]);
    drop(out);
    drop(p);
    drop(d);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn cancellation_during_unreferenced_assembly_and_raster_releases_pending_owners() {
    for steps in [64, 65, 128, 192, 256, 260, 270, 280, 300, 400, 500] {
        let b = pool();
        let mut d = decoder(&b);
        let mut p = Projector::new(&b, usize::MAX);
        let mut packet = pcs(0x80, 1, &[]);
        packet.extend(pds(1, 16));
        packet.extend(ods(7, 1, 0x80, 4, &[1]));
        packet.extend(ods(7, 1, 0x40, 4, &[2, 0, 0]));
        packet.extend(seg(0x80, &[]));
        let mut set = Some(take_source(&mut d, &b, &packet));
        let c = AtomicBool::new(false);
        p.push(&mut set, &c).unwrap();
        for _ in 0..steps {
            p.poll(&mut WorkBudget::new(1), &c).unwrap();
        }
        c.store(true, Ordering::Relaxed);
        assert_eq!(p.poll(&mut WorkBudget::new(0), &c), Err(Error::Cancelled));
        c.store(false, Ordering::Relaxed);
        assert_eq!(p.poll(&mut WorkBudget::new(0), &c), Err(Error::Cancelled));
        drop(p);
        drop(d);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}
