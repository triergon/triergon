//! Original bit-authored source oracles; no native output predicts acceptance.
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputRequest, InputWindow, SourceCapabilities, SourceFailure, SourceId,
    Step, WorkBudget,
};
use triergon_framing::{
    DtsCarrierTail, DtsCoreField as F, DtsExtentRelation, DtsInspectionSourceStatus as Status,
    DtsNormalBoundary, DtsNormalLimits, DtsNormalSourceFrame, DtsNormalSourceReader,
    DtsPackedLimits, DtsPackedPayload, DtsPackedPayloadJob, DtsPacking as P, DtsSourceIdentity,
    FieldObservation, inspect_dts_header_bits,
};
#[path = "../examples/support/dts_bits24.rs"]
mod dts_bits24;

const SOURCE: SourceId = SourceId(1416);
const PACKINGS: [P; 4] = [P::CanonicalBe, P::Words16Le, P::Words14Be, P::Words14Le];
// Independent transcription of primary Table5-1, not production field widths.
const WIDTHS: [usize; 27] = [
    32, 1, 5, 1, 7, 14, 6, 4, 5, 1, 1, 1, 1, 1, 3, 1, 1, 2, 1, 16, 1, 4, 2, 3, 1, 1, 4,
];
fn values(n: usize, cpf: bool) -> [u32; 27] {
    [
        0x7ffe8001,
        1,
        31,
        u32::from(cpf),
        31,
        (n - 1) as u32,
        2,
        8,
        15,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        1,
        0xbeef,
        0,
        7,
        0,
        0,
        0,
        0,
        0,
    ]
}
fn author(values: [u32; 27]) -> Vec<u8> {
    let mut bits = Vec::new();
    for (i, (width, value)) in WIDTHS.into_iter().zip(values).enumerate() {
        if i == 19 && values[3] == 0 {
            continue;
        }
        assert!(u64::from(value) < (1_u64 << width));
        for shift in (0..width).rev() {
            bits.push(((value >> shift) & 1) as u8);
        }
    }
    assert_eq!(bits.len(), if values[3] == 0 { 104 } else { 120 });
    store(&bits)
}
fn logical(n: usize, cpf: bool) -> Vec<u8> {
    let mut out = author(values(n, cpf));
    out.resize(n, 0x55);
    out
}
fn store(bits: &[u8]) -> Vec<u8> {
    let mut out = vec![0; bits.len().div_ceil(8)];
    for (i, bit) in bits.iter().enumerate() {
        assert!(*bit < 2);
        out[i / 8] |= *bit << (7 - i % 8);
    }
    out
}
// Independent fixture packer: expand individual logical bits, append explicitly
// authored carrier bits, then put complete groups in little/big source words.
fn pack(logical: &[u8], packing: P, tail_one: bool, high: u8) -> (Vec<u8>, Vec<u8>) {
    let mut bits: Vec<u8> = logical
        .iter()
        .flat_map(|byte| (0..8).rev().map(move |s| (byte >> s) & 1))
        .collect();
    if packing == P::CanonicalBe {
        return (logical.to_vec(), bits);
    }
    let width = if matches!(packing, P::Words14Be | P::Words14Le) {
        14
    } else {
        16
    };
    while bits.len() % width != 0 {
        bits.push(u8::from(tail_one));
    }
    let mut out = Vec::new();
    for group in bits.chunks_exact(width) {
        let payload = group
            .iter()
            .fold(0_u16, |acc, bit| (acc << 1) | u16::from(*bit));
        let word = payload
            | if width == 14 {
                u16::from(high) << 14
            } else {
                0
            };
        out.extend_from_slice(&if packing == P::Words14Be {
            word.to_be_bytes()
        } else {
            word.to_le_bytes()
        });
    }
    (out, bits)
}
fn identity() -> DtsSourceIdentity {
    DtsSourceIdentity {
        source: SOURCE,
        generation: 7,
        offset: 44,
    }
}
fn job(pool: &Budget, source: &[u8], packing: P) -> DtsPackedPayloadJob {
    DtsPackedPayloadJob::new(
        pool,
        identity(),
        Bytes::copy_from_slice(pool, source).unwrap(),
        packing,
        DtsPackedLimits {
            maximum_source_bytes: 32768,
            maximum_storage_bytes: 32768,
        },
    )
    .unwrap()
}
fn complete(job: &mut DtsPackedPayloadJob, quantum: u64) -> DtsPackedPayload {
    for _ in 0..100000 {
        match job
            .step(&mut WorkBudget::new(quantum), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(value) => return value,
            Step::Yield => {}
            other => panic!("owned progress {other:?}"),
        }
    }
    panic!("bounded owned progress");
}
fn settings() -> DtsNormalLimits {
    DtsNormalLimits {
        maximum_source_frame_bytes: 256,
        maximum_storage_bytes: 256,
        maximum_frames: 16,
        maximum_input_window: 4096,
    }
}
fn reader(
    pool: &Budget,
    length: Option<u64>,
    packing: P,
    limits: DtsNormalLimits,
) -> DtsNormalSourceReader {
    DtsNormalSourceReader::new(
        pool,
        SOURCE,
        SourceCapabilities {
            length,
            random_access: true,
        },
        packing,
        limits,
    )
    .unwrap()
}
fn feed(
    job: &mut DtsNormalSourceReader,
    pool: &Budget,
    source: &[u8],
    request: InputRequest,
    chunk: usize,
) {
    let start = usize::try_from(request.offset).unwrap();
    assert!(start <= source.len());
    if start == source.len() {
        job.finish(request.source, request.token).unwrap();
    } else {
        let end = source.len().min(start + chunk.min(request.maximum));
        assert!(end > start);
        let bytes = Bytes::copy_from_slice(pool, &source[start..end]).unwrap();
        job.provide(
            &InputWindow::new(request.source, request.token, request.offset, bytes).unwrap(),
        )
        .unwrap();
    }
}
fn next(
    job: &mut DtsNormalSourceReader,
    pool: &Budget,
    source: &[u8],
    chunk: usize,
    quantum: u64,
) -> Result<Option<DtsNormalSourceFrame>, Error> {
    for _ in 0..1000000 {
        match job.step(&mut WorkBudget::new(quantum), &AtomicBool::new(false))? {
            Step::Ready(value) => return Ok(Some(value)),
            Step::Yield => {}
            Step::NeedInput(request) => feed(job, pool, source, request, chunk),
            Step::End(end) => {
                assert_eq!(end.source, SOURCE);
                assert_eq!(end.offset, source.len() as u64);
                return Ok(None);
            }
        }
    }
    panic!("bounded source progress");
}

#[test]
fn literal_source_syncs_detect_all_four_packings_with_high_bits_preserved() {
    for (source, packing) in [
        (&[0x7f, 0xfe, 0x80, 0x01, 0xfc, 0x00][..], P::CanonicalBe),
        (&[0xfe, 0x7f, 0x01, 0x80, 0x00, 0xfc][..], P::Words16Le),
        (&[0x1f, 0xff, 0xe8, 0x00, 0x07, 0xf0][..], P::Words14Be),
        (&[0xff, 0x1f, 0x00, 0xe8, 0xf0, 0x07][..], P::Words14Le),
    ] {
        assert_eq!(P::detect(source), Ok(packing));
    }
    assert_eq!(P::detect(&[0x1f, 0xff, 0xe8, 0x00]), Err(Error::Truncated));
    assert!(matches!(P::detect(&[0; 6]), Err(Error::Invalid(_))));
}

#[test]
fn all448_payload_controls_keep_every_bit_all_tails_and_unused_high_values() {
    let mut cases = 0;
    for n in 96..=102 {
        for cpf in [false, true] {
            for packing in PACKINGS {
                for tail in [false, true] {
                    for high in 0..4 {
                        cases += 1;
                        let (raw, bits) = pack(&logical(n, cpf), packing, tail, high);
                        let pool = Budget::new(65536).unwrap();
                        let baseline = pool.used();
                        let mut job = job(&pool, &raw, packing);
                        let output = complete(&mut job, 1);
                        assert_eq!(output.source_bytes().as_ref(), raw);
                        assert_eq!(output.storage_bytes().as_ref(), store(&bits));
                        assert_eq!(output.valid_bits(), bits.len());
                        assert_eq!(output.identity(), identity());
                        assert_eq!(
                            usize::from(output.alignment_zero_bits()),
                            (8 - bits.len() % 8) % 8
                        );
                        for (i, expected) in bits.iter().enumerate() {
                            assert_eq!(output.bit(i).unwrap(), *expected);
                        }
                        let header = inspect_dts_header_bits(
                            output.storage_bytes().as_ref(),
                            output.valid_bits(),
                        )
                        .unwrap();
                        for (i, field) in F::ALL.into_iter().enumerate() {
                            assert_eq!(
                                header.value(field),
                                if i == 19 && !cpf {
                                    None
                                } else {
                                    Some(values(n, cpf)[i])
                                }
                            );
                        }
                        drop(output);
                        drop(job);
                        assert_eq!(pool.used(), baseline);
                    }
                }
            }
        }
    }
    assert_eq!(cases, 448);
}

#[test]
fn every_payload_bit_maps_back_to_exact_source_byte_bit_and_generation() {
    for packing in PACKINGS {
        let (raw, bits) = pack(&logical(97, true), packing, true, 2);
        let pool = Budget::new(65536).unwrap();
        let output = complete(&mut job(&pool, &raw, packing), 31);
        for (i, expected) in bits.iter().enumerate() {
            let at = output.source_bit(i).unwrap();
            assert_eq!(at.source, SOURCE);
            assert_eq!(at.generation, 7);
            let source = raw[(at.offset - 44) as usize];
            assert_eq!((source >> (7 - at.msb0_bit)) & 1, *expected);
        }
        assert!(matches!(
            output.source_bit(bits.len()),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(output.bit(bits.len()), Err(Error::Invalid(_))));
    }
}

#[test]
fn unused_word_high_bits_are_source_facts_not_normalized_marker_requirements() {
    for packing in [P::Words14Be, P::Words14Le] {
        for high in 0..4 {
            let (raw, _) = pack(&logical(96, false), packing, false, high);
            let pool = Budget::new(65536).unwrap();
            let output = complete(&mut job(&pool, &raw, packing), 31);
            for word in 0..raw.len() / 2 {
                assert_eq!(output.unused_word_high_bits(word), Ok(Some(high)));
            }
            assert!(output.unused_word_high_bits(raw.len() / 2).is_err());
        }
    }
    let pool = Budget::new(65536).unwrap();
    let output = complete(&mut job(&pool, &[0; 4], P::Words16Le), 1);
    assert_eq!(output.unused_word_high_bits(0), Ok(None));
}

#[test]
fn canonical_payload_shares_source_owner_and_checked_extents_fail_explicitly() {
    let pool = Budget::new(65536).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &[1, 2, 3]).unwrap();
    let initial = pool.used();
    let mut job = DtsPackedPayloadJob::new(
        &pool,
        identity(),
        raw,
        P::CanonicalBe,
        DtsPackedLimits {
            maximum_source_bytes: 3,
            maximum_storage_bytes: 3,
        },
    )
    .unwrap();
    let out = complete(&mut job, 1);
    assert_eq!(pool.used(), initial);
    assert_eq!(out.storage_bytes().as_ref(), [1, 2, 3]);
    assert_eq!(P::Words14Be.payload_bits(3), Err(Error::Truncated));
    assert_eq!(
        P::CanonicalBe.payload_bits(usize::MAX),
        Err(Error::Overflow)
    );
    let overflowing = DtsSourceIdentity {
        offset: u64::MAX,
        ..identity()
    };
    assert!(matches!(
        DtsPackedPayloadJob::new(
            &pool,
            overflowing,
            out.source_bytes().clone(),
            P::CanonicalBe,
            DtsPackedLimits {
                maximum_source_bytes: 3,
                maximum_storage_bytes: 3
            }
        ),
        Err(Error::Overflow)
    ));
}

#[test]
fn payload_backpressure_retry_keeps_same_source_without_duplicate_publication() {
    let pool = Budget::new(65536).unwrap();
    let (raw, bits) = pack(&logical(96, false), P::Words14Le, true, 0);
    let mut job = job(&pool, &raw, P::Words14Le);
    let blocker = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        job.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    drop(blocker);
    let output = complete(&mut job, 1);
    assert_eq!(output.storage_bytes().as_ref(), store(&bits));
    assert!(matches!(
        job.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Ok(Step::End(_))
    ));
}

#[test]
fn freeze_metadata_pressure_preserves_converted_builder_for_exact_retry() {
    let pool = Budget::new(65536).unwrap();
    let baseline = pool.used();
    let (raw, bits) = pack(&logical(96, false), P::Words14Be, true, 3);
    let mut job = job(&pool, &raw, P::Words14Be);
    let before = pool.used();
    assert!(matches!(
        job.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Ok(Step::Yield)
    ));
    assert!(pool.used() > before);
    let blocker = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        job.step(&mut WorkBudget::new(100000), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    drop(blocker);
    let output = complete(&mut job, 1);
    assert_eq!(output.source_bytes().as_ref(), raw);
    assert_eq!(output.storage_bytes().as_ref(), store(&bits));
    drop(output);
    drop(job);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn zero_work_and_sticky_cancel_release_payload_owners_to_captured_baseline() {
    let pool = Budget::new(65536).unwrap();
    let baseline = pool.used();
    let (raw, _) = pack(&logical(96, true), P::Words14Be, true, 3);
    let mut job = job(&pool, &raw, P::Words14Be);
    let initial = pool.used();
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(Step::Yield)
    ));
    assert_eq!(pool.used(), initial);
    assert_eq!(
        job.step(&mut WorkBudget::new(1), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(pool.used(), baseline);
    assert_eq!(
        job.step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Cancelled
    );
}

#[test]
fn all27_header_fields_follow_independently_authored_conditional_source_widths() {
    for cpf in [false, true] {
        let expected = values(96, cpf);
        let source = author(expected);
        assert_eq!(
            &source[..6],
            if cpf {
                &[0x7f, 0xfe, 0x80, 0x01, 0xfe, 0x7c]
            } else {
                &[0x7f, 0xfe, 0x80, 0x01, 0xfc, 0x7c]
            }
        );
        let header = inspect_dts_header_bits(&source, source.len() * 8).unwrap();
        let mut position = 0;
        for (i, field) in F::ALL.into_iter().enumerate() {
            if i == 19 && !cpf {
                assert_eq!(header.field(field), FieldObservation::NotInSyntax);
                continue;
            }
            assert_eq!(header.value(field), Some(expected[i]));
            assert_eq!(
                header.field(field).bit_range(),
                Some(position..position + WIDTHS[i] as u16)
            );
            position += WIDTHS[i] as u16;
        }
        assert_eq!(
            header.required_header_bits(),
            Some(if cpf { 120 } else { 104 })
        );
        assert!(header.syntax_complete());
        assert_eq!(header.source_status(), Status::NormalHeader);
        assert_eq!(header.issues(), 0);
        let views = header.views();
        assert_eq!(views.core_rate_hz, Some(44100));
        assert_eq!(views.coded_window_samples, Some(1024));
        assert_eq!(views.normal_window.unwrap().numerator(), 256);
        assert_eq!(views.normal_window.unwrap().denominator(), 11025);
        assert_eq!(views.nominal_bitrate, Some(768000));
        assert_eq!(views.full_bandwidth_channels, Some(2));
    }
}

#[test]
fn every_exact_bit_prefix_keeps_presence_and_never_uses_storage_alignment_zeros() {
    for cpf in [false, true] {
        let expected = values(96, cpf);
        let source = author(expected);
        for available in 0..=source.len() * 8 {
            let h = inspect_dts_header_bits(&source, available).unwrap();
            let mut position = 0;
            for (i, field) in F::ALL.into_iter().enumerate() {
                if i == 19 && !cpf && available >= 39 {
                    assert_eq!(h.field(field), FieldObservation::NotInSyntax);
                    continue;
                }
                if position >= available {
                    assert_eq!(h.field(field), FieldObservation::NotReached);
                } else {
                    let observed = WIDTHS[i].min(available - position);
                    let prefix = expected[i] >> (WIDTHS[i] - observed);
                    assert_eq!(
                        h.field(field),
                        FieldObservation::Bits {
                            offset: position as u16,
                            width: WIDTHS[i] as u8,
                            observed_bits: observed as u8,
                            prefix
                        }
                    );
                }
                position += WIDTHS[i];
            }
            assert_eq!(h.syntax_complete(), available == source.len() * 8);
        }
    }
    assert!(matches!(
        inspect_dts_header_bits(&[0; 1], 9),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn seven_low14_words_do_not_turn_six_alignment_zeros_into_header_presence() {
    for packing in [P::Words14Be, P::Words14Le] {
        let (raw, _) = pack(&logical(96, false), packing, false, 0);
        let pool = Budget::new(65536).unwrap();
        let output = complete(&mut job(&pool, &raw[..14], packing), 1);
        assert_eq!(output.valid_bits(), 98);
        assert_eq!(output.storage_bytes().len(), 13);
        assert_eq!(output.alignment_zero_bits(), 6);
        let h =
            inspect_dts_header_bits(output.storage_bytes().as_ref(), output.valid_bits()).unwrap();
        assert_eq!(h.source_status(), Status::Truncated);
        assert!(!h.syntax_complete());
        assert_eq!(h.field(F::DialogWord), FieldObservation::NotReached);
    }
}

#[test]
fn source_header_errors_and_future_fields_stay_distinct_from_native_permissiveness() {
    for (index, raw, status) in [
        (8, 31, Status::Invalid),
        (7, 15, Status::Invalid),
        (17, 3, Status::Invalid),
        (23, 4, Status::Invalid),
        (9, 1, Status::Invalid),
        (21, 12, Status::Unsupported),
        (6, 16, Status::NormalHeader),
    ] {
        let mut fields = values(96, true);
        fields[index] = raw;
        let source = author(fields);
        let h = inspect_dts_header_bits(&source, 120).unwrap();
        assert_eq!(h.value(F::ALL[index]), Some(raw));
        assert_eq!(h.source_status(), status);
        assert!(h.syntax_complete());
    }
}

#[test]
fn full24_projection_matches_frozen_source_domains_and_stays_null_when_incomplete() {
    let source = author(values(96, true));
    let h = inspect_dts_header_bits(&source, 120).unwrap();
    let expected = [
        1, 32, 1, 32, 96, 2, 8, 15, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 7, 0, 0, 0, 0, 0,
    ];
    assert_eq!(dts_bits24::fields(&h), Some(expected));
    let mut serialized = Vec::new();
    dts_bits24::write(&mut serialized, Some(expected)).unwrap();
    assert_eq!(
        String::from_utf8(serialized).unwrap().matches(':').count(),
        24
    );
    for available in 0..120 {
        assert_eq!(
            dts_bits24::fields(&inspect_dts_header_bits(&source, available).unwrap()),
            None
        );
    }
    let mut serialized = Vec::new();
    dts_bits24::write(&mut serialized, None).unwrap();
    assert_eq!(serialized, b"null");
}

#[test]
fn variable_intervals_all_four_packings_and_carrier_tails_survive_chunks_work_and_eof() {
    for packing in PACKINGS {
        for tail_one in [false, true] {
            let records: Vec<_> = (96..=102)
                .map(|n| pack(&logical(n, n % 2 == 0), packing, tail_one, 2))
                .collect();
            let source: Vec<u8> = records
                .iter()
                .flat_map(|(raw, _)| raw.iter().copied())
                .collect();
            for known in [false, true] {
                for chunk in [1, 7, 4096] {
                    for quantum in [1, 31] {
                        let pool = Budget::new(1048576).unwrap();
                        let baseline = pool.used();
                        let mut reader = reader(
                            &pool,
                            known.then_some(source.len() as u64),
                            packing,
                            settings(),
                        );
                        let generation = reader.generation();
                        let mut offset = 0;
                        let mut retained = Vec::new();
                        for (i, (raw, bits)) in records.iter().enumerate() {
                            let frame = next(&mut reader, &pool, &source, chunk, quantum)
                                .unwrap()
                                .unwrap();
                            let f = frame.facts();
                            assert_eq!(frame.source_bytes().as_ref(), raw);
                            assert_eq!(frame.storage_bytes().as_ref(), store(bits));
                            assert_eq!(
                                f.identity,
                                DtsSourceIdentity {
                                    source: SOURCE,
                                    generation,
                                    offset
                                }
                            );
                            assert_eq!(
                                f.header.views().declared_frame_bytes,
                                Some((96 + i) as u32)
                            );
                            assert_eq!(f.header.source_status(), Status::NormalHeader);
                            assert_eq!(f.source_payload_bits, bits.len());
                            assert_eq!(f.packing, packing);
                            assert_eq!(f.skipped_short_syncs, 0);
                            let count = bits.len() - (96 + i) * 8;
                            let value = if tail_one { (1_u16 << count) - 1 } else { 0 };
                            assert_eq!(
                                f.carrier_tail,
                                Some(DtsCarrierTail {
                                    start_bit: (96 + i) * 8,
                                    bit_count: count as u8,
                                    value
                                })
                            );
                            assert_eq!(
                                f.extent_relation,
                                if raw.len() == 96 + i {
                                    DtsExtentRelation::Both
                                } else {
                                    DtsExtentRelation::PayloadWordCount
                                }
                            );
                            offset += raw.len() as u64;
                            assert_eq!(
                                f.boundary,
                                if i == 6 {
                                    DtsNormalBoundary::ExplicitSourceEnd
                                } else {
                                    DtsNormalBoundary::FollowingNormalSync { offset }
                                }
                            );
                            retained.push(frame);
                        }
                        assert!(
                            next(&mut reader, &pool, &source, chunk, quantum)
                                .unwrap()
                                .is_none()
                        );
                        drop(reader);
                        for (frame, (raw, bits)) in retained.iter().zip(&records) {
                            assert_eq!(frame.source_bytes().as_ref(), raw);
                            assert_eq!(frame.storage_bytes().as_ref(), store(bits));
                        }
                        drop(retained);
                        assert_eq!(pool.used(), baseline);
                    }
                }
            }
        }
    }
}

#[test]
fn physical_byte_equality_does_not_invent_missing_declared_canonical_bytes() {
    for packing in [P::Words14Be, P::Words14Le] {
        let mut source = logical(96, false);
        source[..13].copy_from_slice(&author(values(110, false)));
        let (raw, bits) = pack(&source, packing, true, 1);
        assert_eq!(raw.len(), 110);
        let pool = Budget::new(65536).unwrap();
        let mut reader = reader(&pool, None, packing, settings());
        let frame = next(&mut reader, &pool, &raw, 3, 1).unwrap().unwrap();
        let facts = frame.facts();
        assert_eq!(facts.header.views().declared_frame_bytes, Some(110));
        assert_eq!(facts.extent_relation, DtsExtentRelation::PhysicalByteCount);
        assert_eq!(facts.carrier_tail, None);
        assert_eq!(frame.storage_bytes().as_ref(), store(&bits));
        assert_eq!(frame.storage_bytes().len(), 97);
    }
}

#[test]
fn explicit_eof_reports_unmatched_extent_without_cropping_or_certifying_the_frame() {
    let mut raw = logical(100, false);
    raw[..13].copy_from_slice(&author(values(96, false)));
    let pool = Budget::new(65536).unwrap();
    let mut reader = reader(&pool, None, P::CanonicalBe, settings());
    let frame = next(&mut reader, &pool, &raw, 1, 1).unwrap().unwrap();
    assert_eq!(frame.source_bytes().as_ref(), raw);
    assert_eq!(frame.facts().extent_relation, DtsExtentRelation::Neither);
    assert_eq!(frame.facts().carrier_tail, None);
    assert_eq!(frame.facts().boundary, DtsNormalBoundary::ExplicitSourceEnd);
}

#[test]
fn interior_short_normal_sync_is_preserved_as_payload_not_an_early_frame() {
    for packing in PACKINGS {
        let mut logical = logical(96, false);
        logical[28..34].copy_from_slice(&[0x7f, 0xfe, 0x80, 0x01, 0xfc, 0]);
        let (raw, bits) = pack(&logical, packing, true, 3);
        let mut source = raw.clone();
        source.extend_from_slice(&pack(&self::logical(98, true), packing, false, 3).0);
        let pool = Budget::new(65536).unwrap();
        let mut reader = reader(&pool, None, packing, settings());
        let frame = next(&mut reader, &pool, &source, 1, 1).unwrap().unwrap();
        assert_eq!(frame.source_bytes().as_ref(), raw);
        assert_eq!(frame.storage_bytes().as_ref(), store(&bits));
        assert_eq!(frame.facts().skipped_short_syncs, 1);
    }
}

#[test]
fn source_backpressure_is_retryable_and_respects_pending_input_ownership() {
    let pool = Budget::new(65536).unwrap();
    let baseline = pool.used();
    let (raw, _) = pack(&logical(96, true), P::Words14Le, true, 0);
    let mut reader = reader(&pool, None, P::Words14Le, settings());
    let request = reader.request();
    feed(&mut reader, &pool, &raw, request, 4096);
    let blocker = pool.reserve(pool.available()).unwrap();
    let mut saw = false;
    for _ in 0..1000 {
        match reader.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Err(Error::Backpressure { .. }) => {
                saw = true;
                break;
            }
            Ok(Step::Yield) => {}
            other => panic!("expected reserve pressure {other:?}"),
        }
    }
    assert!(saw);
    drop(blocker);
    let frame = next(&mut reader, &pool, &raw, 1, 1).unwrap().unwrap();
    assert_eq!(frame.source_bytes().as_ref(), raw);
    drop(frame);
    drop(reader);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn source_cancel_is_sticky_and_drops_cursor_builders_and_pending_owners() {
    let pool = Budget::new(65536).unwrap();
    let baseline = pool.used();
    let raw = logical(96, false);
    let mut reader = reader(&pool, None, P::CanonicalBe, settings());
    let request = reader.request();
    feed(&mut reader, &pool, &raw, request, 4096);
    assert!(matches!(
        reader.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(Step::Yield)
    ));
    assert_eq!(
        reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(pool.used(), baseline);
    assert_eq!(
        reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Cancelled
    );
}

#[test]
fn reset_advances_generation_rejects_stale_windows_and_preserves_retained_frames() {
    let (raw, _) = pack(&logical(98, false), P::Words14Le, false, 0);
    let pool = Budget::new(65536).unwrap();
    let mut reader = reader(&pool, None, P::Words14Le, settings());
    let old = reader.request();
    let old_generation = reader.generation();
    let first = next(&mut reader, &pool, &raw, 1, 1).unwrap().unwrap();
    let request = reader.reset(0).unwrap();
    assert_ne!(request.token.generation(), old_generation);
    let stale = InputWindow::new(
        SOURCE,
        old.token,
        0,
        Bytes::copy_from_slice(&pool, &raw).unwrap(),
    )
    .unwrap();
    assert_eq!(
        reader.provide(&stale),
        Err(Error::StaleGeneration {
            expected: request.token.generation(),
            received: old_generation
        })
    );
    let second = next(&mut reader, &pool, &raw, 2, 31).unwrap().unwrap();
    assert_ne!(
        first.facts().identity.generation,
        second.facts().identity.generation
    );
    drop(reader);
    assert_eq!(first.source_bytes().as_ref(), raw);
    assert_eq!(second.source_bytes().as_ref(), raw);
}

#[test]
fn odd_word_eof_limits_frame_count_and_foreign_pool_are_explicit_errors() {
    let pool = Budget::new(65536).unwrap();
    let (mut raw, _) = pack(&logical(96, false), P::Words14Le, false, 0);
    raw.push(1);
    let mut r = reader(&pool, None, P::Words14Le, settings());
    assert_eq!(
        next(&mut r, &pool, &raw, 1, 1).unwrap_err(),
        Error::Truncated
    );
    let raw = logical(96, false);
    let mut limits = settings();
    limits.maximum_source_frame_bytes = 95;
    let mut r = reader(&pool, None, P::CanonicalBe, limits);
    assert!(matches!(
        next(&mut r, &pool, &raw, 7, 31),
        Err(Error::Limit { .. })
    ));
    let mut source = raw.clone();
    source.extend_from_slice(&raw);
    let mut limits = settings();
    limits.maximum_frames = 1;
    let mut r = reader(&pool, None, P::CanonicalBe, limits);
    assert!(next(&mut r, &pool, &source, 1, 1).unwrap().is_some());
    assert!(matches!(
        next(&mut r, &pool, &source, 1, 1),
        Err(Error::Limit { .. })
    ));
    let other = Budget::new(4096).unwrap();
    let raw = Bytes::copy_from_slice(&other, &[0; 4]).unwrap();
    assert!(matches!(
        DtsPackedPayloadJob::new(
            &pool,
            identity(),
            raw,
            P::Words14Be,
            DtsPackedLimits {
                maximum_source_bytes: 4,
                maximum_storage_bytes: 4
            }
        ),
        Err(Error::ForeignPool)
    ));
}

#[test]
fn explicit_transport_failure_does_not_become_entity_eof_or_a_recoverable_reset() {
    let pool = Budget::new(65536).unwrap();
    let mut reader = reader(&pool, None, P::Words14Be, settings());
    let request = reader.request();
    reader
        .fail(request.source, request.token, SourceFailure::Transport)
        .unwrap();
    assert_eq!(
        reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Transport
    );
    assert_eq!(reader.reset(0).unwrap_err(), Error::Transport);
}
