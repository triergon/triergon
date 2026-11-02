use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Bytes, Error, InputRequest, InputWindow, SourceCapabilities, SourceFailure, SourceId,
    Step, Time, WorkBudget,
};
use triergon_framing::{
    DtsCoreFrame, DtsCoreFrameLimits, DtsCoreFrameReader, DtsCoreHeader, probe_dts_core_header,
};

const SOURCE: SourceId = SourceId(2150);
const STAGED: &str = "DTS core implementation pending parent runtime red";
const HEADERS: &str = include_str!("../fixtures/dts-core-header-author-v1.tsv");
const STREAMS: &str = include_str!("../fixtures/dts-core-frame-author-v1.tsv");

fn hex(text: &str) -> Vec<u8> {
    if text == "~" || text.is_empty() {
        return Vec::new();
    }
    assert_eq!(text.len() % 2, 0);
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}

fn class(error: Error) -> &'static str {
    match error {
        Error::Unsupported(reason) if reason == STAGED => "pending-implementation",
        Error::Unsupported(_) => "unsupported",
        Error::Invalid(_) => "invalid",
        Error::Truncated => "truncated",
        Error::Limit { .. } => "limit",
        Error::Backpressure { .. } => "backpressure",
        Error::Allocation => "allocation",
        Error::Overflow => "overflow",
        Error::Transport => "transport",
        Error::Cancelled => "cancelled",
        Error::ForeignPool => "foreign-pool",
        Error::SourceMismatch => "source-mismatch",
        Error::StaleGeneration { .. } => "stale-generation",
        Error::StaleRequest { .. } => "stale-request",
    }
}

// Test-only decoder for this frozen JSON subset: no escaped strings, nesting
// greater than four, objects/arrays and scalar values only. It imports no JSON
// implementation and is not a production parser or media oracle.
fn components(text: &str) -> Vec<&str> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut output = Vec::new();
    let (mut depth, mut quoted, mut start) = (0_i32, false, 0);
    for (at, value) in text.char_indices() {
        assert_ne!(value, '\\', "escaped string outside frozen fixture subset");
        if value == '"' {
            quoted = !quoted;
        } else if !quoted {
            match value {
                '{' | '[' => {
                    depth += 1;
                    assert!(depth <= 4);
                }
                '}' | ']' => {
                    depth -= 1;
                    assert!(depth >= 0);
                }
                ',' if depth == 0 => {
                    output.push(text[start..at].trim());
                    start = at + 1;
                }
                _ => {}
            }
        }
    }
    assert!(!quoted && depth == 0);
    output.push(text[start..].trim());
    output
}

fn object(text: &str) -> BTreeMap<String, String> {
    let body = text.strip_prefix('{').unwrap().strip_suffix('}').unwrap();
    let mut output = BTreeMap::new();
    for entry in components(body) {
        let (key, value) = entry.split_once(':').unwrap();
        let key = key
            .trim()
            .strip_prefix('"')
            .unwrap()
            .strip_suffix('"')
            .unwrap();
        assert!(
            output
                .insert(key.to_owned(), value.trim().to_owned())
                .is_none()
        );
    }
    output
}

fn array(text: &str) -> Vec<&str> {
    components(text.strip_prefix('[').unwrap().strip_suffix(']').unwrap())
}

// The frozen TSV writer quotes fields containing JSON quotes and doubles each
// embedded quote. Decode that transport layer once, before parsing JSON. This
// changes neither the fixture's bytes nor its independent expected values.
fn tsv_json(text: &str) -> String {
    let Some(body) = text.strip_prefix('"') else {
        assert!(!text.contains('"'), "quote inside unquoted TSV field");
        return text.to_owned();
    };
    let body = body
        .strip_suffix('"')
        .expect("unterminated quoted TSV field");
    let mut output = String::new();
    let mut chars = body.chars();
    while let Some(value) = chars.next() {
        if value == '"' {
            assert_eq!(chars.next(), Some('"'), "undoubled quote inside TSV field");
        }
        output.push(value);
    }
    output
}

fn fixture_json_parser_controls() {
    assert!(components("").is_empty());
    assert!(components("  ").is_empty());
    assert!(object("{}").is_empty());
    assert!(object("{ }").is_empty());
    assert!(array("[]").is_empty());
    assert!(array("[ ]").is_empty());
    assert!(object(&tsv_json("\"{}\"")).is_empty());
    assert!(array(&tsv_json("\"[]\"")).is_empty());

    let header = tsv_json(r#""{""empty"":{},""window_seconds"":""1/2""}""#);
    assert_eq!(header, r#"{"empty":{},"window_seconds":"1/2"}"#);
    let header = object(&header);
    assert!(object(&header["empty"]).is_empty());
    assert_eq!(header["window_seconds"], "\"1/2\"");

    let frames = tsv_json(r#""[{""header"":{},""length"":0,""offset"":0}]""#);
    assert_eq!(frames, r#"[{"header":{},"length":0,"offset":0}]"#);
    let frames = array(&frames);
    assert_eq!(frames.len(), 1);
    let frame = object(frames[0]);
    assert_eq!(frame.len(), 3);
    assert!(object(&frame["header"]).is_empty());
    assert_eq!(frame["length"], "0");
    assert_eq!(frame["offset"], "0");
}

fn fraction(value: Time) -> String {
    if value.denominator() == 1 {
        value.numerator().to_string()
    } else {
        format!("{}/{}", value.numerator(), value.denominator())
    }
}

fn optional<T: ToString>(value: Option<T>) -> String {
    value.map_or_else(|| "null".to_owned(), |item| item.to_string())
}

fn fields(header: DtsCoreHeader) -> BTreeMap<String, String> {
    let mut output = BTreeMap::new();
    macro_rules! raw {
        ($($name:ident),+ $(,)?) => {
            $(output.insert(stringify!($name).to_owned(), header.$name.to_string());)+
        };
    }
    raw!(
        normal,
        short,
        cpf,
        nblks,
        fsize,
        amode,
        sfreq,
        rate,
        fixed,
        dynamic,
        timestamp,
        auxiliary,
        hdcd,
        extension_id,
        extension,
        audio_sync,
        lfe,
        history,
        filter,
        version,
        copy_history,
        pcm_resolution,
        sum_front,
        sum_surround,
        dialog_word,
        header_bits,
        frame_bytes,
        core_rate_hz,
        coding_window_samples,
        open_bitrate
    );
    output.insert("hcrc".to_owned(), optional(header.hcrc));
    output.insert(
        "full_bandwidth_channels".to_owned(),
        optional(header.full_bandwidth_channels),
    );
    output.insert(
        "nominal_bitrate".to_owned(),
        optional(header.nominal_bitrate),
    );
    output.insert("dialog_gain_db".to_owned(), optional(header.dialog_gain_db));
    output.insert(
        "window_seconds".to_owned(),
        format!("\"{}\"", fraction(header.window)),
    );
    output
}

fn fixture(table: &str, id: &str) -> Vec<u8> {
    let row = table
        .lines()
        .skip(1)
        .find(|line| line.split('\t').next() == Some(id))
        .unwrap();
    hex(row.split('\t').nth(1).unwrap())
}

fn limits() -> DtsCoreFrameLimits {
    DtsCoreFrameLimits {
        maximum_frame_bytes: 16384,
        maximum_frames: 16,
        maximum_input_window: 31,
    }
}

fn new_reader(
    pool: &Budget,
    length: Option<u64>,
    policy: DtsCoreFrameLimits,
) -> DtsCoreFrameReader {
    DtsCoreFrameReader::new(
        pool,
        SOURCE,
        SourceCapabilities {
            length,
            random_access: false,
        },
        policy,
    )
    .unwrap()
}

fn request(reader: &mut DtsCoreFrameReader) -> InputRequest {
    for _ in 0..1000 {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::NeedInput(input) => return input,
            Step::Yield => {}
            other => panic!("expected input request, received {other:?}"),
        }
    }
    panic!("bounded initial request did not arrive");
}

fn respond(reader: &mut DtsCoreFrameReader, needed: InputRequest, source: &Bytes, chunk: usize) {
    let start = usize::try_from(needed.offset).unwrap();
    assert!(
        start <= source.len(),
        "reader requested beyond authored entity"
    );
    if start == source.len() {
        reader.finish(needed.source, needed.token).unwrap();
    } else {
        assert!(needed.maximum > 0);
        let end = source.len().min(start + chunk.min(needed.maximum));
        reader
            .provide(
                &InputWindow::new(
                    needed.source,
                    needed.token,
                    needed.offset,
                    source.slice(start..end).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
    }
}

struct Observed {
    pool: Budget,
    initial: usize,
    generation: Option<u64>,
    frames: Vec<DtsCoreFrame>,
    terminal: String,
    error: Option<Error>,
    end_offset: Option<u64>,
}

fn drive(
    input: &[u8],
    chunk: usize,
    units: u64,
    known: bool,
    policy: DtsCoreFrameLimits,
) -> Observed {
    let pool = Budget::new(1 << 20).unwrap();
    let initial = pool.used();
    let source = Bytes::copy_from_slice(&pool, input).unwrap();
    let mut output = Observed {
        pool: pool.clone(),
        initial,
        generation: None,
        frames: Vec::new(),
        terminal: "guard-exhausted".to_owned(),
        error: None,
        end_offset: None,
    };
    let made = DtsCoreFrameReader::new(
        &pool,
        SOURCE,
        SourceCapabilities {
            length: if known {
                Some(input.len() as u64)
            } else {
                None
            },
            random_access: false,
        },
        policy,
    );
    let mut reader = match made {
        Ok(reader) => reader,
        Err(error) => {
            output.terminal = class(error).to_owned();
            output.error = Some(error);
            return output;
        }
    };
    output.generation = Some(reader.generation());
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(units);
        let step = reader.step(&mut work, &AtomicBool::new(false));
        assert!(work.spent() <= units);
        match step {
            Ok(Step::Ready(frame)) => {
                assert!(frame.bytes().belongs_to(&pool));
                assert_eq!(frame.facts().source, SOURCE);
                assert_eq!(Some(frame.facts().generation), output.generation);
                output.frames.push(frame);
            }
            Ok(Step::Yield) => {}
            Ok(Step::NeedInput(needed)) => respond(&mut reader, needed, &source, chunk),
            Ok(Step::End(end)) => {
                assert_eq!(end.source, SOURCE);
                output.terminal = "end".to_owned();
                output.end_offset = Some(end.offset);
                return output;
            }
            Err(error) => {
                output.terminal = class(error).to_owned();
                output.error = Some(error);
                if !matches!(error, Error::Backpressure { .. } | Error::Allocation) {
                    assert_eq!(
                        reader
                            .step(&mut WorkBudget::new(0), &AtomicBool::new(false))
                            .err(),
                        Some(error)
                    );
                }
                return output;
            }
        }
    }
    output
}

#[test]
fn independent_188_header_sources_all_reach_the_declared_result_and_facts() {
    fixture_json_parser_controls();
    let mut count = 0;
    let mut mismatches = Vec::new();
    for line in HEADERS.lines().skip(1) {
        let row: Vec<_> = line.split('\t').collect();
        assert_eq!(row.len(), 4);
        count += 1;
        match probe_dts_core_header(&hex(row[1])) {
            Ok(header) if row[2] == "ok" => {
                if fields(header) != object(&tsv_json(row[3])) {
                    mismatches.push(format!("{} facts differ", row[0]));
                }
            }
            Ok(_) => mismatches.push(format!("{} expected {}, received header", row[0], row[2])),
            Err(error) if class(error) == row[2] => {}
            Err(error) => mismatches.push(format!(
                "{} expected {}, received {error:?}",
                row[0], row[2]
            )),
        }
    }
    assert_eq!(count, 188);
    assert!(
        mismatches.is_empty(),
        "visited all188 rows; {} mismatches; first: {:?}",
        mismatches.len(),
        &mismatches[..mismatches.len().min(8)]
    );
}

#[test]
fn independent_17_stream_sources_all_preserve_bytes_facts_and_partial_outputs() {
    fixture_json_parser_controls();
    let mut count = 0;
    let mut variants = 0;
    let mut mismatches = Vec::new();
    for line in STREAMS.lines().skip(1) {
        let row: Vec<_> = line.split('\t').collect();
        assert_eq!(row.len(), 5);
        count += 1;
        let input = hex(row[1]);
        assert_eq!(input.len().to_string(), row[2]);
        let expected_json = tsv_json(row[4]);
        let expected = array(&expected_json);
        for chunk in [1, 2, 7, 31] {
            for units in [1, 2, 17, 4096] {
                for known in [false, true] {
                    variants += 1;
                    let actual = drive(&input, chunk, units, known, limits());
                    let context = format!("{} chunk{chunk} work{units} known{known}", row[0]);
                    if actual.terminal != row[3] || actual.frames.len() != expected.len() {
                        mismatches.push(format!(
                            "{context}: {} frames / {}, expected {} / {}",
                            actual.frames.len(),
                            actual.terminal,
                            expected.len(),
                            row[3]
                        ));
                    }
                    if actual.terminal == "end" && actual.end_offset != Some(input.len() as u64) {
                        mismatches.push(format!("{context}: end offset differs"));
                    }
                    for (frame, wanted) in actual.frames.iter().zip(&expected) {
                        let wanted = object(wanted);
                        let offset: usize = wanted["offset"].parse().unwrap();
                        let length: usize = wanted["length"].parse().unwrap();
                        if frame.facts().offset != offset as u64
                            || frame.bytes().len() != length
                            || frame.bytes().as_ref() != &input[offset..offset + length]
                            || fields(frame.facts().header) != object(&wanted["header"])
                        {
                            mismatches
                                .push(format!("{context}: published source/frame facts differ"));
                        }
                    }
                }
            }
        }
    }
    assert_eq!(count, 17);
    assert_eq!(variants, 544);
    assert!(
        mismatches.is_empty(),
        "visited all17 cases /544 variants; {} mismatches; first: {:?}",
        mismatches.len(),
        &mismatches[..mismatches.len().min(8)]
    );
}

#[test]
fn raw_presence_open_bitrate_user_modes_and_versioned_dialog_are_distinct() {
    let get = |id: &str| probe_dts_core_header(&fixture(HEADERS, id)).unwrap();
    let absent = get("rate-13-blocks-15");
    assert_eq!(absent.hcrc, None);
    assert_eq!(absent.header_bits, 104);
    let zero = get("hcrc-0");
    let other = get("hcrc-57005");
    assert_eq!(zero.hcrc, Some(0));
    assert_eq!(other.hcrc, Some(0xdead));
    assert_eq!(zero.header_bits, 120);
    assert_eq!(other.header_bits, 120);
    let open = get("bitrate-29");
    assert!(open.open_bitrate);
    assert_eq!(open.nominal_bitrate, None);
    let fixed = get("bitrate-0");
    assert!(!fixed.open_bitrate);
    assert_eq!(fixed.nominal_bitrate, Some(32000));
    assert_eq!(get("channel-16").full_bandwidth_channels, None);
    assert_eq!(get("channel-63").amode, 63);
    assert_eq!(get("version-6").dialog_gain_db, Some(-27));
    assert_eq!(get("version-7").dialog_gain_db, Some(-11));
    assert_eq!(get("version-0").dialog_gain_db, None);
    assert_eq!(get("version-0").dialog_word, 11);
}

#[test]
fn complete_header_does_not_finish_a_body_and_payload_syncs_are_opaque() {
    let bytes = fixture(STREAMS, "minimum");
    let header = probe_dts_core_header(&bytes[..13]).unwrap();
    assert_eq!(header.frame_bytes, 96);
    let incomplete = drive(&bytes[..13], 1, 1, true, limits());
    assert_eq!(incomplete.terminal, "truncated");
    assert!(incomplete.frames.is_empty());
    let complete = drive(&bytes, 1, 1, true, limits());
    assert_eq!(complete.terminal, "end");
    assert_eq!(complete.frames.len(), 1);
    assert_eq!(complete.frames[0].bytes().as_ref(), bytes.as_slice());
    assert!(
        bytes[13..]
            .windows(4)
            .any(|chunk| chunk == [0x7f, 0xfe, 0x80, 1])
    );
}

#[test]
fn zero_work_and_cancel_before_acquisition_do_not_consume_or_publish() {
    let pool = Budget::new(8192).unwrap();
    let baseline = pool.used();
    let mut reader = new_reader(&pool, None, limits());
    let mut work = WorkBudget::new(0);
    assert!(matches!(
        reader.step(&mut work, &AtomicBool::new(false)).unwrap(),
        Step::Yield
    ));
    assert_eq!(work.spent(), 0);
    assert_eq!(reader.position(), 0);
    let cancel = AtomicBool::new(true);
    assert_eq!(
        reader.step(&mut WorkBudget::new(0), &cancel).err(),
        Some(Error::Cancelled)
    );
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        reader.step(&mut WorkBudget::new(100), &cancel).err(),
        Some(Error::Cancelled)
    );
    assert_eq!(pool.used(), baseline);
}

#[test]
fn source_pool_generation_requests_starvation_and_transport_remain_explicit() {
    let pool = Budget::new(8192).unwrap();
    let foreign = Budget::new(8192).unwrap();
    let source = Bytes::copy_from_slice(&pool, &fixture(STREAMS, "minimum")).unwrap();
    let mut reader = new_reader(&pool, None, limits());
    let needed = request(&mut reader);
    let generation = reader.generation();
    let bad = Bytes::copy_from_slice(&foreign, &[0x7f]).unwrap();
    assert_eq!(
        reader.provide(&InputWindow::new(SOURCE, needed.token.clone(), 0, bad).unwrap()),
        Err(Error::ForeignPool)
    );
    assert_eq!(
        reader.provide(
            &InputWindow::new(
                SourceId(999),
                needed.token.clone(),
                0,
                source.slice(0..1).unwrap()
            )
            .unwrap()
        ),
        Err(Error::SourceMismatch)
    );
    let other = new_reader(&pool, None, limits());
    assert_ne!(reader.generation(), other.generation());
    assert!(matches!(
        reader.provide(
            &InputWindow::new(
                SOURCE,
                other.request().token,
                0,
                source.slice(0..1).unwrap()
            )
            .unwrap()
        ),
        Err(Error::StaleGeneration { .. })
    ));
    reader
        .provide(
            &InputWindow::new(SOURCE, needed.token.clone(), 0, source.slice(0..0).unwrap())
                .unwrap(),
        )
        .unwrap();
    assert_eq!(reader.position(), 0);
    assert_eq!(reader.generation(), generation);
    let current = request(&mut reader);
    assert_eq!(current.offset, 0);
    assert_eq!(current.token.generation(), generation);
    assert_eq!(current.token, needed.token);
    let first_byte =
        InputWindow::new(SOURCE, current.token, 0, source.slice(0..1).unwrap()).unwrap();
    reader.provide(&first_byte).unwrap();
    let advanced = request(&mut reader);
    assert_eq!(advanced.offset, 1);
    assert!(matches!(
        reader.provide(&first_byte),
        Err(Error::Invalid(_))
    ));
    assert_eq!(reader.position(), 1);
    assert_eq!(reader.generation(), generation);
    reader
        .fail(advanced.source, advanced.token, SourceFailure::Transport)
        .unwrap();
    assert_eq!(
        reader
            .step(&mut WorkBudget::new(64), &AtomicBool::new(false))
            .err(),
        Some(Error::Transport)
    );
    assert_eq!(
        reader
            .step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .err(),
        Some(Error::Transport)
    );
}

#[test]
fn reservation_and_possible_freeze_pressure_retry_the_same_source_envelope() {
    let pool = Budget::new(8192).unwrap();
    let baseline = pool.used();
    let bytes = fixture(STREAMS, "minimum");
    let source = Bytes::copy_from_slice(&pool, &bytes).unwrap();
    let mut reader = new_reader(&pool, None, limits());
    let before = pool.used();
    let blocker = pool.reserve(pool.available()).unwrap();
    let mut pressure = false;
    for _ in 0..10_000 {
        match reader.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(Step::NeedInput(needed)) => respond(&mut reader, needed, &source, 1),
            Ok(Step::Yield) => {}
            Err(Error::Backpressure { .. }) => {
                pressure = true;
                break;
            }
            other => panic!("expected bounded allocation pressure, received {other:?}"),
        }
    }
    assert!(pressure);
    let stopped = reader.position();
    assert!(matches!(
        reader
            .step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        Step::Yield
    ));
    assert_eq!(reader.position(), stopped);
    drop(blocker);
    assert_eq!(pool.used(), before);
    let mut freeze_blocker = None;
    let mut armed = false;
    let mut frame = None;
    for _ in 0..20_000 {
        if !armed && pool.used() > before {
            freeze_blocker = Some(pool.reserve(pool.available()).unwrap());
            armed = true;
        }
        match reader.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(Step::NeedInput(needed)) => respond(&mut reader, needed, &source, 1),
            Ok(Step::Yield) => {}
            Ok(Step::Ready(value)) => {
                frame = Some(value);
                break;
            }
            Err(Error::Backpressure { .. }) if freeze_blocker.is_some() => {
                drop(freeze_blocker.take());
            }
            other => panic!("allocation/freeze retry lost frame: {other:?}"),
        }
    }
    let frame = frame.expect("bounded allocation/freeze retry did not publish");
    assert_eq!(frame.bytes().as_ref(), bytes.as_slice());
    assert_eq!(frame.facts().offset, 0);
    drop(freeze_blocker);
    drop(reader);
    drop(source);
    drop(frame);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn published_frame_views_retain_storage_until_cross_thread_final_release() {
    let mut observed = drive(&fixture(STREAMS, "minimum"), 1, 1, false, limits());
    assert_eq!(observed.terminal, "end");
    assert_eq!(observed.frames.len(), 1);
    let pool = observed.pool.clone();
    let baseline = observed.initial;
    let frame = observed.frames.pop().unwrap();
    let facts = frame.facts();
    let view = frame.bytes().slice(13..20).unwrap();
    assert!(view.shares_storage_with(frame.bytes()));
    assert_eq!(facts.source, SOURCE);
    assert_eq!(Some(facts.generation), observed.generation);
    drop(frame);
    drop(observed);
    assert!(pool.used() > baseline);
    std::thread::spawn(move || drop(view)).join().unwrap();
    assert_eq!(pool.used(), baseline);
}

#[test]
fn byte_frame_count_and_zero_policy_limits_preserve_prior_owned_outputs() {
    let pool = Budget::new(8192).unwrap();
    for field in 0..3 {
        let mut policy = limits();
        match field {
            0 => policy.maximum_frame_bytes = 0,
            1 => policy.maximum_frames = 0,
            _ => policy.maximum_input_window = 0,
        }
        assert!(matches!(
            DtsCoreFrameReader::new(
                &pool,
                SOURCE,
                SourceCapabilities {
                    length: None,
                    random_access: false
                },
                policy
            ),
            Err(Error::Invalid(_))
        ));
    }
    let bytes = fixture(STREAMS, "minimum");
    let mut policy = limits();
    policy.maximum_frame_bytes = 95;
    let bounded = drive(&bytes, 1, 1, true, policy);
    assert_eq!(
        bounded.error,
        Some(Error::Limit {
            requested: 96,
            maximum: 95
        })
    );
    assert!(bounded.frames.is_empty());
    policy = limits();
    policy.maximum_frames = 1;
    let multiple = fixture(STREAMS, "two-frames");
    let observed = drive(&multiple, 1, 1, false, policy);
    assert_eq!(
        observed.error,
        Some(Error::Limit {
            requested: 2,
            maximum: 1
        })
    );
    assert_eq!(observed.frames.len(), 1);
    assert_eq!(observed.frames[0].bytes().as_ref(), &multiple[..96]);
}

#[test]
fn cancel_during_partial_header_and_body_releases_private_storage_exactly() {
    for target in [3, 32] {
        let pool = Budget::new(8192).unwrap();
        let baseline = pool.used();
        let source = Bytes::copy_from_slice(&pool, &fixture(STREAMS, "minimum")).unwrap();
        let mut reader = new_reader(&pool, None, limits());
        let before = pool.used();
        for _ in 0..10_000 {
            if reader.position() >= target {
                break;
            }
            match reader
                .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                Step::NeedInput(needed) => respond(&mut reader, needed, &source, 1),
                Step::Yield => {}
                other => panic!("unexpected result before partial cancel: {other:?}"),
            }
        }
        assert!(reader.position() >= target);
        let cancel = AtomicBool::new(true);
        assert_eq!(
            reader.step(&mut WorkBudget::new(0), &cancel).err(),
            Some(Error::Cancelled)
        );
        assert_eq!(pool.used(), before);
        cancel.store(false, Ordering::Relaxed);
        assert_eq!(
            reader.step(&mut WorkBudget::new(1000), &cancel).err(),
            Some(Error::Cancelled)
        );
        drop(reader);
        drop(source);
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn termination_future_encoder_and_extension_sources_keep_required_gaps_explicit() {
    for id in [
        "termination-raw",
        "version-8",
        "version-15",
        "extension-7",
        "reserved-extension-sync",
    ] {
        let error = probe_dts_core_header(&fixture(HEADERS, id)).unwrap_err();
        assert_eq!(class(error), "unsupported", "{id}");
    }
    let observed = drive(
        &fixture(STREAMS, "good-then-extension"),
        1,
        1,
        false,
        limits(),
    );
    assert_eq!(observed.terminal, "unsupported");
    assert_eq!(observed.frames.len(), 1);
    assert_eq!(observed.frames[0].facts().offset, 0);
    // No14-bit unpacking, word swapping, termination-duration or HD-asset API
    // is introduced by this canonical core checkpoint.
}

#[test]
fn known_empty_length_is_not_implicit_completion_and_generations_are_distinct() {
    let pool = Budget::new(8192).unwrap();
    let mut reader = new_reader(&pool, Some(0), limits());
    let first = request(&mut reader);
    assert_eq!(first.offset, 0);
    assert_eq!(first.maximum, 0);
    reader
        .provide(
            &InputWindow::new(
                SOURCE,
                first.token,
                0,
                Bytes::copy_from_slice(&pool, &[]).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let again = request(&mut reader);
    assert_eq!(again.offset, 0);
    reader.finish(again.source, again.token).unwrap();
    for _ in 0..1000 {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Yield => {}
            Step::End(end) => {
                assert_eq!(end.source, SOURCE);
                assert_eq!(end.offset, 0);
                let next = new_reader(&pool, None, limits());
                assert_ne!(reader.generation(), next.generation());
                return;
            }
            other => panic!("completed empty source emitted {other:?}"),
        }
    }
    panic!("explicit empty completion failed bounded progress");
}
