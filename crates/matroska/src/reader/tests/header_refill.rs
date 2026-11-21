//! Header refill is canonical and disabled while resolving, opening, seeking or
//! scanning.
use crate::reader::seeking::ScanState;
use crate::reader::*;
use crate::seek::plan::{LandingPlan, ScanPlan, SeekPlan};

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
    let policy = ScanPlan {
        start: 0,
        stop: None,
        target_ns: 0,
        allow_first_after_target: false,
        allow_first_after_negative_preroll: false,
        replay_whole_cluster: false,
    };
    let plan = SeekPlan {
        requested_ns: 0,
        discard_until_ns: 0,
        preroll_ns: 0,
        subtitle_recovery_delay_ns: 0,
        landing: LandingPlan::Scan(policy),
    };
    let receipt = SeekReceipt {
        requested_ns: 0,
        actual_ns: 0,
        scanned_bytes: 0,
        used_index: false,
        discard_until_ns: 0,
        preroll_ns: 0,
        replays_from_start: false,
    };
    for stage in [
        SeekStage::Plan(None),
        SeekStage::Cue(plan),
        SeekStage::Scan(ScanState::new(policy, plan, 0, false)),
        SeekStage::Epoch(receipt),
        SeekStage::InitialConfiguration(receipt),
        SeekStage::Receipt(receipt),
    ] {
        reader.seek_stage = stage;
        disabled(&reader);
    }
    reader.seek_stage = SeekStage::Idle;
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
