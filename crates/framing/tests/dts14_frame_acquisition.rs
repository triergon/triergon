//! Original source contracts: complete exact-word normal frames, not decoded audio.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Bytes, Error, InputWindow, SourceCapabilities, SourceFailure, SourceId, Step,
    WorkBudget,
};
use triergon_framing::{
    Dts14BoundaryEvidence, Dts14Frame, Dts14FrameReader, Dts14SyncLimits, DtsWordOrder,
};

const SOURCE: SourceId = SourceId(142168);
const BE: &[u8] = include_bytes!("../fixtures/dts-native-v1/variable-big.bin");
const LE: &[u8] = include_bytes!("../fixtures/dts-native-v1/variable-little.bin");
const FIXED: &[u8] = include_bytes!("../fixtures/dts-native-v1/fixed-big.bin");
const REAL: &[u8] = include_bytes!("../fixtures/dts-native-v1/real-prefix-le.bin");
fn limits() -> Dts14SyncLimits {
    Dts14SyncLimits {
        maximum_physical_frame_bytes: 4096,
        maximum_canonical_frame_bytes: 3584,
        maximum_frames: 8,
        maximum_input_window: 4096,
    }
}
fn new(pool: &Budget, order: DtsWordOrder, known: Option<u64>) -> Dts14FrameReader {
    Dts14FrameReader::new(
        pool,
        SOURCE,
        SourceCapabilities {
            length: known,
            random_access: true,
        },
        order,
        limits(),
    )
    .unwrap()
}
fn next(
    job: &mut Dts14FrameReader,
    pool: &Budget,
    input: &[u8],
    chunk: usize,
    q: u64,
) -> Result<Option<Dts14Frame>, Error> {
    for _ in 0..1_000_000 {
        match job.step(&mut WorkBudget::new(q), &AtomicBool::new(false))? {
            Step::Ready(f) => return Ok(Some(f)),
            Step::End(end) => {
                assert_eq!(end.offset, input.len() as u64);
                return Ok(None);
            }
            Step::Yield => {}
            Step::NeedInput(r) => {
                let at = r.offset as usize;
                assert!(at <= input.len());
                if at == input.len() {
                    job.finish(r.source, r.token)?
                } else {
                    let end = (at + chunk.min(r.maximum)).min(input.len());
                    let b = Bytes::copy_from_slice(pool, &input[at..end])?;
                    job.provide(&InputWindow::new(r.source, r.token, r.offset, b)?)?;
                }
            }
        }
    }
    panic!("finite source/work bound")
}
fn assert_variable(input: &[u8], order: DtsWordOrder) {
    for chunk in [1, 3, 31, 4096] {
        for q in [1, 31] {
            for known in [None, Some(input.len() as u64)] {
                let pool = Budget::new(200_000).unwrap();
                let baseline = pool.used();
                let mut job = new(&pool, order, known);
                let generation = job.generation();
                let mut at = 0;
                for (index, (physical, canonical)) in
                    [(112, 98), (128, 112), (160, 140)].into_iter().enumerate()
                {
                    let frame = next(&mut job, &pool, input, chunk, q)
                        .expect("selected normal DTS14 variable exact-word frame")
                        .expect("real complete frame");
                    assert_eq!(frame.physical_bytes().as_ref(), &input[at..at + physical]);
                    assert_eq!(frame.canonical_bytes().len(), canonical);
                    let f = frame.facts();
                    assert_eq!(f.identity.source, SOURCE);
                    assert_eq!(f.identity.generation, generation);
                    assert_eq!(f.identity.offset, at as u64);
                    assert_eq!(f.physical_bytes, physical);
                    assert_eq!(f.payload_bytes, canonical);
                    assert_eq!(f.word_order, order);
                    assert_eq!(f.header.fsize as usize + 1, canonical);
                    assert_eq!(f.header.core_rate_hz, 48_000);
                    assert_eq!(f.header.coding_window_samples, 256);
                    assert!(
                        frame.canonical_bytes().as_ref()[13..]
                            .iter()
                            .all(|b| *b == 0x50 + index as u8)
                    );
                    assert_eq!(
                        f.boundary,
                        if index == 2 {
                            Dts14BoundaryEvidence::ExplicitSourceEnd
                        } else {
                            Dts14BoundaryEvidence::FollowingNormalSync {
                                physical_offset: (at + physical) as u64,
                            }
                        }
                    );
                    at += physical;
                }
                assert!(next(&mut job, &pool, input, chunk, q).unwrap().is_none());
                drop(job);
                assert_eq!(pool.used(), baseline);
            }
        }
    }
}
#[test]
fn varying_normal_frames_big_endian() {
    assert_variable(BE, DtsWordOrder::BigEndian)
}
#[test]
fn varying_normal_frames_little_endian() {
    assert_variable(LE, DtsWordOrder::LittleEndian)
}
#[test]
fn real_prefix_retains_all_four_exact_source_spans() {
    for chunk in [1, 7, 4096] {
        let pool = Budget::new(200_000).unwrap();
        let mut job = new(&pool, DtsWordOrder::LittleEndian, None);
        for i in 0..4 {
            let f = next(&mut job, &pool, REAL, chunk, 31).unwrap().unwrap();
            assert_eq!(f.physical_bytes().as_ref(), &REAL[i * 4096..(i + 1) * 4096]);
            assert_eq!(f.canonical_bytes().len(), 3584);
            assert_eq!(f.facts().header.fsize, 3583);
            assert_eq!(f.facts().identity.offset, (i * 4096) as u64);
        }
        assert!(next(&mut job, &pool, REAL, chunk, 31).unwrap().is_none());
    }
}
#[test]
fn big_and_little_real_storage_have_identical_source_payload() {
    let mut swapped = REAL.to_vec();
    for pair in swapped.chunks_exact_mut(2) {
        pair.swap(0, 1);
    }
    let pool = Budget::new(300_000).unwrap();
    let mut le = new(&pool, DtsWordOrder::LittleEndian, None);
    let mut be = new(&pool, DtsWordOrder::BigEndian, None);
    for _ in 0..4 {
        let a = next(&mut le, &pool, REAL, 13, 31).unwrap().unwrap();
        let b = next(&mut be, &pool, &swapped, 13, 31).unwrap().unwrap();
        assert_eq!(a.canonical_bytes().as_ref(), b.canonical_bytes().as_ref());
        assert_eq!(a.facts().header, b.facts().header);
    }
}
#[test]
fn fixed_interval_control_preserves_all_high_bits() {
    let pool = Budget::new(200_000).unwrap();
    let mut j = new(&pool, DtsWordOrder::BigEndian, None);
    for i in 0..2 {
        let f = next(&mut j, &pool, FIXED, 3, 1).unwrap().unwrap();
        assert_eq!(f.physical_bytes().as_ref(), &FIXED[i * 128..(i + 1) * 128]);
        assert_eq!(f.canonical_bytes().len(), 112);
    }
    assert!(next(&mut j, &pool, FIXED, 3, 1).unwrap().is_none());
}
#[test]
fn one_frame_explicit_end_does_not_need_a_fabricated_sync() {
    let input = &BE[..112];
    let pool = Budget::new(200_000).unwrap();
    let mut j = new(&pool, DtsWordOrder::BigEndian, None);
    let f = next(&mut j, &pool, input, 1, 1).unwrap().unwrap();
    assert_eq!(f.facts().boundary, Dts14BoundaryEvidence::ExplicitSourceEnd);
    assert_eq!(f.physical_bytes().as_ref(), input);
    assert!(next(&mut j, &pool, input, 1, 1).unwrap().is_none());
}
#[test]
fn odd_physical_end_is_truncated_and_sticky() {
    let pool = Budget::new(200_000).unwrap();
    let mut j = new(&pool, DtsWordOrder::BigEndian, None);
    let input = &BE[..111];
    assert_eq!(
        next(&mut j, &pool, input, 3, 31).unwrap_err(),
        Error::Truncated
    );
    assert_eq!(
        j.step(&mut WorkBudget::new(31), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Truncated
    );
}
#[test]
fn geometry_mismatch_stays_unsupported_without_trimming_source() {
    let pool = Budget::new(200_000).unwrap();
    let mut j = new(&pool, DtsWordOrder::BigEndian, None);
    assert!(matches!(
        next(&mut j, &pool, &BE[..110], 3, 31),
        Err(Error::Unsupported(_))
    ));
}
#[test]
fn zero_work_reads_nothing() {
    let p = Budget::new(200_000).unwrap();
    let mut j = new(&p, DtsWordOrder::BigEndian, None);
    let r = j.request();
    let n = p.used();
    assert!(matches!(
        j.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(Step::Yield)
    ));
    assert_eq!(j.position(), 0);
    let after = j.request();
    assert_eq!(
        (after.source, after.offset, after.maximum, after.token),
        (r.source, r.offset, r.maximum, r.token)
    );
    assert_eq!(p.used(), n);
}
#[test]
fn cancel_is_sticky_even_with_zero_work() {
    let p = Budget::new(200_000).unwrap();
    let mut j = new(&p, DtsWordOrder::BigEndian, None);
    let c = AtomicBool::new(true);
    assert_eq!(
        j.step(&mut WorkBudget::new(0), &c).unwrap_err(),
        Error::Cancelled
    );
    c.store(false, Ordering::Relaxed);
    assert_eq!(
        j.step(&mut WorkBudget::new(31), &c).unwrap_err(),
        Error::Cancelled
    );
}
#[test]
fn foreign_window_rejected_before_consumption() {
    let p = Budget::new(200_000).unwrap();
    let foreign = Budget::new(200_000).unwrap();
    let mut j = new(&p, DtsWordOrder::BigEndian, None);
    let r = j.request();
    let w = InputWindow::new(
        r.source,
        r.token,
        r.offset,
        Bytes::copy_from_slice(&foreign, &BE[..16]).unwrap(),
    )
    .unwrap();
    assert_eq!(j.provide(&w), Err(Error::ForeignPool));
    assert_eq!(j.position(), 0);
}
#[test]
fn reset_rejects_stale_window_and_preserves_real_offset() {
    let p = Budget::new(200_000).unwrap();
    let mut j = new(&p, DtsWordOrder::BigEndian, None);
    let old = j.request();
    let stale = InputWindow::new(
        old.source,
        old.token.clone(),
        old.offset,
        Bytes::copy_from_slice(&p, &BE[..16]).unwrap(),
    )
    .unwrap();
    let r = j.reset(112).unwrap();
    assert!(j.provide(&stale).is_err());
    assert_eq!(j.position(), 112);
    assert_ne!(old.token, r.token);
    let f = next(&mut j, &p, BE, 31, 31).unwrap().unwrap();
    assert_eq!(f.facts().identity.offset, 112);
    assert_eq!(f.physical_bytes().as_ref(), &BE[112..240]);
}
#[test]
fn retained_frame_survives_reader_reset_and_drop() {
    let p = Budget::new(200_000).unwrap();
    let baseline = p.used();
    let mut j = new(&p, DtsWordOrder::BigEndian, None);
    let f = next(&mut j, &p, FIXED, 13, 31).unwrap().unwrap();
    let clone = f.clone();
    j.reset(0).unwrap();
    drop(j);
    drop(f);
    assert_eq!(clone.physical_bytes().as_ref(), &FIXED[..128]);
    assert!(p.used() > baseline);
    drop(clone);
    assert_eq!(p.used(), baseline);
}
#[test]
fn transport_failure_is_not_entity_end() {
    let p = Budget::new(200_000).unwrap();
    let mut j = new(&p, DtsWordOrder::BigEndian, None);
    let r = j.request();
    j.fail(r.source, r.token, SourceFailure::Transport).unwrap();
    assert_eq!(
        j.step(&mut WorkBudget::new(31), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Transport
    );
}
#[test]
fn frame_limit_remains_a_real_source_limit() {
    let p = Budget::new(200_000).unwrap();
    let mut l = limits();
    l.maximum_frames = 1;
    let mut j = Dts14FrameReader::new(
        &p,
        SOURCE,
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        DtsWordOrder::BigEndian,
        l,
    )
    .unwrap();
    assert!(next(&mut j, &p, FIXED, 13, 31).unwrap().is_some());
    assert!(matches!(
        next(&mut j, &p, FIXED, 13, 31),
        Err(Error::Limit {
            requested: 2,
            maximum: 1
        })
    ));
}
#[test]
fn impossible_capacity_is_rejected_without_allocating_media() {
    let p = Budget::new(256).unwrap();
    let before = p.used();
    assert!(matches!(
        Dts14FrameReader::new(
            &p,
            SOURCE,
            SourceCapabilities {
                length: None,
                random_access: false
            },
            DtsWordOrder::BigEndian,
            limits()
        ),
        Err(Error::Limit { .. })
    ));
    assert_eq!(p.used(), before);
}

#[test]
fn new_cadence_reader_retries_real_pool_pressure_without_rereading_header() {
    let pool = Budget::new(200_000).unwrap();
    let mut job = new(&pool, DtsWordOrder::BigEndian, None);
    let r = job.request();
    job.provide(
        &InputWindow::new(
            r.source,
            r.token,
            0,
            Bytes::copy_from_slice(&pool, &BE[..18]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let blocker = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        job.step(&mut WorkBudget::new(100), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(job.position(), 18);
    drop(blocker);
    let first = next(&mut job, &pool, BE, 1, 1).unwrap().unwrap();
    assert_eq!(first.physical_bytes().as_ref(), &BE[..112]);
    let second = next(&mut job, &pool, BE, 1, 1).unwrap().unwrap();
    assert_eq!(second.physical_bytes().as_ref(), &BE[112..240]);
    assert_eq!(second.facts().header.frame_bytes, 112);
}

#[test]
fn cancelling_new_cadence_during_body_releases_all_unpublished_owners() {
    let pool = Budget::new(200_000).unwrap();
    let baseline = pool.used();
    let mut job = new(&pool, DtsWordOrder::LittleEndian, None);
    let r = job.request();
    job.provide(
        &InputWindow::new(
            r.source,
            r.token,
            0,
            Bytes::copy_from_slice(&pool, &LE[..64]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(matches!(
        job.step(&mut WorkBudget::new(64), &AtomicBool::new(false)),
        Ok(Step::Yield)
    ));
    assert!(job.position() > 18 && job.position() < 112);
    assert_eq!(
        job.step(&mut WorkBudget::new(0), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(pool.used(), baseline);
    assert_eq!(
        job.step(&mut WorkBudget::new(31), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Cancelled
    );
}
