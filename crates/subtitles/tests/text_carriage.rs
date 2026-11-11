#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, SourceId, SourceLocation, Time, WorkBudget};
use triergon_subtitles::compat::document::Syntax;
use triergon_subtitles::reference_trace::carriage::{
    CallerClock, Discovery, DocumentInput, FormatFacts, Limits, Operation, Packet, Progress,
    ProgressKind, Session, SideData, Snapshot, Source, Status, Stream,
};
use triergon_subtitles::text::{Addition, Encoding, Format, Input, RawClock, Timing};

struct ExpectedSide {
    native_type: i32,
    bytes: &'static [u8],
}
struct ExpectedStream {
    index: u32,
    codec_type: i32,
    codec_id: i32,
    codec_name: &'static str,
    codec_tag: u32,
    time_base: [i32; 2],
    start_raw: i64,
    duration_raw: i64,
    disposition: i32,
    extradata: Option<&'static [u8]>,
    side: &'static [ExpectedSide],
}
struct ExpectedPacket {
    ordinal: u32,
    stream: i32,
    data: &'static [u8],
    position_raw: i64,
    flags: i32,
    buffer_is_null: bool,
    pts_raw: i64,
    dts_raw: i64,
    duration_raw: i64,
    packet_time_base: [i32; 2],
    stream_time_base: [i32; 2],
    side: &'static [ExpectedSide],
}
/// What a case reads: a sample of a packet format or a whole document.
#[derive(Clone, Copy)]
enum Kind {
    Sample(Format),
    Document(Syntax),
}
enum ExpectedOperation {
    Sample(CallerClock),
    Document(Discovery, &'static [u8]),
}
struct Case {
    id: &'static str,
    kind: Kind,
    encoding: Encoding,
    payload: &'static [u8],
    codec_id: Option<&'static [u8]>,
    codec_private: Option<&'static [u8]>,
    charset_label: Option<&'static [u8]>,
    addition: Option<(u64, &'static [u8])>,
    start: Option<(i128, u128)>,
    duration: Option<(i128, u128)>,
    clock: Option<RawClock>,
    operation: ExpectedOperation,
    native_format: Option<FormatFacts>,
    stream: Option<ExpectedStream>,
    packets: &'static [ExpectedPacket],
    progress: &'static [Progress],
    native_error: Option<i32>,
}
#[path = "text-carriage-fixtures/cases.rs"]
mod cases;

fn time(value: Option<(i128, u128)>) -> Option<Time> {
    value.map(|(n, d)| Time::new(n, d).unwrap())
}
fn bytes(pool: &Budget, value: Option<&[u8]>) -> Option<Bytes> {
    value.map(|value| Bytes::copy_from_slice(pool, value).unwrap())
}
fn input(pool: &Budget, case: &Case) -> Source {
    let payload = Bytes::copy_from_slice(pool, case.payload).unwrap();
    let format = match case.kind {
        Kind::Sample(format) => format,
        Kind::Document(syntax) => {
            return Source::Document(DocumentInput {
                syntax,
                payload,
                source_id: SourceId(91),
                generation: 3,
                sequence: 11,
                configuration_epoch: 17,
            });
        }
    };
    Source::Sample(Input {
        format,
        encoding: case.encoding,
        payload,
        codec_id: bytes(pool, case.codec_id),
        codec_private: bytes(pool, case.codec_private),
        charset_label: bytes(pool, case.charset_label),
        addition: case.addition.map(|(id, value)| Addition {
            id,
            bytes: Bytes::copy_from_slice(pool, value).unwrap(),
        }),
        timing: case.clock.map_or_else(
            || Timing::exact(time(case.start), time(case.duration)),
            |clock| Timing::from_ticks(clock).unwrap(),
        ),
        source_id: SourceId(91),
        generation: 3,
        sequence: 11,
        configuration_epoch: 17,
        container_position: Some(SourceLocation {
            source: SourceId(91),
            offset: 301,
        }),
    })
}
fn operation(pool: &Budget, case: &Case) -> Operation {
    match case.operation {
        ExpectedOperation::Sample(clock) => Operation::Sample(clock),
        ExpectedOperation::Document(discovery, path) => Operation::Document {
            discovery,
            file_name: Bytes::copy_from_slice(pool, path).unwrap(),
        },
    }
}
fn same_bytes(actual: Option<&Bytes>, expected: Option<&Bytes>) {
    match (actual, expected) {
        (None, None) => {}
        (Some(a), Some(b)) => {
            assert_eq!(a.as_ref(), b.as_ref());
            assert!(a.shares_storage_with(b));
        }
        _ => panic!("absent/present-empty source fact changed"),
    }
}
fn source(actual: &Source, expected: &Source) {
    match (actual, expected) {
        (Source::Sample(actual), Source::Sample(expected)) => sample(actual, expected),
        (Source::Document(actual), Source::Document(expected)) => {
            assert_eq!(actual.syntax, expected.syntax);
            same_bytes(Some(&actual.payload), Some(&expected.payload));
            assert_eq!(actual.source_id, expected.source_id);
            assert_eq!(actual.generation, expected.generation);
            assert_eq!(actual.sequence, expected.sequence);
            assert_eq!(actual.configuration_epoch, expected.configuration_epoch);
        }
        _ => panic!("source kind changed"),
    }
}
fn sample(actual: &Input, expected: &Input) {
    assert_eq!(actual.format, expected.format);
    assert_eq!(actual.encoding, expected.encoding);
    same_bytes(Some(&actual.payload), Some(&expected.payload));
    same_bytes(actual.codec_id.as_ref(), expected.codec_id.as_ref());
    same_bytes(
        actual.codec_private.as_ref(),
        expected.codec_private.as_ref(),
    );
    same_bytes(
        actual.charset_label.as_ref(),
        expected.charset_label.as_ref(),
    );
    match (&actual.addition, &expected.addition) {
        (None, None) => {}
        (Some(a), Some(b)) => {
            assert_eq!(a.id, b.id);
            same_bytes(Some(&a.bytes), Some(&b.bytes));
        }
        _ => panic!("source addition presence changed"),
    }
    assert_eq!(actual.timing, expected.timing);
    assert_eq!(actual.source_id, expected.source_id);
    assert_eq!(actual.generation, expected.generation);
    assert_eq!(actual.sequence, expected.sequence);
    assert_eq!(actual.configuration_epoch, expected.configuration_epoch);
    assert_eq!(actual.container_position, expected.container_position);
}
fn side(actual: &[SideData], expected: &[ExpectedSide]) {
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(actual.side_data_type, expected.native_type);
        assert_eq!(actual.bytes.as_ref(), expected.bytes);
    }
}
fn stream(actual: &Stream, expected: &ExpectedStream) {
    assert_eq!(actual.index, expected.index);
    assert_eq!(actual.codec_type, expected.codec_type);
    assert_eq!(actual.codec_id, expected.codec_id);
    assert_eq!(actual.codec_name, expected.codec_name);
    assert_eq!(actual.codec_tag, expected.codec_tag);
    assert_eq!(actual.time_base, expected.time_base);
    assert_eq!(actual.start_raw, expected.start_raw);
    assert_eq!(actual.duration_raw, expected.duration_raw);
    assert_eq!(actual.disposition, expected.disposition);
    assert_eq!(
        actual.extradata.as_ref().map(AsRef::as_ref),
        expected.extradata
    );
    side(actual.coded_side_data.as_slice(), expected.side);
    assert!(actual.metadata.is_empty());
}
fn packet(actual: &Packet, expected: &ExpectedPacket) {
    assert_eq!(actual.ordinal, expected.ordinal);
    assert_eq!(actual.stream, expected.stream);
    assert_eq!(actual.data.as_ref(), expected.data);
    assert_eq!(actual.position_raw, expected.position_raw);
    assert_eq!(actual.flags, expected.flags);
    assert_eq!(actual.buffer_is_null, expected.buffer_is_null);
    assert_eq!(actual.pts_raw, expected.pts_raw);
    assert_eq!(actual.dts_raw, expected.dts_raw);
    assert_eq!(actual.duration_raw, expected.duration_raw);
    assert_eq!(actual.packet_time_base, expected.packet_time_base);
    assert_eq!(actual.stream_time_base, expected.stream_time_base);
    side(actual.side_data.as_slice(), expected.side);
}
fn snapshot(actual: &Snapshot, original: &Source, case: &Case) {
    source(&actual.input, original);
    assert_eq!(actual.format, case.native_format);
    assert_eq!(actual.error_code, case.native_error);
    match (&actual.stream, &case.stream) {
        (None, None) => {}
        (Some(a), Some(b)) => stream(a, b),
        _ => panic!("native stream presence differs"),
    }
    assert_eq!(actual.packets.len(), case.packets.len());
    for (actual, expected) in actual.packets.as_slice().iter().zip(case.packets) {
        packet(actual, expected);
    }
    assert_eq!(actual.progress.as_slice(), case.progress);
    match (&actual.operation, &case.operation) {
        (Operation::Sample(a), ExpectedOperation::Sample(b)) => assert_eq!(a, b),
        (
            Operation::Document {
                discovery,
                file_name,
            },
            ExpectedOperation::Document(b, name),
        ) => {
            assert_eq!(discovery, b);
            assert_eq!(file_name.as_ref(), *name);
        }
        _ => panic!("original caller operation changed"),
    }
}
fn run_case(index: usize) {
    let case = &cases::ALL[index];
    for units in [1, 7, 4096] {
        let pool = Budget::new(64 * 1024 * 1024).unwrap();
        let baseline = pool.used();
        let original = input(&pool, case);
        let mut offered = Some(original.clone());
        let mut session = Session::new(&pool, Limits::default());
        session
            .provide(&mut offered, operation(&pool, case))
            .unwrap();
        assert!(offered.is_none());
        session.finish();
        let cancel = AtomicBool::new(false);
        let mut held = None;
        for _ in 0..case.payload.len().saturating_mul(12).saturating_add(4096) {
            let mut work = WorkBudget::new(units);
            match session
                .poll(&mut work, &cancel)
                .unwrap_or_else(|error| panic!("{}: {error:?}", case.id))
            {
                Status::Yield => assert!(work.spent() > 0),
                Status::Ready => {
                    let mut blocked = WorkBudget::new(4096);
                    assert_eq!(
                        session.poll(&mut blocked, &cancel).unwrap(),
                        Status::NeedOutput
                    );
                    assert_eq!(blocked.spent(), 0);
                    assert!(held.is_none());
                    held = session.take_output();
                    assert!(held.is_some());
                }
                Status::End => break,
                other => panic!("{} unexpected {other:?}", case.id),
            }
        }
        let held = held.expect("bounded parser must publish a complete outcome");
        snapshot(&held, &original, case);
        let retained = pool.used();
        drop(original);
        drop(session);
        drop(offered);
        assert!(pool.used() >= baseline);
        assert!(pool.used() <= retained);
        if let Some(packet) = held.packets.as_slice().first() {
            assert_eq!(packet.data.as_ref(), case.packets[0].data);
        }
        drop(held);
        assert_eq!(
            pool.used(),
            baseline,
            "{} leaked retained capacity",
            case.id
        );
    }
}
include!("text-carriage-fixtures/names.rs");

fn sample_case() -> &'static Case {
    cases::ALL
        .iter()
        .find(|case| matches!(case.operation, ExpectedOperation::Sample(_)))
        .unwrap()
}
fn ready(session: &mut Session) -> Snapshot {
    let cancel = AtomicBool::new(false);
    for _ in 0..1000 {
        match session.poll(&mut WorkBudget::new(4096), &cancel).unwrap() {
            Status::Ready => return session.take_output().unwrap(),
            Status::Yield => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    panic!("no bounded output")
}
#[test]
fn zero_work_yields_without_spending_or_losing_input() {
    let pool = Budget::new(1 << 20).unwrap();
    let case = sample_case();
    let mut offered = Some(input(&pool, case));
    let mut session = Session::new(&pool, Limits::default());
    session
        .provide(&mut offered, operation(&pool, case))
        .unwrap();
    let mut work = WorkBudget::new(0);
    assert_eq!(
        session.poll(&mut work, &AtomicBool::new(false)).unwrap(),
        Status::Yield
    );
    assert_eq!(work.spent(), 0);
    assert!(session.recover_pending().is_some());
}
#[test]
fn cancellation_keeps_the_original_recoverable() {
    let pool = Budget::new(1 << 20).unwrap();
    let case = sample_case();
    let original = input(&pool, case);
    let mut offered = Some(original.clone());
    let mut session = Session::new(&pool, Limits::default());
    session
        .provide(&mut offered, operation(&pool, case))
        .unwrap();
    let mut work = WorkBudget::new(4096);
    assert!(matches!(
        session.poll(&mut work, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!(work.spent(), 0);
    source(&session.recover_pending().unwrap(), &original);
}
#[test]
fn bounded_payload_rejection_preserves_the_offered_owner() {
    let pool = Budget::new(1 << 20).unwrap();
    let case = sample_case();
    let original = input(&pool, case);
    let limits = Limits {
        payload_bytes: case.payload.len() - 1,
        ..Limits::default()
    };
    let mut offered = Some(original.clone());
    let mut session = Session::new(&pool, limits);
    assert!(matches!(
        session.provide(&mut offered, operation(&pool, case)),
        Err(Error::Limit { .. })
    ));
    source(offered.as_ref().unwrap(), &original);
}
#[test]
fn busy_input_and_finished_input_preserve_the_callers_value() {
    let pool = Budget::new(1 << 20).unwrap();
    let case = sample_case();
    let original = input(&pool, case);
    let mut session = Session::new(&pool, Limits::default());
    let mut first = Some(original.clone());
    session.provide(&mut first, operation(&pool, case)).unwrap();
    let mut second = Some(original.clone());
    assert!(
        session
            .provide(&mut second, operation(&pool, case))
            .is_err()
    );
    source(second.as_ref().unwrap(), &original);
    session.finish();
    assert!(
        session
            .provide(&mut second, operation(&pool, case))
            .is_err()
    );
    source(second.as_ref().unwrap(), &original);
}
#[test]
fn raw_packet_carriage_does_not_inherit_strict_text_validation() {
    let index = cases::ALL
        .iter()
        .position(|case| case.id == "utf8-packet-invalid-utf8--none")
        .or_else(|| {
            cases::ALL.iter().position(|case| {
                case.id.contains("invalid-utf8")
                    && matches!(case.operation, ExpectedOperation::Sample(_))
            })
        })
        .unwrap();
    run_case(index);
}
#[test]
fn native_outputs_retain_source_and_configuration_after_session_teardown() {
    let pool = Budget::new(1 << 20).unwrap();
    let baseline = pool.used();
    let case = cases::ALL
        .iter()
        .find(|case| {
            case.codec_private.is_some() && matches!(case.operation, ExpectedOperation::Sample(_))
        })
        .unwrap();
    let original = input(&pool, case);
    let mut offered = Some(original.clone());
    let mut session = Session::new(&pool, Limits::default());
    session
        .provide(&mut offered, operation(&pool, case))
        .unwrap();
    let output = ready(&mut session);
    drop(original);
    drop(session);
    drop(offered);
    assert!(pool.used() > baseline);
    assert_eq!(
        output
            .stream
            .as_ref()
            .unwrap()
            .extradata
            .as_ref()
            .unwrap()
            .as_ref(),
        case.codec_private.unwrap()
    );
    assert_eq!(output.packets.as_slice()[0].data.as_ref(), case.payload);
    drop(output);
    assert_eq!(pool.used(), baseline);
}
#[test]
fn input_empty_offer_is_an_error() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut session = Session::new(&pool, Limits::default());
    assert!(matches!(
        session.provide(&mut None, operation(&pool, sample_case())),
        Err(Error::Invalid(_))
    ));
}
