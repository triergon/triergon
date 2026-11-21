//! Header refill is canonical and disabled while resolving, opening, seeking or
//! scanning.
use crate::reader::*;

fn reader() -> Reader {
    let pool = Budget::new(128 * 1024).unwrap();
    Reader::new(
        &pool,
        SourceId(2103),
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        Limits::default(),
        8192,
    )
    .unwrap()
}

#[test]
fn header_refill_is_canonical_and_disabled_for_resolver_opening_seek_and_scan() {
    let mut reader = reader();
    let request = match reader
        .step_parse(&mut WorkBudget::new(64), &AtomicBool::new(false))
        .unwrap()
    {
        ParseStep::NeedInput(request) => request,
        other => panic!("initial header: {other:?}"),
    };
    assert_eq!(request.maximum, 4096);
    assert!(
        reader
            .header_refill_for_request(&request)
            .unwrap()
            .is_some()
    );
    let exact = reader.envelope.exact_header_request().unwrap();
    assert_eq!(exact.maximum, 1);
    let disabled = |r: &Reader| {
        assert!(r.ordinary_header_refill().unwrap().is_none());
        let ParseStep::NeedInput(hint) = r.canonical_input_hint(exact.clone()).unwrap() else {
            panic!("disabled policy must retain NeedInput");
        };
        assert_eq!(hint.maximum, exact.maximum);
        assert_eq!(hint.token, exact.token);
        assert_eq!(hint.offset, exact.offset);
    };
    reader.opening = Some(Opening::new());
    disabled(&reader);
    reader.opening = None;
    reader.resolver = Some(Resolver::new(ResolvePurpose::Opening, &reader.pool));
    disabled(&reader);
    reader.resolver = None;
    for mode in [
        SeekMode::Plan,
        SeekMode::Cue,
        SeekMode::Scan,
        SeekMode::Epoch,
        SeekMode::InitialConfiguration,
        SeekMode::Receipt,
    ] {
        reader.seek_mode = mode;
        disabled(&reader);
    }
    reader.seek_mode = SeekMode::None;
    reader.scan_remaining = Some(100);
    disabled(&reader);
    reader.scan_remaining = None;
    reader.pending_seek = Some(SeekRequest {
        track: 0,
        requested_ns: 0,
        playback_preroll: false,
    });
    disabled(&reader);
    reader.pending_seek = None;
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
