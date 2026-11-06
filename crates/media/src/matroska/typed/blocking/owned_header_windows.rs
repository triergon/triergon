//! Source-window contracts.
use super::*;
use std::{io::Cursor, sync::atomic::Ordering};
use triergon_core::{Budget, OwnedInputBytes, SourceCapabilities, SourceId};

fn setup(
    length: usize,
) -> (
    Budget,
    Bytes,
    InputFeeder<Cursor<OwnedInputBytes>>,
    InputCursor,
) {
    let pool = Budget::new(32_768).unwrap();
    let source =
        OwnedInputBytes::try_from_vec(&pool, (0..length).map(|n| n as u8).collect::<Vec<_>>())
            .unwrap();
    let bytes = source.as_bytes().clone();
    let feeder = InputFeeder::from_owned(Cursor::new(source), 8192).unwrap();
    let cursor = InputCursor::new(
        &pool,
        SourceId(7031),
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        8192,
    )
    .unwrap();
    (pool, bytes, feeder, cursor)
}
fn seek(source: &mut Cursor<OwnedInputBytes>, offset: u64) -> io::Result<u64> {
    source.seek(SeekFrom::Start(offset))
}
fn supply(
    feeder: &mut InputFeeder<Cursor<OwnedInputBytes>>,
    input: &mut InputCursor,
    count: usize,
    header: bool,
) -> Result<FeedStep> {
    let mut request = input.request();
    request.maximum = count;
    feeder.feed_typed_request(input, &request, header, &AtomicBool::new(false), Some(seek))
}

#[test]
fn expanded_header_is_the_existing_charged_owner_without_new_storage() {
    let (pool, source, mut feeder, mut input) = setup(5000);
    let used = pool.used();
    assert!(matches!(
        supply(&mut feeder, &mut input, 4096, true),
        Ok(FeedStep::Provided)
    ));
    let window = input.retained_window().unwrap();
    assert!(
        window.shares_storage_with(&source),
        "must remove a real copied/frozen owner"
    );
    assert_eq!(window.as_ref(), &source[..4096]);
    assert_eq!(window.retained_capacity(), source.retained_capacity());
    assert_eq!(
        pool.used(),
        used,
        "an immutable view needs no new capacity/header"
    );
    assert_eq!(feeder.reader_mut().position(), 4096);
    assert_eq!(input.position(), 0, "supply itself consumes no parser work");
}

#[test]
fn body_request_keeps_the_original_exact_capacity_path() {
    let (_, source, mut feeder, mut input) = setup(90);
    assert!(matches!(
        supply(&mut feeder, &mut input, 19, false),
        Ok(FeedStep::Provided)
    ));
    let window = input.retained_window().unwrap();
    assert!(!window.shares_storage_with(&source));
    assert_eq!((window.len(), window.retained_capacity()), (19, 19));
    assert_eq!(window.as_ref(), &source[..19]);
}

#[test]
fn fixed_inline_header_representation_stays_literal() {
    let (_, _, mut feeder, mut input) = setup(90);
    assert!(matches!(
        supply(&mut feeder, &mut input, 5, true),
        Ok(FeedStep::Provided)
    ));
    assert!(input.retained_window().is_none());
    let mut output = [0; 5];
    let mut work = WorkBudget::new(1);
    assert!(matches!(
        input.read_into(&mut output, &mut work, &AtomicBool::new(false)),
        Ok(Step::Ready(1))
    ));
    assert_eq!(input.position(), 1);
    assert_eq!(work.remaining(), 0);
}

#[test]
fn zero_work_then_one_byte_consumption_preserves_view_and_position() {
    let (pool, source, mut feeder, mut input) = setup(90);
    supply(&mut feeder, &mut input, 40, true).unwrap();
    let used = pool.used();
    assert!(matches!(
        input.read_span(40, &mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(Step::Yield)
    ));
    assert_eq!(input.position(), 0);
    let Step::Ready(first) = input
        .read_span(40, &mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
    else {
        panic!("byte must advance")
    };
    assert_eq!(first.as_ref(), &[0]);
    assert!(first.shares_storage_with(&source));
    assert_eq!(pool.used(), used);
    assert_eq!(input.position(), 1);
}

#[test]
fn logical_reset_keeps_old_caller_alias_and_supplies_new_token_at_exact_offset() {
    let (_, source, mut feeder, mut input) = setup(90);
    supply(&mut feeder, &mut input, 40, true).unwrap();
    let old = input.retained_window().unwrap();
    let token = input.token();
    input.reset(17).unwrap();
    assert_ne!(input.token(), token);
    supply(&mut feeder, &mut input, 25, true).unwrap();
    let new = input.retained_window().unwrap();
    assert_eq!(old.as_ref(), &source[..40]);
    assert_eq!(new.as_ref(), &source[17..42]);
    assert!(old.shares_storage_with(&new));
    assert_eq!(feeder.reader_mut().position(), 42);
}

#[test]
fn cancellation_before_supply_does_not_move_source_or_publish_view() {
    let (_, _, mut feeder, mut input) = setup(90);
    let request = input.request();
    let cancelled = AtomicBool::new(true);
    assert!(matches!(
        feeder.feed_typed_request(&mut input, &request, true, &cancelled, Some(seek)),
        Err(Error::Cancelled)
    ));
    assert_eq!(feeder.reader_mut().position(), 0);
    assert!(input.retained_window().is_none());
    cancelled.store(false, Ordering::Relaxed);
    assert!(matches!(
        input.read_into(&mut [0], &mut WorkBudget::new(1), &cancelled),
        Err(triergon_core::Error::Cancelled)
    ));
}

#[test]
fn previous_transport_failure_wins_over_new_cancellation() {
    let (_, _, mut feeder, mut input) = setup(90);
    let request = input.request();
    input
        .fail(
            request.source,
            request.token.clone(),
            SourceFailure::Transport,
        )
        .unwrap();
    assert!(matches!(
        feeder.feed_typed_request(
            &mut input,
            &request,
            true,
            &AtomicBool::new(true),
            Some(seek)
        ),
        Err(triergon_core::Error::Transport)
    ));
    assert_eq!(feeder.reader_mut().position(), 0);
}

#[test]
fn short_final_window_and_next_real_eof_keep_exact_source_endpoints() {
    let (_, source, mut feeder, mut input) = setup(23);
    supply(&mut feeder, &mut input, 80, true).unwrap();
    let Step::Ready(last) = input
        .read_span(80, &mut WorkBudget::new(80), &AtomicBool::new(false))
        .unwrap()
    else {
        panic!("final prefix")
    };
    assert_eq!(last.as_ref(), source.as_ref());
    assert_eq!(input.position(), 23);
    assert!(matches!(
        supply(&mut feeder, &mut input, 80, true),
        Ok(FeedStep::End)
    ));
    assert_eq!(feeder.reader_mut().position(), 23);
}

#[test]
fn stale_request_cannot_reposition_or_publish_an_old_owner() {
    let (_, _, mut feeder, mut input) = setup(90);
    let request = input.request();
    input.reset(17).unwrap();
    assert!(matches!(
        feeder.feed_typed_request(
            &mut input,
            &request,
            true,
            &AtomicBool::new(false),
            Some(seek)
        ),
        Err(Error::Invalid("blocking input request identity changed"))
    ));
    assert_eq!(feeder.reader_mut().position(), 0);
    assert!(input.retained_window().is_none());
}

#[test]
fn retained_subview_keeps_full_source_charge_after_source_and_feeder_drop() {
    let (pool, source, mut feeder, mut input) = setup(90);
    let charge = source.retained_storage_bytes();
    supply(&mut feeder, &mut input, 40, true).unwrap();
    let Step::Ready(alias) = input
        .read_span(1, &mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
    else {
        panic!("retained prefix")
    };
    drop(input);
    drop(source);
    drop(feeder);
    assert_eq!(pool.used(), Budget::control_storage_bytes() + charge);
    assert_eq!(alias.as_ref(), &[0]);
    drop(alias);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
