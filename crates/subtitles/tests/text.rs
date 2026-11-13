#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, SourceId, SourceLocation, Time, WorkBudget};
use triergon_subtitles::text::{
    Addition, Cue, Encoding, Extractor, Field, FieldOwner, Format, Input, Limits, RawClock, Status,
    Timing,
};

#[derive(Clone, Copy)]
struct ExpectedField {
    owner: FieldOwner,
    range: (usize, usize),
}

type ExpectedModifier = ([u8; 4], (usize, usize));

struct ExpectedEvent {
    raw: (usize, usize),
    text: (usize, usize),
    decoded: Option<&'static [u8]>,
    encoding: Encoding,
    identifier: Option<ExpectedField>,
    settings: Option<ExpectedField>,
    comments: Option<ExpectedField>,
    start: Option<(i128, u128)>,
    end: Option<(i128, u128)>,
    duration: Option<(i128, u128)>,
    modifiers: &'static [ExpectedModifier],
}

struct Case {
    id: &'static str,
    format: Format,
    encoding: Encoding,
    payload: &'static [u8],
    codec_id: Option<&'static [u8]>,
    codec_private: Option<&'static [u8]>,
    charset_label: Option<&'static [u8]>,
    addition: Option<(u64, &'static [u8])>,
    start: Option<(i128, u128)>,
    duration: Option<(i128, u128)>,
    clock: Option<RawClock>,
    input_limit: Option<usize>,
    error: Option<&'static str>,
    events: &'static [ExpectedEvent],
}

/// What a fixture row leaves unstated: nothing supplied, nothing expected.
const EMPTY: Case = Case {
    id: "",
    format: Format::Utf8Packet,
    encoding: Encoding::Utf8,
    payload: &[],
    codec_id: None,
    codec_private: None,
    charset_label: None,
    addition: None,
    start: None,
    duration: None,
    clock: None,
    input_limit: None,
    error: None,
    events: &[],
};

/// What an expected event leaves unstated.
const EMPTY_EVENT: ExpectedEvent = ExpectedEvent {
    raw: (0, 0),
    text: (0, 0),
    decoded: None,
    encoding: Encoding::Utf8,
    identifier: None,
    settings: None,
    comments: None,
    start: None,
    end: None,
    duration: None,
    modifiers: &[],
};

#[path = "text-fixtures/fixtures/cases.rs"]
mod cases;

fn exact(value: Option<(i128, u128)>) -> Option<Time> {
    value.map(|(numerator, denominator)| Time::new(numerator, denominator).unwrap())
}

fn owned(pool: &Budget, value: Option<&[u8]>) -> Option<Bytes> {
    value.map(|value| Bytes::copy_from_slice(pool, value).unwrap())
}

fn input_for(pool: &Budget, case: &Case) -> Input {
    Input {
        format: case.format,
        encoding: case.encoding,
        payload: Bytes::copy_from_slice(pool, case.payload).unwrap(),
        codec_id: owned(pool, case.codec_id),
        codec_private: owned(pool, case.codec_private),
        charset_label: owned(pool, case.charset_label),
        addition: case.addition.map(|(id, value)| Addition {
            id,
            bytes: Bytes::copy_from_slice(pool, value).unwrap(),
        }),
        timing: case.clock.map_or_else(
            || Timing::exact(exact(case.start), exact(case.duration)),
            |clock| Timing::from_ticks(clock).unwrap(),
        ),
        source_id: SourceId(17),
        generation: 23,
        sequence: 41,
        configuration_epoch: 5,
        container_position: Some(SourceLocation {
            source: SourceId(17),
            offset: 1234,
        }),
    }
}

fn same_optional(actual: Option<&Bytes>, expected: Option<&Bytes>) {
    match (actual, expected) {
        (None, None) => {}
        (Some(actual), Some(expected)) => {
            assert_eq!(actual.as_ref(), expected.as_ref());
            assert!(actual.shares_storage_with(expected));
        }
        _ => panic!("absent and present-empty source facts differ"),
    }
}

fn same_input(actual: &Input, expected: &Input) {
    assert_eq!(actual.format, expected.format);
    assert_eq!(actual.encoding, expected.encoding);
    assert_eq!(actual.payload.as_ref(), expected.payload.as_ref());
    assert!(actual.payload.shares_storage_with(&expected.payload));
    same_optional(actual.codec_id.as_ref(), expected.codec_id.as_ref());
    same_optional(
        actual.codec_private.as_ref(),
        expected.codec_private.as_ref(),
    );
    same_optional(
        actual.charset_label.as_ref(),
        expected.charset_label.as_ref(),
    );
    match (&actual.addition, &expected.addition) {
        (None, None) => {}
        (Some(actual), Some(expected)) => {
            assert_eq!(actual.id, expected.id);
            same_optional(Some(&actual.bytes), Some(&expected.bytes));
        }
        _ => panic!("lost raw BlockAdditional presence"),
    }
    assert_eq!(actual.timing, expected.timing);
    assert_eq!(actual.source_id, expected.source_id);
    assert_eq!(actual.generation, expected.generation);
    assert_eq!(actual.sequence, expected.sequence);
    assert_eq!(actual.configuration_epoch, expected.configuration_epoch);
    assert_eq!(actual.container_position, expected.container_position);
}

fn field(actual: Option<&Field>, expected: Option<ExpectedField>, input: &Input) {
    match (actual, expected) {
        (None, None) => {}
        (Some(actual), Some(expected)) => {
            assert_eq!(actual.owner, expected.owner);
            assert_eq!(actual.range, expected.range.0..expected.range.1);
            let view = actual.view(input).unwrap();
            let source = match expected.owner {
                FieldOwner::Payload => &input.payload,
                FieldOwner::Addition => &input.addition.as_ref().unwrap().bytes,
                FieldOwner::CodecPrivate => input.codec_private.as_ref().unwrap(),
            };
            assert_eq!(
                view.as_ref(),
                &source.as_ref()[expected.range.0..expected.range.1]
            );
            assert!(view.shares_storage_with(source));
        }
        _ => panic!("text field presence differs"),
    }
}

fn check_event(cue: &Cue, expected: &ExpectedEvent, original: &Input) {
    same_input(&cue.input, original);
    assert_eq!(cue.raw_range, expected.raw.0..expected.raw.1);
    assert_eq!(cue.text_range, expected.text.0..expected.text.1);
    assert!(cue.raw_range.start <= cue.text_range.start);
    assert!(cue.text_range.end <= cue.raw_range.end);
    assert!(cue.raw_range.end <= original.payload.len());
    assert_eq!(
        cue.decoded_utf8.as_ref().map(AsRef::as_ref),
        expected.decoded
    );
    assert_eq!(cue.effective_encoding, expected.encoding);
    assert_eq!(cue.start, exact(expected.start));
    assert_eq!(cue.end, exact(expected.end));
    assert_eq!(cue.duration, exact(expected.duration));
    field(cue.identifier.as_ref(), expected.identifier, &cue.input);
    field(cue.settings.as_ref(), expected.settings, &cue.input);
    field(cue.comments.as_ref(), expected.comments, &cue.input);
    assert_eq!(cue.modifiers.len(), expected.modifiers.len());
    for (actual, (kind, range)) in cue.modifiers.as_slice().iter().zip(expected.modifiers) {
        assert_eq!(&actual.kind, kind);
        assert_eq!(actual.range, range.0..range.1);
        assert!(actual.range.end <= original.payload.len());
        assert_eq!(
            &original.payload.as_ref()[actual.range.start + 4..actual.range.start + 8],
            kind
        );
    }
}

fn check_error(error: Error, expected: Option<&str>, id: &str) {
    let Some(expected) = expected else {
        panic!("{id}: unexpected extraction failure: {error:?}")
    };
    let agrees = match (expected, error) {
        ("Invalid", Error::Invalid(_))
        | ("Truncated", Error::Truncated)
        | ("Limit", Error::Limit { .. }) => true,
        (expected, Error::Unsupported(actual)) => {
            expected.strip_prefix("Unsupported:") == Some(actual)
        }
        _ => false,
    };
    assert!(agrees, "{id}: expected {expected}, observed {error:?}");
}

fn run_case(index: usize) {
    let case = &cases::ALL[index];
    for units in [1, 7, 4096] {
        let pool = Budget::new(64 * 1024 * 1024).unwrap();
        let baseline = pool.used();
        {
            let input = input_for(&pool, case);
            let original = input.clone();
            let limits = Limits {
                max_input_bytes: case
                    .input_limit
                    .unwrap_or(Limits::default().max_input_bytes),
                ..Limits::default()
            };
            let mut parser = Extractor::new(&pool, limits, input.generation);
            if let Err(rejected) = parser.provide(input) {
                check_error(rejected.error, case.error, case.id);
                same_input(&rejected.input, &original);
                assert!(case.events.is_empty());
            } else {
                parser.finish_input();
                let mut output = Vec::new();
                let mut failed = false;
                let bound = (case.payload.len() + 1024 * 1024) * 8 + 256;
                let mut finished = false;
                for _ in 0..bound {
                    let mut work = WorkBudget::new(units);
                    let status = parser.step(&mut work, &AtomicBool::new(false));
                    assert!(work.spent() <= units);
                    match status {
                        Ok(Status::Yield) => assert!(work.spent() != 0),
                        Ok(Status::Cue(cue)) => {
                            let expected = case
                                .events
                                .get(output.len())
                                .unwrap_or_else(|| panic!("{}: unexpected prefix event", case.id));
                            check_event(&cue, expected, &original);
                            if let Some(decoded) = &cue.decoded_utf8 {
                                assert!(decoded.belongs_to(&pool));
                            }
                            output.push(cue);
                        }
                        Ok(Status::Finished) => {
                            finished = true;
                            break;
                        }
                        Ok(other) => panic!("{}: unexpected {other:?}", case.id),
                        Err(error) => {
                            check_error(error, case.error, case.id);
                            failed = true;
                            break;
                        }
                    }
                }
                assert!(
                    failed || finished,
                    "{}: finite source made no terminal progress",
                    case.id
                );
                assert_eq!(failed, case.error.is_some(), "{}", case.id);
                assert_eq!(output.len(), case.events.len(), "{}", case.id);
            }
        }
        assert_eq!(pool.used(), baseline, "{} retained allocation", case.id);
    }
}

macro_rules! fixture_test {
    ($name:ident, $index:expr) => {
        #[test]
        fn $name() {
            run_case($index);
        }
    };
}
include!("text-fixtures/fixtures/test_names.rs");

fn opaque(pool: &Budget, bytes: &[u8], generation: u64) -> Input {
    Input {
        format: Format::OpaqueAss,
        encoding: Encoding::Opaque,
        payload: Bytes::copy_from_slice(pool, bytes).unwrap(),
        codec_id: None,
        codec_private: None,
        charset_label: None,
        addition: None,
        timing: Timing::unknown(),
        source_id: SourceId(19),
        generation,
        sequence: 0,
        configuration_epoch: 0,
        container_position: None,
    }
}

fn next_opaque(parser: &mut Extractor) -> Cue {
    let mut work = WorkBudget::new(4096);
    match parser.step(&mut work, &AtomicBool::new(false)).unwrap() {
        Status::Cue(cue) => cue,
        other => panic!("expected opaque event, got {other:?}"),
    }
}

#[test]
fn idle_requests_input_without_work() {
    let pool = Budget::new(4096).unwrap();
    let mut parser = Extractor::new(&pool, Limits::default(), 1);
    let mut work = WorkBudget::new(0);
    assert!(matches!(
        parser.step(&mut work, &AtomicBool::new(false)).unwrap(),
        Status::NeedInput
    ));
    assert_eq!(work.spent(), 0);
}

#[test]
fn explicit_finish_is_distinct_from_empty_packet() {
    let pool = Budget::new(4096).unwrap();
    let mut parser = Extractor::new(&pool, Limits::default(), 1);
    parser.provide(opaque(&pool, b"", 1)).unwrap();
    parser.finish_input();
    assert!(next_opaque(&mut parser).input.payload.is_empty());
    assert!(matches!(
        parser
            .step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        Status::Finished
    ));
}

#[test]
fn zero_work_yields_without_advancing_pending_source() {
    let pool = Budget::new(4096).unwrap();
    let mut parser = Extractor::new(&pool, Limits::default(), 1);
    parser.provide(opaque(&pool, b"unchanged", 1)).unwrap();
    let before = pool.used();
    let mut work = WorkBudget::new(0);
    assert!(matches!(
        parser.step(&mut work, &AtomicBool::new(false)).unwrap(),
        Status::Yield
    ));
    assert_eq!(work.spent(), 0);
    assert_eq!(pool.used(), before);
    assert_eq!(
        next_opaque(&mut parser).input.payload.as_ref(),
        b"unchanged"
    );
}

#[test]
fn cancellation_before_work_preserves_source_and_clock() {
    let pool = Budget::new(4096).unwrap();
    let mut parser = Extractor::new(&pool, Limits::default(), 1);
    let input = opaque(&pool, b"cancel", 1);
    let original = input.clone();
    parser.provide(input).unwrap();
    let mut work = WorkBudget::new(9);
    assert_eq!(
        parser.step(&mut work, &AtomicBool::new(true)).unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(work.spent(), 0);
    same_input(&next_opaque(&mut parser).input, &original);
}

#[test]
fn busy_input_rejection_returns_exact_owner() {
    let pool = Budget::new(4096).unwrap();
    let mut parser = Extractor::new(&pool, Limits::default(), 1);
    parser.provide(opaque(&pool, b"first", 1)).unwrap();
    let next = opaque(&pool, b"second", 1);
    let original = next.clone();
    let rejection = parser.provide(next).unwrap_err();
    assert_eq!(
        rejection.error,
        Error::Invalid("text input already pending")
    );
    same_input(&rejection.input, &original);
    assert_eq!(next_opaque(&mut parser).input.payload.as_ref(), b"first");
}

#[test]
fn stale_generation_returns_exact_owner() {
    let pool = Budget::new(4096).unwrap();
    let mut parser = Extractor::new(&pool, Limits::default(), 2);
    let input = opaque(&pool, b"old", 1);
    let original = input.clone();
    let rejection = parser.provide(input).unwrap_err();
    assert_eq!(
        rejection.error,
        Error::StaleGeneration {
            expected: 2,
            received: 1
        }
    );
    same_input(&rejection.input, &original);
}

#[test]
fn every_source_owner_must_belong_to_the_same_pool() {
    let pool = Budget::new(8192).unwrap();
    let foreign = Budget::new(8192).unwrap();
    for slot in 0..5 {
        let mut input = opaque(&pool, b"local", 1);
        let other = Bytes::copy_from_slice(&foreign, b"foreign").unwrap();
        match slot {
            0 => input.payload = other,
            1 => input.codec_private = Some(other),
            2 => input.codec_id = Some(other),
            3 => input.charset_label = Some(other),
            _ => {
                input.addition = Some(Addition {
                    id: 1,
                    bytes: other,
                });
            }
        }
        let original = input.clone();
        let mut parser = Extractor::new(&pool, Limits::default(), 1);
        let rejection = parser.provide(input).unwrap_err();
        assert_eq!(rejection.error, Error::ForeignPool);
        same_input(&rejection.input, &original);
    }
}

#[test]
fn input_limit_rejection_does_not_consume_input() {
    let pool = Budget::new(4096).unwrap();
    let limits = Limits {
        max_input_bytes: 3,
        ..Limits::default()
    };
    let mut parser = Extractor::new(&pool, limits, 1);
    let input = opaque(&pool, b"four", 1);
    let original = input.clone();
    let rejection = parser.provide(input).unwrap_err();
    assert_eq!(
        rejection.error,
        Error::Limit {
            requested: 4,
            maximum: 3
        }
    );
    same_input(&rejection.input, &original);
    parser.provide(opaque(&pool, b"ok", 1)).unwrap();
    assert_eq!(next_opaque(&mut parser).input.payload.as_ref(), b"ok");
}

#[test]
fn configuration_limit_is_independent_of_payload_limit() {
    let pool = Budget::new(4096).unwrap();
    let limits = Limits {
        max_config_bytes: 1,
        ..Limits::default()
    };
    let mut parser = Extractor::new(&pool, limits, 1);
    let mut input = opaque(&pool, b"small", 1);
    input.codec_private = Some(Bytes::copy_from_slice(&pool, b"xx").unwrap());
    let original = input.clone();
    let rejection = parser.provide(input).unwrap_err();
    assert_eq!(
        rejection.error,
        Error::Limit {
            requested: 2,
            maximum: 1
        }
    );
    same_input(&rejection.input, &original);
}

#[test]
fn output_alias_retains_exact_charge_until_last_owner_drops() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut parser = Extractor::new(&pool, Limits::default(), 1);
    parser.provide(opaque(&pool, b"retained", 1)).unwrap();
    let held = pool.used();
    let output = next_opaque(&mut parser);
    drop(parser);
    assert_eq!(pool.used(), held);
    let alias = output.input.payload.slice(1..3).unwrap();
    assert!(alias.shares_storage_with(&output.input.payload));
    drop(output);
    assert_eq!(pool.used(), held);
    assert_eq!(alias.as_ref(), b"et");
    drop(alias);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn reset_keeps_old_output_immutable_and_changes_generation() {
    let pool = Budget::new(4096).unwrap();
    let mut parser = Extractor::new(&pool, Limits::default(), 1);
    parser.provide(opaque(&pool, b"old", 1)).unwrap();
    let old = next_opaque(&mut parser);
    parser
        .provide(opaque(&pool, b"discarded pending", 1))
        .unwrap();
    parser.reset(2);
    parser.provide(opaque(&pool, b"new", 2)).unwrap();
    let new = next_opaque(&mut parser);
    assert_eq!(old.input.payload.as_ref(), b"old");
    assert_eq!(old.input.generation, 1);
    assert_eq!(new.input.payload.as_ref(), b"new");
    assert_eq!(new.input.generation, 2);
}

#[test]
fn opaque_readorder_and_source_sequence_are_never_reordered() {
    let pool = Budget::new(8192).unwrap();
    let mut parser = Extractor::new(&pool, Limits::default(), 1);
    for (sequence, raw) in [
        b"9,0,Default,,0,0,0,,first".as_slice(),
        b"2,0,Default,,0,0,0,,second".as_slice(),
    ]
    .iter()
    .enumerate()
    {
        let mut input = opaque(&pool, raw, 1);
        input.sequence = u64::try_from(sequence).unwrap();
        parser.provide(input).unwrap();
        let cue = next_opaque(&mut parser);
        assert_eq!(cue.input.payload.as_ref(), *raw);
        assert_eq!(cue.input.sequence, u64::try_from(sequence).unwrap());
        assert!(cue.decoded_utf8.is_none());
    }
}

fn named(id: &str) -> &Case {
    cases::ALL.iter().find(|case| case.id == id).unwrap()
}

fn resource_failure(id: &str, limits: Limits, expected_cues: usize) {
    for units in [1, 7, 4096] {
        let pool = Budget::new(64 * 1024 * 1024).unwrap();
        let baseline = pool.used();
        {
            let source = input_for(&pool, named(id));
            let original = source.clone();
            let mut parser = Extractor::new(&pool, limits, source.generation);
            parser.provide(source).unwrap();
            parser.finish_input();
            let mut cues = 0;
            let mut limited = false;
            for _ in 0..100000 {
                let mut work = WorkBudget::new(units);
                let result = parser.step(&mut work, &AtomicBool::new(false));
                assert!(work.spent() <= units);
                match result {
                    Ok(Status::Yield) => assert!(work.spent() != 0),
                    Ok(Status::Cue(cue)) => {
                        same_input(&cue.input, &original);
                        cues += 1;
                    }
                    Err(Error::Limit { .. }) => {
                        limited = true;
                        break;
                    }
                    other => panic!("{id}: expected typed resource limit, got {other:?}"),
                }
            }
            assert!(limited);
            assert_eq!(cues, expected_cues);
        }
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn modifier_count_limit_precedes_atomic_packet_publication() {
    resource_failure(
        "tx3g-style-highlight-raw",
        Limits {
            max_modifiers: 1,
            ..Limits::default()
        },
        0,
    );
}

#[test]
fn decoded_utf8_limit_is_independent_of_source_byte_limit() {
    resource_failure(
        "tx3g-utf16be-bom",
        Limits {
            max_decoded_bytes: 2,
            ..Limits::default()
        },
        0,
    );
}

fn first_cue(parser: &mut Extractor, units: u64) -> Cue {
    for _ in 0..100000 {
        let mut work = WorkBudget::new(units);
        match parser.step(&mut work, &AtomicBool::new(false)).unwrap() {
            Status::Yield => assert!(work.spent() != 0),
            Status::Cue(cue) => return cue,
            other => panic!("expected cue, got {other:?}"),
        }
    }
    panic!("bounded authored input never produced a cue")
}

#[test]
fn validated_utf8_packet_view_aliases_original_charged_storage() {
    let pool = Budget::new(8192).unwrap();
    let input = input_for(&pool, named("utf8-packet-markup"));
    let original = input.payload.clone();
    let before = pool.used();
    let mut parser = Extractor::new(&pool, Limits::default(), input.generation);
    parser.provide(input).unwrap();
    let cue = first_cue(&mut parser, 1);
    let decoded = cue.decoded_utf8.as_ref().unwrap();
    assert!(decoded.shares_storage_with(&original));
    assert_eq!(decoded.as_ref(), original.as_ref());
    assert_eq!(pool.used(), before);
}

#[test]
fn utf16_output_retains_source_and_new_immutable_utf8_charge() {
    let pool = Budget::new(8192).unwrap();
    let baseline = pool.used();
    let input = input_for(&pool, named("tx3g-utf16be-bom"));
    // The payload, codec identifier and sample description.
    let source_charge = pool.used() - baseline;
    let mut parser = Extractor::new(&pool, Limits::default(), input.generation);
    parser.provide(input).unwrap();
    let cue = first_cue(&mut parser, 7);
    let decoded = cue.decoded_utf8.as_ref().unwrap();
    assert!(!decoded.shares_storage_with(&cue.input.payload));
    assert!(decoded.belongs_to(&pool));
    let decoded_charge = decoded.retained_storage_bytes();
    drop(parser);
    assert_eq!(pool.used(), baseline + source_charge + decoded_charge);
    let alias = decoded.slice(0..2).unwrap();
    drop(cue);
    assert_eq!(pool.used(), baseline + decoded_charge);
    drop(alias);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn output_pressure_yields_then_resumes_identical_utf16_cue() {
    let pool = Budget::new(8192).unwrap();
    let case = named("tx3g-utf16be-bom");
    let input = input_for(&pool, case);
    let original = input.clone();
    let mut parser = Extractor::new(&pool, Limits::default(), input.generation);
    parser.provide(input).unwrap();
    let held = pool.reserve(pool.available()).unwrap();
    let mut blocked = false;
    for _ in 0..1000 {
        let mut work = WorkBudget::new(4096);
        match parser.step(&mut work, &AtomicBool::new(false)).unwrap() {
            Status::Yield => assert!(work.spent() != 0),
            Status::NeedOutput => {
                blocked = true;
                break;
            }
            other => panic!("expected output pressure, got {other:?}"),
        }
    }
    assert!(blocked);
    drop(held);
    let event = first_cue(&mut parser, 1);
    check_event(&event, &case.events[0], &original);
}

#[test]
fn cancellation_during_utf16_work_resumes_without_skipping_scalar() {
    let pool = Budget::new(8192).unwrap();
    let case = named("tx3g-utf16be-bom");
    let input = input_for(&pool, case);
    let original = input.clone();
    let mut parser = Extractor::new(&pool, Limits::default(), input.generation);
    parser.provide(input).unwrap();
    let mut first = WorkBudget::new(1);
    assert!(matches!(
        parser.step(&mut first, &AtomicBool::new(false)).unwrap(),
        Status::Yield
    ));
    let mut cancelled = WorkBudget::new(7);
    assert_eq!(
        parser
            .step(&mut cancelled, &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(cancelled.spent(), 0);
    let event = first_cue(&mut parser, 1);
    check_event(&event, &case.events[0], &original);
}

#[test]
fn long_utf8_validation_yields_and_charges_every_source_byte() {
    let pool = Budget::new(256 * 1024).unwrap();
    let whole =
        Bytes::copy_from_slice(&pool, named("tx3g-u16-byte-count-boundary").payload).unwrap();
    let payload = whole.slice(2..whole.len()).unwrap();
    let mut input = opaque(&pool, b"", 1);
    input.payload = payload;
    input.format = Format::Utf8Packet;
    input.encoding = Encoding::Utf8;
    let original = input.payload.clone();
    let mut parser = Extractor::new(&pool, Limits::default(), 1);
    parser.provide(input).unwrap();
    let mut spent = 0;
    let mut yielded = 0;
    let mut emitted = false;
    for _ in 0..262144 {
        let mut work = WorkBudget::new(1);
        let result = parser.step(&mut work, &AtomicBool::new(false)).unwrap();
        spent += work.spent();
        match result {
            Status::Yield => {
                yielded += 1;
                assert_eq!(work.spent(), 1);
            }
            Status::Cue(cue) => {
                assert_eq!(cue.decoded_utf8.unwrap().as_ref(), original.as_ref());
                emitted = true;
                break;
            }
            other => panic!("unexpected long-input status {other:?}"),
        }
    }
    assert!(emitted);
    assert!(yielded > 1);
    assert!(spent >= u64::try_from(original.len()).unwrap());
}

#[test]
fn oversized_charset_label_returns_exact_unconsumed_source() {
    let pool = Budget::new(8192).unwrap();
    let mut input = opaque(&pool, b"opaque", 1);
    input.charset_label = Some(Bytes::copy_from_slice(&pool, &[b'x'; 257]).unwrap());
    let original = input.clone();
    let mut parser = Extractor::new(&pool, Limits::default(), 1);
    let rejection = parser.provide(input).unwrap_err();
    assert_eq!(
        rejection.error,
        Error::Limit {
            requested: 257,
            maximum: 256
        }
    );
    same_input(&rejection.input, &original);
}

#[test]
fn addition_limit_returns_all_source_facts_without_reclassification() {
    let pool = Budget::new(8192).unwrap();
    let input = input_for(&pool, named("mkv-vtt-settings-id-comments"));
    let original = input.clone();
    let limits = Limits {
        max_addition_bytes: 16,
        ..Limits::default()
    };
    let mut parser = Extractor::new(&pool, limits, input.generation);
    let rejection = parser.provide(input).unwrap_err();
    assert_eq!(
        rejection.error,
        Error::Limit {
            requested: 51,
            maximum: 16
        }
    );
    same_input(&rejection.input, &original);
}
