mod common;
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Error, InputCursor, InputWindow, SourceCapabilities, SourceFailure, SourceId, Step,
    WorkBudget,
};
use triergon_isobmff::{BoxEvent, BoxReader};

#[test]
fn one_byte_windows_and_internal_ranges_keep_the_logical_generation() {
    let fixture = common::fixtures::CASES
        .iter()
        .find(|c| c.name == "r-end-co64")
        .unwrap();
    let budget = Budget::new(1024 * 1024).unwrap();
    let mut input = InputCursor::new(
        &budget,
        SourceId(7),
        SourceCapabilities {
            length: Some(fixture.bytes.len() as u64),
            random_access: true,
        },
        16,
    )
    .unwrap();
    let generation = input.generation();
    let mut reader = BoxReader::new(&input, 64 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut metadata = Vec::new();
    let mut skipped = 0;
    let mut finished = false;
    for _ in 0..100_000 {
        match reader
            .poll(&mut input, &mut WorkBudget::new(3), &cancel)
            .unwrap()
        {
            Step::NeedInput(request) => {
                if request.offset == fixture.bytes.len() as u64 {
                    assert!(!finished);
                    input.finish(request.source, request.token).unwrap();
                    finished = true;
                } else {
                    let at = request.offset as usize;
                    let bytes = common::bytes(&budget, &fixture.bytes[at..at + 1]);
                    input
                        .provide(
                            &InputWindow::new(request.source, request.token, request.offset, bytes)
                                .unwrap(),
                        )
                        .unwrap();
                }
            }
            Step::Ready(BoxEvent::Header(header)) => {
                if header.kind == *b"mdat" {
                    reader.skip(&mut input).unwrap();
                } else {
                    reader.read_metadata(&input).unwrap();
                }
            }
            Step::Ready(BoxEvent::Metadata(raw)) => metadata.push(raw),
            Step::Ready(BoxEvent::Skipped(header)) => {
                assert_eq!(header.kind, *b"mdat");
                skipped += 1;
            }
            Step::Yield => {}
            Step::End(end) => {
                assert_eq!(end.offset, fixture.bytes.len() as u64);
                break;
            }
        }
    }
    assert!(finished);
    assert_eq!(input.generation(), generation);
    assert_eq!(skipped, 1);
    assert!(metadata.iter().any(|b| b.header().kind == *b"moov"));
    assert_eq!(reader.source(), SourceId(7));
}

#[test]
fn a_transport_failure_near_known_end_does_not_become_clean_eof() {
    let budget = Budget::new(4096).unwrap();
    let mut input = InputCursor::new(
        &budget,
        SourceId(8),
        SourceCapabilities {
            length: Some(8),
            random_access: true,
        },
        8,
    )
    .unwrap();
    let mut reader = BoxReader::new(&input, 1024).unwrap();
    let request = input.request();
    input
        .fail(request.source, request.token, SourceFailure::Transport)
        .unwrap();
    assert!(matches!(
        reader.poll(
            &mut input,
            &mut WorkBudget::new(32),
            &AtomicBool::new(false)
        ),
        Err(Error::Transport)
    ));
}

#[test]
fn cursor_reset_requires_an_explicit_format_reader_reset() {
    let budget = Budget::new(4096).unwrap();
    let mut input = InputCursor::new(
        &budget,
        SourceId(8),
        SourceCapabilities {
            length: Some(8),
            random_access: true,
        },
        8,
    )
    .unwrap();
    let mut reader = BoxReader::new(&input, 1024).unwrap();
    input.reset(0).unwrap();
    assert!(
        reader
            .poll(
                &mut input,
                &mut WorkBudget::new(32),
                &AtomicBool::new(false)
            )
            .is_err()
    );
}

#[test]
fn unknown_terminal_mdat_requires_explicit_source_completion() {
    let budget = Budget::new(4096).unwrap();
    let mut input = InputCursor::new(
        &budget,
        SourceId(9),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        16,
    )
    .unwrap();
    let mut reader = BoxReader::new(&input, 1024).unwrap();
    let bytes = common::bytes(&budget, &[0, 0, 0, 0, b'm', b'd', b'a', b't', 1, 2, 3]);
    let request = input.request();
    input
        .provide(&InputWindow::new(request.source, request.token, 0, bytes).unwrap())
        .unwrap();
    let cancel = AtomicBool::new(false);
    let Step::Ready(BoxEvent::Header(header)) = reader
        .poll(&mut input, &mut WorkBudget::new(32), &cancel)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(header.size, None);
    reader.skip(&mut input).unwrap();
    assert!(matches!(
        reader
            .poll(&mut input, &mut WorkBudget::new(32), &cancel)
            .unwrap(),
        Step::NeedInput(_)
    ));
    input.finish(input.source(), input.token()).unwrap();
    let Step::Ready(BoxEvent::Skipped(resolved)) = reader
        .poll(&mut input, &mut WorkBudget::new(32), &cancel)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(resolved.size, Some(11));
    assert!(matches!(
        reader
            .poll(&mut input, &mut WorkBudget::new(32), &cancel)
            .unwrap(),
        Step::End(_)
    ));
}

#[test]
fn freeze_backpressure_keeps_the_completed_metadata_for_exact_retry() {
    let budget = Budget::new(4096).unwrap();
    let mut input = InputCursor::new(
        &budget,
        SourceId(20),
        SourceCapabilities {
            length: Some(12),
            random_access: false,
        },
        16,
    )
    .unwrap();
    let mut reader = BoxReader::new(&input, 1024).unwrap();
    {
        let request = input.request();
        let bytes = common::bytes(&budget, &[0, 0, 0, 12, b'f', b'r', b'e', b'e', 1, 2, 3, 4]);
        input
            .provide(&InputWindow::new(request.source, request.token, 0, bytes).unwrap())
            .unwrap();
    }
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        reader
            .poll(&mut input, &mut WorkBudget::new(9), &cancel)
            .unwrap(),
        Step::Ready(BoxEvent::Header(_))
    ));
    reader.read_metadata(&input).unwrap();
    assert!(matches!(
        reader
            .poll(&mut input, &mut WorkBudget::new(4), &cancel)
            .unwrap(),
        Step::Yield
    ));
    let retained = budget.reserve(budget.available()).unwrap();
    assert!(matches!(
        reader.poll(&mut input, &mut WorkBudget::new(1), &cancel),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(input.position(), 12);
    drop(retained);
    let Step::Ready(BoxEvent::Metadata(raw)) = reader
        .poll(&mut input, &mut WorkBudget::new(1), &cancel)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(raw.payload(), &[1, 2, 3, 4]);
    assert!(matches!(
        reader
            .poll(&mut input, &mut WorkBudget::new(1), &cancel)
            .unwrap(),
        Step::NeedInput(_)
    ));
    input.finish(input.source(), input.token()).unwrap();
    assert!(matches!(
        reader
            .poll(&mut input, &mut WorkBudget::new(1), &cancel)
            .unwrap(),
        Step::End(_)
    ));
}

#[test]
fn stale_range_reply_after_mdat_skip_is_rejected_without_losing_reader_state() {
    let budget = Budget::new(4096).unwrap();
    let mut input = InputCursor::new(
        &budget,
        SourceId(21),
        SourceCapabilities {
            length: Some(24),
            random_access: true,
        },
        16,
    )
    .unwrap();
    let mut reader = BoxReader::new(&input, 1024).unwrap();
    let old = input.request();
    input
        .provide(
            &InputWindow::new(
                old.source,
                old.token.clone(),
                0,
                common::bytes(&budget, &[0, 0, 0, 16, b'm', b'd', b'a', b't']),
            )
            .unwrap(),
        )
        .unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        reader
            .poll(&mut input, &mut WorkBudget::new(16), &cancel)
            .unwrap(),
        Step::Ready(BoxEvent::Header(_))
    ));
    let generation = input.generation();
    reader.skip(&mut input).unwrap();
    let stale = InputWindow::new(
        old.source,
        old.token,
        16,
        common::bytes(&budget, &[0, 0, 0, 8, b'f', b'r', b'e', b'e']),
    )
    .unwrap();
    assert!(matches!(
        input.provide(&stale),
        Err(Error::StaleRequest { .. })
    ));
    assert_eq!(input.position(), 16);
    assert_eq!(input.generation(), generation);
    assert!(matches!(
        reader
            .poll(&mut input, &mut WorkBudget::new(1), &cancel)
            .unwrap(),
        Step::Ready(BoxEvent::Skipped(_))
    ));
    let Step::NeedInput(request) = reader
        .poll(&mut input, &mut WorkBudget::new(16), &cancel)
        .unwrap()
    else {
        panic!()
    };
    input
        .provide(
            &InputWindow::new(
                request.source,
                request.token,
                request.offset,
                common::bytes(&budget, &[0, 0, 0, 8, b'f', b'r', b'e', b'e']),
            )
            .unwrap(),
        )
        .unwrap();
    let Step::Ready(BoxEvent::Header(header)) = reader
        .poll(&mut input, &mut WorkBudget::new(16), &cancel)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(header.kind, *b"free");
}

#[test]
fn starvation_then_explicit_end_inside_header_is_truncation() {
    let budget = Budget::new(4096).unwrap();
    let mut input = InputCursor::new(
        &budget,
        SourceId(22),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        8,
    )
    .unwrap();
    let mut reader = BoxReader::new(&input, 1024).unwrap();
    let request = input.request();
    input
        .provide(
            &InputWindow::new(
                request.source,
                request.token,
                0,
                common::bytes(&budget, &[0, 0, 0, 8]),
            )
            .unwrap(),
        )
        .unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        reader
            .poll(&mut input, &mut WorkBudget::new(16), &cancel)
            .unwrap(),
        Step::NeedInput(_)
    ));
    input.finish(input.source(), input.token()).unwrap();
    assert!(matches!(
        reader.poll(&mut input, &mut WorkBudget::new(16), &cancel),
        Err(Error::Truncated)
    ));
}

#[test]
fn cancellation_during_metadata_stays_cancelled_after_flag_clear() {
    use std::sync::atomic::Ordering;
    let budget = Budget::new(4096).unwrap();
    let mut input = InputCursor::new(
        &budget,
        SourceId(23),
        SourceCapabilities {
            length: Some(12),
            random_access: false,
        },
        16,
    )
    .unwrap();
    let mut reader = BoxReader::new(&input, 1024).unwrap();
    let request = input.request();
    input
        .provide(
            &InputWindow::new(
                request.source,
                request.token,
                0,
                common::bytes(&budget, &[0, 0, 0, 12, b'f', b'r', b'e', b'e']),
            )
            .unwrap(),
        )
        .unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        reader
            .poll(&mut input, &mut WorkBudget::new(16), &cancel)
            .unwrap(),
        Step::Ready(BoxEvent::Header(_))
    ));
    reader.read_metadata(&input).unwrap();
    cancel.store(true, Ordering::Relaxed);
    assert!(matches!(
        reader.poll(&mut input, &mut WorkBudget::new(16), &cancel),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        reader.poll(&mut input, &mut WorkBudget::new(16), &cancel),
        Err(Error::Cancelled)
    ));
}

#[test]
fn source_continuation_keeps_logical_generation_and_old_metadata_bytes() {
    let budget = Budget::new(4096).unwrap();
    let mut input = InputCursor::new(
        &budget,
        SourceId(24),
        SourceCapabilities {
            length: Some(8),
            random_access: false,
        },
        8,
    )
    .unwrap();
    let generation = input.generation();
    let mut reader = BoxReader::new(&input, 1024).unwrap();
    let request = input.request();
    input
        .provide(
            &InputWindow::new(
                request.source,
                request.token,
                0,
                common::bytes(&budget, &[0, 0, 0, 8, b'f', b'r', b'e', b'e']),
            )
            .unwrap(),
        )
        .unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        reader
            .poll(&mut input, &mut WorkBudget::new(16), &cancel)
            .unwrap(),
        Step::Ready(BoxEvent::Header(_))
    ));
    reader.read_metadata(&input).unwrap();
    let Step::Ready(BoxEvent::Metadata(retained)) = reader
        .poll(&mut input, &mut WorkBudget::new(16), &cancel)
        .unwrap()
    else {
        panic!()
    };
    input.finish(input.source(), input.token()).unwrap();
    assert!(matches!(
        reader
            .poll(&mut input, &mut WorkBudget::new(16), &cancel)
            .unwrap(),
        Step::End(_)
    ));
    input
        .continue_source(
            SourceId(25),
            SourceCapabilities {
                length: Some(8),
                random_access: false,
            },
        )
        .unwrap();
    assert_eq!(input.generation(), generation);
    assert!(
        reader
            .poll(&mut input, &mut WorkBudget::new(16), &cancel)
            .is_err()
    );
    let mut next = BoxReader::new(&input, 1024).unwrap();
    assert!(matches!(
        next.poll(&mut input, &mut WorkBudget::new(16), &cancel)
            .unwrap(),
        Step::NeedInput(_)
    ));
    assert_eq!(next.source(), SourceId(25));
    assert_eq!(
        retained.bytes().as_ref(),
        &[0, 0, 0, 8, b'f', b'r', b'e', b'e']
    );
    assert!(retained.bytes().belongs_to(&budget));
}
