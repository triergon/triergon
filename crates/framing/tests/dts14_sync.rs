//! Independent source geometry controls. No native verdict is inferred here.
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputRequest, InputToken, InputWindow, SourceCapabilities, SourceFailure,
    SourceId, Step, WorkBudget,
};
use triergon_framing::{
    DTS14_SYNC_PENDING, Dts14BoundaryEvidence, Dts14SyncFrame, Dts14SyncLimits, Dts14SyncReader,
    DtsWordOrder,
};

const SOURCE: SourceId = SourceId(211401);
const CASES: &str = include_str!("../fixtures/dts14-sync-v1.tsv");

fn window(source: SourceId, token: InputToken, offset: u64, bytes: Bytes) -> InputWindow {
    InputWindow::new(source, token, offset, bytes).unwrap()
}
fn empty(pool: &Budget) -> Bytes {
    Bytes::copy_from_slice(pool, &[]).unwrap()
}

fn hex(source: &str) -> Vec<u8> {
    assert_eq!(source.len() % 2, 0);
    source
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn rows() -> Vec<Vec<&'static str>> {
    CASES
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| {
            let row: Vec<_> = line.split('\t').collect();
            assert_eq!(row.len(), 9);
            row
        })
        .collect()
}
fn case(id: &str) -> Vec<&'static str> {
    rows().into_iter().find(|row| row[0] == id).unwrap()
}
fn settings() -> Dts14SyncLimits {
    Dts14SyncLimits {
        maximum_physical_frame_bytes: 4096,
        maximum_canonical_frame_bytes: 3584,
        maximum_frames: 8,
        maximum_input_window: 4096,
    }
}
fn reader(
    pool: &Budget,
    length: Option<u64>,
    order: DtsWordOrder,
    limits: Dts14SyncLimits,
) -> Dts14SyncReader {
    Dts14SyncReader::new(
        pool,
        SOURCE,
        SourceCapabilities {
            length,
            random_access: true,
        },
        order,
        limits,
    )
    .unwrap()
}
fn feed(
    job: &mut Dts14SyncReader,
    pool: &Budget,
    source: &[u8],
    request: InputRequest,
    chunk: usize,
) {
    let at = usize::try_from(request.offset).unwrap();
    assert!(at <= source.len());
    if at == source.len() {
        job.finish(request.source, request.token).unwrap();
    } else {
        let count = (source.len() - at).min(request.maximum).min(chunk);
        assert!(count > 0);
        job.provide(&window(
            request.source,
            request.token,
            request.offset,
            Bytes::copy_from_slice(pool, &source[at..at + count]).unwrap(),
        ))
        .unwrap();
    }
}
fn next(
    job: &mut Dts14SyncReader,
    pool: &Budget,
    source: &[u8],
    chunk: usize,
    quantum: u64,
) -> Result<Option<Dts14SyncFrame>, Error> {
    for _ in 0..1_000_000 {
        match job.step(&mut WorkBudget::new(quantum), &AtomicBool::new(false)) {
            Ok(Step::Ready(frame)) => return Ok(Some(frame)),
            Ok(Step::NeedInput(request)) => feed(job, pool, source, request, chunk),
            Ok(Step::Yield) => {}
            Ok(Step::End(end)) => {
                assert_eq!(end.source, SOURCE);
                assert_eq!(end.offset as usize, source.len());
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
    }
    panic!("finite DTS14 source/work driver guard");
}
fn collect(
    job: &mut Dts14SyncReader,
    pool: &Budget,
    source: &[u8],
    chunk: usize,
    quantum: u64,
) -> (Vec<Dts14SyncFrame>, Option<Error>) {
    let mut out = Vec::new();
    for _ in 0..16 {
        match next(job, pool, source, chunk, quantum) {
            Ok(Some(frame)) => out.push(frame),
            Ok(None) => return (out, None),
            Err(error) => return (out, Some(error)),
        }
    }
    panic!("finite output count");
}
fn class(error: Option<Error>) -> &'static str {
    match error {
        None => "end",
        Some(Error::Unsupported(reason)) if reason == DTS14_SYNC_PENDING => {
            panic!("parent RED guard observed: {reason}")
        }
        Some(Error::Unsupported(_)) => "unsupported",
        Some(Error::Truncated) => "truncated",
        Some(other) => panic!("unexpected source-case error: {other:?}"),
    }
}
fn list(source: &str, separator: char) -> Vec<&str> {
    if source.is_empty() {
        Vec::new()
    } else {
        source.split(separator).collect()
    }
}

#[test]
fn all30_source_cases_preserve_bytes_offsets_boundaries_across_chunk_work_and_eof_modes() {
    assert_eq!(rows().len(), 30);
    for row in rows() {
        let raw = hex(row[2]);
        let order = if row[1] == "little" {
            DtsWordOrder::LittleEndian
        } else {
            DtsWordOrder::BigEndian
        };
        for known in [false, true] {
            for chunk in [1, 3, 13, 4096] {
                for quantum in [1, 31] {
                    let pool = Budget::new(1_048_576).unwrap();
                    let baseline = pool.used();
                    let mut job =
                        reader(&pool, known.then_some(raw.len() as u64), order, settings());
                    let generation = job.generation();
                    let (frames, error) = collect(&mut job, &pool, &raw, chunk, quantum);
                    assert_eq!(class(error), row[3], "{} chunk{chunk} q{quantum}", row[0]);
                    let physical = list(row[5], ';');
                    let canonical = list(row[6], ';');
                    let offsets = list(row[4], ',');
                    let boundaries = list(row[7], ',');
                    let skipped = list(row[8], ',');
                    assert_eq!(frames.len(), physical.len(), "{}", row[0]);
                    for (i, frame) in frames.iter().enumerate() {
                        assert_eq!(
                            frame.physical_bytes().as_ref(),
                            hex(physical[i]),
                            "{}",
                            row[0]
                        );
                        assert_eq!(
                            frame.canonical_bytes().as_ref(),
                            hex(canonical[i]),
                            "{}",
                            row[0]
                        );
                        let facts = frame.facts();
                        assert_eq!(facts.identity.source, SOURCE);
                        assert_eq!(facts.identity.generation, generation);
                        assert_eq!(facts.identity.offset, offsets[i].parse::<u64>().unwrap());
                        assert_eq!(facts.word_order, order);
                        assert_eq!(facts.physical_bytes, frame.physical_bytes().len());
                        assert_eq!(facts.payload_bytes, frame.canonical_bytes().len());
                        assert_eq!(facts.payload_bytes, usize::from(facts.header.fsize) + 1);
                        assert_eq!(facts.header.cpf, u8::from(row[0].contains("cpf1")));
                        assert_eq!(facts.header.hcrc, row[0].contains("cpf1").then_some(0xbeef));
                        assert_eq!(facts.header.core_rate_hz, 44100);
                        assert_eq!(facts.header.coding_window_samples, 1024);
                        assert_eq!(
                            facts.skipped_interior_syncs,
                            skipped[i].parse::<usize>().unwrap()
                        );
                        assert_eq!(frame.payload().residual_bits(), 0);
                        assert_eq!(frame.payload().residual_value(), 0);
                        match facts.boundary {
                            Dts14BoundaryEvidence::FollowingNormalSync { physical_offset } => {
                                assert_eq!(boundaries[i], "sync");
                                assert_eq!(
                                    physical_offset,
                                    facts.identity.offset + facts.physical_bytes as u64
                                );
                            }
                            Dts14BoundaryEvidence::ExplicitSourceEnd => {
                                assert_eq!(boundaries[i], "eof")
                            }
                        }
                    }
                    drop(frames);
                    drop(job);
                    assert_eq!(pool.used(), baseline, "{} teardown", row[0]);
                }
            }
        }
    }
}

#[test]
fn entity_eof_is_explicit_and_fsize_does_not_publish_or_crop_a_starved_source() {
    let raw = hex(case("unused-high-0")[2]);
    let pool = Budget::new(65536).unwrap();
    let mut job = reader(
        &pool,
        Some(raw.len() as u64),
        DtsWordOrder::LittleEndian,
        settings(),
    );
    let request = job.request();
    job.provide(&window(
        SOURCE,
        request.token,
        0,
        Bytes::copy_from_slice(&pool, &raw).unwrap(),
    ))
    .unwrap();
    for _ in 0..20000 {
        match job
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Yield => {}
            Step::NeedInput(request) => {
                assert_eq!(request.offset, raw.len() as u64);
                // Empty provision remains starvation, despite a known length.
                job.provide(&window(SOURCE, request.token, request.offset, empty(&pool)))
                    .unwrap();
                assert!(matches!(
                    job.step(&mut WorkBudget::new(1), &AtomicBool::new(false))
                        .unwrap(),
                    Step::NeedInput(_)
                ));
                let request = job.request();
                job.finish(SOURCE, request.token).unwrap();
                let frame = next(&mut job, &pool, &raw, 1, 1).unwrap().unwrap();
                assert_eq!(frame.physical_bytes().as_ref(), raw);
                assert_eq!(
                    frame.facts().boundary,
                    Dts14BoundaryEvidence::ExplicitSourceEnd
                );
                return;
            }
            other => panic!("source must wait for actual entity completion: {other:?}"),
        }
    }
    panic!("starvation guard");
}

#[test]
fn first_boundary_uses_measured_source_span_and_only_six_bytes_of_sync_lookahead() {
    let row = case("exact-little-cpf0-n98-three");
    let raw = hex(row[2]);
    let pool = Budget::new(65536).unwrap();
    let mut job = reader(&pool, None, DtsWordOrder::LittleEndian, settings());
    let frame = next(&mut job, &pool, &raw, 1, 1).unwrap().unwrap();
    assert_eq!(job.position(), 118);
    assert_eq!(frame.physical_bytes().as_ref(), &raw[..112]);
    assert_eq!(frame.canonical_bytes().len(), 98);
    assert_eq!(
        frame.facts().boundary,
        Dts14BoundaryEvidence::FollowingNormalSync {
            physical_offset: 112
        }
    );
}

#[test]
fn interior_extended_sync_is_preserved_when_its_measured_span_has_wrong_geometry() {
    let row = case("interior-normal-sync");
    let raw = hex(row[2]);
    let pool = Budget::new(65536).unwrap();
    let mut job = reader(&pool, None, DtsWordOrder::LittleEndian, settings());
    let frame = next(&mut job, &pool, &raw, 2, 1).unwrap().unwrap();
    assert_eq!(frame.facts().skipped_interior_syncs, 1);
    assert_eq!(frame.physical_bytes().as_ref(), &raw[..112]);
    assert_eq!(
        &frame.canonical_bytes().as_ref()[21..27],
        hex("7ffe8001fc00")
    );
}

#[test]
fn artificial_prefix_end_is_retained_and_raw_source_end_is_not_a_native_future_offset() {
    let row = case("authored-real-geometry-four");
    let raw = hex(row[2]);
    let pool = Budget::new(65536).unwrap();
    let mut job = reader(&pool, None, DtsWordOrder::LittleEndian, settings());
    let (frames, error) = collect(&mut job, &pool, &raw, 7, 31);
    assert!(error.is_none());
    assert_eq!(frames.len(), 4);
    assert_eq!(frames[3].facts().identity.offset, 12288);
    assert_eq!(frames[3].physical_bytes().len(), 4096);
    assert_eq!(
        frames[3].facts().boundary,
        Dts14BoundaryEvidence::ExplicitSourceEnd
    );
    assert_eq!(
        frames[3].facts().identity.offset + frames[3].physical_bytes().len() as u64,
        16384
    );
}

#[test]
fn healthy_reset_invalidates_old_input_tokens_without_relabeling_retained_outputs() {
    let raw = hex(case("exact-little-cpf0-n98-three")[2]);
    let pool = Budget::new(65536).unwrap();
    let mut job = reader(&pool, None, DtsWordOrder::LittleEndian, settings());
    let first = next(&mut job, &pool, &raw, 7, 7).unwrap().unwrap();
    let old = job.request();
    let new = job.reset(0).unwrap();
    assert_ne!(new.token.generation(), old.token.generation());
    assert!(matches!(
        job.provide(&window(SOURCE, old.token, old.offset, empty(&pool))),
        Err(Error::StaleGeneration { .. })
    ));
    let again = next(&mut job, &pool, &raw, 3, 1).unwrap().unwrap();
    assert_eq!(
        first.physical_bytes().as_ref(),
        again.physical_bytes().as_ref()
    );
    assert_ne!(
        first.facts().identity.generation,
        again.facts().identity.generation
    );
    let generation = job.generation();
    assert!(matches!(job.reset(1), Err(Error::Unsupported(_))));
    assert_eq!(job.generation(), generation);
    job.reset(112).unwrap();
    let later = next(&mut job, &pool, &raw, 1, 1).unwrap().unwrap();
    assert_eq!(later.facts().identity.offset, 112);
    assert_eq!(later.physical_bytes().as_ref(), &raw[112..224]);
}

#[test]
fn zero_work_consumes_and_allocates_nothing_but_quantum_one_finishes() {
    let raw = hex(case("unused-high-1")[2]);
    let pool = Budget::new(65536).unwrap();
    let mut job = reader(&pool, None, DtsWordOrder::LittleEndian, settings());
    let used = pool.used();
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        Step::Yield
    ));
    assert_eq!(job.position(), 0);
    assert_eq!(pool.used(), used);
    assert_eq!(
        next(&mut job, &pool, &raw, 1, 1)
            .unwrap()
            .unwrap()
            .physical_bytes()
            .as_ref(),
        raw
    );
    let pool = Budget::new(65536).unwrap();
    let baseline = pool.used();
    let mut empty_job = reader(&pool, Some(0), DtsWordOrder::LittleEndian, settings());
    assert!(next(&mut empty_job, &pool, &[], 1, 1).unwrap().is_none());
    assert_eq!(pool.used(), baseline);
}

#[test]
fn raw_reserve_pressure_retries_without_losing_the_fixed_header_or_source_position() {
    let raw = hex(case("unused-high-2")[2]);
    let pool = Budget::new(65536).unwrap();
    let mut job = reader(&pool, None, DtsWordOrder::LittleEndian, settings());
    let request = job.request();
    job.provide(&window(
        SOURCE,
        request.token,
        0,
        Bytes::copy_from_slice(&pool, &raw[..18]).unwrap(),
    ))
    .unwrap();
    let blocker = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        job.step(&mut WorkBudget::new(100), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(job.position(), 18);
    drop(blocker);
    assert_eq!(
        next(&mut job, &pool, &raw, 1, 1)
            .unwrap()
            .unwrap()
            .physical_bytes()
            .as_ref(),
        raw
    );
}

#[test]
fn raw_freeze_and_canonical_reserve_pressure_preserve_owned_bytes_for_retry() {
    let raw = hex(case("unused-high-3")[2]);
    for frozen in [false, true] {
        let pool = Budget::new(65536).unwrap();
        let baseline = pool.used();
        let mut limits = settings();
        limits.maximum_physical_frame_bytes = 256;
        let mut job = reader(&pool, None, DtsWordOrder::LittleEndian, limits);
        let target = baseline
            + 256
            + if frozen {
                Bytes::owner_overhead_bytes()
            } else {
                0
            };
        let mut checkpoint = false;
        for _ in 0..20000 {
            match job
                .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                Step::NeedInput(request) => feed(&mut job, &pool, &raw, request, 1),
                Step::Yield => {}
                other => panic!("premature result before pressure checkpoint: {other:?}"),
            }
            if job.position() == raw.len() as u64 && pool.used() == target {
                // EOF is not implicit in the source length; establish it while
                // no new allocation is needed, then close all spare capacity.
                let request = job.request();
                job.finish(SOURCE, request.token).unwrap();
                checkpoint = true;
                break;
            }
        }
        assert!(checkpoint);
        let blocker = pool.reserve(pool.available()).unwrap();
        let mut observed = false;
        for _ in 0..1000 {
            match job.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
                Err(Error::Backpressure { .. }) => {
                    observed = true;
                    break;
                }
                Ok(Step::Yield) => {}
                other => panic!("lost pressure checkpoint: {other:?}"),
            }
        }
        assert!(observed);
        drop(blocker);
        assert_eq!(
            next(&mut job, &pool, &raw, 1, 1)
                .unwrap()
                .unwrap()
                .physical_bytes()
                .as_ref(),
            raw
        );
    }
}

#[test]
fn cancellation_is_sticky_and_releases_header_scan_and_output_construction_owners() {
    let raw = hex(case("unused-high-cycle")[2]);
    for stop in [0, 5, 18, 30, 112] {
        let pool = Budget::new(65536).unwrap();
        let baseline = pool.used();
        let mut job = reader(&pool, None, DtsWordOrder::LittleEndian, settings());
        for _ in 0..20000 {
            if job.position() >= stop {
                break;
            }
            match job
                .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                Step::NeedInput(request) => feed(&mut job, &pool, &raw, request, 1),
                Step::Yield => {}
                other => panic!("unexpected cancellation setup: {other:?}"),
            }
        }
        assert_eq!(
            job.step(&mut WorkBudget::new(1), &AtomicBool::new(true))
                .unwrap_err(),
            Error::Cancelled
        );
        assert_eq!(
            job.step(&mut WorkBudget::new(100), &AtomicBool::new(false))
                .unwrap_err(),
            Error::Cancelled
        );
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn transport_failure_and_short_known_entity_stay_failures_not_successful_eof() {
    for failure in [SourceFailure::Transport, SourceFailure::Cancelled] {
        let pool = Budget::new(65536).unwrap();
        let mut job = reader(&pool, None, DtsWordOrder::LittleEndian, settings());
        let request = job.request();
        job.fail(SOURCE, request.token, failure).unwrap();
        assert_eq!(
            job.step(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap_err(),
            if failure == SourceFailure::Transport {
                Error::Transport
            } else {
                Error::Cancelled
            }
        );
    }
    let pool = Budget::new(65536).unwrap();
    let mut job = reader(&pool, Some(112), DtsWordOrder::LittleEndian, settings());
    let request = job.request();
    assert_eq!(job.finish(SOURCE, request.token), Err(Error::Truncated));
    assert_eq!(
        job.step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Truncated
    );
}

#[test]
fn independent_source_limits_and_pool_impossibility_are_explicit_without_growth() {
    let raw = hex(case("exact-little-cpf0-n98-three")[2]);
    for selector in 0..4 {
        let pool = Budget::new(65536).unwrap();
        let mut limits = settings();
        match selector {
            0 => limits.maximum_physical_frame_bytes = 100,
            1 => limits.maximum_canonical_frame_bytes = 97,
            2 => limits.maximum_frames = 1,
            _ => limits.maximum_input_window = 1,
        }
        let mut job = reader(&pool, None, DtsWordOrder::LittleEndian, limits);
        let (_, error) = collect(&mut job, &pool, &raw, 4096, 1);
        if selector == 3 {
            assert!(error.is_none());
        } else {
            assert!(matches!(error, Some(Error::Limit { .. })));
        }
    }
    let pool = Budget::new(256).unwrap();
    assert!(matches!(
        Dts14SyncReader::new(
            &pool,
            SOURCE,
            SourceCapabilities {
                length: None,
                random_access: false
            },
            DtsWordOrder::LittleEndian,
            settings()
        ),
        Err(Error::Limit { .. })
    ));
    let pool = Budget::new(65536).unwrap();
    let mut limits = settings();
    limits.maximum_frames = 0;
    assert!(matches!(
        Dts14SyncReader::new(
            &pool,
            SOURCE,
            SourceCapabilities {
                length: None,
                random_access: false
            },
            DtsWordOrder::LittleEndian,
            limits
        ),
        Err(Error::Invalid(_))
    ));
    let mut limits = settings();
    limits.maximum_physical_frame_bytes = usize::MAX;
    assert!(matches!(
        Dts14SyncReader::new(
            &pool,
            SOURCE,
            SourceCapabilities {
                length: None,
                random_access: false
            },
            DtsWordOrder::LittleEndian,
            limits
        ),
        Err(Error::Overflow)
    ));
}

#[test]
fn rejected_foreign_pool_and_source_responses_leave_the_expected_request_usable() {
    let raw = hex(case("unused-high-0")[2]);
    let pool = Budget::new(65536).unwrap();
    let foreign = Budget::new(65536).unwrap();
    let mut job = reader(&pool, None, DtsWordOrder::LittleEndian, settings());
    let request = job.request();
    assert_eq!(
        job.provide(&window(
            SOURCE,
            request.token.clone(),
            0,
            Bytes::copy_from_slice(&foreign, &raw[..1]).unwrap()
        )),
        Err(Error::ForeignPool)
    );
    assert_eq!(
        job.provide(&window(SourceId(99), request.token, 0, empty(&pool))),
        Err(Error::SourceMismatch)
    );
    assert_eq!(job.position(), 0);
    assert_eq!(
        next(&mut job, &pool, &raw, 1, 1)
            .unwrap()
            .unwrap()
            .physical_bytes()
            .as_ref(),
        raw
    );
}

#[test]
fn caller_clones_keep_both_full_owners_charged_after_reader_drop_and_cross_thread_release() {
    let raw = hex(case("unused-high-cycle")[2]);
    let pool = Budget::new(65536).unwrap();
    let baseline = pool.used();
    let mut job = reader(&pool, None, DtsWordOrder::LittleEndian, settings());
    let frame = next(&mut job, &pool, &raw, 3, 1).unwrap().unwrap();
    let copy = frame.clone();
    let charged = pool.used();
    assert!(
        charged
            >= baseline
                + frame.physical_bytes().retained_storage_bytes()
                + frame.canonical_bytes().retained_storage_bytes()
    );
    for i in 0..frame.physical_bytes().len() / 2 {
        assert_eq!(frame.payload().unused_high_bits(i).unwrap(), (i % 4) as u8);
    }
    drop(frame);
    drop(job);
    assert_eq!(pool.used(), charged);
    std::thread::spawn(move || {
        assert_eq!(copy.physical_bytes().as_ref(), raw);
        drop(copy);
    })
    .join()
    .unwrap();
    assert_eq!(pool.used(), baseline);
}
