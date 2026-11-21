//! Scan seeks that need the reader's private state: the released span on
//! cancellation, the pending best landing, layout, and the seek index.
use super::*;
use crate::ebml_kit::{cluster, uint as u};
use crate::seek_scan_kit::{
    Feed, drain, entry, file, group, immutable_fixture, new_reader, open, repeated, seek_error,
    simple,
};
use triergon_core::Bytes;

#[test]
fn scan_prefix_cancellation_releases_the_complete_span() {
    let fixture = repeated(1, 128);
    let pool = Budget::new(128 * 1024).unwrap();
    let mut reader = new_reader(&pool, &fixture.raw, Limits::default());
    let mut feed = Feed::new(&fixture.raw, 8192);
    open(&mut reader, &mut feed).unwrap();
    reader.seek_for_playback(0, 0).unwrap();
    let body_size = 132;
    let body_start = (fixture.raw.len() - body_size) as u64;
    let cancel = AtomicBool::new(false);
    for _ in 0..100_000 {
        match reader.step(&mut WorkBudget::new(1), &cancel).unwrap() {
            Step::NeedInput(request) => {
                feed.hold = request.offset == body_start;
                feed.supply(&mut reader, request).unwrap();
                if feed.hold {
                    break;
                }
            }
            Step::Yield | Step::Ready(Event::Envelope(_)) => (),
            other => panic!("expected the block input request, got {other:?}"),
        }
    }
    let charge = feed.alias.take().unwrap().retained_storage_bytes();
    assert!(matches!(
        reader
            .step(&mut WorkBudget::new(body_size as u64), &cancel)
            .unwrap(),
        Step::Yield
    ));
    assert_eq!(reader.position(), fixture.raw.len() as u64);
    let header = reader.envelope.scan_prefix_header().unwrap();
    let retained = pool.used();

    // Cancel between accepting the prefix and charging the drain completion.
    let mut work = WorkBudget::new(1);
    assert_eq!(
        reader
            .envelope
            .resolve_scan_prefix(header, true, &mut work, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    );
    assert_eq!(work.spent(), 0);
    assert_eq!(pool.used(), retained - charge);
}
#[test]
fn scan_in_place_pending_best_cancel_reset_retained_alias() {
    let fixture = file(
        &entry(1, 1, &u(0x55ee, 1)),
        &[],
        &[
            cluster(0, group(false)),
            cluster(1, simple(1, 1, 0, 0x80, b"best")),
            cluster(2, simple(1, 1, 0, 0, &[b'x'; 256])),
        ],
    );
    let pool = Budget::new(256 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader = new_reader(&pool, &fixture.raw, Limits::default());
    let mut feed = Feed::new(&fixture.raw, 7);
    feed.hidden = true;
    feed.hold = true;
    open(&mut reader, &mut feed).unwrap();
    reader.seek_for_playback(0, 1_000_000).unwrap();
    let mut reached = false;
    for _ in 0..100_000 {
        let step = reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap();
        if let Step::NeedInput(r) = step {
            feed.supply(&mut reader, r).unwrap();
        }
        if reader
            .seek_stage
            .scan()
            .is_some_and(|s| s.best.is_some() && s.best_configuration.is_some())
            && reader.position() < fixture.raw.len() as u64
        {
            reached = true;
            break;
        }
    }
    assert!(reached);
    let configuration = reader.configuration.clone().unwrap();
    let alias = feed.alias.take().unwrap();
    let position = reader.position();
    let used = pool.used();
    let old_generation = reader.generation();
    let mut work = WorkBudget::new(0);
    assert!(matches!(
        reader.step(&mut work, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!(
        (reader.position(), work.spent(), pool.used()),
        (position, 0, used)
    );
    reader.reset().unwrap();
    assert_ne!(reader.generation(), old_generation);
    drop((reader, feed));
    assert!(pool.used() > baseline);
    println!(
        "SCAN_INPLACE_TRACE|cancel|{position}|{used}|{}|{}|{}",
        pool.used(),
        alias.retained_capacity(),
        configuration.revision()
    );
    drop((configuration, alias));
    assert_eq!(pool.used(), baseline);
}
#[test]
fn scan_in_place_layout_and_late_fault_priority() {
    assert!(size_of::<Reader>() <= 10048);
    assert!(size_of::<PacketBatchJob>() <= 3408);
    println!(
        "SCAN_INPLACE_LAYOUT|reader={}|batch={}|scan={}|stage_offset={}",
        size_of::<Reader>(),
        size_of::<PacketBatchJob>(),
        size_of::<ScanState>(),
        std::mem::offset_of!(Reader, seek_stage)
    );
    let fixture = repeated(3, 256);
    let end = fixture.raw.len() - 1;
    for chunk in [1, 64] {
        assert!(matches!(
            seek_error(&fixture, Some((end, true)), chunk),
            Error::Transport
        ));
        assert!(matches!(
            seek_error(&fixture, Some((end, false)), chunk),
            Error::Truncated
        ));
    }
}

#[test]
fn immutable_seek_index_rejects_foreign_owner_and_wrong_offset_views() {
    let fixture = repeated(2, 16);
    let (mut reader, mut feed) = immutable_fixture(&fixture.raw, Limits::default());
    let request = reader.request();
    let incorrect_views = [
        Bytes::copy_from_slice(reader.budget(), &fixture.raw[..1]).unwrap(),
        feed.immutable.as_ref().unwrap().slice(1..2).unwrap(),
    ];
    for bytes in incorrect_views {
        let window =
            InputWindow::new(request.source, request.token.clone(), request.offset, bytes).unwrap();
        assert!(matches!(
            reader.provide(&window),
            Err(Error::SourceMismatch)
        ));
    }
    open(&mut reader, &mut feed).unwrap();
    drain(&mut reader, &mut feed, 8192).unwrap();
    assert!(reader.seek_index.as_ref().unwrap().is_complete());
    reader.reset().unwrap();
    assert!(!reader.seek_index.as_ref().unwrap().is_complete());
}
