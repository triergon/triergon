use super::*;
use crate::Limits;
use triergon_core::{Budget, SourceCapabilities, SourceFailure, SourceId};

fn setup(pool: &Budget) -> ElementReader {
    let mut reader = ElementReader::new(
        pool,
        SourceId(701),
        SourceCapabilities {
            length: Some(1000),
            random_access: true,
        },
        Limits::default(),
        64,
    )
    .unwrap();
    reader.segment = Some(Master {
        id: 0x18538067,
        start: 16,
        end: Some(1000),
    });
    reader.cluster = Some(Master {
        id: 0x1f43b675,
        start: 90,
        end: Some(500),
    });
    reader
}
fn header(reader: &ElementReader, start: u64) -> ElementHeader {
    ElementHeader {
        id: 0xa3,
        size: Some(8),
        start,
        data: start + 2,
        generation: reader.generation(),
    }
}
fn record(location: &ReplayLocation) -> (u64, u64, u64, u64, Option<u64>, u64, Option<u64>, u64) {
    (
        location.position,
        location.cluster.start,
        location.checkpoint.offset,
        location.generation,
        location.cluster_time,
        location.checkpoint.segment.start,
        location.checkpoint.segment.end,
        location.checkpoint.source.0,
    )
}

#[test]
fn replay_refresh_keeps_prior_clone_independent_and_restorable() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut reader = setup(&pool);
    let generation = reader.generation();
    let mut slot = None;
    reader
        .refresh_block_replay_location(&mut slot, header(&reader, 100), Some(5))
        .unwrap();
    let prior = slot.clone().unwrap();
    reader
        .refresh_block_replay_location(&mut slot, header(&reader, 130), Some(6))
        .unwrap();
    reader.cluster = Some(Master {
        id: 0x1f43b675,
        start: 500,
        end: Some(900),
    });
    reader
        .refresh_block_replay_location(&mut slot, header(&reader, 550), None)
        .unwrap();
    assert_eq!(
        record(&prior),
        (100, 90, 90, generation, Some(5), 16, Some(1000), 701)
    );
    assert_eq!(
        record(slot.as_ref().unwrap()),
        (550, 500, 500, generation, None, 16, Some(1000), 701)
    );
    assert_eq!(pool.used(), baseline);
    let last = slot.clone().unwrap();
    assert_eq!(reader.restore_internal_block(&prior).unwrap().offset, 100);
    assert_eq!(reader.cluster.unwrap().start, 90);
    assert_eq!(reader.restore_internal_block(&last).unwrap().offset, 550);
    assert_eq!(reader.cluster.unwrap().start, 500);
    drop((reader, slot, prior, last));
    assert_eq!(pool.used(), baseline);
}

#[test]
fn replay_refresh_rejection_preserves_prior_value_and_error_order() {
    let pool = Budget::new(4096).unwrap();
    let mut reader = setup(&pool);
    let mut slot = Some(
        reader
            .block_replay_location(header(&reader, 100), Some(5))
            .unwrap(),
    );
    let expected = record(slot.as_ref().unwrap());
    let good = header(&reader, 130);
    for bad in [
        ElementHeader { id: 0xe7, ..good },
        ElementHeader {
            generation: good.generation + 1,
            ..good
        },
        ElementHeader { start: 89, ..good },
        ElementHeader {
            size: Some(900),
            ..good
        },
        ElementHeader {
            data: u64::MAX,
            ..good
        },
    ] {
        assert!(
            reader
                .refresh_block_replay_location(&mut slot, bad, None)
                .is_err()
        );
        assert_eq!(record(slot.as_ref().unwrap()), expected);
    }
    reader.cluster = None;
    assert!(matches!(
        reader.refresh_block_replay_location(
            &mut slot,
            ElementHeader {
                generation: good.generation + 1,
                ..good
            },
            None
        ),
        Err(Error::Invalid("invalid replay block identity"))
    ));
    reader.cluster = Some(Master {
        id: 0x1f43b675,
        start: 90,
        end: Some(500),
    });
    reader.segment = None;
    assert!(matches!(
        reader.refresh_block_replay_location(&mut slot, good, None),
        Err(Error::Invalid("missing replay Cluster context"))
    ));
    assert_eq!(record(slot.as_ref().unwrap()), expected);
}

#[test]
fn replay_refresh_foreign_slot_and_reset_keep_source_fences() {
    let first_pool = Budget::new(4096).unwrap();
    let second_pool = Budget::new(4096).unwrap();
    let first = setup(&first_pool);
    let mut reader = setup(&second_pool);
    let foreign = first
        .block_replay_location(header(&first, 100), None)
        .unwrap();
    let mut slot = Some(foreign.clone());
    reader
        .refresh_block_replay_location(&mut slot, header(&reader, 130), Some(7))
        .unwrap();
    assert!(
        slot.as_ref()
            .unwrap()
            .checkpoint
            .pool
            .same_pool(&second_pool)
    );
    assert!(foreign.checkpoint.pool.same_pool(&first_pool));
    assert!(matches!(
        reader.restore_internal_block(&foreign),
        Err(Error::Core(triergon_core::Error::ForeignPool))
    ));
    let old = slot.clone().unwrap();
    reader.reset().unwrap();
    reader.segment = Some(Master {
        id: 0x18538067,
        start: 16,
        end: Some(1000),
    });
    reader.cluster = Some(Master {
        id: 0x1f43b675,
        start: 500,
        end: None,
    });
    reader
        .refresh_block_replay_location(&mut slot, header(&reader, 550), None)
        .unwrap();
    assert_ne!(slot.as_ref().unwrap().generation, old.generation);
    assert!(matches!(
        reader.restore_internal_block(&old),
        Err(Error::Core(triergon_core::Error::StaleGeneration { .. }))
    ));
    assert_eq!(
        reader
            .restore_internal_block(slot.as_ref().unwrap())
            .unwrap()
            .offset,
        550
    );
    assert_eq!(foreign.position, 100);
}

#[test]
fn replay_refresh_does_not_hoist_a_late_source_fault_or_advance_input() {
    let pool = Budget::new(4096).unwrap();
    let mut reader = setup(&pool);
    let baseline = pool.used();
    let request = reader.request();
    reader
        .fail(request.source, request.token, SourceFailure::Transport)
        .unwrap();
    let mut slot = None;
    reader
        .refresh_block_replay_location(&mut slot, header(&reader, 100), Some(5))
        .unwrap();
    assert_eq!(reader.position(), 0);
    assert_eq!(pool.used(), baseline);
    let mut work = WorkBudget::new(0);
    assert!(matches!(
        reader.healthy_for_job(&mut work, &AtomicBool::new(false)),
        Err(Error::Core(triergon_core::Error::Transport))
    ));
    assert_eq!(
        (reader.position(), work.spent(), pool.used()),
        (0, 0, baseline)
    );
    assert_eq!(record(slot.as_ref().unwrap()).0, 100);
}
