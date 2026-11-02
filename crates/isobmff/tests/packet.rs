mod common;
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Error, InputCursor, InputWindow, SourceCapabilities, SourceFailure, SourceId, Step,
    WorkBudget,
};
use triergon_isobmff::{Access, Sample, SampleReader};

fn sample(range: std::ops::Range<u64>) -> Sample {
    Sample {
        ordinal: 1,
        description_index: 1,
        range,
        dts_ticks: Some(-2),
        cts_ticks: Some(1),
        duration_ticks: 3,
        composition_offset: None,
        access: Access::ContainerSync,
        partial_sync: false,
        dependency: None,
        fragment_flags: None,
    }
}
fn input(budget: &Budget, length: Option<u64>) -> InputCursor {
    InputCursor::new(
        budget,
        SourceId(31),
        SourceCapabilities {
            length,
            random_access: true,
        },
        4,
    )
    .unwrap()
}

#[test]
fn packet_tiny_windows_preserve_source_and_retained_ownership() {
    let budget = Budget::new(4096).unwrap();
    let pool_baseline = budget.used();
    let mut input = input(&budget, Some(16));
    let generation = input.generation();
    let source = sample(8..12);
    let mut reader = SampleReader::new(&mut input, source.clone(), 8..16, 4).unwrap();
    let cancel = AtomicBool::new(false);
    let mut packet = None;
    for _ in 0..32 {
        match reader
            .poll(&mut input, &mut WorkBudget::new(1), &cancel)
            .unwrap()
        {
            Step::NeedInput(request) => {
                let bytes = common::bytes(&budget, &[request.offset as u8]);
                input
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
            }
            Step::Yield => {}
            Step::Ready(value) => {
                packet = Some(value);
                break;
            }
            Step::End(_) => panic!("sample completion is not logical EOF"),
        }
    }
    let packet = packet.unwrap();
    assert_eq!(packet.sample, source);
    assert_eq!(packet.payload.as_ref(), &[8, 9, 10, 11]);
    assert_eq!(packet.source, SourceId(31));
    assert_eq!(packet.token.generation(), generation);
    assert_eq!(input.generation(), generation);
    drop(reader);
    drop(input);
    let payload_usage = budget.used();
    assert!(payload_usage - pool_baseline >= packet.payload.retained_capacity());
    let retained = packet.payload.clone();
    assert!(retained.shares_storage_with(&packet.payload));
    assert_eq!(budget.used(), payload_usage);
    drop(packet);
    assert_eq!(budget.used(), payload_usage);
    assert_eq!(retained.as_ref(), &[8, 9, 10, 11]);
    drop(retained);
    assert_eq!(budget.used(), pool_baseline);
}

#[test]
fn packet_range_and_limit_rejection_leave_input_transaction_untouched() {
    let budget = Budget::new(4096).unwrap();
    let mut input = input(&budget, Some(16));
    let token = input.token();
    assert!(matches!(
        SampleReader::new(&mut input, sample(8..12), 9..16, 8),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        SampleReader::new(&mut input, sample(8..12), 8..16, 3),
        Err(Error::Limit { .. })
    ));
    assert_eq!(input.token(), token);
    assert_eq!(input.position(), 0);
}

#[test]
fn packet_does_not_reinterpret_transport_failure_as_eof() {
    let budget = Budget::new(4096).unwrap();
    let mut input = input(&budget, Some(16));
    let mut reader = SampleReader::new(&mut input, sample(8..12), 8..16, 4).unwrap();
    let request = input.request();
    input
        .fail(request.source, request.token, SourceFailure::Transport)
        .unwrap();
    assert!(matches!(
        reader.poll(&mut input, &mut WorkBudget::new(3), &AtomicBool::new(false)),
        Err(Error::Transport)
    ));
}

#[test]
fn packet_explicit_short_unknown_source_is_truncated() {
    let budget = Budget::new(4096).unwrap();
    let mut input = input(&budget, None);
    let mut reader = SampleReader::new(&mut input, sample(8..12), 8..16, 4).unwrap();
    input.finish(input.source(), input.token()).unwrap();
    assert!(matches!(
        reader.poll(&mut input, &mut WorkBudget::new(3), &AtomicBool::new(false)),
        Err(Error::Truncated)
    ));
}

#[test]
fn packet_rejects_external_range_transaction_and_zero_size_cancellation() {
    let budget = Budget::new(4096).unwrap();
    let mut input = input(&budget, Some(16));
    let mut reader = SampleReader::new(&mut input, sample(8..12), 8..16, 4).unwrap();
    input.request_range(8).unwrap();
    assert!(matches!(
        reader.poll(&mut input, &mut WorkBudget::new(3), &AtomicBool::new(false)),
        Err(Error::StaleRequest { .. })
    ));
    let mut reader = SampleReader::new(&mut input, sample(16..16), 8..16, 4).unwrap();
    assert!(matches!(
        reader.poll(&mut input, &mut WorkBudget::new(3), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
}

#[test]
fn completed_packet_survives_freeze_backpressure_without_repeating_io() {
    let budget = Budget::new(4096).unwrap();
    let mut input = input(&budget, Some(4));
    let mut reader = SampleReader::new(&mut input, sample(0..4), 0..4, 4).unwrap();
    let request = input.request();
    let bytes = common::bytes(&budget, &[11, 12, 13, 14]);
    input
        .provide(&InputWindow::new(request.source, request.token, 0, bytes).unwrap())
        .unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        reader
            .poll(&mut input, &mut WorkBudget::new(4), &cancel)
            .unwrap(),
        Step::Yield
    ));
    let token = input.token();
    let hold = budget.reserve(budget.available()).unwrap();
    assert!(matches!(
        reader.poll(&mut input, &mut WorkBudget::new(1), &cancel),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(input.position(), 4);
    assert_eq!(input.token(), token);
    drop(hold);
    let Step::Ready(packet) = reader
        .poll(&mut input, &mut WorkBudget::new(1), &cancel)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(packet.payload.as_ref(), &[11, 12, 13, 14]);
    assert_eq!(packet.token, token);
}
