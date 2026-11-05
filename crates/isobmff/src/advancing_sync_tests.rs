use super::*;
use triergon_core::Bytes;

#[test]
fn sample_error_after_membership_does_not_commit_either_row() {
    let fixture = include_bytes!("../tests/fixtures/r-front-stco.mov");
    let at = fixture.windows(4).position(|word| word == b"stbl").unwrap() - 4;
    let length = u32::from_be_bytes(fixture[at..at + 4].try_into().unwrap()) as usize;
    let budget = Budget::new(1024 * 1024).unwrap();
    let raw = BoxView::new(
        Bytes::copy_from_slice(&budget, &fixture[at..at + length]).unwrap(),
        at as u64,
    )
    .unwrap();
    let mut builder = SampleTableBuilder::new(raw, budget, Limits::default()).unwrap();
    let mut ready = false;
    for _ in 0..100_000 {
        if builder.poll(&mut WorkBudget::new(1)).unwrap() == BuildStatus::Ready {
            ready = true;
            break;
        }
    }
    assert!(ready);
    let mut table = builder.finish().unwrap();
    assert_eq!(table.previous_sync(1).unwrap(), Some(1));
    // Private-state fault control: reuse the validated rows for both counters,
    // then place the decode clock where the post-sample increment overflows.
    // This is not a claim about reachable valid-media timestamps or stps syntax.
    table.partial = table.sync.clone();
    let mut cursor = table.cursor();
    cursor.dts = i128::MAX;
    let mut trial = cursor;
    assert_eq!(trial.advance().unwrap_err(), Error::Overflow);
    assert_eq!((trial.sync_row, trial.partial_row), (1, 1));
    for _ in 0..2 {
        let mut work = WorkBudget::new(1);
        assert_eq!(cursor.poll(&mut work).unwrap_err(), Error::Overflow);
        assert_eq!(work.spent(), 1);
        assert_eq!(
            (cursor.ordinal, cursor.sync_row, cursor.partial_row),
            (0, 0, 0)
        );
        assert_eq!(cursor.dts, i128::MAX);
    }
}
