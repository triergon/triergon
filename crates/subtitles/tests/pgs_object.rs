//! Original ODS-payload assembly guards. Wire assembly is separate from RLE
//! validity, display sets, caches, timestamp unwrapping and reference parity.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::pgs::{
    AssemblyLimits, AssemblyStatus, EncodedObject, ObjectAssembly, ObjectFragment, RasterJob,
    RasterLimits, RasterStatus,
};

const ID: u16 = 0x1234;
const VERSION: u8 = 7;

fn first(flags: u8, encoded: usize, dimensions: (u16, u16), data: &[u8]) -> Vec<u8> {
    let mut payload = ID.to_be_bytes().to_vec();
    payload.extend_from_slice(&[VERSION, flags]);
    let length = u32::try_from(encoded).unwrap().checked_add(4).unwrap();
    assert!(length <= 0xff_ffff);
    payload.extend_from_slice(&length.to_be_bytes()[1..]);
    payload.extend_from_slice(&dimensions.0.to_be_bytes());
    payload.extend_from_slice(&dimensions.1.to_be_bytes());
    payload.extend_from_slice(data);
    payload
}

fn next(flags: u8, data: &[u8]) -> Vec<u8> {
    let mut payload = ID.to_be_bytes().to_vec();
    payload.extend_from_slice(&[VERSION, flags]);
    payload.extend_from_slice(data);
    payload
}

fn owned(budget: &Budget, payload: &[u8]) -> Bytes {
    Bytes::copy_from_slice(budget, payload).unwrap()
}

fn boundary(
    assembly: &mut ObjectAssembly,
    quantum: u64,
    cancel: &AtomicBool,
) -> Result<AssemblyStatus, Error> {
    assert_ne!(quantum, 0);
    for _ in 0..100_000 {
        let before_work = assembly.work_spent();
        let before_copy = assembly.copied_bytes();
        let mut work = WorkBudget::new(quantum);
        let status = assembly.poll(&mut work, cancel);
        assert_eq!(assembly.work_spent() - before_work, work.spent() as usize);
        assert!(assembly.copied_bytes() - before_copy <= work.spent() as usize);
        assert!(assembly.pending_payloads() <= 2);
        match status {
            Ok(AssemblyStatus::Yield) => {}
            other => return other,
        }
    }
    panic!("assembly failed to reach a bounded input/output boundary");
}

fn sticky(assembly: &mut ObjectAssembly, error: Error, budget: &Budget) {
    let cancel = AtomicBool::new(false);
    assert_eq!(assembly.pending_payloads(), 0);
    assert_eq!(assembly.poll(&mut WorkBudget::new(0), &cancel), Err(error));
    assert_eq!(assembly.poll(&mut WorkBudget::new(99), &cancel), Err(error));
    assert_eq!(assembly.try_finish(&cancel).unwrap_err(), error);
    assert_eq!(assembly.finish_input(&cancel), Err(error));
    let input = owned(budget, &first(0xc0, 1, (1, 1), &[1]));
    let pointer = input.as_ptr();
    let failure = assembly.push(input, &cancel).unwrap_err();
    assert_eq!(failure.error, error);
    assert_eq!(failure.payload.as_ptr(), pointer);
    drop(failure);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

fn assemble_parts(encoded: &[u8], cuts: &[usize], quantum: u64) -> (EncodedObject, usize) {
    let budget = Budget::new(256 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
    let mut start = 0;
    let mut ends = cuts.to_vec();
    ends.push(encoded.len());
    for (index, &end) in ends.iter().enumerate() {
        let last = index + 1 == ends.len();
        let payload = if index == 0 {
            first(
                if last { 0xc0 } else { 0x80 },
                encoded.len(),
                (4, 2),
                &encoded[start..end],
            )
        } else {
            next(if last { 0x40 } else { 0 }, &encoded[start..end])
        };
        assembly.push(owned(&budget, &payload), &cancel).unwrap();
        assert_eq!(
            boundary(&mut assembly, quantum, &cancel).unwrap(),
            if last {
                AssemblyStatus::Ready
            } else {
                AssemblyStatus::NeedFragment
            }
        );
        start = end;
    }
    assert_eq!(assembly.fragments_received(), ends.len());
    assert_eq!(assembly.encoded_received(), encoded.len());
    assert_eq!(
        assembly.copied_bytes(),
        if cuts.is_empty() { 0 } else { encoded.len() }
    );
    let expected_work = if cuts.is_empty() {
        1
    } else {
        ends.len() + 1 + encoded.len()
    };
    assert_eq!(assembly.work_spent(), expected_work);
    assembly.finish_input(&cancel).unwrap();
    let object = assembly.try_finish(&cancel).unwrap();
    assert_eq!(object.encoded().as_ref(), encoded);
    assert_eq!(
        (object.id, object.version, object.width, object.height),
        (ID, VERSION, 4, 2)
    );
    assert_eq!(object.data_length as usize, encoded.len() + 4);
    drop(assembly);
    assert_eq!(
        budget.used(),
        Budget::control_storage_bytes() + object.encoded().retained_storage_bytes()
    );
    (object, expected_work)
}

#[test]
fn payload_parser_borrows_without_a_fabricated_segment_and_preserves_raw_flags() {
    for flags in 0x80..=0xff {
        let payload = first(flags, 2, (2, 1), &[1, 2]);
        let fragment = ObjectFragment::parse_payload(&payload).unwrap();
        assert_eq!(
            (fragment.id, fragment.version, fragment.flags),
            (ID, VERSION, flags)
        );
        assert_eq!(fragment.last, flags & 0x40 != 0);
        assert_eq!(fragment.first.unwrap().data_length, 6);
        assert_eq!(fragment.data.as_ptr(), payload[11..].as_ptr());
        assert_eq!(fragment.data, [1, 2]);
    }
    let payload = next(0x40, &[3, 4]);
    let fragment = ObjectFragment::parse_payload(&payload).unwrap();
    assert!(fragment.first.is_none());
    assert_eq!(fragment.data.as_ptr(), payload[4..].as_ptr());
}

#[test]
fn complete_c0_is_a_consuming_view_at_zero_free_bytes_including_empty_and_nested_views() {
    for data in [&[][..], &[1, 2][..]] {
        let budget = Budget::new(16 * 1024).unwrap();
        let baseline = budget.used();
        let payload = first(0xc0, data.len(), (2, 1), data);
        let mut backing = Vec::with_capacity(4096);
        backing.extend_from_slice(b"prefix!");
        backing.extend_from_slice(&payload);
        backing.extend_from_slice(b"hidden suffix");
        let original = Bytes::try_from_vec(&budget, backing).unwrap();
        let alias = original.clone();
        let input = original.into_slice(7..7 + payload.len()).unwrap();
        let pointer = input[11..].as_ptr();
        let capacity = input.retained_capacity();
        let charge = input.retained_storage_bytes();
        let blocker = budget.reserve(budget.available()).unwrap();
        let before = budget.used();
        let cancel = AtomicBool::new(false);
        let mut assembly = ObjectAssembly::new(
            &budget,
            AssemblyLimits {
                max_encoded: data.len(),
                max_work: 1,
                max_fragments: 1,
                ..AssemblyLimits::default()
            },
        );
        assert_eq!(budget.used(), before);
        assembly.push(input, &cancel).unwrap();
        assert_eq!(
            assembly.poll(&mut WorkBudget::new(1), &cancel).unwrap(),
            AssemblyStatus::Ready
        );
        assert_eq!((assembly.work_spent(), assembly.copied_bytes()), (1, 0));
        let object = assembly.try_finish(&cancel).unwrap();
        assert_eq!(object.encoded().as_ref(), data);
        assert_eq!(object.encoded().as_ptr(), pointer);
        assert!(object.encoded().shares_storage_with(&alias));
        assert_eq!(object.encoded().retained_capacity(), capacity);
        assert_eq!(object.encoded().retained_storage_bytes(), charge);
        assert_eq!(budget.used(), before);
        drop(assembly);
        drop(alias);
        drop(blocker);
        assert_eq!(budget.used(), baseline + charge);
        drop(object);
        assert_eq!(budget.used(), baseline);
    }
}

#[test]
fn every_two_fragment_split_and_quantum_preserves_literal_rle_and_decoded_indices() {
    let encoded = [1, 2, 0, 2, 0, 0, 0, 0x84, 9];
    for split in 0..=encoded.len() {
        for quantum in [1, 2, 3, 7, 4096, u64::MAX] {
            let (object, _) = assemble_parts(&encoded, &[split], quantum);
            let budget = Budget::new(128 * 1024).unwrap();
            // Raster requires its own pool. This copy belongs to this separate
            // decoding oracle, outside the assembly copy/work assertions.
            let mut raster = RasterJob::new(
                owned(&budget, object.encoded()),
                4,
                2,
                &budget,
                RasterLimits::default(),
            )
            .unwrap();
            let cancel = AtomicBool::new(false);
            for _ in 0..100 {
                if raster.poll(&mut WorkBudget::new(quantum), &cancel).unwrap()
                    == RasterStatus::Ready
                {
                    break;
                }
            }
            assert_eq!(
                raster.try_finish(&cancel).unwrap().pixels().as_ref(),
                &[1, 2, 0, 0, 9, 9, 9, 9]
            );
        }
    }
}

#[test]
fn all_partitions_and_quanta_copy_each_encoded_byte_once() {
    let encoded = [1, 2, 0, 2, 0, 0, 0, 0x84, 9];
    for mask in 0usize..1 << (encoded.len() - 1) {
        let cuts = (1..encoded.len())
            .filter(|at| mask & (1 << (at - 1)) != 0)
            .collect::<Vec<_>>();
        for quantum in [1, 2, 3, 7, 4096, u64::MAX] {
            assemble_parts(&encoded, &cuts, quantum);
        }
    }
}

#[test]
fn large_copies_are_chunked_and_each_written_byte_consumes_work() {
    let budget = Budget::new(256 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let data = vec![7; 20_000];
    let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
    assembly
        .push(
            owned(&budget, &first(0x80, data.len(), (1, 1), &data[..10_000])),
            &cancel,
        )
        .unwrap();
    assert_eq!(
        boundary(&mut assembly, 1, &cancel).unwrap(),
        AssemblyStatus::NeedFragment
    );
    assembly
        .push(owned(&budget, &next(0x40, &data[10_000..])), &cancel)
        .unwrap();
    assert_eq!(
        assembly
            .poll(&mut WorkBudget::new(u64::MAX), &cancel)
            .unwrap(),
        AssemblyStatus::Yield
    );
    assert_eq!(assembly.copied_bytes(), 4096);
    assert_eq!(assembly.work_spent(), 3 + 4096);
    let before = (
        assembly.copied_bytes(),
        assembly.work_spent(),
        budget.used(),
    );
    assert_eq!(
        assembly.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
        AssemblyStatus::Yield
    );
    assert_eq!(
        (
            assembly.copied_bytes(),
            assembly.work_spent(),
            budget.used()
        ),
        before
    );
    assert_eq!(
        boundary(&mut assembly, u64::MAX, &cancel).unwrap(),
        AssemblyStatus::Ready
    );
    assert_eq!(assembly.copied_bytes(), data.len());
    assert_eq!(assembly.work_spent(), data.len() + 3);
    assert_eq!(
        assembly.try_finish(&cancel).unwrap().encoded().as_ref(),
        data
    );
}

#[test]
fn pending_slot_pressure_returns_exact_owner_and_never_retains_a_third_payload() {
    let budget = Budget::new(16 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
    let first_input = owned(&budget, &first(0x80, 3, (3, 1), &[1]));
    let second = owned(&budget, &next(0, &[2]));
    let third = owned(&budget, &next(0x40, &[3]));
    let second_pointer = second.as_ptr();
    let third_pointer = third.as_ptr();
    assembly.push(first_input, &cancel).unwrap();
    let failure = assembly.push(second, &cancel).unwrap_err();
    assert_eq!(
        failure.error,
        Error::Backpressure {
            requested: 1,
            available: 0
        }
    );
    assert_eq!(failure.payload.as_ptr(), second_pointer);
    assert_eq!(assembly.pending_payloads(), 1);
    assert_eq!(
        boundary(&mut assembly, 1, &cancel).unwrap(),
        AssemblyStatus::NeedFragment
    );
    assembly.push(failure.payload, &cancel).unwrap();
    assert_eq!(
        assembly.poll(&mut WorkBudget::new(2), &cancel).unwrap(),
        AssemblyStatus::Yield
    );
    assert_eq!(
        (assembly.pending_payloads(), assembly.copied_bytes()),
        (2, 0)
    );
    let before = (assembly.work_spent(), budget.used());
    let failure = assembly.push(third, &cancel).unwrap_err();
    assert_eq!(
        failure.error,
        Error::Backpressure {
            requested: 1,
            available: 0
        }
    );
    assert_eq!(failure.payload.as_ptr(), third_pointer);
    assert_eq!((assembly.work_spent(), budget.used()), before);
    assert_eq!(
        boundary(&mut assembly, 1, &cancel).unwrap(),
        AssemblyStatus::NeedFragment
    );
    assembly.push(failure.payload, &cancel).unwrap();
    assert_eq!(
        boundary(&mut assembly, 1, &cancel).unwrap(),
        AssemblyStatus::Ready
    );
    assert_eq!(assembly.work_spent(), 3 + 1 + 3);
    assert_eq!(
        assembly.try_finish(&cancel).unwrap().encoded().as_ref(),
        &[1, 2, 3]
    );
}

#[test]
fn zero_work_neither_parses_headers_nor_allocates_nor_copies() {
    let budget = Budget::new(16 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
    let baseline = budget.used();
    assert_eq!(
        assembly.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
        AssemblyStatus::Yield
    );
    assert_eq!(budget.used(), baseline);
    assembly
        .push(owned(&budget, &first(0x80, 2, (2, 1), &[1])), &cancel)
        .unwrap();
    let before = budget.used();
    for _ in 0..5 {
        assert_eq!(
            assembly.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
            AssemblyStatus::Yield
        );
        assert_eq!(
            (
                assembly.fragments_received(),
                assembly.encoded_received(),
                assembly.work_spent()
            ),
            (0, 0, 0)
        );
        assert_eq!(budget.used(), before);
    }
    assert_eq!(
        boundary(&mut assembly, 1, &cancel).unwrap(),
        AssemblyStatus::NeedFragment
    );
    assembly
        .push(owned(&budget, &next(0x40, &[2])), &cancel)
        .unwrap();
    let before = budget.used();
    for _ in 0..5 {
        assert_eq!(
            assembly.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
            AssemblyStatus::Yield
        );
        assert_eq!(
            (
                assembly.fragments_received(),
                assembly.encoded_received(),
                assembly.copied_bytes(),
                assembly.work_spent()
            ),
            (1, 1, 0, 1)
        );
        assert_eq!(budget.used(), before);
    }
    assert_eq!(
        assembly.poll(&mut WorkBudget::new(1), &cancel).unwrap(),
        AssemblyStatus::Yield
    );
    assert_eq!(
        (assembly.fragments_received(), assembly.work_spent()),
        (2, 2)
    );
    assert_eq!(budget.used(), before);
    assert_eq!(
        assembly.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
        AssemblyStatus::Yield
    );
    assert_eq!(budget.used(), before);
    assert_eq!(
        boundary(&mut assembly, 1, &cancel).unwrap(),
        AssemblyStatus::Ready
    );
}

#[test]
fn malformed_headers_and_dimension_length_underflow_fail_stickily_before_copy() {
    let full = first(0xc0, 0, (1, 1), &[]);
    let mut malformed = (0..11).map(|end| full[..end].to_vec()).collect::<Vec<_>>();
    for length in 0u8..4 {
        let mut input = full.clone();
        input[4..7].copy_from_slice(&[0, 0, length]);
        malformed.push(input);
    }
    for input in malformed {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
        assembly.push(owned(&budget, &input), &cancel).unwrap();
        let error = boundary(&mut assembly, 1, &cancel).unwrap_err();
        assert!(matches!(error, Error::Truncated | Error::Invalid(_)));
        assert_eq!(
            (assembly.copied_bytes(), assembly.fragments_received()),
            (0, 0)
        );
        sticky(&mut assembly, error, &budget);
    }
}

#[test]
fn truncated_continuation_headers_fail_stickily() {
    let complete = next(0x40, &[]);
    for end in 0..4 {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
        assembly
            .push(owned(&budget, &first(0x80, 1, (1, 1), &[1])), &cancel)
            .unwrap();
        assert_eq!(
            boundary(&mut assembly, 1, &cancel).unwrap(),
            AssemblyStatus::NeedFragment
        );
        assembly
            .push(owned(&budget, &complete[..end]), &cancel)
            .unwrap();
        assert_eq!(boundary(&mut assembly, 1, &cancel), Err(Error::Truncated));
        sticky(&mut assembly, Error::Truncated, &budget);
    }
}

#[test]
fn reserved_sequence_flags_are_explicitly_unsupported_on_first_and_continuations() {
    for reserved in 1u8..=0x3f {
        for on_first in [false, true] {
            let budget = Budget::new(4096).unwrap();
            let cancel = AtomicBool::new(false);
            let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
            if !on_first {
                assembly
                    .push(owned(&budget, &first(0x80, 1, (1, 1), &[])), &cancel)
                    .unwrap();
                assert_eq!(
                    boundary(&mut assembly, 1, &cancel).unwrap(),
                    AssemblyStatus::NeedFragment
                );
            }
            let input = if on_first {
                first(0xc0 | reserved, 1, (1, 1), &[1])
            } else {
                next(0x40 | reserved, &[1])
            };
            assembly.push(owned(&budget, &input), &cancel).unwrap();
            let error = Error::Unsupported("PGS object reserved sequence flags");
            assert_eq!(boundary(&mut assembly, 1, &cancel), Err(error));
            sticky(&mut assembly, error, &budget);
        }
    }
}

#[test]
fn early_last_and_overshoot_are_rejected_before_output_publication() {
    for (initial, following) in [
        (first(0xc0, 2, (2, 1), &[1]), None),
        (first(0xc0, 1, (1, 1), &[1, 2]), None),
        (first(0x80, 1, (1, 1), &[1, 2]), None),
        (first(0x80, 3, (3, 1), &[1]), Some(next(0x40, &[2]))),
        (first(0x80, 2, (2, 1), &[1]), Some(next(0x40, &[2, 3]))),
        (first(0x80, 2, (2, 1), &[1]), Some(next(0, &[2, 3]))),
    ] {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
        assembly.push(owned(&budget, &initial), &cancel).unwrap();
        let result = boundary(&mut assembly, 1, &cancel);
        let error = if let Some(following) = following {
            assert_eq!(result.unwrap(), AssemblyStatus::NeedFragment);
            assembly.push(owned(&budget, &following), &cancel).unwrap();
            boundary(&mut assembly, 1, &cancel).unwrap_err()
        } else {
            result.unwrap_err()
        };
        assert!(matches!(error, Error::Invalid(_)));
        assert_eq!(assembly.copied_bytes(), 0);
        sticky(&mut assembly, error, &budget);
    }
}

#[test]
fn continuation_before_first_repeated_first_and_wrong_identity_are_sticky_errors() {
    let mut wrong_id = next(0x40, &[2]);
    wrong_id[1] ^= 1;
    let mut wrong_version = next(0x40, &[2]);
    wrong_version[2] ^= 1;
    for initial in [None, Some(first(0x80, 2, (2, 1), &[1]))] {
        for following in [
            next(0x40, &[2]),
            first(0xc0, 1, (1, 1), &[2]),
            wrong_id.clone(),
            wrong_version.clone(),
        ] {
            if initial.is_some() && following == next(0x40, &[2]) {
                continue;
            }
            if initial.is_none() && following[3] & 0x80 != 0 {
                continue;
            }
            let budget = Budget::new(4096).unwrap();
            let cancel = AtomicBool::new(false);
            let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
            if let Some(initial) = &initial {
                assembly.push(owned(&budget, initial), &cancel).unwrap();
                assert_eq!(
                    boundary(&mut assembly, 1, &cancel).unwrap(),
                    AssemblyStatus::NeedFragment
                );
            }
            assembly.push(owned(&budget, &following), &cancel).unwrap();
            let error = boundary(&mut assembly, 1, &cancel).unwrap_err();
            assert!(matches!(error, Error::Invalid(_)));
            sticky(&mut assembly, error, &budget);
        }
    }
}

#[test]
fn foreign_pool_first_and_following_inputs_return_exact_owner_and_poison_assembly() {
    for following in [false, true] {
        let budget = Budget::new(4096).unwrap();
        let foreign = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
        if following {
            assembly
                .push(owned(&budget, &first(0x80, 2, (2, 1), &[1])), &cancel)
                .unwrap();
            assert_eq!(
                boundary(&mut assembly, 1, &cancel).unwrap(),
                AssemblyStatus::NeedFragment
            );
        }
        let input = owned(&foreign, &first(0xc0, 1, (1, 1), &[1]));
        let pointer = input.as_ptr();
        let failure = assembly.push(input, &cancel).unwrap_err();
        assert_eq!(failure.error, Error::ForeignPool);
        assert_eq!(failure.payload.as_ptr(), pointer);
        assert!(failure.payload.belongs_to(&foreign));
        assert_eq!(budget.used(), Budget::control_storage_bytes());
        drop(failure);
        assert_eq!(foreign.used(), Budget::control_storage_bytes());
        sticky(&mut assembly, Error::ForeignPool, &budget);
    }
}

#[test]
fn zero_dimensions_and_axis_pixel_encoded_and_fragment_limits_are_checked() {
    let defaults = AssemblyLimits::default();
    for (dimensions, encoded, limits, expected) in [
        (
            (0, 1),
            1,
            defaults,
            Error::Invalid("PGS zero object dimension"),
        ),
        (
            (1, 0),
            1,
            defaults,
            Error::Invalid("PGS zero object dimension"),
        ),
        (
            (3, 1),
            1,
            AssemblyLimits {
                max_axis: 2,
                ..defaults
            },
            Error::Limit {
                requested: 3,
                maximum: 2,
            },
        ),
        (
            (1, 3),
            1,
            AssemblyLimits {
                max_axis: 2,
                ..defaults
            },
            Error::Limit {
                requested: 3,
                maximum: 2,
            },
        ),
        (
            (2, 2),
            1,
            AssemblyLimits {
                max_pixels: 3,
                ..defaults
            },
            Error::Limit {
                requested: 4,
                maximum: 3,
            },
        ),
        (
            (1, 1),
            2,
            AssemblyLimits {
                max_encoded: 1,
                ..defaults
            },
            Error::Limit {
                requested: 2,
                maximum: 1,
            },
        ),
        (
            (1, 1),
            1,
            AssemblyLimits {
                max_fragments: 0,
                ..defaults
            },
            Error::Limit {
                requested: 1,
                maximum: 0,
            },
        ),
    ] {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut assembly = ObjectAssembly::new(&budget, limits);
        assembly
            .push(
                owned(&budget, &first(0xc0, encoded, dimensions, &[1])),
                &cancel,
            )
            .unwrap();
        assert_eq!(boundary(&mut assembly, 1, &cancel), Err(expected));
        assert_eq!(assembly.copied_bytes(), 0);
        sticky(&mut assembly, expected, &budget);
    }
    // Dimension multiplication is checked at the full wire range, without
    // allocating the resulting pixel area or an encoded output buffer.
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut assembly = ObjectAssembly::new(
        &budget,
        AssemblyLimits {
            max_axis: u16::MAX,
            ..defaults
        },
    );
    assembly
        .push(
            owned(&budget, &first(0xc0, 0, (u16::MAX, u16::MAX), &[])),
            &cancel,
        )
        .unwrap();
    assert!(matches!(
        boundary(&mut assembly, 1, &cancel),
        Err(Error::Limit { .. })
    ));
}

#[test]
fn declared_length_excludes_exactly_four_dimension_bytes_and_honours_the_full_u24_bound() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut assembly = ObjectAssembly::new(
        &budget,
        AssemblyLimits {
            max_encoded: 2,
            ..AssemblyLimits::default()
        },
    );
    assembly
        .push(owned(&budget, &first(0xc0, 2, (2, 1), &[1, 2])), &cancel)
        .unwrap();
    assert_eq!(
        boundary(&mut assembly, 1, &cancel).unwrap(),
        AssemblyStatus::Ready
    );
    assert_eq!(assembly.try_finish(&cancel).unwrap().data_length, 6);
    let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
    let input = first(0x80, 0xff_ffff - 4, (1, 1), &[]);
    assembly.push(owned(&budget, &input), &cancel).unwrap();
    assert_eq!(
        boundary(&mut assembly, 1, &cancel),
        Err(Error::Limit {
            requested: 0xff_ffff - 4,
            maximum: AssemblyLimits::default().max_encoded
        })
    );
}

#[test]
fn fragment_bound_counts_empty_fragments_and_allows_the_exact_limit() {
    for maximum in [1, 2, 3] {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut assembly = ObjectAssembly::new(
            &budget,
            AssemblyLimits {
                max_fragments: maximum,
                ..AssemblyLimits::default()
            },
        );
        assembly
            .push(owned(&budget, &first(0x80, 0, (1, 1), &[])), &cancel)
            .unwrap();
        assert_eq!(
            boundary(&mut assembly, 1, &cancel).unwrap(),
            AssemblyStatus::NeedFragment
        );
        for count in 2..=maximum {
            assembly
                .push(owned(&budget, &next(0, &[])), &cancel)
                .unwrap();
            assert_eq!(
                boundary(&mut assembly, 1, &cancel).unwrap(),
                AssemblyStatus::NeedFragment
            );
            assert_eq!(assembly.fragments_received(), count);
        }
        assembly
            .push(owned(&budget, &next(0x40, &[])), &cancel)
            .unwrap();
        let error = Error::Limit {
            requested: maximum + 1,
            maximum,
        };
        assert_eq!(boundary(&mut assembly, 1, &cancel), Err(error));
        sticky(&mut assembly, error, &budget);
    }
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut assembly = ObjectAssembly::new(
        &budget,
        AssemblyLimits {
            max_fragments: 2,
            max_work: 3,
            ..AssemblyLimits::default()
        },
    );
    assembly
        .push(owned(&budget, &first(0x80, 0, (1, 1), &[])), &cancel)
        .unwrap();
    assert_eq!(
        boundary(&mut assembly, 1, &cancel).unwrap(),
        AssemblyStatus::NeedFragment
    );
    assembly
        .push(owned(&budget, &next(0x40, &[])), &cancel)
        .unwrap();
    assert_eq!(
        boundary(&mut assembly, 1, &cancel).unwrap(),
        AssemblyStatus::Ready
    );
    let object = assembly.try_finish(&cancel).unwrap();
    assert!(object.encoded().is_empty());
    assert_eq!(assembly.work_spent(), 3);
}

#[test]
fn explicit_end_of_input_rejects_missing_last_even_at_exact_declared_length() {
    for encoded_received in [0, 1] {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
        assembly
            .push(
                owned(&budget, &first(0x80, 1, (1, 1), &[1][..encoded_received])),
                &cancel,
            )
            .unwrap();
        assert_eq!(
            boundary(&mut assembly, 1, &cancel).unwrap(),
            AssemblyStatus::NeedFragment
        );
        assert_eq!(assembly.finish_input(&cancel), Err(Error::Truncated));
        sticky(&mut assembly, Error::Truncated, &budget);
    }
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
    assert_eq!(assembly.finish_input(&cancel), Err(Error::Truncated));
    sticky(&mut assembly, Error::Truncated, &budget);
}

#[test]
fn queued_last_can_drain_after_end_of_input_but_a_queued_nonlast_cannot_publish() {
    for last in [false, true] {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
        assembly
            .push(owned(&budget, &first(0x80, 2, (2, 1), &[1])), &cancel)
            .unwrap();
        assert_eq!(
            boundary(&mut assembly, 1, &cancel).unwrap(),
            AssemblyStatus::NeedFragment
        );
        assembly
            .push(
                owned(&budget, &next(if last { 0x40 } else { 0 }, &[2])),
                &cancel,
            )
            .unwrap();
        assembly.finish_input(&cancel).unwrap();
        if last {
            assert_eq!(
                boundary(&mut assembly, 1, &cancel).unwrap(),
                AssemblyStatus::Ready
            );
            assert_eq!(
                assembly.try_finish(&cancel).unwrap().encoded().as_ref(),
                &[1, 2]
            );
        } else {
            assert_eq!(boundary(&mut assembly, 1, &cancel), Err(Error::Truncated));
            sticky(&mut assembly, Error::Truncated, &budget);
        }
    }
}

#[test]
fn empty_final_fragment_is_required_when_the_first_already_contains_all_bytes() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
    assembly
        .push(owned(&budget, &first(0x80, 2, (2, 1), &[1, 2])), &cancel)
        .unwrap();
    assert_eq!(
        boundary(&mut assembly, 1, &cancel).unwrap(),
        AssemblyStatus::NeedFragment
    );
    assembly
        .push(owned(&budget, &next(0x40, &[])), &cancel)
        .unwrap();
    assert_eq!(
        boundary(&mut assembly, 1, &cancel).unwrap(),
        AssemblyStatus::Ready
    );
    assert_eq!(
        assembly.try_finish(&cancel).unwrap().encoded().as_ref(),
        &[1, 2]
    );
}

#[test]
fn allocation_pressure_retries_never_repeat_header_or_setup_and_zero_work_cannot_retry_allocation()
{
    let budget = Budget::new(4096).unwrap();
    let held = owned(&budget, &[0; 1500]);
    let cancel = AtomicBool::new(false);
    let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
    assembly
        .push(
            owned(&budget, &first(0x80, 1500, (1500, 1), &[7; 750])),
            &cancel,
        )
        .unwrap();
    assert_eq!(
        boundary(&mut assembly, 1, &cancel).unwrap(),
        AssemblyStatus::NeedFragment
    );
    assembly
        .push(owned(&budget, &next(0x40, &[7; 750])), &cancel)
        .unwrap();
    let before = budget.used();
    for _ in 0..3 {
        assert!(matches!(
            assembly.poll(&mut WorkBudget::new(2), &cancel),
            Err(Error::Backpressure { .. })
        ));
        assert_eq!(
            (
                assembly.fragments_received(),
                assembly.work_spent(),
                assembly.copied_bytes(),
                assembly.pending_payloads()
            ),
            (2, 3, 0, 2)
        );
        assert_eq!(budget.used(), before);
    }
    drop(held);
    let before = budget.used();
    assert_eq!(
        assembly.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
        AssemblyStatus::Yield
    );
    assert_eq!((assembly.work_spent(), budget.used()), (3, before));
    assert_eq!(
        boundary(&mut assembly, 1, &cancel).unwrap(),
        AssemblyStatus::Ready
    );
    assert_eq!(assembly.work_spent(), 1503);
    assert_eq!(
        assembly.try_finish(&cancel).unwrap().encoded().as_ref(),
        &[7; 1500]
    );
    drop(assembly);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn freeze_pressure_restores_ready_buffer_and_retries_need_only_owner_header_space() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
    assembly
        .push(owned(&budget, &first(0x80, 2, (2, 1), &[1])), &cancel)
        .unwrap();
    assert_eq!(
        boundary(&mut assembly, 1, &cancel).unwrap(),
        AssemblyStatus::NeedFragment
    );
    assembly
        .push(owned(&budget, &next(0x40, &[2])), &cancel)
        .unwrap();
    assert_eq!(
        boundary(&mut assembly, 1, &cancel).unwrap(),
        AssemblyStatus::Ready
    );
    assert_eq!(assembly.pending_payloads(), 0);
    let blocker = budget.reserve(budget.available()).unwrap();
    let before = (
        budget.used(),
        assembly.work_spent(),
        assembly.copied_bytes(),
    );
    for _ in 0..3 {
        assert!(matches!(
            assembly.try_finish(&cancel),
            Err(Error::Backpressure { .. })
        ));
        assert_eq!(
            (
                budget.used(),
                assembly.work_spent(),
                assembly.copied_bytes()
            ),
            before
        );
        assert_eq!(
            assembly.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
            AssemblyStatus::Ready
        );
    }
    drop(blocker);
    let blocker = budget
        .reserve(budget.available() - Bytes::owner_overhead_bytes())
        .unwrap();
    let object = assembly.try_finish(&cancel).unwrap();
    assert_eq!(object.encoded().as_ref(), &[1, 2]);
    assert_eq!(budget.available(), 0);
    let retained = object.encoded().clone();
    drop(assembly);
    drop(object);
    drop(blocker);
    assert_eq!(
        budget.used(),
        Budget::control_storage_bytes() + retained.retained_storage_bytes()
    );
    assert_eq!(retained.as_ref(), &[1, 2]);
    drop(retained);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn cancellation_at_input_setup_both_copy_stages_and_ready_is_sticky_and_keeps_external_aliases() {
    for phase in 0..=7 {
        let budget = Budget::new(16 * 1024).unwrap();
        let cancel = AtomicBool::new(false);
        let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
        let input = owned(&budget, &first(0x80, 4, (4, 1), &[1, 2]));
        let alias = input.clone();
        let charge = alias.retained_storage_bytes();
        assembly.push(input, &cancel).unwrap();
        if phase >= 1 {
            assert_eq!(
                boundary(&mut assembly, 1, &cancel).unwrap(),
                AssemblyStatus::NeedFragment
            );
        }
        if phase >= 2 {
            assembly
                .push(owned(&budget, &next(0x40, &[3, 4])), &cancel)
                .unwrap();
        }
        if phase >= 3 {
            assert_eq!(
                assembly.poll(&mut WorkBudget::new(1), &cancel).unwrap(),
                AssemblyStatus::Yield
            );
        }
        if phase >= 4 {
            assert_eq!(
                assembly.poll(&mut WorkBudget::new(1), &cancel).unwrap(),
                AssemblyStatus::Yield
            );
        }
        if phase >= 5 {
            assert_eq!(
                assembly.poll(&mut WorkBudget::new(1), &cancel).unwrap(),
                AssemblyStatus::Yield
            );
            assert_eq!(assembly.copied_bytes(), 1);
        }
        if phase >= 6 {
            assert_eq!(
                assembly.poll(&mut WorkBudget::new(2), &cancel).unwrap(),
                AssemblyStatus::Yield
            );
            assert_eq!(assembly.copied_bytes(), 3);
        }
        if phase >= 7 {
            assert_eq!(
                boundary(&mut assembly, 1, &cancel).unwrap(),
                AssemblyStatus::Ready
            );
        }
        cancel.store(true, Ordering::Relaxed);
        if phase % 2 == 0 {
            assert_eq!(
                assembly.poll(&mut WorkBudget::new(0), &cancel),
                Err(Error::Cancelled)
            );
        } else {
            assert_eq!(assembly.try_finish(&cancel).unwrap_err(), Error::Cancelled);
        }
        cancel.store(false, Ordering::Relaxed);
        assert_eq!(
            assembly.poll(&mut WorkBudget::new(99), &cancel),
            Err(Error::Cancelled)
        );
        assert_eq!(assembly.try_finish(&cancel).unwrap_err(), Error::Cancelled);
        assert_eq!(assembly.finish_input(&cancel), Err(Error::Cancelled));
        assert_eq!(assembly.pending_payloads(), 0);
        assert_eq!(budget.used(), Budget::control_storage_bytes() + charge);
        assert_eq!(alias[11..], [1, 2]);
        drop(alias);
        sticky(&mut assembly, Error::Cancelled, &budget);
    }
}

#[test]
fn cancel_before_push_returns_the_owner_and_stays_cancelled_after_the_flag_clears() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(true);
    let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
    let input = owned(&budget, &first(0xc0, 1, (1, 1), &[1]));
    let pointer = input.as_ptr();
    let failure = assembly.push(input, &cancel).unwrap_err();
    assert_eq!(failure.error, Error::Cancelled);
    assert_eq!(failure.payload.as_ptr(), pointer);
    drop(failure);
    cancel.store(false, Ordering::Relaxed);
    sticky(&mut assembly, Error::Cancelled, &budget);
}

#[test]
fn cancelling_during_allocation_or_freeze_pressure_drops_only_assembly_owners() {
    for at_freeze in [false, true] {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let held = if at_freeze {
            None
        } else {
            Some(owned(&budget, &[0; 1500]))
        };
        let length = if at_freeze { 2 } else { 1500 };
        let data = vec![1; length / 2];
        let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
        assembly
            .push(owned(&budget, &first(0x80, length, (1, 1), &data)), &cancel)
            .unwrap();
        assert_eq!(
            boundary(&mut assembly, 1, &cancel).unwrap(),
            AssemblyStatus::NeedFragment
        );
        assembly
            .push(owned(&budget, &next(0x40, &data)), &cancel)
            .unwrap();
        let blocker = if at_freeze {
            assert_eq!(
                boundary(&mut assembly, 1, &cancel).unwrap(),
                AssemblyStatus::Ready
            );
            let blocker = budget.reserve(budget.available()).unwrap();
            assert!(matches!(
                assembly.try_finish(&cancel),
                Err(Error::Backpressure { .. })
            ));
            Some(blocker)
        } else {
            assert!(matches!(
                assembly.poll(&mut WorkBudget::new(2), &cancel),
                Err(Error::Backpressure { .. })
            ));
            None
        };
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            assembly.poll(&mut WorkBudget::new(0), &cancel),
            Err(Error::Cancelled)
        );
        drop(blocker);
        drop(held);
        cancel.store(false, Ordering::Relaxed);
        sticky(&mut assembly, Error::Cancelled, &budget);
    }
}

#[test]
fn total_work_limit_is_exact_for_zero_copy_and_fragmented_inputs() {
    for maximum in 0..=5 {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut assembly = ObjectAssembly::new(
            &budget,
            AssemblyLimits {
                max_work: maximum,
                ..AssemblyLimits::default()
            },
        );
        assembly
            .push(owned(&budget, &first(0x80, 2, (2, 1), &[1])), &cancel)
            .unwrap();
        if maximum == 0 {
            let error = boundary(&mut assembly, 1, &cancel).unwrap_err();
            assert!(matches!(error, Error::Limit { maximum: 0, .. }));
            sticky(&mut assembly, error, &budget);
            continue;
        }
        assert_eq!(
            boundary(&mut assembly, 1, &cancel).unwrap(),
            AssemblyStatus::NeedFragment
        );
        assembly
            .push(owned(&budget, &next(0x40, &[2])), &cancel)
            .unwrap();
        let status = boundary(&mut assembly, 1, &cancel);
        if maximum == 5 {
            assert_eq!(status.unwrap(), AssemblyStatus::Ready);
            assert_eq!(assembly.work_spent(), 5);
            assert_eq!(
                assembly.try_finish(&cancel).unwrap().encoded().as_ref(),
                &[1, 2]
            );
        } else {
            let error = status.unwrap_err();
            assert!(matches!(error, Error::Limit { .. }));
            assert!(assembly.work_spent() <= maximum);
            sticky(&mut assembly, error, &budget);
        }
    }
}

#[test]
fn premature_finish_is_sticky_and_releases_unpublished_input_and_partial_output() {
    for phase in 0..=3 {
        let budget = Budget::new(4096).unwrap();
        let cancel = AtomicBool::new(false);
        let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
        if phase >= 1 {
            assembly
                .push(owned(&budget, &first(0x80, 4, (4, 1), &[1, 2])), &cancel)
                .unwrap();
        }
        if phase >= 2 {
            assert_eq!(
                boundary(&mut assembly, 1, &cancel).unwrap(),
                AssemblyStatus::NeedFragment
            );
        }
        if phase >= 3 {
            assembly
                .push(owned(&budget, &next(0x40, &[3, 4])), &cancel)
                .unwrap();
            assert_eq!(
                assembly.poll(&mut WorkBudget::new(3), &cancel).unwrap(),
                AssemblyStatus::Yield
            );
            assert_eq!(assembly.copied_bytes(), 1);
        }
        let error = Error::Invalid("PGS object not ready");
        assert_eq!(assembly.try_finish(&cancel).unwrap_err(), error);
        sticky(&mut assembly, error, &budget);
    }
}

#[test]
fn terminal_misuse_cannot_publish_twice_or_invalidate_already_published_storage() {
    let budget = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut assembly = ObjectAssembly::new(&budget, AssemblyLimits::default());
    assembly
        .push(owned(&budget, &first(0xc0, 2, (2, 1), &[1, 2])), &cancel)
        .unwrap();
    assert_eq!(
        boundary(&mut assembly, 1, &cancel).unwrap(),
        AssemblyStatus::Ready
    );
    let object = assembly.try_finish(&cancel).unwrap();
    let error = assembly.try_finish(&cancel).unwrap_err();
    assert_eq!(error, Error::Invalid("PGS object not ready"));
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(assembly.poll(&mut WorkBudget::new(0), &cancel), Err(error));
    assert_eq!(object.encoded().as_ref(), &[1, 2]);
    drop(assembly);
    assert_eq!(
        budget.used(),
        Budget::control_storage_bytes() + object.encoded().retained_storage_bytes()
    );
    drop(object);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}
