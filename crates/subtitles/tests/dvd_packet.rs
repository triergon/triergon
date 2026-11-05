use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::dvd::{
    SpuEncoding, SpuPacket, SpuPacketJob, SpuPacketLimits, SpuPacketStatus, SpuTailReason,
};

fn finish(job: &mut SpuPacketJob, quantum: u64) -> (SpuPacket, u64) {
    let cancel = AtomicBool::new(false);
    let mut spent = 0;
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(quantum);
        let status = job.poll(&mut work, &cancel).unwrap();
        assert!(work.spent() <= quantum);
        spent += work.spent();
        if status == SpuPacketStatus::Ready {
            return (job.try_finish(&cancel).unwrap(), spent);
        }
    }
    panic!("bounded source packet framing did not finish");
}
fn parse(raw: &[u8], quantum: u64) -> (SpuPacket, u64) {
    let budget = Budget::new(1 << 20).unwrap();
    let mut job = SpuPacketJob::new(
        Bytes::copy_from_slice(&budget, raw).unwrap(),
        &budget,
        SpuPacketLimits::default(),
    )
    .unwrap();
    finish(&mut job, quantum)
}

#[test]
fn every_concatenated_unit_retains_its_bytes_order_and_physical_offset() {
    let raw = [0, 4, 0, 0, 0, 7, 0, 0, 0x11, 0x22, 0x33, 0, 5, 0, 0, 0x99];
    for quantum in [1, 4096] {
        let (packet, spent) = parse(&raw, quantum);
        assert_eq!(spent, 3);
        assert_eq!(packet.tail(), None);
        assert_eq!(packet.raw().as_ref(), raw);
        assert_eq!(packet.units().len(), 3);
        for (i, range) in [0..4, 4..11, 11..16].into_iter().enumerate() {
            assert_eq!(packet.units()[i].offset, range.start);
            assert_eq!(packet.units()[i].bytes, range.len());
            assert_eq!(packet.unit(i).unwrap().as_ref(), &raw[range]);
        }
        assert!(
            packet
                .units()
                .iter()
                .all(|s| s.encoding == SpuEncoding::Traditional)
        );
    }
}

#[test]
fn extended_units_are_framed_without_claiming_their_commands_are_valid() {
    let raw = [0, 4, 0, 0, 0, 0, 0, 0, 0, 10, 0, 0, 0, 0];
    let (packet, _) = parse(&raw, 1);
    assert_eq!(packet.units().len(), 2);
    assert_eq!(packet.units()[1].encoding, SpuEncoding::Extended);
    assert_eq!(packet.unit(1).unwrap().as_ref(), &raw[4..]);
}

#[test]
fn incomplete_tail_does_not_erase_the_first_complete_source_unit() {
    let raw = [0, 4, 0, 0, 0, 9, 0, 0, 0x77];
    let (packet, _) = parse(&raw, 1);
    assert_eq!(packet.units().len(), 1);
    assert_eq!(packet.tail().unwrap().offset, 4);
    assert_eq!(
        packet.tail().unwrap().reason,
        SpuTailReason::Incomplete {
            declared: 9,
            available: 5
        }
    );
    assert_eq!(packet.tail_bytes(), &raw[4..]);
    assert_eq!(packet.raw().as_ref(), raw);
}

#[test]
fn short_and_impossible_headers_are_distinct_retained_source_tails() {
    for (raw, reason) in [
        (&[0x42][..], SpuTailReason::ShortHeader),
        (&[0, 0, 0, 0][..], SpuTailReason::ShortHeader),
        (
            &[0, 3, 0][..],
            SpuTailReason::DeclaredSizeTooSmall {
                declared: 3,
                minimum: 4,
            },
        ),
        (
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0][..],
            SpuTailReason::DeclaredSizeTooSmall {
                declared: 0,
                minimum: 10,
            },
        ),
    ] {
        let (packet, _) = parse(raw, 1);
        assert_eq!(packet.tail().unwrap().reason, reason);
        assert!(packet.units().is_empty());
        assert_eq!(packet.tail_bytes(), raw);
    }
}

#[test]
fn huge_extended_declaration_allocates_no_declared_payload() {
    let raw = [0, 0, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0];
    let budget = Budget::new(512).unwrap();
    let mut job = SpuPacketJob::new(
        Bytes::copy_from_slice(&budget, &raw).unwrap(),
        &budget,
        SpuPacketLimits::default(),
    )
    .unwrap();
    let used = budget.used();
    let (packet, spent) = finish(&mut job, 1);
    assert_eq!(spent, 1);
    assert_eq!(budget.used(), used);
    assert_eq!(
        packet.tail().unwrap().reason,
        SpuTailReason::Incomplete {
            declared: usize::try_from(u32::MAX).unwrap(),
            available: 10
        }
    );
}

#[test]
fn zero_copy_unit_retains_the_whole_original_backing_after_packet_drop() {
    let raw = [0, 4, 0, 0, 0, 4, 0, 0];
    let budget = Budget::new(4096).unwrap();
    let owned = Bytes::copy_from_slice(&budget, &raw).unwrap();
    let original_charge = budget.used();
    let mut job = SpuPacketJob::new(owned, &budget, SpuPacketLimits::default()).unwrap();
    let (packet, _) = finish(&mut job, 1);
    let unit = packet.unit(1).unwrap();
    assert_eq!(unit.retained_capacity(), raw.len());
    drop(packet);
    drop(job);
    assert_eq!(budget.used(), original_charge);
    assert_eq!(unit.as_ref(), &raw[4..]);
    drop(unit);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn allocation_pressure_retries_the_same_paid_unit_without_duplicates() {
    let raw = [0, 4, 0, 0, 0, 4, 0, 0];
    let budget = Budget::new(4096).unwrap();
    let mut job = SpuPacketJob::new(
        Bytes::copy_from_slice(&budget, &raw).unwrap(),
        &budget,
        SpuPacketLimits::default(),
    )
    .unwrap();
    let blocker = budget.reserve(budget.available()).unwrap();
    let cancel = AtomicBool::new(false);
    for _ in 0..3 {
        assert!(matches!(
            job.poll(&mut WorkBudget::new(4096), &cancel),
            Err(Error::Backpressure { .. })
        ));
        assert_eq!(job.headers_processed(), 1);
    }
    drop(blocker);
    let (packet, spent) = finish(&mut job, 1);
    assert_eq!(spent, 1);
    assert_eq!(job.headers_processed(), 2);
    assert_eq!(packet.units().len(), 2);
}

#[test]
fn zero_work_and_sticky_cancellation_release_every_charged_owner() {
    let budget = Budget::new(4096).unwrap();
    let raw = [0, 4, 0, 0];
    let mut job = SpuPacketJob::new(
        Bytes::copy_from_slice(&budget, &raw).unwrap(),
        &budget,
        SpuPacketLimits::default(),
    )
    .unwrap();
    let before = budget.used();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
        SpuPacketStatus::Yield
    );
    assert_eq!(job.headers_processed(), 0);
    assert_eq!(budget.used(), before);
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        job.poll(&mut WorkBudget::new(1), &cancel).unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(budget.used(), Budget::control_storage_bytes());
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        job.poll(&mut WorkBudget::new(4096), &cancel).unwrap_err(),
        Error::Cancelled
    );
}

#[test]
fn count_and_work_caps_are_not_bypassed_by_concatenation() {
    for work_limit in [false, true] {
        let budget = Budget::new(4096).unwrap();
        let mut limits = SpuPacketLimits::default();
        if work_limit {
            limits.max_headers = 1;
        } else {
            limits.max_units = 1;
        }
        let mut job = SpuPacketJob::new(
            Bytes::copy_from_slice(&budget, &[0, 4, 0, 0, 0, 4, 0, 0]).unwrap(),
            &budget,
            limits,
        )
        .unwrap();
        assert_eq!(
            job.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false))
                .unwrap_err(),
            Error::Limit {
                requested: 2,
                maximum: 1
            }
        );
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn empty_input_and_invalid_unit_access_do_not_invent_source_frames() {
    let (packet, spent) = parse(&[], 1);
    assert_eq!(spent, 0);
    assert!(packet.units().is_empty());
    assert_eq!(
        packet.unit(0).unwrap_err(),
        Error::Invalid("DVD source unit index")
    );
    assert_eq!(packet.tail(), None);
    assert_eq!(packet.raw().as_ref(), &[]);
}
