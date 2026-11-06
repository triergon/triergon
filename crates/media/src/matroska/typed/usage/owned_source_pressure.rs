//! The source root is operation-owned even with no current parser window.
use super::*;

#[test]
fn a_source_only_owner_is_an_unavoidable_limit_not_caller_backpressure() {
    let limit = Budget::control_storage_bytes() + Bytes::owner_overhead_bytes() + 64;
    let pool = Budget::new(limit).unwrap();
    let source = Bytes::copy_from_slice(&pool, &[7; 64]).unwrap();
    let none: Option<Bytes> = None;
    let graph = WithSource {
        graph: &none,
        source: &source,
    };
    let cancel = AtomicBool::new(false);
    let mut pressure = Pressure::new(1);
    assert!(matches!(
        pressure.step(&graph, &pool, &mut WorkBudget::new(0), &cancel),
        Ok(ParseStep::Yield)
    ));
    let mut result = Ok(ParseStep::Yield);
    for _ in 0..12 {
        result = pressure.step(&graph, &pool, &mut WorkBudget::new(1), &cancel);
        if !matches!(result, Ok(ParseStep::Yield)) {
            break;
        }
    }
    assert!(matches!(
        result,
        Err(Error::Limit("operation retained storage"))
    ));
}

#[test]
fn source_and_current_packet_window_are_deduplicated_by_the_real_owner() {
    let limit = Budget::control_storage_bytes() + Bytes::owner_overhead_bytes() + 64 + 16;
    let pool = Budget::new(limit).unwrap();
    let source = Bytes::copy_from_slice(&pool, &[7; 64]).unwrap();
    let alias = source.slice(1..2).unwrap();
    let external = pool.reserve(16).unwrap();
    let graph = WithSource {
        graph: &alias,
        source: &source,
    };
    let cancel = AtomicBool::new(false);
    let mut pressure = Pressure::new(1);
    let result = pressure.step(&graph, &pool, &mut WorkBudget::new(64), &cancel);
    assert!(matches!(
        result,
        Ok(ParseStep::Backpressure {
            requested: 1,
            available: 0
        })
    ));
    drop(external);
    assert!(matches!(
        pressure.step(&graph, &pool, &mut WorkBudget::new(1), &cancel),
        Ok(ParseStep::Event(()))
    ));
}

#[test]
fn source_only_pressure_still_checks_cancellation_before_zero_work() {
    let pool = Budget::new(4096).unwrap();
    let source = Bytes::copy_from_slice(&pool, &[7; 64]).unwrap();
    let none: Option<Bytes> = None;
    let graph = WithSource {
        graph: &none,
        source: &source,
    };
    assert!(matches!(
        Pressure::new(1).step(
            &graph,
            &pool,
            &mut WorkBudget::new(0),
            &AtomicBool::new(true)
        ),
        Err(Error::Cancelled)
    ));
}
