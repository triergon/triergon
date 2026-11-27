use super::*;
use crate::ebml_kit::element;
use triergon_core::{InputWindow, SourceCapabilities};

#[test]
fn source_range_job_uses_the_existing_core_cursor_and_preserves_generation() {
    let raw = element(0x1c53bb6b, &element(0xbb, &[]));
    let pool = Budget::new(64 * 1024).unwrap();
    let mut input = InputCursor::new(
        &pool,
        SourceId(7),
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        1,
    )
    .unwrap();
    let generation = input.generation();
    let mut job = RangeLeafJob::begin(&mut input, 123, 0x1c53bb6b, None, 1024).unwrap();
    let old = input.token();
    let mut result = None;
    for _ in 0..1000 {
        match job
            .step(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::NeedInput(request) => {
                let at = (request.offset - 123) as usize;
                assert!(
                    at < raw.len(),
                    "a finite complete leaf must not require source EOF"
                );
                let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1]).unwrap();
                input
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
            }
            ParseStep::Event(value) => {
                result = Some(value);
                break;
            }
            ParseStep::Yield => (),
            _ => panic!("complete one-byte ranged leaf"),
        }
    }
    let result = result.unwrap();
    assert_eq!(result.data.as_ref(), element(0xbb, &[]).as_slice());
    assert_eq!(input.generation(), generation);
    let new = input.request_range(0).unwrap();
    assert_ne!(old, new.token);
    assert_eq!(input.generation(), generation);
    drop(input);
    assert_eq!(result.data.as_ref(), element(0xbb, &[]).as_slice());
}

#[test]
fn range_job_rejects_stale_generation_and_a_transport_failure_even_at_its_pending_freeze() {
    use triergon_core::SourceFailure;
    let raw = element(0x1c53bb6b, &element(0xbb, &[]));
    let pool = Budget::new(4096).unwrap();
    let mut input = InputCursor::new(
        &pool,
        SourceId(41),
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        1,
    )
    .unwrap();
    let mut stale = RangeLeafJob::begin(&mut input, 0, 0x1c53bb6b, None, 1024).unwrap();
    input.reset(0).unwrap();
    assert!(matches!(
        stale.step(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::StaleGeneration { .. })
    ));
    drop(stale);
    let mut job = RangeLeafJob::begin(&mut input, 0, 0x1c53bb6b, None, 1024).unwrap();
    let mut blockers = Vec::new();
    let mut last_supplied = false;
    let mut blocked = false;
    for _ in 0..1000 {
        match job
            .step(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => {
                if last_supplied && pool.available() != 0 {
                    blockers.push(pool.reserve(pool.available()).unwrap());
                }
            }
            ParseStep::NeedInput(request) => {
                let at = request.offset as usize;
                assert!(at < raw.len());
                let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1]).unwrap();
                input
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
                if at + 1 == raw.len() {
                    last_supplied = true;
                    blockers.push(pool.reserve(pool.available()).unwrap());
                }
            }
            ParseStep::Backpressure { .. } => {
                blocked = true;
                break;
            }
            other => panic!("pending freeze: {other:?}"),
        }
    }
    assert!(blocked);
    let request = input.request();
    input
        .fail(request.source, request.token, SourceFailure::Transport)
        .unwrap();
    drop(blockers);
    assert!(matches!(
        job.step(
            &mut input,
            &mut WorkBudget::new(1000),
            &AtomicBool::new(false)
        ),
        Err(Error::Transport)
    ));
}
