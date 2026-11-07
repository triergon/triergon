mod common;
use common::{child, roots};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error, InputCursor, SourceCapabilities, SourceId, WorkBudget};
use triergon_isobmff::{
    BuildStatus, FragmentBuilder, FragmentContext, FragmentIndex, ImplicitBase, Limits, Trex,
    reference_trace::mov::fragment_clock::{
        FragmentClockJob, FragmentClockProfile, check_signed_decode_runs,
    },
};

const DATA: &[u8] = include_bytes!("fixtures/native-open-v6/tfdt-control-08.mp4");

fn fragment(pool: &Budget, data: &[u8]) -> FragmentIndex {
    let top = roots(pool, data).unwrap();
    let movie = top.iter().find(|x| x.header().kind == *b"moov").unwrap();
    let moof = top.iter().find(|x| x.header().kind == *b"moof").unwrap();
    let trex = Trex::parse(&child(&child(movie, b"mvex").unwrap(), b"trex").unwrap()).unwrap();
    let mut job = FragmentBuilder::new(
        child(moof, b"traf").unwrap(),
        trex,
        FragmentContext {
            moof_start: moof.header().start,
            implicit_base: ImplicitBase::FirstTraf,
            decode_origin: None,
            first_ordinal: 1,
            description_count: 1,
        },
        pool.clone(),
        Limits::default(),
    )
    .unwrap();
    while job.poll(&mut WorkBudget::new(1)).unwrap() != BuildStatus::Ready {}
    job.finish().unwrap()
}
fn input(pool: &Budget) -> InputCursor {
    InputCursor::new(
        pool,
        SourceId(7),
        SourceCapabilities {
            length: Some(DATA.len() as u64),
            random_access: true,
        },
        64,
    )
    .unwrap()
}
fn run(
    base: u64,
    quantum: u64,
) -> triergon_isobmff::reference_trace::mov::fragment_clock::FragmentClockResult {
    let pool = Budget::new(1 << 20).unwrap();
    let mut bytes = DATA.to_vec();
    let at = bytes.windows(4).position(|x| x == b"tfdt").unwrap() + 8;
    bytes[at..at + 8].copy_from_slice(&base.to_be_bytes());
    let index = fragment(&pool, &bytes);
    let mut source = input(&pool);
    let original_end = index.decode_end();
    let mut job = FragmentClockJob::new(&index, &source, 64, FragmentClockProfile).unwrap();
    for _ in 0..1000 {
        if job
            .poll(
                &mut source,
                &mut WorkBudget::new(quantum),
                &AtomicBool::new(false),
            )
            .unwrap()
            == BuildStatus::Ready
        {
            let result = job.take_result().unwrap();
            assert_eq!(index.decode_end(), original_end);
            assert_eq!(source.position(), 0);
            return result;
        }
    }
    panic!("bounded startup clock failed to progress")
}
#[test]
fn source_valid_clock_crossing_is_explicit_demux_header_error() {
    let r = run((1 << 63) - 2, 1);
    assert_eq!(r.demux_open_result(), Some(-1_094_995_529));
    assert_eq!(r.source_start, (1 << 63) - 2);
    assert_eq!(r.source_end, (1i128 << 63) + 7);
    assert!(r.source_end > i128::from(u64::MAX) / 2);
}
#[test]
fn complete_decode_endpoint_matters_even_when_last_sample_start_fits() {
    let r = run((1 << 63) - 9, 1);
    assert_eq!(r.demux_open_result(), Some(-1_094_995_529));
    assert_eq!(r.source_end, 1i128 << 63);
}
#[test]
fn exact_signed_endpoint_control_is_not_rejected() {
    let r = run((1 << 63) - 10, 1);
    assert_eq!(r.demux_open_result(), None);
    assert_eq!(r.demux_end, Some(i64::MAX));
}
#[test]
fn api_sentinel_bits_do_not_destroy_source_knownness() {
    let r = run(1 << 63, 1);
    assert_eq!(r.demux_open_result(), None);
    assert_eq!(r.source_start, 1 << 63);
    assert_eq!(r.demux_start, i64::MIN);
    assert_eq!(r.source_end, (1i128 << 63) + 9);
}
#[test]
fn unsigned_source_end_beyond_u64_stays_exact_and_distinct_from_demux() {
    let r = run(u64::MAX, 1);
    assert_eq!(r.source_end, i128::from(u64::MAX) + 9);
    assert_eq!(r.demux_start, -1);
    assert_eq!(r.demux_end, Some(8));
}
#[test]
fn zero_work_yields_without_cursor_or_source_progress() {
    let pool = Budget::new(1 << 20).unwrap();
    let index = fragment(&pool, DATA);
    let mut source = input(&pool);
    let mut job = FragmentClockJob::new(&index, &source, 64, FragmentClockProfile).unwrap();
    let mut work = WorkBudget::new(0);
    assert_eq!(
        job.poll(&mut source, &mut work, &AtomicBool::new(false))
            .unwrap(),
        BuildStatus::Yield
    );
    assert_eq!(source.position(), 0);
    assert_eq!(work.spent(), 0);
}
#[test]
fn cancellation_precedes_zero_work_and_is_sticky() {
    let pool = Budget::new(1 << 20).unwrap();
    let index = fragment(&pool, DATA);
    let mut source = input(&pool);
    let mut job = FragmentClockJob::new(&index, &source, 64, FragmentClockProfile).unwrap();
    assert_eq!(
        job.poll(&mut source, &mut WorkBudget::new(0), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        job.poll(
            &mut source,
            &mut WorkBudget::new(1),
            &AtomicBool::new(false)
        )
        .unwrap_err(),
        Error::Cancelled
    );
}
#[test]
fn foreign_pool_never_becomes_demux_header_failure() {
    let pool = Budget::new(1 << 20).unwrap();
    let foreign = Budget::new(1 << 20).unwrap();
    let index = fragment(&pool, DATA);
    assert!(matches!(
        FragmentClockJob::new(&index, &input(&foreign), 64, FragmentClockProfile),
        Err(Error::ForeignPool)
    ));
}
#[test]
fn source_sample_limit_never_becomes_demux_header_failure() {
    let pool = Budget::new(1 << 20).unwrap();
    let index = fragment(&pool, DATA);
    assert!(matches!(
        FragmentClockJob::new(&index, &input(&pool), 0, FragmentClockProfile),
        Err(Error::Limit { .. })
    ));
}
#[test]
fn work_schedule_preserves_exact_owned_result() {
    let a = run((1 << 63) - 2, 1);
    let b = run((1 << 63) - 2, 4096);
    assert_eq!(a.demux_open_result(), b.demux_open_result());
    assert_eq!(a.source_start, b.source_start);
    assert_eq!(a.raw_tfdt.bytes().as_ref(), b.raw_tfdt.bytes().as_ref());
}

#[test]
fn stale_generation_is_not_demux_invaliddata() {
    let pool = Budget::new(1 << 20).unwrap();
    let index = fragment(&pool, DATA);
    let mut source = input(&pool);
    let mut job = FragmentClockJob::new(&index, &source, 64, FragmentClockProfile).unwrap();
    source.reset(0).unwrap();
    assert!(matches!(
        job.poll(
            &mut source,
            &mut WorkBudget::new(1),
            &AtomicBool::new(false)
        ),
        Err(Error::StaleGeneration { .. })
    ));
}
#[test]
fn range_request_is_not_a_clock_epoch() {
    let pool = Budget::new(1 << 20).unwrap();
    let index = fragment(&pool, DATA);
    let mut source = input(&pool);
    let mut job = FragmentClockJob::new(&index, &source, 64, FragmentClockProfile).unwrap();
    source.request_range(0).unwrap();
    assert!(matches!(
        job.poll(
            &mut source,
            &mut WorkBudget::new(1),
            &AtomicBool::new(false)
        ),
        Err(Error::StaleRequest { .. })
    ));
}
#[test]
fn source_transport_error_precedes_work_and_is_not_demux_invaliddata() {
    let pool = Budget::new(1 << 20).unwrap();
    let index = fragment(&pool, DATA);
    let mut source = input(&pool);
    let mut job = FragmentClockJob::new(&index, &source, 64, FragmentClockProfile).unwrap();
    source
        .fail(
            source.source(),
            source.token(),
            triergon_core::SourceFailure::Transport,
        )
        .unwrap();
    assert_eq!(
        job.poll(
            &mut source,
            &mut WorkBudget::new(0),
            &AtomicBool::new(false)
        )
        .unwrap_err(),
        Error::Transport
    );
}
#[test]
fn ready_publication_rechecks_source_transaction() {
    let pool = Budget::new(1 << 20).unwrap();
    let index = fragment(&pool, DATA);
    let mut source = input(&pool);
    let mut job = FragmentClockJob::new(&index, &source, 64, FragmentClockProfile).unwrap();
    while job
        .poll(
            &mut source,
            &mut WorkBudget::new(1),
            &AtomicBool::new(false),
        )
        .unwrap()
        != BuildStatus::Ready
    {}
    source.reset(0).unwrap();
    assert!(matches!(
        job.try_finish(&mut source, &AtomicBool::new(false)),
        Err(Error::StaleGeneration { .. })
    ));
}
#[test]
fn no_workspace_allocation_and_retained_full_backing_remain_charged() {
    let pool = Budget::new(1 << 20).unwrap();
    let index = fragment(&pool, DATA);
    let mut source = input(&pool);
    let used = pool.used();
    let reservation = pool.reserve(pool.available()).unwrap();
    let mut job = FragmentClockJob::new(&index, &source, 64, FragmentClockProfile).unwrap();
    while job
        .poll(
            &mut source,
            &mut WorkBudget::new(1),
            &AtomicBool::new(false),
        )
        .unwrap()
        != BuildStatus::Ready
    {}
    assert_eq!(pool.available(), 0);
    let result = job
        .try_finish(&mut source, &AtomicBool::new(false))
        .unwrap();
    drop(reservation);
    assert_eq!(pool.used(), used);
    drop(job);
    drop(index);
    drop(source);
    assert!(result.raw_tfdt.bytes().retained_capacity() >= DATA.len());
    assert!(pool.used() > Budget::control_storage_bytes());
    drop(result);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn signed_decode_runs_refuse_one_tick_past_the_signed_end() {
    let pool = Budget::new(1 << 20).unwrap();
    let with_base = |base: u64| {
        let mut bytes = DATA.to_vec();
        let at = bytes.windows(4).position(|x| x == b"tfdt").unwrap() + 8;
        bytes[at..at + 8].copy_from_slice(&base.to_be_bytes());
        fragment(&pool, &bytes)
    };
    let check = |base| {
        let index = with_base(base);
        check_signed_decode_runs(std::slice::from_ref(&index), FragmentClockProfile)
    };
    let span = with_base(0).decode_end().unwrap();
    let last = i64::MAX.unsigned_abs() - u64::try_from(span).unwrap();
    check(last).unwrap();
    assert_eq!(
        check(last + 1).unwrap_err(),
        Error::Invalid("native signed decode run overflow")
    );
    check(i64::MAX.unsigned_abs() + 1).unwrap();
}
