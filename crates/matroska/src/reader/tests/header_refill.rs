//! Header refill is canonical and disabled while resolving, opening, seeking or
//! scanning.
use crate::reader::*;
use crate::reader_kit::source_reader;
use crate::seek_scan_kit::{Feed, open, repeated};
use crate::storage::PayloadOwnership;

fn reader() -> Reader {
    let pool = Budget::new(128 * 1024).unwrap();
    source_reader(
        &pool,
        None,
        Limits::default(),
        8192,
        PayloadOwnership::OwnedExact,
    )
}

#[test]
fn header_refill_is_canonical_and_disabled_for_resolver_opening_seek_and_scan() {
    let mut reader = reader();
    let request = match reader
        .step(&mut WorkBudget::new(64), &AtomicBool::new(false))
        .unwrap()
    {
        Step::NeedInput(request) => request,
        other => panic!("initial header: {other:?}"),
    };
    assert_eq!(request.maximum, 4096);
    assert!(
        reader
            .header_refill_for_request(&request)
            .unwrap()
            .is_some()
    );
    let exact = reader.stream.envelope.exact_header_request().unwrap();
    assert_eq!(exact.maximum, 1);
    let disabled = |r: &Reader| {
        assert!(r.ordinary_header_refill().unwrap().is_none());
        let mut hint = exact.clone();
        r.widen_input_request(&mut hint).unwrap();
        assert_eq!(hint.maximum, exact.maximum);
        assert_eq!(hint.token, exact.token);
        assert_eq!(hint.offset, exact.offset);
    };
    // A first Cluster from a second reader: these checks read only the controller.
    let fixture = repeated(1, 16);
    let mut opened = self::reader();
    open(&mut opened, &mut Feed::new(&fixture.raw, 4096)).unwrap();
    let first = opened.first_cluster().unwrap().clone();
    let request_seek = SeekRequest {
        track: 0,
        requested_ns: 0,
        playback_preroll: false,
    };
    let pool = reader.stream.pool.clone();
    for activity in [
        Activity::Opening(Opening::new()),
        Activity::Metadata(Resolver::new(&pool)),
        Activity::Seek(Seek::new(request_seek, RecoveryAudio::Overlap, &pool)),
    ] {
        reader.control = Control::Clusters {
            first: first.clone(),
            activity,
        };
        disabled(&reader);
    }
    reader.control = Control::Clusters {
        first,
        activity: Activity::Blocks,
    };
    reader.stream.scan_remaining = Some(100);
    disabled(&reader);
    reader.stream.scan_remaining = None;
    reader.control = Control::Head;
    assert!(
        reader
            .header_refill_for_request(&request)
            .unwrap()
            .is_some()
    );
    let mut wrong = request.clone();
    wrong.maximum += 1;
    assert!(matches!(
        reader.header_refill_for_request(&wrong),
        Err(Error::Invalid("canonical header supply identity changed"))
    ));
    assert_eq!(reader.position(), 0);
}
