use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error, InputCursor, SourceCapabilities, SourceId, Step, WorkBudget};
use triergon_isobmff::{Access, Sample, SampleReader};

#[test]
fn reserved_packet_consumes_inline_body_under_complete_pool_pressure() {
    let budget = Budget::new(4096).unwrap();
    let baseline = budget.used();
    let source = Sample {
        ordinal: 7,
        description_index: 3,
        range: 64..70,
        dts_ticks: Some(-2),
        cts_ticks: Some(1),
        duration_ticks: 3,
        composition_offset: None,
        access: Access::ContainerSync,
        partial_sync: false,
        dependency: None,
        fragment_flags: None,
    };
    let body = [11, 22, 33, 44, 55, 66];
    let mut input = InputCursor::new(
        &budget,
        SourceId(420),
        SourceCapabilities {
            length: Some(70),
            random_access: true,
        },
        1,
    )
    .unwrap();
    let generation = input.generation();
    let before_reserve = budget.used();
    let mut reader = SampleReader::new(&mut input, source.clone(), 64..70, 6).unwrap();
    assert_eq!(budget.used(), before_reserve + body.len());
    assert_eq!(input.position(), 64);
    assert_eq!(input.generation(), generation);
    let token = input.token();
    let blocker = budget.reserve(budget.available()).unwrap();
    let held = budget.used();
    assert_eq!(held, budget.limit());
    let cancel = AtomicBool::new(false);
    for (at, value) in body.iter().enumerate() {
        let request = input.request();
        input
            .provide_inline(
                request.source,
                request.token,
                request.offset,
                std::slice::from_ref(value),
            )
            .unwrap();
        if at + 1 == body.len() {
            input.finish(input.source(), input.token()).unwrap();
        }
        let mut work = WorkBudget::new(1);
        let result = reader.poll(&mut input, &mut work, &cancel);
        assert!(
            result.is_ok(),
            "reserved packet must advance without temporary Bytes; result={result:?} offset={} spent={}",
            input.position(),
            work.spent()
        );
        let end = 65 + u64::try_from(at).unwrap();
        if at + 1 == body.len() {
            assert!(matches!(result.unwrap(), Step::Yield));
        } else {
            assert!(matches!(result.unwrap(), Step::NeedInput(request) if request.offset == end));
        }
        assert_eq!(work.spent(), 1);
        assert_eq!(input.position(), end);
        assert_eq!(input.token(), token);
        assert_eq!(input.source(), SourceId(420));
        assert!(input.retained_window().is_none());
        assert_eq!(budget.used(), held);
    }
    // Only the immutable owner may wait after the complete reserved body.
    assert!(matches!(
        reader.poll(&mut input, &mut WorkBudget::new(1), &cancel),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(input.position(), 70);
    assert_eq!(input.token(), token);
    drop(blocker);
    let Step::Ready(packet) = reader
        .poll(&mut input, &mut WorkBudget::new(1), &cancel)
        .unwrap()
    else {
        panic!("completed packet must freeze after pressure releases");
    };
    assert_eq!(packet.sample, source);
    assert_eq!(packet.payload.as_ref(), body);
    assert_eq!(packet.source, SourceId(420));
    assert_eq!(packet.token, token);
    assert!(
        matches!(input.read_into(&mut [0], &mut WorkBudget::new(0), &cancel).unwrap(), Step::End(end) if end.offset == 70 && end.source == SourceId(420))
    );
    let retained = packet.payload.clone();
    drop(packet);
    drop(reader);
    drop(input);
    assert!(budget.used() > baseline);
    assert_eq!(retained.as_ref(), body);
    drop(retained);
    assert_eq!(budget.used(), baseline);
}
