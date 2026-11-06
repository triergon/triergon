//! Public controller polling after a real inline-span allocation rejection.
use super::*;
use crate::incremental::typed_bridge::CompactEnvelopeProgress;

fn pressure_trace(original_workspace: bool) -> Vec<String> {
    let pool = Budget::new(65536).unwrap();
    let baseline = pool.used();
    let mut reader = Reader::new(
        &pool,
        SourceId(4017),
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        Limits::default(),
        32,
    )
    .unwrap();
    let request = reader.request();
    reader
        .envelope
        .range_input()
        .provide_inline(
            request.source,
            request.token,
            0,
            &[0x1a, 0x45, 0xdf, 0xa3, 0x83],
        )
        .unwrap();
    let result = reader
        .envelope
        .step_typed_compact(&mut WorkBudget::new(64), &AtomicBool::new(false))
        .unwrap();
    let CompactEnvelopeProgress::Header(header) = result else {
        panic!("missing actual envelope admission {result:?}");
    };
    reader
        .envelope
        .admit_typed_leaf(header, None, false)
        .unwrap();
    if original_workspace {
        // Conservative control: the original begin() constructs this exact
        // empty workspace. No charge, source byte or work credit is fabricated.
        crate::incremental::lazy_body_regressions::original_empty_workspace(&mut reader.envelope);
    }
    let request = reader.request();
    reader
        .envelope
        .range_input()
        .provide_inline(request.source, request.token, request.offset, b"a")
        .unwrap();
    let blocker = pool.reserve(pool.available()).unwrap();
    let mut rows = Vec::new();
    for _ in 0..4096 {
        let mut work = WorkBudget::new(1);
        let step = reader.step(&mut work, &AtomicBool::new(false)).unwrap();
        let (label, done) = match step {
            Step::Yield => ("yield".to_owned(), false),
            Step::Backpressure(Backpressure {
                requested,
                available,
            }) => {
                assert!(requested > 0);
                assert_eq!(available, 0);
                (format!("pressure {requested} {available}"), true)
            }
            other => panic!("unexpected rejection progress {other:?}"),
        };
        rows.push(format!(
            "{label} work={} pos={} live={}",
            work.spent(),
            reader.position(),
            pool.used() - baseline
        ));
        if done {
            assert_eq!(reader.position(), 5); // rejected promotion consumed no input
            drop(blocker);
            reader.reset().unwrap();
            drop(reader);
            assert_eq!(pool.used(), baseline);
            return rows;
        }
    }
    panic!("allocation-pressure graph walk stalled");
}

#[test]
fn lazy_pressure_public_poll_schedule_keeps_original_empty_workspace_descriptor() {
    let original = pressure_trace(true);
    let natural = pressure_trace(false);
    println!("LAZY_PRESSURE_ORIGINAL {original:?}");
    println!("LAZY_PRESSURE_NATURAL {natural:?}");
    assert_eq!(
        natural, original,
        "same public pressure/work/source/charge trace"
    );
}
