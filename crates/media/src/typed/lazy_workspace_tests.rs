//! Parent-executed resource/progress guards; no native or compiler ran here.
use super::*;
use crate::matroska::typed::usage::{Owner, Storage};

const FIXED_ABC: [u8; 11] = [
    0x78, 0x01, 0x4b, 0x4c, 0x4a, 0x06, 0x00, 0x02, 0x4d, 0x01, 0x27,
];

fn owned_bytes(graph: &dyn Storage) -> usize {
    let mut total = Budget::control_storage_bytes();
    let mut byte_owners = Vec::new();
    for at in 0..graph.storage_slots().unwrap() {
        match graph.storage_owner(at).unwrap() {
            None => (),
            Some(Owner::Unique(bytes)) => total += bytes,
            Some(Owner::Bytes(bytes)) => {
                if !byte_owners
                    .iter()
                    .any(|old: &triergon_core::Bytes| old.shares_storage_with(&bytes))
                {
                    total += bytes.retained_storage_bytes();
                    byte_owners.push(bytes);
                }
            }
            other => panic!("unexpected restore owner: {other:?}"),
        }
    }
    total
}

#[test]
fn identity_view_needs_no_allocator_or_hidden_output_copy() {
    let pool = Budget::new(8192).unwrap();
    let baseline = pool.used();
    let source = SharedBytes::copy_from_slice(&pool, b"opaque immutable frame").unwrap();
    let pointer = source.as_ptr();
    let before = pool.used();
    let mut job = RestoreJob::new(
        source,
        RestorePlan::identity(),
        Limits::default(),
        64,
        &pool,
    )
    .unwrap();
    let result = match job
        .step(&mut WorkBudget::new(0), &AtomicBool::new(false))
        .unwrap()
    {
        ParseStep::Event(bytes) => bytes,
        other => panic!("existing immutable view: {other:?}"),
    };
    assert_eq!(result.as_ptr(), pointer);
    assert_eq!(pool.used(), before);
    assert_eq!(owned_bytes(&job), pool.used());
    drop(job);
    assert_eq!(result.as_ref(), b"opaque immutable frame");
    drop(result);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn compressed_workspace_is_charged_once_retryable_and_visited_under_work_one() {
    let pool = Budget::new(64 * 1024).unwrap();
    let baseline = pool.used();
    let source = SharedBytes::copy_from_slice(&pool, &FIXED_ABC).unwrap();
    let mut job =
        RestoreJob::new(source, RestorePlan::zlib(), Limits::default(), 33, &pool).unwrap();
    let before = pool.used();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &cancel).unwrap(),
        ParseStep::Yield
    ));
    assert_eq!(pool.used(), before, "work zero cannot allocate a workspace");
    let external = pool.reserve(pool.available()).unwrap();
    let mut requested = None;
    for _ in 0..1000 {
        let mut work = WorkBudget::new(1);
        let result = job.step(&mut work, &cancel).unwrap();
        assert!(work.spent() <= 1);
        match result {
            ParseStep::Yield => (),
            ParseStep::Backpressure {
                requested: bytes, ..
            } => {
                requested = Some(bytes);
                break;
            }
            other => panic!("charged workspace must wait before output: {other:?}"),
        }
    }
    assert!(
        requested.unwrap() >= size_of::<Inflater>(),
        "fixed inflater state cannot escape live-capacity charging"
    );
    drop(external);
    let mut result = None;
    for _ in 0..10000 {
        let mut work = WorkBudget::new(1);
        let step = job.step(&mut work, &cancel).unwrap();
        assert!(work.spent() <= 1);
        assert_eq!(
            owned_bytes(&job),
            pool.used(),
            "slot plus all nested source/output/replacement owners must be visible once"
        );
        match step {
            ParseStep::Yield => (),
            ParseStep::Event(bytes) => {
                result = Some(bytes);
                break;
            }
            other => panic!("retry: {other:?}"),
        }
    }
    let result = result.unwrap();
    assert_eq!(result.as_ref(), b"abc");
    drop(job);
    assert!(pool.used() > baseline);
    drop(result);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn compressed_workspace_cancellation_never_publishes_and_drop_releases_every_owner() {
    let pool = Budget::new(64 * 1024).unwrap();
    let baseline = pool.used();
    let source = SharedBytes::copy_from_slice(&pool, &FIXED_ABC).unwrap();
    let mut job =
        RestoreJob::new(source, RestorePlan::zlib(), Limits::default(), 33, &pool).unwrap();
    assert!(matches!(
        job.step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        ParseStep::Yield
    ));
    assert_eq!(owned_bytes(&job), pool.used());
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    drop(job);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn dormant_restore_workspace_does_not_exceed_one_kibibyte() {
    assert!(
        size_of::<RestoreJob>() <= 1024,
        "identity/stored paths cannot carry the maximum inflater state: RestoreJob={} Transform={}",
        size_of::<RestoreJob>(),
        size_of::<Transform>()
    );
}
