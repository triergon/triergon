//! Private feature guards: real work-one progress, retry, cancellation and exact
//! owner accounting.
use super::*;
use crate::matroska::typed::{
    packet::PacketBatchJob,
    side_data::GroupJob,
    usage::{Pressure, Storage},
};

fn group_raw(extra: &[u8]) -> Vec<u8> {
    [element(0xa1, &[0x81, 0, 0, 0, b'V']), extra.to_vec()].concat()
}
fn finish_group(mut job: GroupJob) -> crate::typed::BlockGroupSource {
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(1);
        let result = job.step(&mut work, &AtomicBool::new(false)).unwrap();
        assert!(work.spent() <= 1);
        match result {
            ParseStep::Event(value) => return value,
            ParseStep::Yield => (),
            other => panic!("group: {other:?}"),
        }
    }
    panic!("group did not finish")
}
#[test]
fn group_alias_graph_equals_actual_charges_and_can_outlive_source_and_reader_state() {
    let pool = Budget::new(64 * 1024).unwrap();
    let baseline = pool.used();
    let raw = group_raw(
        &[
            element(0xfb, &[-1_i8 as u8]),
            element(0x4fff, b"opaque"),
            element(0xa4, b"state"),
        ]
        .concat(),
    );
    let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
    let group = finish_group(
        GroupJob::new(source.clone(), 0, None, crate::Limits::default(), &pool).unwrap(),
    );
    assert_eq!(group.references(), [-1]);
    let used = pool.used();
    let alias = group.clone();
    assert_eq!(used, pool.used());
    assert!(alias.shares_owner_with(&group));
    // Enumerate a fixed alias graph and deduplicate with the production cold
    // proof. No external owner is needed for its exact own-storage sum.
    struct Graph<'a>(
        &'a crate::typed::BlockGroupSource,
        &'a crate::typed::BlockGroupSource,
        &'a SharedBytes,
    );
    impl Storage for Graph<'_> {
        fn storage_slots(&self) -> Result<usize> {
            usage::slots(&[self.0, self.1, self.2])
        }
        fn storage_owner(&self, at: usize) -> Result<Option<usage::Owner>> {
            usage::owner(&[self.0, self.1, self.2], at)
        }
    }
    let graph = Graph(&group, &alias, &source);
    let mut sum = baseline;
    let mut owners = Vec::new();
    for at in 0..graph.storage_slots().unwrap() {
        let Some(owner) = graph.storage_owner(at).unwrap() else {
            continue;
        };
        match owner {
            usage::Owner::Unique(bytes) => sum += bytes,
            usage::Owner::Bytes(bytes) => {
                if !owners
                    .iter()
                    .any(|old: &Bytes| old.shares_storage_with(&bytes))
                {
                    sum += bytes.retained_storage_bytes();
                    owners.push(bytes);
                }
            }
            usage::Owner::Group(owner) => {
                // This graph has exactly one group owner and two aliases.
                if at == 0 {
                    sum += owner.structure_bytes().unwrap();
                }
            }
            other => panic!("unexpected owner: {other:?}"),
        }
    }
    assert_eq!(
        sum, used,
        "hidden source capacity and descriptor/header allocations must all be visited once"
    );
    drop((owners, source, group));
    assert_eq!(alias.codec_state().unwrap().data(), b"state");
    assert!(pool.used() > baseline);
    drop(alias);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn group_and_state_batch_work_zero_is_inert_and_cancellation_precedes_publication() {
    let pool = Budget::new(64 * 1024).unwrap();
    let configuration = seed_configuration(&pool, &[]);
    let source = SharedBytes::copy_from_slice(&pool, &group_raw(&element(0xa4, b"state"))).unwrap();
    let mut batch = PacketBatchJob::new(
        source.clone(),
        false,
        3,
        1_000_000,
        configuration.clone(),
        crate::Limits::default(),
        &pool,
    )
    .unwrap();
    let before = pool.used();
    let mut work = WorkBudget::new(0);
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        batch.step(&mut work, &cancel).unwrap(),
        ParseStep::Yield
    ));
    assert_eq!(pool.used(), before);
    assert_eq!(work.spent(), 0);
    assert!(batch.take_configuration_change().is_none());
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(matches!(
        batch.step(&mut work, &cancel),
        Err(Error::Cancelled)
    ));
    assert!(batch.take_configuration_change().is_none());
}

#[test]
fn retained_external_pressure_keeps_a_real_group_job_pending_and_retries_same_batch() {
    let pool = Budget::new(64 * 1024).unwrap();
    let baseline = pool.used();
    let configuration = seed_configuration(&pool, &[]);
    let source = SharedBytes::copy_from_slice(&pool, &group_raw(&element(0xa4, b"state"))).unwrap();
    let mut batch = PacketBatchJob::new(
        source,
        false,
        3,
        1_000_000,
        configuration,
        crate::Limits::default(),
        &pool,
    )
    .unwrap();
    let caller = pool.reserve(pool.available()).unwrap();
    let cancel = AtomicBool::new(false);
    let mut blocked = false;
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(1);
        let result = batch.step(&mut work, &cancel).unwrap();
        assert!(work.spent() <= 1);
        match result {
            ParseStep::Yield => (),
            ParseStep::Backpressure { .. } => {
                blocked = true;
                break;
            }
            other => panic!("must block before output: {other:?}"),
        }
    }
    assert!(blocked);
    assert!(batch.take_configuration_change().is_none());
    drop(caller);
    let mut revision = None;
    let mut packet = None;
    for _ in 0..100_000 {
        if let Some(configuration) = batch.take_configuration_change() {
            revision = Some(configuration.revision());
        }
        let mut work = WorkBudget::new(1);
        let result = batch.step(&mut work, &cancel).unwrap();
        assert!(work.spent() <= 1);
        match result {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                assert_eq!(revision, Some(value.configuration.revision()));
                packet = Some(value);
                break;
            }
            other => panic!("released caller capacity must permit retry: {other:?}"),
        }
    }
    let packet = packet.unwrap();
    assert_eq!(packet.packet.data.as_ref(), b"V");
    assert_eq!(
        packet.configuration.codec_state(0).unwrap().data(),
        b"state"
    );
    drop((packet, batch));
    assert_eq!(pool.used(), baseline);
}

#[test]
fn group_own_storage_is_a_permanent_limit_not_an_infinite_backpressure_facade() {
    let pool = Budget::new(512).unwrap();
    let raw = group_raw(&element(0x4fff, b"opaque"));
    let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
    let mut job = GroupJob::new(source, 0, None, crate::Limits::default(), &pool).unwrap();
    let cancel = AtomicBool::new(false);
    let mut requested = None;
    for _ in 0..100_000 {
        match job.step(&mut WorkBudget::new(1), &cancel) {
            Ok(ParseStep::Yield) => (),
            Ok(ParseStep::Backpressure {
                requested: amount, ..
            }) => {
                requested = Some(amount);
                break;
            }
            Err(Error::Policy(_)) => return,
            other => panic!("tight owned pool must fail, got {other:?}"),
        }
    }
    let mut proof =
        Pressure::new(requested.expect("tight owned job must reach its allocation boundary"));
    for _ in 0..100_000 {
        match proof.step(&job, &pool, &mut WorkBudget::new(1), &cancel) {
            Err(Error::Policy("operation retained storage")) => return,
            Ok(ParseStep::Yield) => (),
            other => panic!("operation-owned shortage is permanent: {other:?}"),
        }
    }
    panic!("own-storage proof did not terminate")
}
