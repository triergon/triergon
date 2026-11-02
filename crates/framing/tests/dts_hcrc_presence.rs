//! Parent-approved source-presence clarification, independent of native validity.
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, SourceId, Step, WorkBudget};
use triergon_framing::{
    DtsCoreField as F, DtsHeaderInspector, DtsInspection, DtsInspectionIdentity,
    DtsInspectionIssues as I, DtsInspectionStep, DtsPackedLimits, DtsPackedPayloadJob, DtsPacking,
    DtsSourceIdentity, FieldObservation, inspect_dts_header_bits,
};

fn identity() -> DtsInspectionIdentity {
    DtsInspectionIdentity {
        source: SourceId(7162),
        generation: 23,
        offset: 91,
    }
}

fn finish(job: &mut DtsHeaderInspector, quantum: u64) -> DtsInspection {
    for _ in 0..1024 {
        match job
            .step(&mut WorkBudget::new(quantum), &AtomicBool::new(false))
            .unwrap()
        {
            DtsInspectionStep::Yield => {}
            DtsInspectionStep::Ready(value) => return value,
            _ => panic!("owned inspection publication missing"),
        }
    }
    panic!("bounded inspection did not finish");
}

fn inspect(raw: &[u8], quantum: u64) -> DtsInspection {
    let pool = Budget::new(65536).unwrap();
    let raw = Bytes::copy_from_slice(&pool, raw).unwrap();
    let mut job = DtsHeaderInspector::new(&pool, identity(), raw, 256).unwrap();
    finish(&mut job, quantum)
}

fn hex(raw: &str) -> Vec<u8> {
    assert_eq!(raw.len() % 2, 0);
    raw.as_bytes()
        .chunks_exact(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}

fn assert_observed_ranges(value: &DtsInspection, logical_bits: usize) {
    for field in F::ALL {
        if let FieldObservation::Bits {
            offset,
            width,
            observed_bits,
            prefix,
        } = value.field(field)
        {
            assert!(usize::from(offset) + usize::from(observed_bits) <= logical_bits);
            assert!(observed_bits <= width);
            if observed_bits < 32 {
                assert!(prefix < (1u32 << observed_bits));
            }
            if observed_bits < width {
                assert_eq!(value.field(field).value(), None);
            }
        }
    }
}

#[test]
fn all_fourteen_original_prefix_findings_publish_known_absence() {
    let mut checked = 0;
    let mut differences = Vec::new();
    for row in include_str!("../fixtures/dts-hcrc-known-v1.tsv")
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let columns: Vec<_> = row.split('\t').collect();
        assert_eq!(columns.len(), 2);
        let raw = hex(columns[1]);
        assert!((5..=10).contains(&raw.len()));
        assert_eq!((raw[4] >> 1) & 1, 0);
        for quantum in [1, 7, 31] {
            let value = inspect(&raw, quantum);
            assert_eq!(value.source_bytes().as_ref(), raw);
            assert_eq!(value.field(F::CrcPresent).value(), Some(0));
            assert_eq!(value.required_header_bits(), Some(104));
            assert!(value.issues().contains(I::TRUNCATED));
            assert!(!value.syntax_complete());
            assert_observed_ranges(&value, raw.len() * 8);
            if value.field(F::Hcrc) != FieldObservation::NotInSyntax {
                differences.push((columns[0], quantum, value.field(F::Hcrc)));
            }
        }
        checked += 1;
    }
    assert_eq!(checked, 14);
    assert!(
        differences.is_empty(),
        "known source CPF=0 must publish HCRC NotInSyntax: {differences:?}"
    );
}

// ETSI Table5-1 in syntax order; these are authored source bits, not Rust outputs.
fn primary_header(cpf: bool) -> Vec<bool> {
    let mut bits = Vec::new();
    for (width, value) in [
        (32, 0x7ffe8001u32),
        (1, 1),
        (5, 31),
        (1, u32::from(cpf)),
        (7, 15),
        (14, 95),
        (6, 2),
        (4, 13),
        (5, 15),
        (1, 0),
        (1, 0),
        (1, 0),
        (1, 0),
        (1, 0),
        (3, 0),
        (1, 0),
        (1, 0),
        (2, 0),
        (1, 1),
        (if cpf { 16 } else { 0 }, 0x1234),
        (1, 0),
        (4, 7),
        (2, 0),
        (3, 0),
        (1, 0),
        (1, 0),
        (4, 11),
    ] {
        for shift in (0..width).rev() {
            bits.push((value >> shift) & 1 != 0);
        }
    }
    assert_eq!(bits.len(), if cpf { 120 } else { 104 });
    bits
}

fn byte_storage(bits: &[bool]) -> Vec<u8> {
    let mut raw = vec![0; bits.len().div_ceil(8)];
    for (i, value) in bits.iter().enumerate() {
        raw[i / 8] |= u8::from(*value) << (7 - i % 8);
    }
    raw
}

fn packed_source(header: &[bool], packing: DtsPacking) -> Vec<u8> {
    let width = match packing {
        DtsPacking::CanonicalBe => return byte_storage(header),
        DtsPacking::Words16Le => 16,
        DtsPacking::Words14Be | DtsPacking::Words14Le => 14,
    };
    let mut bits = header.to_vec();
    // Explicit carrier source bits, not decoder padding or presumed header bits.
    bits.resize(header.len().div_ceil(width) * width, true);
    let mut raw = Vec::new();
    for (ordinal, word) in bits.chunks_exact(width).enumerate() {
        let mut value = 0u16;
        for bit in word {
            value = (value << 1) | u16::from(*bit);
        }
        if width == 14 {
            value |= ((ordinal % 4) as u16) << 14;
        }
        let bytes = if packing == DtsPacking::Words14Be {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        };
        raw.extend_from_slice(&bytes);
    }
    raw
}

fn expected_hcrc(cpf: bool, bits: usize) -> FieldObservation {
    if bits < 39 {
        FieldObservation::NotReached
    } else if !cpf {
        FieldObservation::NotInSyntax
    } else if bits <= 88 {
        FieldObservation::NotReached
    } else {
        let observed = (bits - 88).min(16) as u8;
        FieldObservation::Bits {
            offset: 88,
            width: 16,
            observed_bits: observed,
            prefix: 0x1234 >> (16 - observed),
        }
    }
}

#[test]
fn packed_swapped_and_bit_prefixes_never_invent_crc_presence_or_values() {
    let mut differences = Vec::new();
    for cpf in [false, true] {
        let primary = primary_header(cpf);
        for packing in [
            DtsPacking::CanonicalBe,
            DtsPacking::Words16Le,
            DtsPacking::Words14Be,
            DtsPacking::Words14Le,
        ] {
            let source = packed_source(&primary, packing);
            let stride = if packing == DtsPacking::CanonicalBe {
                1
            } else {
                2
            };
            for bytes in (0..=source.len()).step_by(stride) {
                let pool = Budget::new(65536).unwrap();
                let baseline = pool.used();
                {
                    let raw = Bytes::copy_from_slice(&pool, &source[..bytes]).unwrap();
                    let mut job = DtsPackedPayloadJob::new(
                        &pool,
                        DtsSourceIdentity {
                            source: SourceId(7163),
                            generation: 24,
                            offset: 92,
                        },
                        raw,
                        packing,
                        DtsPackedLimits {
                            maximum_source_bytes: 256,
                            maximum_storage_bytes: 256,
                        },
                    )
                    .unwrap();
                    let payload = loop {
                        match job
                            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
                            .unwrap()
                        {
                            Step::Yield => {}
                            Step::Ready(value) => break value,
                            _ => panic!("owned payload publication missing"),
                        }
                    };
                    assert_eq!(payload.source_bytes().as_ref(), &source[..bytes]);
                    let valid = bytes
                        * if stride == 2 && packing != DtsPacking::Words16Le {
                            7
                        } else {
                            8
                        };
                    assert_eq!(payload.valid_bits(), valid);
                    let packed =
                        inspect_dts_header_bits(payload.storage_bytes().as_ref(), valid).unwrap();
                    assert_eq!(packed.field(F::Hcrc), expected_hcrc(cpf, valid));
                    // The byte inspector gets only whole valid source bytes. Storage
                    // alignment zeros are never supplied as additional logical bits.
                    let whole = valid / 8;
                    let inspected = inspect(&payload.storage_bytes().as_ref()[..whole], 1);
                    assert_observed_ranges(&inspected, whole * 8);
                    if inspected.field(F::Hcrc) != expected_hcrc(cpf, whole * 8) {
                        differences.push((cpf, packing, bytes, inspected.field(F::Hcrc)));
                    }
                    for visible in 0..=valid {
                        let partial =
                            inspect_dts_header_bits(payload.storage_bytes().as_ref(), visible)
                                .unwrap();
                        assert_eq!(partial.field(F::Hcrc), expected_hcrc(cpf, visible));
                        if visible < primary.len() {
                            assert!(partial.issues() & I::TRUNCATED != 0);
                        }
                    }
                }
                assert_eq!(pool.used(), baseline);
            }
        }
    }
    assert!(
        differences.is_empty(),
        "packed known-CPF presence differs: {differences:?}"
    );
}

#[test]
fn presence_publication_obeys_work_cancel_source_bounds_and_owner_release() {
    let primary = byte_storage(&primary_header(false));
    for length in [4, 5, 9, 10, 13] {
        let pool = Budget::new(65536).unwrap();
        let baseline = pool.used();
        {
            let raw = Bytes::copy_from_slice(&pool, &primary[..length]).unwrap();
            let mut job = DtsHeaderInspector::new(&pool, identity(), raw, 256).unwrap();
            let used = pool.used();
            assert!(matches!(
                job.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
                    .unwrap(),
                DtsInspectionStep::Yield
            ));
            assert_eq!(pool.used(), used);
            let value = finish(&mut job, 1);
            assert_eq!(value.field(F::Hcrc), expected_hcrc(false, length * 8));
            assert_observed_ranges(&value, length * 8);
        }
        assert_eq!(pool.used(), baseline);
        {
            let raw = Bytes::copy_from_slice(&pool, &primary[..length]).unwrap();
            let mut job = DtsHeaderInspector::new(&pool, identity(), raw, 256).unwrap();
            assert!(matches!(
                job.step(&mut WorkBudget::new(1), &AtomicBool::new(true)),
                Err(Error::Cancelled)
            ));
            assert!(matches!(
                job.step(&mut WorkBudget::new(100), &AtomicBool::new(false)),
                Err(Error::Cancelled)
            ));
        }
        assert_eq!(pool.used(), baseline);
    }
}
