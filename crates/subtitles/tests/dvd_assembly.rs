use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::dvd::{
    AssembledSpu, DvdPacket, SpuAssembly, SpuAssemblyLimits, SpuAssemblyStatus,
};

const SPU: &[u8] = &[
    0, 38, 0, 8, 0x45, 0x67, 0x76, 0x54, 0, 0, 0, 32, 1, 3, 0x32, 0x10, 4, 0xff, 0xff, 5, 0, 0xa0,
    0x0d, 0x01, 0x40, 0x15, 6, 0, 4, 0, 6, 0xff, 0, 0x5a, 0, 32, 2, 0xff,
];

fn drive(job: &mut SpuAssembly, quantum: u64) -> SpuAssemblyStatus {
    assert_ne!(quantum, 0);
    let cancel = AtomicBool::new(false);
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(quantum);
        let status = job.poll(&mut work, &cancel).unwrap();
        assert!(work.spent() <= quantum);
        assert!(job.pending_payloads() <= 2);
        if status != SpuAssemblyStatus::Yield {
            return status;
        }
    }
    panic!("bounded SPU did not reach an input/output boundary");
}

fn assemble(chunks: &[&[u8]], quantum: u64) -> (AssembledSpu, usize, usize) {
    let budget = Budget::new(1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = SpuAssembly::new(&budget, SpuAssemblyLimits::default());
    for (i, chunk) in chunks.iter().enumerate() {
        job.push(Bytes::copy_from_slice(&budget, chunk).unwrap(), &cancel)
            .unwrap();
        let status = drive(&mut job, quantum);
        assert_eq!(
            status,
            if i + 1 == chunks.len() {
                SpuAssemblyStatus::Ready
            } else {
                SpuAssemblyStatus::NeedFragment
            }
        );
    }
    job.finish_input(&cancel).unwrap();
    assert_eq!(drive(&mut job, quantum), SpuAssemblyStatus::Ready);
    let copied = job.copied_bytes();
    let work = job.work_spent();
    let result = job.try_finish(&cancel).unwrap();
    assert_eq!(result.packet().as_ref(), SPU);
    DvdPacket::parse(result.packet()).unwrap();
    (result, copied, work)
}

#[test]
fn complete_input_and_tail_are_zero_copy_views_of_the_original_owner() {
    let budget = Budget::new(65536).unwrap();
    let cancel = AtomicBool::new(false);
    let mut raw = SPU.to_vec();
    raw.extend_from_slice(SPU);
    raw.extend_from_slice(&[0xde, 0xad]);
    let input = Bytes::copy_from_slice(&budget, &raw).unwrap();
    let alias = input.clone();
    let charged = budget.used();
    let mut job = SpuAssembly::new(&budget, SpuAssemblyLimits::default());
    job.push(input, &cancel).unwrap();
    assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::Ready);
    assert_eq!((job.work_spent(), job.copied_bytes()), (2, 0));
    let result = job.try_finish(&cancel).unwrap();
    assert_eq!(result.packet().as_ref(), SPU);
    let tail = result.trailing().unwrap();
    assert_eq!(tail.as_ref(), &raw[SPU.len()..]);
    assert!(result.packet().shares_storage_with(&alias));
    assert!(tail.shares_storage_with(&alias));
    assert_eq!(budget.used(), charged);
    drop(job);
    drop(alias);
    assert_eq!(budget.used(), charged);
    drop(result);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn every_two_way_split_and_work_quantum_preserves_exact_bytes() {
    for split in 1..SPU.len() {
        for quantum in [1, 2, 3, 7, 65536] {
            let (result, copied, work) = assemble(&[&SPU[..split], &SPU[split..]], quantum);
            assert_eq!(copied, SPU.len());
            assert_eq!(work, SPU.len() + 4);
            assert!(result.trailing().is_none());
        }
    }
}

#[test]
fn one_byte_fragments_include_a_split_size_word_and_preserve_tail() {
    let mut chunks: Vec<&[u8]> = SPU.chunks(1).collect();
    let last = [SPU[SPU.len() - 1], 0xde, 0xad, 0xbe, 0xef];
    *chunks.last_mut().unwrap() = &last;
    for quantum in [1, 5, 4096] {
        let (result, copied, work) = assemble(&chunks, quantum);
        assert_eq!(copied, SPU.len());
        assert_eq!(work, SPU.len() * 2 + 2);
        assert_eq!(
            result.trailing().unwrap().as_ref(),
            [0xde, 0xad, 0xbe, 0xef]
        );
    }
}

#[test]
fn empty_fragments_consume_work_but_never_imply_eof() {
    let chunks: &[&[u8]] = &[b"", &SPU[..1], b"", &SPU[1..7], b"", &SPU[7..]];
    for quantum in [1, 7, 65536] {
        let (_, copied, work) = assemble(chunks, quantum);
        assert_eq!(copied, SPU.len());
        assert_eq!(work, SPU.len() + chunks.len() + 2);
    }
    let (_, copied, work) = assemble(&[b"", SPU], 1);
    assert_eq!((copied, work), (0, 3));
}

#[test]
fn zero_work_does_not_inspect_input_or_allocate() {
    let budget = Budget::new(65536).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = SpuAssembly::new(
        &budget,
        SpuAssemblyLimits {
            max_fragments: 0,
            ..SpuAssemblyLimits::default()
        },
    );
    job.push(Bytes::copy_from_slice(&budget, SPU).unwrap(), &cancel)
        .unwrap();
    let before = budget.used();
    for _ in 0..3 {
        assert_eq!(
            job.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
            SpuAssemblyStatus::Yield
        );
        assert_eq!(
            (
                job.work_spent(),
                job.bytes_received(),
                job.fragments_received()
            ),
            (0, 0, 0)
        );
        assert_eq!(budget.used(), before);
    }
    assert!(matches!(
        job.poll(&mut WorkBudget::new(1), &cancel),
        Err(Error::Limit {
            requested: 1,
            maximum: 0
        })
    ));
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn busy_submission_returns_the_exact_owner_and_can_be_retried() {
    let budget = Budget::new(65536).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = SpuAssembly::new(&budget, SpuAssemblyLimits::default());
    job.push(Bytes::copy_from_slice(&budget, &SPU[..1]).unwrap(), &cancel)
        .unwrap();
    let rest = Bytes::copy_from_slice(&budget, &SPU[1..]).unwrap();
    let alias = rest.clone();
    let rejected = job.push(rest, &cancel).unwrap_err();
    assert_eq!(
        rejected.error,
        Error::Backpressure {
            requested: 1,
            available: 0
        }
    );
    assert!(rejected.payload.shares_storage_with(&alias));
    assert_eq!(job.fragments_received(), 0);
    assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::NeedFragment);
    job.push(rejected.payload, &cancel).unwrap();
    assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::Ready);
    assert_eq!(job.try_finish(&cancel).unwrap().packet().as_ref(), SPU);
}

#[test]
fn foreign_pool_is_sticky_and_returns_the_foreign_payload() {
    let budget = Budget::new(65536).unwrap();
    let other = Budget::new(65536).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = SpuAssembly::new(&budget, SpuAssemblyLimits::default());
    job.push(Bytes::copy_from_slice(&budget, &SPU[..1]).unwrap(), &cancel)
        .unwrap();
    assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::NeedFragment);
    let input = Bytes::copy_from_slice(&other, &SPU[1..]).unwrap();
    let alias = input.clone();
    let rejected = job.push(input, &cancel).unwrap_err();
    assert_eq!(rejected.error, Error::ForeignPool);
    assert!(rejected.payload.shares_storage_with(&alias));
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &cancel).unwrap_err(),
        Error::ForeignPool
    );
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn allocation_pressure_retries_without_recharging_setup() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let first = Bytes::copy_from_slice(&budget, &SPU[..1]).unwrap();
    let second = Bytes::copy_from_slice(&budget, &SPU[1..]).unwrap();
    let mut job = SpuAssembly::new(&budget, SpuAssemblyLimits::default());
    job.push(first, &cancel).unwrap();
    assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::NeedFragment);
    job.push(second, &cancel).unwrap();
    let blocker = budget.reserve(budget.available()).unwrap();
    for _ in 0..3 {
        assert!(matches!(
            job.poll(&mut WorkBudget::new(64), &cancel),
            Err(Error::Backpressure { .. })
        ));
        assert_eq!((job.work_spent(), job.copied_bytes()), (3, 0));
        assert_eq!(
            job.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
            SpuAssemblyStatus::Yield
        );
        assert_eq!(job.work_spent(), 3);
    }
    drop(blocker);
    assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::Ready);
    assert_eq!(
        (job.work_spent(), job.copied_bytes()),
        (SPU.len() + 4, SPU.len())
    );
    assert_eq!(job.try_finish(&cancel).unwrap().packet().as_ref(), SPU);
}

#[test]
fn freeze_pressure_keeps_the_complete_buffer_and_original_tail() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = SpuAssembly::new(&budget, SpuAssemblyLimits::default());
    job.push(Bytes::copy_from_slice(&budget, &SPU[..2]).unwrap(), &cancel)
        .unwrap();
    assert_eq!(drive(&mut job, 7), SpuAssemblyStatus::NeedFragment);
    let mut last = SPU[2..].to_vec();
    last.extend_from_slice(&[9, 8, 7]);
    let input = Bytes::copy_from_slice(&budget, &last).unwrap();
    let alias = input.clone();
    job.push(input, &cancel).unwrap();
    assert_eq!(drive(&mut job, 7), SpuAssemblyStatus::Ready);
    let spent = job.work_spent();
    let blocker = budget.reserve(budget.available()).unwrap();
    for _ in 0..3 {
        assert!(matches!(
            job.try_finish(&cancel),
            Err(Error::Backpressure { .. })
        ));
        assert_eq!(job.work_spent(), spent);
        assert_eq!(job.copied_bytes(), SPU.len());
        assert_eq!(
            job.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
            SpuAssemblyStatus::Ready
        );
    }
    drop(blocker);
    let result = job.try_finish(&cancel).unwrap();
    assert_eq!(result.packet().as_ref(), SPU);
    assert_eq!(result.trailing().unwrap().as_ref(), [9, 8, 7]);
    assert!(result.trailing().unwrap().shares_storage_with(&alias));
    assert!(!result.packet().shares_storage_with(&alias));
    drop(job);
    drop(alias);
    assert_eq!(
        budget.used(),
        Budget::control_storage_bytes()
            + result.packet().retained_storage_bytes()
            + result.trailing().unwrap().retained_storage_bytes()
    );
    drop(result);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn starvation_and_a_queued_final_fragment_have_distinct_end_behavior() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = SpuAssembly::new(&budget, SpuAssemblyLimits::default());
    for _ in 0..3 {
        assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::NeedFragment);
        assert_eq!(job.work_spent(), 0);
    }
    job.push(Bytes::copy_from_slice(&budget, &SPU[..1]).unwrap(), &cancel)
        .unwrap();
    assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::NeedFragment);
    job.push(Bytes::copy_from_slice(&budget, &SPU[1..]).unwrap(), &cancel)
        .unwrap();
    job.finish_input(&cancel).unwrap();
    assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::Ready);
    assert_eq!(job.try_finish(&cancel).unwrap().packet().as_ref(), SPU);
}

#[test]
fn explicit_incomplete_eof_is_sticky_and_releases_all_partial_owners() {
    let cancel = AtomicBool::new(false);
    for queued in [false, true] {
        let budget = Budget::new(4096).unwrap();
        let mut job = SpuAssembly::new(&budget, SpuAssemblyLimits::default());
        job.push(Bytes::copy_from_slice(&budget, &SPU[..1]).unwrap(), &cancel)
            .unwrap();
        if !queued {
            assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::NeedFragment);
            assert_eq!(job.finish_input(&cancel).unwrap_err(), Error::Truncated);
        } else {
            job.finish_input(&cancel).unwrap();
        }
        assert_eq!(
            job.poll(&mut WorkBudget::new(65536), &cancel).unwrap_err(),
            Error::Truncated
        );
        assert_eq!(
            job.poll(&mut WorkBudget::new(0), &cancel).unwrap_err(),
            Error::Truncated
        );
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn declared_byte_fragment_and_structural_header_limits_are_enforced() {
    let cancel = AtomicBool::new(false);
    for size in 0..9_u16 {
        let budget = Budget::new(4096).unwrap();
        let mut job = SpuAssembly::new(&budget, SpuAssemblyLimits::default());
        job.push(
            Bytes::copy_from_slice(&budget, &size.to_be_bytes()).unwrap(),
            &cancel,
        )
        .unwrap();
        assert!(matches!(
            job.poll(&mut WorkBudget::new(2), &cancel),
            Err(Error::Invalid(_))
        ));
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
    for raw in [
        vec![0, 9, 0, 3, 0, 0, 0, 4, 0xff],
        vec![0, 9, 0, 5, 0, 0, 0, 5, 0xff],
    ] {
        let budget = Budget::new(4096).unwrap();
        let mut job = SpuAssembly::new(&budget, SpuAssemblyLimits::default());
        job.push(Bytes::copy_from_slice(&budget, &raw).unwrap(), &cancel)
            .unwrap();
        assert!(matches!(
            job.poll(&mut WorkBudget::new(2), &cancel),
            Err(Error::Invalid(_))
        ));
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
    let budget = Budget::new(4096).unwrap();
    let mut job = SpuAssembly::new(
        &budget,
        SpuAssemblyLimits {
            max_packet: SPU.len() - 1,
            ..SpuAssemblyLimits::default()
        },
    );
    job.push(Bytes::copy_from_slice(&budget, SPU).unwrap(), &cancel)
        .unwrap();
    assert_eq!(
        job.poll(&mut WorkBudget::new(1), &cancel).unwrap_err(),
        Error::Limit {
            requested: SPU.len(),
            maximum: SPU.len() - 1
        }
    );
}

#[test]
fn empty_fragments_also_obey_the_fragment_ceiling() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = SpuAssembly::new(
        &budget,
        SpuAssemblyLimits {
            max_fragments: 1,
            ..SpuAssemblyLimits::default()
        },
    );
    job.push(Bytes::copy_from_slice(&budget, b"").unwrap(), &cancel)
        .unwrap();
    assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::NeedFragment);
    job.push(Bytes::copy_from_slice(&budget, b"").unwrap(), &cancel)
        .unwrap();
    assert_eq!(
        job.poll(&mut WorkBudget::new(1), &cancel).unwrap_err(),
        Error::Limit {
            requested: 2,
            maximum: 1
        }
    );
    assert_eq!(job.fragments_received(), 1);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn a_copy_work_limit_does_not_publish_partial_output() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let limit = SPU.len() + 3;
    let mut job = SpuAssembly::new(
        &budget,
        SpuAssemblyLimits {
            max_work: limit,
            ..SpuAssemblyLimits::default()
        },
    );
    job.push(Bytes::copy_from_slice(&budget, &SPU[..1]).unwrap(), &cancel)
        .unwrap();
    assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::NeedFragment);
    job.push(Bytes::copy_from_slice(&budget, &SPU[1..]).unwrap(), &cancel)
        .unwrap();
    let mut observed = None;
    for _ in 0..1000 {
        match job.poll(&mut WorkBudget::new(1), &cancel) {
            Ok(SpuAssemblyStatus::Yield) => (),
            Err(error) => {
                observed = Some(error);
                break;
            }
            other => panic!("unexpected limit outcome {other:?}"),
        }
    }
    assert_eq!(
        observed,
        Some(Error::Limit {
            requested: limit + 1,
            maximum: limit
        })
    );
    assert_eq!(job.work_spent(), limit);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
    assert_eq!(job.try_finish(&cancel).unwrap_err(), observed.unwrap());
}

#[test]
fn cancellation_at_input_copy_and_ready_is_sticky_until_reset() {
    for stage in 0..3 {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut job = SpuAssembly::new(&budget, SpuAssemblyLimits::default());
        job.push(Bytes::copy_from_slice(&budget, &SPU[..1]).unwrap(), &cancel)
            .unwrap();
        if stage != 0 {
            assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::NeedFragment);
            job.push(Bytes::copy_from_slice(&budget, &SPU[1..]).unwrap(), &cancel)
                .unwrap();
            if stage == 1 {
                assert_eq!(
                    job.poll(&mut WorkBudget::new(4), &cancel).unwrap(),
                    SpuAssemblyStatus::Yield
                );
                assert!(job.copied_bytes() > 0);
            } else {
                assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::Ready);
            }
        }
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            job.poll(&mut WorkBudget::new(0), &cancel).unwrap_err(),
            Error::Cancelled
        );
        cancel.store(false, Ordering::Relaxed);
        assert_eq!(
            job.poll(&mut WorkBudget::new(1), &cancel).unwrap_err(),
            Error::Cancelled
        );
        assert_eq!(budget.used(), Budget::control_storage_bytes());
        job.reset();
        assert_eq!(
            (
                job.work_spent(),
                job.fragments_received(),
                job.pending_payloads()
            ),
            (0, 0, 0)
        );
        job.push(Bytes::copy_from_slice(&budget, SPU).unwrap(), &cancel)
            .unwrap();
        assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::Ready);
        assert_eq!(job.try_finish(&cancel).unwrap().packet().as_ref(), SPU);
    }
}

#[test]
fn reset_can_consume_multiple_packets_without_invalidating_retained_output() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut raw = SPU.to_vec();
    raw.extend_from_slice(SPU);
    let input = Bytes::copy_from_slice(&budget, &raw).unwrap();
    let mut job = SpuAssembly::new(&budget, SpuAssemblyLimits::default());
    job.push(input, &cancel).unwrap();
    assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::Ready);
    let (first, tail) = job.try_finish(&cancel).unwrap().into_parts();
    let charged = budget.used();
    job.reset();
    job.push(tail.unwrap(), &cancel).unwrap();
    assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::Ready);
    let second = job.try_finish(&cancel).unwrap();
    assert_eq!(first.as_ref(), SPU);
    assert_eq!(second.packet().as_ref(), SPU);
    assert!(second.packet().shares_storage_with(&first));
    assert_eq!(budget.used(), charged);
    drop(job);
    drop(first);
    drop(second);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn premature_finish_is_sticky_and_reset_discards_private_partial_state() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = SpuAssembly::new(&budget, SpuAssemblyLimits::default());
    job.push(Bytes::copy_from_slice(&budget, &SPU[..1]).unwrap(), &cancel)
        .unwrap();
    assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::NeedFragment);
    let error = job.try_finish(&cancel).unwrap_err();
    assert!(matches!(error, Error::Invalid(_)));
    assert_eq!(
        job.poll(&mut WorkBudget::new(1), &cancel).unwrap_err(),
        error
    );
    assert_eq!(budget.used(), Budget::control_storage_bytes());
    job.reset();
    assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::NeedFragment);
    assert_eq!(job.declared_size(), None);
}

#[test]
fn maximum_size_packet_is_copied_once_under_the_default_work_ceiling() {
    let budget = Budget::new(1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut raw = vec![0; usize::from(u16::MAX)];
    raw[..9].copy_from_slice(&[0xff, 0xff, 0, 4, 0, 0, 0, 4, 0xff]);
    let mut job = SpuAssembly::new(&budget, SpuAssemblyLimits::default());
    job.push(Bytes::copy_from_slice(&budget, &raw[..1]).unwrap(), &cancel)
        .unwrap();
    assert_eq!(drive(&mut job, 1), SpuAssemblyStatus::NeedFragment);
    job.push(Bytes::copy_from_slice(&budget, &raw[1..]).unwrap(), &cancel)
        .unwrap();
    assert_eq!(drive(&mut job, 4096), SpuAssemblyStatus::Ready);
    assert_eq!(
        (job.copied_bytes(), job.work_spent()),
        (raw.len(), raw.len() + 4)
    );
    assert_eq!(job.try_finish(&cancel).unwrap().packet().as_ref(), raw);
}
