//! Compatibility guards for bounded independent-byte text reductions.
use super::*;
use crate::{Error, storage::text_job::TextJob};

fn text_result(raw: &[u8], ascii: bool, quantum: u64) -> crate::Result<(String, usize, u64)> {
    let pool = Budget::new(32 * 1024).unwrap();
    let baseline = pool.used();
    let source = SharedBytes::copy_from_slice(&pool, raw).unwrap();
    let pointer = source.as_ptr();
    let before = pool.used();
    let mut job = TextJob::new(source, 0..raw.len(), ascii).unwrap();
    let mut work = WorkBudget::new(0);
    assert!(matches!(
        job.step(&mut work, &AtomicBool::new(false)).unwrap(),
        ParseStep::Yield
    ));
    assert_eq!(work.spent(), 0);
    assert_eq!(pool.used(), before);
    let mut spent = 0;
    for _ in 0..10_000 {
        let mut work = WorkBudget::new(quantum);
        let step = job.step(&mut work, &AtomicBool::new(false));
        assert!(work.spent() <= quantum);
        spent += work.spent();
        assert_eq!(
            pool.used(),
            before,
            "validation cannot allocate a second string owner"
        );
        match step {
            Ok(ParseStep::Yield) => (),
            Ok(ParseStep::Event(value)) => {
                assert_eq!(value.text.as_ptr(), pointer);
                let result = (value.text.as_str().to_owned(), value.range.end, spent);
                drop(job);
                assert!(
                    pool.used() > baseline,
                    "the returned alias retains its full input owner"
                );
                drop(value);
                assert_eq!(pool.used(), baseline);
                return Ok(result);
            }
            Err(error) => {
                assert_eq!(
                    spent,
                    raw.len() as u64,
                    "the existing deferred error precedence scans the whole field"
                );
                drop(job);
                assert_eq!(pool.used(), baseline);
                return Err(error);
            }
            other => panic!("complete bounded text span: {other:?}"),
        }
    }
    panic!("bounded text did not finish");
}

#[test]
fn long_ascii_and_zero_suffix_keep_exact_work_owner_and_source_range() {
    for quantum in [1, 7, 256, 8192] {
        let mut raw = vec![b'A'; 769];
        raw.extend([0; 259]);
        let (text, end, spent) = text_result(&raw, true, quantum).unwrap();
        assert_eq!(text, "A".repeat(769));
        assert_eq!(end, 769);
        assert_eq!(spent, raw.len() as u64);
    }
}

#[test]
fn utf8_scalar_across_batch_boundary_keeps_ascii_and_terminator_error_precedence() {
    for quantum in [1, 7, 256, 8192] {
        let mut unicode = vec![b'A'; 255];
        unicode.extend("😀é".as_bytes());
        unicode.extend([0, 0]);
        let (text, end, spent) = text_result(&unicode, false, quantum).unwrap();
        assert_eq!(text, format!("{}😀é", "A".repeat(255)));
        assert_eq!(end, 261);
        assert_eq!(spent, unicode.len() as u64);
        assert!(matches!(
            text_result(&unicode, true, quantum),
            Err(Error::Invalid("ASCII metadata"))
        ));

        let mut bad = vec![b'A'; 255];
        bad.extend([0xf0, 0x80, 0, 0]);
        assert!(matches!(
            text_result(&bad, true, quantum),
            Err(Error::Invalid("UTF-8 metadata"))
        ));
        bad.push(1);
        assert!(matches!(
            text_result(&bad, true, quantum),
            Err(Error::Invalid("metadata string terminator"))
        ));
        let mut control = vec![b'A'; 255];
        control.push(0x1f);
        assert!(matches!(
            text_result(&control, true, quantum),
            Err(Error::Invalid("ASCII metadata"))
        ));
    }
}

#[test]
fn text_cancel_after_prefix_preserves_no_partial_event_or_owner_escape() {
    let pool = Budget::new(8192).unwrap();
    let baseline = pool.used();
    let source = SharedBytes::copy_from_slice(&pool, &[b'V'; 769]).unwrap();
    let mut job = TextJob::new(source, 0..769, true).unwrap();
    let mut work = WorkBudget::new(256);
    assert!(matches!(
        job.step(&mut work, &AtomicBool::new(false)).unwrap(),
        ParseStep::Yield
    ));
    assert_eq!(work.spent(), 256);
    let mut work = WorkBudget::new(0);
    assert!(matches!(
        job.step(&mut work, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!(work.spent(), 0);
    assert!(matches!(
        job.step(&mut WorkBudget::new(8192), &AtomicBool::new(false)),
        Err(Error::Invalid("text requires replacement after error"))
    ));
    drop(job);
    assert_eq!(pool.used(), baseline);
}
