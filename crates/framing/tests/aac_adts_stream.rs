#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputWindow, SourceCapabilities, SourceFailure, SourceId,
    SourceLocation, Step, WorkBudget,
};
use triergon_framing::{AacAdtsFrame, AacAdtsStreamLimits, AacAdtsStreamReader};

fn limits() -> AacAdtsStreamLimits {
    AacAdtsStreamLimits {
        maximum_frame_bytes: 8191,
        maximum_frames: 1000,
        maximum_input_window: 8192,
    }
}
fn wire(payload: &[u8], crc: Option<u16>) -> Vec<u8> {
    let size = payload.len() + if crc.is_some() { 9 } else { 7 };
    let mut out = vec![
        0xff,
        if crc.is_some() { 0xf0 } else { 0xf1 },
        0x4c,
        0x80 | ((size >> 11) as u8),
        (size >> 3) as u8,
        ((size & 7) as u8) << 5 | 31,
        0xfc,
    ];
    if let Some(n) = crc {
        out.extend_from_slice(&n.to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}
fn new(pool: &Budget, length: Option<u64>) -> AacAdtsStreamReader {
    AacAdtsStreamReader::new(
        pool,
        SourceId(901),
        SourceCapabilities {
            length,
            random_access: true,
        },
        limits(),
    )
    .unwrap()
}
fn provide(
    pool: &Budget,
    r: &mut AacAdtsStreamReader,
    data: &[u8],
    chunk: usize,
    inline: bool,
) -> Result<bool, Error> {
    let q = r.request();
    let at = q.offset as usize;
    if at == data.len() {
        return Ok(false);
    }
    let count = chunk.min(q.maximum).min(data.len() - at);
    assert_ne!(count, 0);
    if inline {
        r.provide_inline(q.source, q.token, q.offset, &data[at..at + count])?;
    } else {
        r.provide_owned(InputWindow::new(
            q.source,
            q.token,
            q.offset,
            Bytes::copy_from_slice(pool, &data[at..at + count])?,
        )?)?;
    }
    Ok(at + count == data.len())
}
struct Outcome {
    frames: Vec<AacAdtsFrame>,
    end: Result<SourceLocation, Error>,
}
fn drive(
    pool: &Budget,
    r: &mut AacAdtsStreamReader,
    data: &[u8],
    chunk: usize,
    inline: bool,
    work: u64,
) -> Outcome {
    let mut frames = Vec::new();
    let mut finished = false;
    for _ in 0..2_000_000 {
        match r.step(&mut WorkBudget::new(work), &AtomicBool::new(false)) {
            Ok(Step::Ready(f)) => frames.push(f),
            Ok(Step::Yield) => {}
            Ok(Step::End(e)) => return Outcome { frames, end: Ok(e) },
            Err(e) => {
                return Outcome {
                    frames,
                    end: Err(e),
                };
            }
            Ok(Step::NeedInput(q)) => {
                if q.offset as usize == data.len() {
                    assert!(!finished, "requested past completed source");
                    let token = r.token();
                    if let Err(e) = r.finish(q.source, token) {
                        return Outcome {
                            frames,
                            end: Err(e),
                        };
                    }
                    finished = true;
                } else {
                    match provide(pool, r, data, chunk, inline) {
                        Ok(last) => {
                            if last {
                                let token = r.token();
                                if let Err(e) = r.finish(q.source, token) {
                                    return Outcome {
                                        frames,
                                        end: Err(e),
                                    };
                                }
                                finished = true;
                            }
                        }
                        Err(e) => {
                            return Outcome {
                                frames,
                                end: Err(e),
                            };
                        }
                    }
                }
            }
        }
    }
    panic!("bounded stream did not terminate")
}

#[test]
fn complete_ordered_frames_are_literal_for_every_window_and_work_partition() {
    let originals = [
        wire(&[0x21, 0x10, 0x80], None),
        wire(&[0xff, 0, 0x81], Some(0xabcd)),
        wire(&[], None),
    ];
    let data: Vec<_> = originals.iter().flatten().copied().collect();
    for known in [false, true] {
        for chunk in [1, 2, 3, 7, 9, 31, 8192] {
            for work in [1, 7, 4096] {
                let pool = Budget::new(100_000).unwrap();
                let mut r = new(&pool, known.then_some(data.len() as u64));
                let generation = r.generation();
                let result = drive(&pool, &mut r, &data, chunk, false, work);
                assert_eq!(result.end.unwrap().offset, data.len() as u64);
                assert_eq!(result.frames.len(), 3);
                assert_eq!(r.completed_frames(), 3);
                let mut at = 0;
                for (f, raw) in result.frames.iter().zip(&originals) {
                    assert_eq!(f.source_bytes().as_ref(), raw);
                    assert_eq!(f.source().source, SourceId(901));
                    assert_eq!(f.source().offset, at);
                    assert_eq!(f.source().generation, generation);
                    at += raw.len() as u64;
                    assert!(f.payload_bytes().shares_storage_with(f.source_bytes()));
                }
                assert_eq!(result.frames[1].crc_word(), Some(0xabcd));
                assert_eq!(result.frames[2].header().selected_wire_issues() & 32, 32);
            }
        }
    }
}

#[test]
fn inline_input_uses_the_same_source_contract_without_heap_window_owners() {
    let a = wire(&[0; 112], None);
    let b = wire(&[0x80, 0, 0xff], Some(0xffff));
    let data = [a.clone(), b.clone()].concat();
    for chunk in 1..=12 {
        let pool = Budget::new(100_000).unwrap();
        let baseline = pool.used();
        let mut r = new(&pool, None);
        let out = drive(&pool, &mut r, &data, chunk, true, 1);
        assert!(out.end.is_ok());
        assert_eq!(
            out.frames
                .iter()
                .map(|f| f.source_bytes().as_ref())
                .collect::<Vec<_>>(),
            vec![a.as_slice(), b.as_slice()]
        );
        drop(out);
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn known_length_and_zero_byte_provision_never_advertise_eof() {
    for bytes in [vec![], wire(&[0x80], None)] {
        for known in [false, true] {
            let pool = Budget::new(100_000).unwrap();
            let mut r = new(&pool, known.then_some(bytes.len() as u64));
            let mut got = 0;
            for _ in 0..1000 {
                match r
                    .step(&mut WorkBudget::new(1000), &AtomicBool::new(false))
                    .unwrap()
                {
                    Step::Ready(_) => got += 1,
                    Step::Yield => {}
                    Step::NeedInput(q) => {
                        if q.offset as usize == bytes.len() {
                            r.provide_inline(q.source, q.token, q.offset, &[]).unwrap();
                            assert!(matches!(
                                r.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
                                Ok(Step::NeedInput(_))
                            ));
                            let token = r.token();
                            r.finish(q.source, token).unwrap();
                            assert!(
                                matches!(r.step(&mut WorkBudget::new(0),&AtomicBool::new(false)),Ok(Step::End(e))if e.offset==bytes.len()as u64)
                            );
                            break;
                        }
                        provide(&pool, &mut r, &bytes, 12, true).unwrap();
                    }
                    Step::End(_) => panic!("implicit EOF from source length/starvation"),
                }
            }
            assert_eq!(got, usize::from(!bytes.is_empty()));
        }
    }
}

#[test]
fn every_physical_header_crc_and_payload_truncation_keeps_its_error() {
    let full = wire(&[0x80, 0, 0xff, 0x81], Some(0xabcd));
    for known in [false, true] {
        for end in 1..full.len() {
            let pool = Budget::new(100_000).unwrap();
            let baseline = pool.used();
            let mut r = new(&pool, known.then_some(full.len() as u64));
            let out = drive(&pool, &mut r, &full[..end], 1, true, 1);
            assert!(
                matches!(out.end, Err(Error::Truncated)),
                "known={known} end={end}"
            );
            assert!(out.frames.is_empty());
            assert_eq!(pool.used(), baseline);
            assert!(matches!(
                r.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
                Err(Error::Truncated)
            ));
        }
    }
}

#[test]
fn transport_failure_before_header_mid_body_and_pending_publication_is_sticky() {
    let data = wire(&[0x80; 112], Some(0x8000));
    for cut in [0, 3, 7, 19, data.len()] {
        let pool = Budget::new(100_000).unwrap();
        let baseline = pool.used();
        let mut r = new(&pool, Some(data.len() as u64));
        for _ in 0..10000 {
            if r.position() as usize == cut {
                break;
            }
            match r
                .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                Step::NeedInput(_) => {
                    provide(&pool, &mut r, &data, 1, true).unwrap();
                }
                Step::Yield => {}
                Step::Ready(_) => panic!("published before failure boundary"),
                _ => panic!(),
            }
        }
        assert_eq!(r.position(), cut as u64);
        let token = r.token();
        r.fail(SourceId(901), token, SourceFailure::Transport)
            .unwrap();
        assert!(matches!(
            r.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
            Err(Error::Transport)
        ));
        assert_eq!(pool.used(), baseline);
        assert!(matches!(
            r.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
            Err(Error::Transport)
        ));
    }
}

#[test]
fn zero_work_and_cancellation_at_independent_progress_boundaries_release_scratch() {
    let data = wire(&[0xa5; 256], None);
    for boundary in [0, 1, 3, 7, 10, 20, 40, 80, 140] {
        let pool = Budget::new(100_000).unwrap();
        let baseline = pool.used();
        let mut r = new(&pool, None);
        for _ in 0..boundary {
            match r
                .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                Step::NeedInput(_) => {
                    provide(&pool, &mut r, &data, 12, true).unwrap();
                }
                Step::Yield => {}
                _ => panic!("unexpected publication at selected early boundary"),
            }
        }
        let position = r.position();
        let used = pool.used();
        let _ = r
            .step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(r.position(), position);
        assert_eq!(pool.used(), used);
        assert!(matches!(
            r.step(&mut WorkBudget::new(0), &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
        assert_eq!(pool.used(), baseline);
        assert!(matches!(
            r.step(&mut WorkBudget::new(10000), &AtomicBool::new(false)),
            Err(Error::Cancelled)
        ));
    }
}

#[test]
fn rejected_foreign_source_pool_and_stale_windows_do_not_poison_correct_input() {
    let pool = Budget::new(100_000).unwrap();
    let other = Budget::new(100_000).unwrap();
    let data = wire(&[0x80], None);
    let mut r = new(&pool, Some(data.len() as u64));
    let q = r.request();
    let wrong = InputWindow::new(
        SourceId(902),
        q.token.clone(),
        0,
        Bytes::copy_from_slice(&pool, &data).unwrap(),
    )
    .unwrap();
    assert_eq!(r.provide(&wrong), Err(Error::SourceMismatch));
    let foreign = InputWindow::new(
        q.source,
        q.token.clone(),
        0,
        Bytes::copy_from_slice(&other, &data).unwrap(),
    )
    .unwrap();
    assert_eq!(r.provide(&foreign), Err(Error::ForeignPool));
    let stale = InputWindow::new(
        q.source,
        q.token,
        0,
        Bytes::copy_from_slice(&pool, &data).unwrap(),
    )
    .unwrap();
    r.reset(0).unwrap();
    assert!(matches!(
        r.provide(&stale),
        Err(Error::StaleGeneration { .. })
    ));
    assert_eq!(r.position(), 0);
    let out = drive(&pool, &mut r, &data, 3, true, 1);
    assert!(out.end.is_ok());
    assert_eq!(out.frames[0].source_bytes().as_ref(), data);
}

#[test]
fn frame_count_limit_keeps_all_preceding_owned_packets() {
    let a = wire(&[0x11], None);
    let b = wire(&[0x22], None);
    let c = wire(&[0x33], None);
    let data = [a.clone(), b.clone(), c].concat();
    let pool = Budget::new(100_000).unwrap();
    let mut r = AacAdtsStreamReader::new(
        &pool,
        SourceId(901),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        AacAdtsStreamLimits {
            maximum_frames: 2,
            ..limits()
        },
    )
    .unwrap();
    let out = drive(&pool, &mut r, &data, 12, true, 1);
    assert!(matches!(
        out.end,
        Err(Error::Limit {
            requested: 3,
            maximum: 2
        })
    ));
    assert_eq!(out.frames.len(), 2);
    assert_eq!(out.frames[0].source_bytes().as_ref(), a);
    assert_eq!(out.frames[1].source_bytes().as_ref(), b);
    assert_eq!(r.completed_frames(), 2);
    drop(r);
    assert_eq!(out.frames[1].payload_bytes().as_ref(), [0x22]);
}

#[test]
fn declared_frame_and_input_window_bounds_do_not_truncate_or_relabel_output() {
    let data = wire(&[0x80; 112], None);
    let pool = Budget::new(100_000).unwrap();
    let mut r = AacAdtsStreamReader::new(
        &pool,
        SourceId(901),
        SourceCapabilities {
            length: Some(data.len() as u64),
            random_access: false,
        },
        AacAdtsStreamLimits {
            maximum_frame_bytes: data.len() - 1,
            maximum_input_window: 3,
            ..limits()
        },
    )
    .unwrap();
    let q = r.request();
    let window = InputWindow::new(
        q.source,
        q.token,
        0,
        Bytes::copy_from_slice(&pool, &data[..4]).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        r.provide(&window),
        Err(Error::Limit {
            requested: 4,
            maximum: 3
        })
    ));
    assert_eq!(r.position(), 0);
    let out = drive(&pool, &mut r, &data, 3, true, 1);
    assert!(matches!(out.end, Err(Error::Limit { .. })));
    assert!(out.frames.is_empty());
}

#[test]
fn embedded_complete_adts_signature_is_opaque_payload_not_a_new_boundary() {
    let inner = wire(&[0x21, 0x10], None);
    let data = wire(
        &[vec![0xa5, 0], inner, vec![0xff, 0x80]].concat(),
        Some(0xffff),
    );
    for chunk in [1, 7, 12] {
        let pool = Budget::new(100_000).unwrap();
        let mut r = new(&pool, None);
        let out = drive(&pool, &mut r, &data, chunk, true, 1);
        assert!(out.end.is_ok());
        assert_eq!(out.frames.len(), 1);
        assert_eq!(out.frames[0].source_bytes().as_ref(), data);
        assert_eq!(out.frames[0].header().frame_length() as usize, data.len());
    }
}

#[test]
fn retained_first_frame_causes_retryable_pressure_without_repeating_source_work() {
    let a = wire(&[0xa5; 112], None);
    let mut b = wire(&[0x5a; 112], None);
    b[2] = 0x50;
    let data = [a.clone(), b.clone()].concat();
    let pool = Budget::new(400).unwrap();
    let baseline = pool.used();
    let mut r = new(&pool, Some(data.len() as u64));
    let mut retained = None;
    let mut saw_pressure = false;
    let mut second = None;
    let mut ended = false;
    for _ in 0..10000 {
        match r.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(Step::NeedInput(_)) => {
                provide(&pool, &mut r, &data, 12, true).unwrap();
            }
            Ok(Step::Yield) => {}
            Ok(Step::Ready(f)) => {
                if r.completed_frames() == 1 {
                    assert!(retained.is_none());
                    retained = Some(f);
                } else {
                    second = Some(f);
                    let token = r.token();
                    r.finish(SourceId(901), token).unwrap();
                }
            }
            Err(Error::Backpressure { .. }) => {
                assert!(retained.is_some());
                let position = r.position();
                let generation = r.generation();
                let charged = pool.used();
                assert!(matches!(
                    r.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
                    Ok(Step::Yield)
                ));
                assert_eq!(
                    (r.position(), r.generation(), pool.used()),
                    (position, generation, charged)
                );
                assert_eq!(retained.as_ref().unwrap().source_bytes().as_ref(), a);
                drop(retained.take());
                saw_pressure = true;
            }
            Ok(Step::End(_)) => {
                ended = true;
                break;
            }
            other => panic!("unexpected pressure transition: {other:?}"),
        }
    }
    assert!(saw_pressure && ended);
    let second = second.unwrap();
    assert_eq!(second.source_bytes().as_ref(), b);
    assert_eq!(second.source().offset, a.len() as u64);
    assert_eq!(second.header().coded_frequency_hz(), Some(44100));
    drop(second);
    drop(r);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn completed_payload_survives_freeze_backpressure_and_zero_work_retry() {
    let data = wire(&[0x83; 112], None);
    let pool = Budget::new(10_000).unwrap();
    let baseline = pool.used();
    let mut r = new(&pool, None);
    for _ in 0..10000 {
        if r.position() == data.len() as u64 {
            break;
        }
        match r
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::NeedInput(_) => {
                provide(&pool, &mut r, &data, 12, true).unwrap();
            }
            Step::Yield => {}
            _ => panic!("premature publication"),
        }
    }
    let mut blocker = BudgetVec::<u8>::new(&pool);
    blocker
        .try_reserve_exact(pool.available() - (Bytes::owner_overhead_bytes() - 1))
        .unwrap();
    assert!(matches!(
        r.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    let position = r.position();
    let used = pool.used();
    assert!(matches!(
        r.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(Step::Yield)
    ));
    assert_eq!((r.position(), pool.used()), (position, used));
    drop(blocker);
    let Step::Ready(f) = r
        .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
    else {
        panic!("freeze retry must not repeat prefix/payload work")
    };
    assert_eq!(f.source_bytes().as_ref(), data);
    assert_eq!(r.completed_frames(), 1);
    drop(f);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn intrinsically_impossible_growth_is_limit_and_releases_every_private_owner() {
    let data = wire(&[0; 256], None);
    let pool = Budget::new(Budget::control_storage_bytes() + 80).unwrap();
    let baseline = pool.used();
    let mut r = new(&pool, None);
    let out = drive(&pool, &mut r, &data, 12, true, 1);
    assert!(matches!(out.end, Err(Error::Limit { .. })));
    assert!(out.frames.is_empty());
    assert_eq!(pool.used(), baseline);
    assert!(matches!(
        r.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Err(Error::Limit { .. })
    ));
}

#[test]
fn tiny_frames_do_not_reserve_the_maximum_packet_capacity() {
    let data = wire(&[0; 112], None);
    let pool = Budget::new(1000).unwrap();
    let baseline = pool.used();
    let mut r = new(&pool, None);
    let out = drive(&pool, &mut r, &data, 12, true, 1);
    assert!(out.end.is_ok());
    assert_eq!(out.frames[0].source_bytes().len(), 119);
    assert!(out.frames[0].source_bytes().retained_capacity() < 1000);
    assert!(pool.peak() < 1000);
    drop(out);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn failed_known_length_completion_does_not_turn_transport_or_truncation_into_eof() {
    let data = wire(&[0x80], None);
    let pool = Budget::new(100_000).unwrap();
    let mut r = new(&pool, Some((data.len() + 1) as u64));
    let token = r.token();
    r.provide_inline(SourceId(901), token.clone(), 0, &data)
        .unwrap();
    assert_eq!(r.finish(SourceId(901), token), Err(Error::Truncated));
    assert!(matches!(
        r.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(Error::Truncated)
    ));
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn aligned_seek_reset_changes_input_generation_and_rejects_old_callbacks() {
    let a = wire(&[0x11; 32], None);
    let b = wire(&[0x22; 16], Some(0x1234));
    let data = [a.clone(), b.clone()].concat();
    let pool = Budget::new(100_000).unwrap();
    let mut r = new(&pool, Some(data.len() as u64));
    let q = r.request();
    r.provide_inline(q.source, q.token.clone(), 0, &data[..7])
        .unwrap();
    let _ = r
        .step(&mut WorkBudget::new(3), &AtomicBool::new(false))
        .unwrap();
    let request = r.reset(a.len() as u64).unwrap();
    assert_ne!(request.token.generation(), q.token.generation());
    assert_eq!(request.offset, a.len() as u64);
    assert!(matches!(
        r.fail(q.source, q.token, SourceFailure::Transport),
        Err(Error::StaleGeneration { .. })
    ));
    let out = drive(&pool, &mut r, &data, 12, true, 1);
    assert!(out.end.is_ok());
    assert_eq!(out.frames.len(), 1);
    assert_eq!(out.frames[0].source_bytes().as_ref(), b);
    assert_eq!(out.frames[0].source().offset, a.len() as u64);
    assert_eq!(out.frames[0].source().generation, r.generation());
}

#[test]
fn fresh_reset_recovers_from_a_misaligned_epoch_without_changing_source_bytes() {
    let data = wire(&[0x21; 32], None);
    let pool = Budget::new(100_000).unwrap();
    let mut r = new(&pool, Some(data.len() as u64));
    r.reset(1).unwrap();
    let old = r.token();
    let bad = drive(&pool, &mut r, &data, 12, true, 1);
    assert!(matches!(bad.end, Err(Error::Invalid(_))));
    assert!(bad.frames.is_empty());
    assert_eq!(pool.used(), Budget::control_storage_bytes());
    r.reset(0).unwrap();
    assert_ne!(r.generation(), old.generation());
    let out = drive(&pool, &mut r, &data, 12, true, 1);
    assert!(out.end.is_ok());
    assert_eq!(out.frames[0].source_bytes().as_ref(), data);
}

#[test]
fn coded_rate_changes_are_source_headers_not_inferred_clock_or_epoch_resets() {
    let a = wire(&[0x80], None);
    let mut b = wire(&[0x81], None);
    b[2] = 0x50;
    b[3] = 0x40 | b[3] & 3;
    let data = [a.clone(), b.clone()].concat();
    let pool = Budget::new(100_000).unwrap();
    let mut r = new(&pool, None);
    let generation = r.generation();
    let out = drive(&pool, &mut r, &data, 12, true, 1);
    assert!(out.end.is_ok());
    assert_eq!(out.frames[0].header().coded_frequency_hz(), Some(48000));
    assert_eq!(out.frames[1].header().coded_frequency_hz(), Some(44100));
    assert_eq!(out.frames[1].header().coded_channel_count(), Some(1));
    assert!(
        out.frames
            .iter()
            .all(|f| f.source().generation == generation)
    );
    assert_eq!(out.frames[0].source_bytes().as_ref(), a);
    assert_eq!(out.frames[1].source_bytes().as_ref(), b);
}

#[test]
fn source_window_hidden_backing_and_output_are_charged_once_until_last_alias() {
    let raw = wire(&[0x80, 0x81], None);
    let pool = Budget::new(1000).unwrap();
    let baseline = pool.used();
    let mut backing = vec![0xa5; 512];
    backing[97..97 + raw.len()].copy_from_slice(&raw);
    let owner = Bytes::copy_from_slice(&pool, &backing).unwrap();
    let charge = owner.retained_storage_bytes();
    let mut r = new(&pool, Some(raw.len() as u64));
    let q = r.request();
    let w = InputWindow::new(
        q.source,
        q.token,
        0,
        owner.slice(97..97 + raw.len()).unwrap(),
    )
    .unwrap();
    r.provide(&w).unwrap();
    drop(w);
    let token = r.token();
    r.finish(SourceId(901), token).unwrap();
    let f = loop {
        match r
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(f) => break f,
            Step::Yield => {}
            _ => panic!(),
        }
    };
    assert_eq!(f.source_bytes().as_ref(), raw);
    assert_eq!(
        pool.used(),
        baseline + charge + f.source_bytes().retained_storage_bytes()
    );
    drop(f);
    assert_eq!(pool.used(), baseline + charge);
    drop(owner);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn eof_followed_by_transport_failure_remains_failure_and_owned_output_survives() {
    let data = wire(&[0xff, 0x80], None);
    let pool = Budget::new(100_000).unwrap();
    let baseline = pool.used();
    let mut r = new(&pool, None);
    let out = drive(&pool, &mut r, &data, 12, true, 1);
    assert!(out.end.is_ok());
    let token = r.token();
    r.fail(SourceId(901), token, SourceFailure::Transport)
        .unwrap();
    assert!(matches!(
        r.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Err(Error::Transport)
    ));
    assert_eq!(out.frames[0].source_bytes().as_ref(), data);
    let alias = out.frames[0].payload_bytes().clone();
    drop(out);
    drop(r);
    assert!(pool.used() > baseline);
    assert_eq!(alias.as_ref(), [0xff, 0x80]);
    std::thread::spawn(move || drop(alias)).join().unwrap();
    assert_eq!(pool.used(), baseline);
}

#[test]
fn no_unselected_mode_is_silently_resynchronized_or_emitted() {
    let good = wire(&[0x80], None);
    for variant in [0, 1, 2, 3] {
        let mut bad = good.clone();
        match variant {
            0 => bad[0] = 0,
            1 => bad[1] |= 2,
            2 => bad[2] &= 63,
            _ => bad[6] |= 1,
        }
        let data = [good.clone(), bad].concat();
        let pool = Budget::new(100_000).unwrap();
        let mut r = new(&pool, None);
        let out = drive(&pool, &mut r, &data, 12, true, 1);
        assert_eq!(out.frames.len(), 1);
        assert_eq!(out.frames[0].source_bytes().as_ref(), good);
        assert!(out.end.is_err());
        assert!(
            r.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
                .is_err()
        );
    }
}
