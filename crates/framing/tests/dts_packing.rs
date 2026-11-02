//! Original bit-string packing fixtures from ETSI TS102114 V1.2.1 §5.3 prose.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, Step, WorkBudget};
use triergon_framing::{
    Dts14BitPayload, Dts14BitUnpacker, DtsInspectionIdentity, DtsPackingLimits, DtsWordOrder,
};

fn identity() -> DtsInspectionIdentity {
    DtsInspectionIdentity {
        source: SourceId(1414),
        generation: 9,
        offset: 65,
    }
}
fn limits() -> DtsPackingLimits {
    DtsPackingLimits {
        maximum_source_bytes: 4096,
        maximum_output_bytes: 4096,
    }
}

// Independent author: concatenate literal binary strings and split into words.
// Added low tail bits are authored control data, not a DTS frame-padding rule.
fn pack(source: &[u8], order: DtsWordOrder) -> Vec<u8> {
    let mut bits: String = source.iter().map(|byte| format!("{byte:08b}")).collect();
    while bits.len() % 14 != 0 {
        bits.push('0');
    }
    let mut output = Vec::new();
    for (index, chunk) in bits.as_bytes().chunks_exact(14).enumerate() {
        let low = u16::from_str_radix(std::str::from_utf8(chunk).unwrap(), 2).unwrap();
        let high = (index % 4) as u16;
        let word = low | high << 14;
        output.extend(match order {
            DtsWordOrder::BigEndian => word.to_be_bytes(),
            DtsWordOrder::LittleEndian => word.to_le_bytes(),
        });
    }
    output
}

fn oracle(raw: &[u8], order: DtsWordOrder) -> (Vec<u8>, u8, u8) {
    let bits: String = raw
        .chunks_exact(2)
        .map(|pair| {
            let word = match order {
                DtsWordOrder::BigEndian => u16::from_be_bytes([pair[0], pair[1]]),
                DtsWordOrder::LittleEndian => u16::from_le_bytes([pair[0], pair[1]]),
            };
            format!("{:014b}", word & 0x3fff)
        })
        .collect();
    let count = bits.len() / 8;
    let bytes = bits.as_bytes()[..count * 8]
        .chunks_exact(8)
        .map(|chunk| u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 2).unwrap())
        .collect();
    let tail = &bits[count * 8..];
    (
        bytes,
        tail.len() as u8,
        if tail.is_empty() {
            0
        } else {
            u8::from_str_radix(tail, 2).unwrap()
        },
    )
}

fn finish(job: &mut Dts14BitUnpacker, quantum: u64) -> Result<Dts14BitPayload, Error> {
    for _ in 0..10000 {
        match job.step(&mut WorkBudget::new(quantum), &AtomicBool::new(false))? {
            Step::Ready(value) => return Ok(value),
            Step::Yield => {}
            Step::End(_) => panic!("missing payload"),
            Step::NeedInput(_) => panic!("owned input requested more data"),
        }
    }
    panic!("no bounded progress");
}
fn run(pool: &Budget, raw: &[u8], order: DtsWordOrder, quantum: u64) -> Dts14BitPayload {
    let mut job = Dts14BitUnpacker::new(
        pool,
        identity(),
        Bytes::copy_from_slice(pool, raw).unwrap(),
        order,
        limits(),
    )
    .unwrap();
    finish(&mut job, quantum).unwrap()
}

#[test]
fn big_endian_low14_payload_recovers_a_bit_authored_canonical_sync() {
    let pool = Budget::new(65536).unwrap();
    let source = [
        0x7f, 0xfe, 0x80, 0x01, 0xfc, 0x3c, 0x05, 0xf0, 0xb5, 0xe0, 0x01, 0x38, 0,
    ];
    let raw = pack(&source, DtsWordOrder::BigEndian);
    let value = run(&pool, &raw, DtsWordOrder::BigEndian, 1);
    assert_eq!(&value.canonical_bytes().as_ref()[..source.len()], source);
    assert_eq!(value.source_bytes().as_ref(), raw);
    assert_eq!(value.payload_bits(), 112);
    assert_eq!(value.canonical_bytes().len(), 14);
    assert_eq!(value.residual_bits(), 0);
    assert_eq!(value.identity(), identity());
}

#[test]
fn declared_little_endian_words_have_identical_payload_and_keep_physical_bytes() {
    let pool = Budget::new(65536).unwrap();
    let source = [0x7f, 0xfe, 0x80, 0x01, 0xfc, 0x3c, 0x05];
    let be = pack(&source, DtsWordOrder::BigEndian);
    let le = pack(&source, DtsWordOrder::LittleEndian);
    let left = run(&pool, &be, DtsWordOrder::BigEndian, 1);
    let right = run(&pool, &le, DtsWordOrder::LittleEndian, 3);
    assert_eq!(
        left.canonical_bytes().as_ref(),
        right.canonical_bytes().as_ref()
    );
    assert_eq!(right.source_bytes().as_ref(), le);
    assert_eq!(right.word_order(), DtsWordOrder::LittleEndian);
}

#[test]
fn unused_high_bits_are_source_facts_and_never_guessed_as_sign_extension() {
    let pool = Budget::new(65536).unwrap();
    let mut canonical = None;
    for high in 0..4_u16 {
        let word = 0x1234 | high << 14;
        let value = run(&pool, &word.to_be_bytes(), DtsWordOrder::BigEndian, 1);
        assert_eq!(value.unused_high_bits(0).unwrap(), high as u8);
        let observed = (
            value.canonical_bytes().as_ref().to_vec(),
            value.residual_bits(),
            value.residual_value(),
        );
        if let Some(expected) = &canonical {
            assert_eq!(&observed, expected);
        } else {
            canonical = Some(observed);
        }
        assert!(value.unused_high_bits(1).is_err());
        assert!(value.unused_high_bits(usize::MAX).is_err());
    }
}

#[test]
fn all_word_alignment_residues_are_reported_without_tail_zero_fill_or_loss() {
    let pool = Budget::new(65536).unwrap();
    for words in 0..=12 {
        let raw: Vec<u8> = (0..words)
            .flat_map(|index| (0x1fff_u16 ^ index as u16).to_be_bytes())
            .collect();
        let expected = oracle(&raw, DtsWordOrder::BigEndian);
        let value = run(&pool, &raw, DtsWordOrder::BigEndian, 1);
        assert_eq!(value.payload_bits(), words * 14);
        assert_eq!(value.canonical_bytes().as_ref(), expected.0);
        assert_eq!(
            (value.residual_bits(), value.residual_value()),
            (expected.1, expected.2)
        );
        assert_eq!(
            value.canonical_bytes().len() * 8 + usize::from(value.residual_bits()),
            words * 14
        );
    }
}

#[test]
fn every_frozen396_source_prefix_roundtrips_without_changing_old_inputs() {
    let pool = Budget::new(65536).unwrap();
    let mut count = 0;
    for line in include_str!("../fixtures/dts-inspect-source-v1.tsv")
        .lines()
        .filter(|line| !line.starts_with('#'))
    {
        let parts: Vec<_> = line.split('\t').collect();
        let bytes: Vec<u8> = if parts[1] == "~" {
            Vec::new()
        } else {
            parts[1]
                .as_bytes()
                .chunks_exact(2)
                .map(|s| u8::from_str_radix(std::str::from_utf8(s).unwrap(), 16).unwrap())
                .collect()
        };
        for order in [DtsWordOrder::BigEndian, DtsWordOrder::LittleEndian] {
            let raw = pack(&bytes, order);
            let value = run(&pool, &raw, order, 7);
            assert_eq!(
                &value.canonical_bytes().as_ref()[..bytes.len()],
                bytes,
                "{}",
                parts[0]
            );
            assert_eq!(value.source_bytes().as_ref(), raw);
        }
        count += 1;
    }
    assert_eq!(count, 396);
}

#[test]
fn zero_work_does_not_allocate_and_quantum_one_always_finishes() {
    let pool = Budget::new(65536).unwrap();
    let raw = pack(&[0xa5; 37], DtsWordOrder::BigEndian);
    let source = Bytes::copy_from_slice(&pool, &raw).unwrap();
    let baseline = pool.used();
    let mut job =
        Dts14BitUnpacker::new(&pool, identity(), source, DtsWordOrder::BigEndian, limits())
            .unwrap();
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        Step::Yield
    ));
    assert_eq!(pool.used(), baseline);
    let first = finish(&mut job, 1).unwrap();
    for quantum in [2, 3, 7, 16, 1024] {
        let other = run(&pool, &raw, DtsWordOrder::BigEndian, quantum);
        assert_eq!(
            other.canonical_bytes().as_ref(),
            first.canonical_bytes().as_ref()
        );
        assert_eq!(other.residual_value(), first.residual_value());
    }
}

#[test]
fn reserve_backpressure_retries_with_the_exact_original_source() {
    let pool = Budget::new(65536).unwrap();
    let source = Bytes::copy_from_slice(&pool, &[0x1f, 0xff, 0xe8, 0]).unwrap();
    let mut job =
        Dts14BitUnpacker::new(&pool, identity(), source, DtsWordOrder::BigEndian, limits())
            .unwrap();
    let blocker = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        finish(&mut job, 1),
        Err(Error::Backpressure { .. })
    ));
    drop(blocker);
    assert_eq!(
        finish(&mut job, 1).unwrap().source_bytes().as_ref(),
        [0x1f, 0xff, 0xe8, 0]
    );
}

#[test]
fn freeze_backpressure_retains_materialized_bytes_for_retry() {
    let pool = Budget::new(65536).unwrap();
    let source = Bytes::copy_from_slice(&pool, &[0x1f, 0xff, 0xe8, 0]).unwrap();
    let mut job =
        Dts14BitUnpacker::new(&pool, identity(), source, DtsWordOrder::BigEndian, limits())
            .unwrap();
    assert!(matches!(
        job.step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        Step::Yield
    ));
    let blocker = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        finish(&mut job, 1024),
        Err(Error::Backpressure { .. })
    ));
    drop(blocker);
    let value = finish(&mut job, 1).unwrap();
    assert_eq!(value.canonical_bytes().as_ref(), [0x7f, 0xfe, 0x80]);
    assert_eq!((value.residual_bits(), value.residual_value()), (4, 0));
}

#[test]
fn cancellation_releases_each_construction_stage_and_is_sticky() {
    for quantum in 0..=42 {
        let pool = Budget::new(65536).unwrap();
        let baseline = pool.used();
        let source = Bytes::copy_from_slice(&pool, &[0x1f, 0xff, 0xe8, 0]).unwrap();
        let mut job =
            Dts14BitUnpacker::new(&pool, identity(), source, DtsWordOrder::BigEndian, limits())
                .unwrap();
        drop(
            job.step(&mut WorkBudget::new(quantum), &AtomicBool::new(false))
                .unwrap(),
        );
        let cancel = AtomicBool::new(true);
        assert_eq!(
            job.step(&mut WorkBudget::new(1), &cancel).err(),
            Some(Error::Cancelled)
        );
        cancel.store(false, Ordering::Relaxed);
        assert_eq!(
            job.step(&mut WorkBudget::new(1), &cancel).err(),
            Some(Error::Cancelled)
        );
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn retained_packed_and_canonical_owners_release_independently() {
    let pool = Budget::new(65536).unwrap();
    let baseline = pool.used();
    let value = run(&pool, &[0x1f, 0xff, 0xe8, 0], DtsWordOrder::BigEndian, 1);
    assert!(
        !value
            .source_bytes()
            .shares_storage_with(value.canonical_bytes())
    );
    let packed = value.source_bytes().slice(0..1).unwrap();
    let canonical = value.canonical_bytes().slice(0..1).unwrap();
    let both = pool.used();
    drop(value);
    assert_eq!(pool.used(), both);
    drop(packed);
    assert!(pool.used() < both && pool.used() > baseline);
    drop(canonical);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn odd_sizes_foreign_pool_limits_offsets_and_impossible_owner_capacity_fail_explicitly() {
    let pool = Budget::new(65536).unwrap();
    let foreign = Budget::new(65536).unwrap();
    assert!(matches!(
        Dts14BitUnpacker::new(
            &pool,
            identity(),
            Bytes::copy_from_slice(&foreign, &[0, 0]).unwrap(),
            DtsWordOrder::BigEndian,
            limits()
        ),
        Err(Error::ForeignPool)
    ));
    assert!(matches!(
        Dts14BitUnpacker::new(
            &pool,
            identity(),
            Bytes::copy_from_slice(&pool, &[0]).unwrap(),
            DtsWordOrder::BigEndian,
            limits()
        ),
        Err(Error::Truncated)
    ));
    let small = DtsPackingLimits {
        maximum_source_bytes: 1,
        ..limits()
    };
    assert!(matches!(
        Dts14BitUnpacker::new(
            &pool,
            identity(),
            Bytes::copy_from_slice(&pool, &[0, 0]).unwrap(),
            DtsWordOrder::BigEndian,
            small
        ),
        Err(Error::Limit { .. })
    ));
    let small = DtsPackingLimits {
        maximum_output_bytes: 0,
        ..limits()
    };
    assert!(matches!(
        Dts14BitUnpacker::new(
            &pool,
            identity(),
            Bytes::copy_from_slice(&pool, &[0, 0]).unwrap(),
            DtsWordOrder::BigEndian,
            small
        ),
        Err(Error::Limit { .. })
    ));
    let overflowing = DtsInspectionIdentity {
        offset: u64::MAX,
        ..identity()
    };
    assert!(matches!(
        Dts14BitUnpacker::new(
            &pool,
            overflowing,
            Bytes::copy_from_slice(&pool, &[0, 0]).unwrap(),
            DtsWordOrder::BigEndian,
            limits()
        ),
        Err(Error::Overflow)
    ));
    let tiny =
        Budget::new(Budget::control_storage_bytes() + Bytes::owner_overhead_bytes() + 2).unwrap();
    assert!(matches!(
        Dts14BitUnpacker::new(
            &tiny,
            identity(),
            Bytes::copy_from_slice(&tiny, &[0, 0]).unwrap(),
            DtsWordOrder::BigEndian,
            limits()
        ),
        Err(Error::Limit { .. })
    ));
}

#[test]
fn empty_word_record_is_not_a_frame_and_end_is_a_checked_record_boundary() {
    let pool = Budget::new(65536).unwrap();
    let source = Bytes::copy_from_slice(&pool, &[]).unwrap();
    let mut job =
        Dts14BitUnpacker::new(&pool, identity(), source, DtsWordOrder::BigEndian, limits())
            .unwrap();
    let value = finish(&mut job, 1).unwrap();
    assert!(value.canonical_bytes().is_empty());
    assert_eq!(value.payload_bits(), 0);
    match job
        .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
    {
        Step::End(end) => {
            assert_eq!(end.source, identity().source);
            assert_eq!(end.offset, identity().offset);
        }
        _ => panic!("missing record end"),
    }
}

#[test]
fn payload_bit_locations_name_the_exact_original_physical_bit_in_both_orders() {
    let pool = Budget::new(65536).unwrap();
    for order in [DtsWordOrder::BigEndian, DtsWordOrder::LittleEndian] {
        let raw: Vec<u8> = [0x1fff_u16, 0xe800, 0x07f0, 0xffff, 1]
            .into_iter()
            .flat_map(|word| match order {
                DtsWordOrder::BigEndian => word.to_be_bytes(),
                DtsWordOrder::LittleEndian => word.to_le_bytes(),
            })
            .collect();
        let value = run(&pool, &raw, order, 1);
        for bit in 0..value.payload_bits() {
            let location = value.payload_bit_source(bit).unwrap();
            assert_eq!(location.source, identity().source);
            assert_eq!(location.generation, identity().generation);
            let index = usize::try_from(location.offset - identity().offset).unwrap();
            let physical = (raw[index] >> (7 - location.msb0_bit)) & 1;
            let full = value.canonical_bytes().len() * 8;
            let logical = if bit < full {
                (value.canonical_bytes().as_ref()[bit / 8] >> (7 - bit % 8)) & 1
            } else {
                (value.residual_value() >> (usize::from(value.residual_bits()) - 1 - (bit - full)))
                    & 1
            };
            assert_eq!(physical, logical);
        }
        assert!(value.payload_bit_source(value.payload_bits()).is_err());
    }
}
