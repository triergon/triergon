use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Error, InputCursor, SourceCapabilities, SourceFailure, SourceId, Step, WorkBudget,
};
use triergon_isobmff::{BoxEvent, BoxReader};
fn announced(pool: &Budget, size: u32, known: bool) -> (InputCursor, BoxReader) {
    let mut input = InputCursor::new(
        pool,
        SourceId(401),
        SourceCapabilities {
            length: known.then_some(u64::from(size)),
            random_access: false,
        },
        16,
    )
    .unwrap();
    let mut header = [0, 0, 0, 0, b'f', b'r', b'e', b'e'];
    header[..4].copy_from_slice(&size.to_be_bytes());
    let request = input.request();
    input
        .provide_inline(request.source, request.token, request.offset, &header)
        .unwrap();
    let mut reader = BoxReader::new(&input, usize::try_from(size).unwrap()).unwrap();
    assert!(matches!(
        reader
            .poll(&mut input, &mut WorkBudget::new(9), &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(BoxEvent::Header(_))
    ));
    reader.read_metadata(&input).unwrap();
    (input, reader)
}
#[test]
fn a_large_reserved_body_with_no_buffered_bytes_is_not_materialized_as_zero_metadata() {
    let size = 16 * 1024 * 1024;
    let pool = Budget::new(usize::try_from(size).unwrap() + 4096).unwrap();
    let baseline = pool.used();
    {
        let (mut input, mut reader) = announced(&pool, size, true);
        let held = pool.used();
        let token = input.token();
        for units in [0, 1, 3] {
            let mut work = WorkBudget::new(units);
            assert!(
                matches!(reader.poll(&mut input,&mut work,&AtomicBool::new(false)).unwrap(),Step::NeedInput(at) if at.offset == 8)
            );
            assert_eq!(work.spent(), 0);
            assert_eq!(input.position(), 8);
            assert_eq!(input.token(), token);
            assert_eq!(pool.used(), held);
        }
    }
    assert_eq!(pool.used(), baseline);
}
#[test]
fn metadata_transport_cancel_and_short_eof_keep_errors_and_do_not_publish_partial_bytes() {
    for failure in [SourceFailure::Transport, SourceFailure::Cancelled] {
        let pool = Budget::new(4096).unwrap();
        let (mut input, mut reader) = announced(&pool, 14, true);
        input.fail(input.source(), input.token(), failure).unwrap();
        let mut work = WorkBudget::new(0);
        let expected = match failure {
            SourceFailure::Transport => Error::Transport,
            SourceFailure::Cancelled => Error::Cancelled,
        };
        assert!(
            matches!(reader.poll(&mut input,&mut work,&AtomicBool::new(true)),Err(error) if error == expected)
        );
        assert_eq!(work.spent(), 0);
        assert_eq!(input.position(), 8);
    }
    let pool = Budget::new(4096).unwrap();
    let (mut input, mut reader) = announced(&pool, 14, false);
    input.finish(input.source(), input.token()).unwrap();
    let mut work = WorkBudget::new(0);
    assert!(matches!(
        reader.poll(&mut input, &mut work, &AtomicBool::new(false)),
        Err(Error::Truncated)
    ));
    assert_eq!(work.spent(), 0);
    assert_eq!(input.position(), 8);
    assert!(matches!(
        reader.poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Truncated)
    ));
}
