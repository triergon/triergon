use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error, InputCursor, SourceCapabilities, SourceId, Step, WorkBudget};
use triergon_isobmff::{BoxEvent, BoxReader};

#[test]
fn reserved_metadata_body_consumes_inline_bytes_under_complete_pool_pressure() {
    let budget = Budget::new(4096).unwrap();
    let baseline = budget.used();
    let header = [0, 0, 0, 14, b'f', b'r', b'e', b'e'];
    let body = [1, 2, 3, 4, 5, 6];
    let mut input = InputCursor::new(
        &budget,
        SourceId(400),
        SourceCapabilities {
            length: Some(14),
            random_access: false,
        },
        16,
    )
    .unwrap();
    let mut reader = BoxReader::new(&input, 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let request = input.request();
    input
        .provide_inline(request.source, request.token, request.offset, &header)
        .unwrap();
    assert!(matches!(
        reader
            .poll(&mut input, &mut WorkBudget::new(9), &cancel)
            .unwrap(),
        Step::Ready(BoxEvent::Header(_))
    ));
    assert_eq!(input.position(), 8);
    assert!(input.retained_window().is_none());
    let before_reserve = budget.used();
    reader.read_metadata(&input).unwrap();
    assert_eq!(budget.used(), before_reserve + 14);
    let token = input.token();
    let blocker = budget.reserve(budget.available()).unwrap();
    let held = budget.used();
    assert_eq!(held, budget.limit());
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
            "reserved body must advance without temporary Bytes; result={result:?} offset={} spent={}",
            input.position(),
            work.spent()
        );
        if at + 1 == body.len() {
            assert!(matches!(result.unwrap(), Step::Yield));
        } else {
            assert!(
                matches!(result.unwrap(), Step::NeedInput(request) if request.offset == 9 + u64::try_from(at).unwrap())
            );
        }
        assert_eq!(input.position(), 9 + u64::try_from(at).unwrap());
        assert_eq!(work.spent(), 1);
        assert_eq!(input.token(), token);
        assert_eq!(input.source(), SourceId(400));
        assert!(input.retained_window().is_none());
        assert_eq!(budget.used(), held);
    }
    // All body bytes are retained; only the immutable storage header may wait.
    assert!(matches!(
        reader.poll(&mut input, &mut WorkBudget::new(1), &cancel),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(input.position(), 14);
    assert_eq!(budget.used(), held);
    drop(blocker);
    let Step::Ready(BoxEvent::Metadata(raw)) = reader
        .poll(&mut input, &mut WorkBudget::new(1), &cancel)
        .unwrap()
    else {
        panic!("completed metadata must freeze after the blocker releases");
    };
    assert_eq!(raw.payload(), body);
    assert_eq!(&raw.bytes().as_ref()[..8], &header);
    assert_eq!(input.token(), token);
    assert!(
        matches!(reader.poll(&mut input, &mut WorkBudget::new(0), &cancel).unwrap(), Step::End(end) if end.offset == 14 && end.source == SourceId(400))
    );
    let retained = raw.bytes().clone();
    drop(raw);
    drop(reader);
    drop(input);
    assert!(budget.used() > baseline);
    assert_eq!(&retained.as_ref()[8..], &body);
    drop(retained);
    assert_eq!(budget.used(), baseline);
}
