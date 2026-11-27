//! Compares facade seek receipts and every delivered packet with the reference's own
//! native seek, and checks each listed difference against the outputs it pins.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::harness::{oracle, report, validation, worker};
use crate::json::{self, Value};
use crate::{python, sha256};

type Result<T> = std::result::Result<T, String>;

const MANIFEST: &str = "harness/features/facade-seek/container-pins.json";
const MKV: &str = "harness/features/matroska-cues-preroll/corpus.json";
const MP4: &str = "harness/features/isobmff-seek/corpus.json";
const MOVIES: [&str; 3] = [
    "regular-composition-600.mov",
    "regular-end-co64-1800.mov",
    "regular-front-1800.mov",
];
const TIMES_NS: [i64; 6] = [
    0,
    250_000_000,
    2_500_000_000,
    7_500_000_000,
    15_500_000_000,
    25_000_000_000,
];
/// The compared fields of a packet, in the order [`packet`] builds them.
const FIELDS: [&str; 7] = [
    "stream", "track", "pts", "dts", "duration", "key", "payload",
];
const USAGE: &str =
    "usage: cargo xtask play-views seek --record WORKER [--cases ID...] | WORKER OUTPUT";

struct Case {
    id: String,
    input: PathBuf,
    sha: String,
    candidate: Vec<String>,
    reference: Vec<String>,
    matroska: bool,
}

fn get<'a>(value: &'a Value, field: &str) -> Result<&'a Value> {
    value.get(field).ok_or_else(|| format!("missing {field}"))
}

fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    get(value, field)?
        .as_str()
        .ok_or_else(|| format!("{field} must be text"))
}

fn list<'a>(value: &'a Value, field: &str) -> Result<&'a [Value]> {
    get(value, field)?
        .as_array()
        .ok_or_else(|| format!("{field} must be a list"))
}

fn cases(root: &Path) -> Result<Vec<Case>> {
    let mkv = json::read(&root.join(MKV))?;
    let mp4 = json::read(&root.join(MP4))?;
    let mut cases = Vec::new();
    for row in list(&mkv, "cases")? {
        let input = root
            .join(".research/seek-fixtures")
            .join(text(row, "file")?);
        for target in TIMES_NS {
            for state in ["fresh", "after-eof"] {
                let mut reference = vec![target.to_string()];
                if state == "after-eof" {
                    reference.push(state.to_owned());
                }
                cases.push(Case {
                    id: format!("{}-{target}-{state}", text(row, "id")?),
                    input: input.clone(),
                    sha: text(row, "sha256")?.to_owned(),
                    candidate: vec!["mkv".into(), target.to_string(), state.into()],
                    reference,
                    matroska: true,
                });
            }
        }
    }
    for row in list(&mp4, "cases")? {
        if text(row, "request")? != "seek"
            || text(row, "schedule")? != "ordinary"
            || !MOVIES.contains(&text(row, "file")?)
        {
            continue;
        }
        let reference = list(row, "arguments")?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or("invalid ISO BMFF seek argument".to_owned())
            })
            .collect::<Result<Vec<_>>>()?;
        let [track, ticks, limit, after, ..] = reference.as_slice() else {
            return Err("missing ISO BMFF seek arguments".into());
        };
        let input = get(row, "input")?;
        cases.push(Case {
            id: text(row, "id")?.to_owned(),
            input: root
                .join(text(input, "directory")?)
                .join(text(input, "path")?),
            sha: text(input, "sha256")?.to_owned(),
            candidate: vec![
                "mp4".into(),
                track.clone(),
                ticks.clone(),
                limit.clone(),
                if after == "1" {
                    "after-eof".into()
                } else {
                    "fresh".into()
                },
            ],
            reference,
            matroska: false,
        });
    }
    if cases.is_empty() {
        return Err("no facade container seek cases".into());
    }
    Ok(cases)
}

fn run(
    root: &Path,
    binary: &Path,
    mode: &str,
    case: &Case,
    args: &[String],
) -> Result<worker::Finished> {
    let mut argv: Vec<OsString> = vec![binary.into(), mode.into(), case.input.as_os_str().into()];
    argv.extend(args.iter().map(OsString::from));
    worker::run_in(&argv, &[], Some(root), Duration::from_secs(30))?
        .ok_or_else(|| format!("{}: {mode} timed out", case.id))
}

/// One packet as `[stream, track, pts, dts, duration, key, payload]`. A reference
/// duration of 0 is FFmpeg's "unknown" and compares as absent.
fn packet(row: &Value, reference: bool, matroska: bool) -> Result<Value> {
    let common = |ours, native| get(row, if reference { native } else { ours }).cloned();
    let clock = |ours, native| common(ours, if matroska { native } else { ours });
    let key = if reference && !matroska {
        let flags = python::int(get(row, "flags")?).ok_or("missing ISO BMFF packet flags")?;
        Value::Bool(flags & 1 != 0)
    } else {
        get(row, "key")?.clone()
    };
    let duration = clock("duration", "duration_ns")?;
    let duration = if reference && duration == Value::int(0) {
        Value::Null
    } else {
        duration
    };
    Ok(Value::Array(vec![
        common("stream", if matroska { "track" } else { "stream" })?,
        if matroska {
            Value::Null
        } else {
            common("track", "pid")?
        },
        clock("pts", "pts_ns")?,
        clock("dts", "dts_ns")?,
        duration,
        key,
        get(row, "payload")?.clone(),
    ]))
}

fn rows(data: &[u8]) -> Result<Vec<Value>> {
    std::str::from_utf8(data)
        .map_err(|error| error.to_string())?
        .lines()
        .map(|line| json::parse(line).map_err(|error| error.to_string()))
        .collect()
}

fn number(value: &Value) -> Result<i128> {
    python::int(value)
        .ok_or_else(|| format!("expected integer, got {}", json::to_python(value, None)))
}

/// The reference landing (the presentation time of the first key packet of the
/// requested track) and every packet the reference's seek delivered.
fn reference_value(case: &Case, stdout: &[u8]) -> Result<(Value, Vec<Value>)> {
    let mut rows = rows(stdout)?;
    if rows.len() != 1 {
        return Err("expected one native seek trace".into());
    }
    let trace = rows.pop().ok_or("missing reference trace")?;
    if !case.matroska && number(get(&trace, "seek_result")?)? != 0 {
        return Err("the native ISO BMFF seek refused".into());
    }
    let packets = list(&trace, "packets")?
        .iter()
        .map(|row| packet(row, true, case.matroska))
        .collect::<Result<Vec<_>>>()?;
    let track = if case.matroska {
        let ordinal = list(&trace, "tracks")?
            .iter()
            .position(|track| track.get("kind") == Some(&Value::int(1)))
            .ok_or("no reference video track")?;
        Value::int(ordinal as i64)
    } else {
        Value::int(
            case.reference[0]
                .parse::<i64>()
                .map_err(|_| "invalid reference track")?,
        )
    };
    let landing = packets
        .iter()
        .filter_map(Value::as_array)
        .find_map(|fields| {
            let index = usize::from(!case.matroska);
            (fields[index] == track && fields[5] == Value::Bool(true)).then(|| fields[2].clone())
        })
        .unwrap_or(Value::Null);
    Ok((landing, packets))
}

/// The reference's own backward seek to the requested time: the Matroska worker's
/// native seek by nanoseconds, or the MOV worker's `av_seek_frame` with the
/// requested ticks on the requested track.
fn reference_seek(root: &Path, worker: &Path, case: &Case) -> Result<worker::Finished> {
    run(root, worker, "seek-trace", case, &case.reference)
}

/// Identify repeated payloads by the complete sequential packet row and its
/// occurrence. Extra preroll must be earlier than the first wanted occurrence
/// on its track, even where the wanted packet itself precedes the request.
fn matroska_preroll(
    root: &Path,
    worker: &Path,
    case: &Case,
    received: Vec<Value>,
    reference: &[Value],
    extra: usize,
) -> Result<Vec<Value>> {
    if received.len() != reference.len() + extra {
        return Err(format!(
            "{}: playback preroll packet count changed",
            case.id
        ));
    }
    let source = run(root, worker, "trace", case, &[])?;
    if source.code != 0 {
        return Err(format!("{}: sequential Matroska reference failed", case.id));
    }
    let source_rows = rows(&source.stdout)?;
    let [source] = source_rows.as_slice() else {
        return Err("expected one sequential Matroska trace".into());
    };
    let tracks = list(source, "tracks")?;
    let source = list(source, "packets")?
        .iter()
        .map(|row| packet(row, true, true))
        .collect::<Result<Vec<_>>>()?;
    let mut ordered = source.iter().enumerate();
    let wanted = reference
        .iter()
        .map(|row| {
            ordered
                .find(|(_, original)| *original == row)
                .map(|(position, _)| position)
                .ok_or("native seek packet missing from sequential trace".to_owned())
        })
        .collect::<Result<Vec<_>>>()?;

    let timing = oracle::run(root, oracle::Kind::Timing, &case.input)??;
    let timing = list(&timing, "packets")?;
    if timing.len() != source.len() {
        return Err("sequential Matroska and Block packet counts differ".into());
    }
    let ticks = |packet: &Value, field| -> Result<Value> {
        let time = get(packet, field)?;
        if time == &Value::Null {
            return Ok(Value::Null);
        }
        let denominator = number(get(time, "denominator")?)?;
        if denominator <= 0 {
            return Err("invalid source clock denominator".into());
        }
        Ok(Value::int(
            number(get(time, "numerator")?)?.div_euclid(denominator),
        ))
    };
    let source_clock = source
        .into_iter()
        .zip(timing)
        .map(|(packet, timing)| {
            let Value::Array(mut fields) = packet else {
                return Err("invalid sequential Matroska packet".into());
            };
            if &fields[0] != get(timing, "track")? {
                return Err("sequential Matroska and Block tracks differ".into());
            }
            fields[2] = ticks(timing, "pts")?;
            fields[3] = Value::Null;
            fields[4] = ticks(timing, "duration")?;
            Ok(Value::Array(fields))
        })
        .collect::<Result<Vec<_>>>()?;

    let track_at = |position: usize| -> Result<usize> {
        let fields = source_clock[position]
            .as_array()
            .ok_or("invalid Block packet")?;
        let track = usize::try_from(number(&fields[0])?).map_err(|_| "invalid Block track")?;
        (track < tracks.len())
            .then_some(track)
            .ok_or("unknown Block track".to_owned())
    };
    let mut first_wanted = vec![source_clock.len(); tracks.len()];
    for &position in &wanted {
        let track = track_at(position)?;
        first_wanted[track] = first_wanted[track].min(position);
    }
    let mut wanted = wanted.into_iter().peekable();
    let mut ordered = source_clock.iter().enumerate();
    let target: i128 = case.reference[0]
        .parse()
        .map_err(|_| "invalid Matroska seek time")?;
    let mut matched = Vec::new();
    let mut anchor = vec![None; tracks.len()];
    for packet in received {
        let (position, _) = ordered
            .find(|(_, original)| **original == packet)
            .ok_or("playback seek packet differs from the sequential Block view")?;
        if wanted.peek() == Some(&position) {
            wanted.next();
            matched.push(packet);
            continue;
        }
        let track = track_at(position)?;
        if position >= first_wanted[track]
            || !matches!(number(get(&tracks[track], "kind")?)?, 2 | 17)
        {
            return Err("extra packet lies outside the playback preroll".into());
        }
        let fields = packet.as_array().ok_or("invalid preroll packet")?;
        if fields[2] == Value::Null {
            let start =
                anchor[track].ok_or("untimestamped preroll lacks its timestamped packet")?;
            for previous in start + 1..position {
                if track_at(previous)? == track
                    && source_clock[previous]
                        .as_array()
                        .is_some_and(|row| row[2] != Value::Null)
                {
                    return Err("untimestamped preroll skips a source timestamp".into());
                }
            }
        } else if number(&fields[2])? < target {
            anchor[track] = Some(position);
        } else {
            return Err("extra preroll reaches the presentation target".into());
        }
    }
    if wanted.next().is_some() {
        return Err("playback seek dropped a packet from the native suffix".into());
    }
    Ok(matched)
}

/// A packet with its payload replaced by the payload's SHA-256, as reports keep it.
fn summarised(row: Option<&Value>) -> Value {
    row.and_then(Value::as_array).map_or(Value::Null, |fields| {
        let mut fields = fields.to_vec();
        if let Some(Value::String(payload)) = fields.last_mut() {
            *payload = sha256::hex(payload.as_bytes());
        }
        Value::Array(fields)
    })
}

/// Every difference between two aligned packet lists: per field, `F` where both
/// carry a different value, `F+` where only the candidate carries one and `F-`
/// where only the reference does, and `count` where the lists differ in length.
/// Returns the kinds, how many packets differ and the first differing pair.
fn packet_differences(
    candidate: &[Value],
    reference: &[Value],
    kinds: &mut BTreeSet<String>,
) -> (usize, Value) {
    let mut differing = 0;
    let mut first = Value::Null;
    for index in 0..candidate.len().max(reference.len()) {
        let (ours, theirs) = (candidate.get(index), reference.get(index));
        if ours == theirs {
            continue;
        }
        differing += 1;
        if first == Value::Null {
            first = Value::object([
                ("index", Value::int(index as i64)),
                ("candidate", summarised(ours)),
                ("reference", summarised(theirs)),
            ]);
        }
        let (Some(ours), Some(theirs)) = (
            ours.and_then(Value::as_array),
            theirs.and_then(Value::as_array),
        ) else {
            kinds.insert("count".into());
            continue;
        };
        for ((name, a), b) in FIELDS.iter().zip(ours).zip(theirs) {
            if a == b {
                continue;
            }
            kinds.insert(match (a, b) {
                (_, Value::Null) => format!("{name}+"),
                (Value::Null, _) => format!("{name}-"),
                _ => (*name).to_owned(),
            });
        }
    }
    (differing, first)
}

/// A case's result: every difference between the candidate's receipt and packets
/// and the reference's landing and packets, and the SHA-256 of the candidate's
/// complete output (receipt and every packet it delivered, preroll included).
fn compare(
    root: &Path,
    worker: &Path,
    case: &Case,
    ours: &worker::Finished,
    landing: &Value,
    reference: &[Value],
    extra: usize,
) -> Result<Value> {
    if ours.code != 0 {
        return Err(format!(
            "{}: facade seek worker exited {}: {}",
            case.id,
            ours.code,
            String::from_utf8_lossy(&ours.stderr)
        ));
    }
    let rows = rows(&ours.stdout)?;
    let receipt = rows
        .iter()
        .find_map(|row| row.get("receipt"))
        .cloned()
        .unwrap_or(Value::Null);
    let received = rows
        .iter()
        .filter(|row| row.get("payload").is_some())
        .map(|row| packet(row, false, case.matroska))
        .collect::<Result<Vec<_>>>()?;
    let output = sha256::hex(
        json::to_python(
            &Value::Array(vec![receipt.clone(), Value::Array(received.clone())]),
            None,
        )
        .as_bytes(),
    );
    let packet_count = received.len();
    let mut kinds = BTreeSet::new();
    let received = if extra != 0 {
        kinds.insert("preroll".to_owned());
        matroska_preroll(root, worker, case, received, reference, extra)?
    } else {
        received
    };
    let target = Value::int(
        case.reference[usize::from(!case.matroska)]
            .parse::<i64>()
            .map_err(|_| "invalid reference target")?,
    );
    let expected_discard = if case.matroska { &target } else { landing };
    match receipt.as_array() {
        Some([requested, actual, discard]) => {
            for (name, ours, theirs) in [
                ("requested", requested, &target),
                ("landing", actual, landing),
                ("discard", discard, expected_discard),
            ] {
                if ours != theirs {
                    kinds.insert(format!("receipt-{name}"));
                }
            }
        }
        _ => {
            kinds.insert("receipt".to_owned());
        }
    }
    let (differing, first) = packet_differences(&received, reference, &mut kinds);
    Ok(Value::object([
        ("id", Value::str(&case.id)),
        (
            "status",
            Value::str(if kinds.is_empty() { "pass" } else { "fail" }),
        ),
        ("differences", Value::strings(kinds)),
        ("differing_packets", Value::int(differing as i64)),
        ("candidate_sha256", Value::str(output)),
        ("candidate_exit", Value::int(ours.code)),
        (
            "candidate_error",
            Value::str(String::from_utf8_lossy(&ours.stderr)),
        ),
        ("candidate_receipt", receipt),
        ("reference_landing", landing.clone()),
        ("candidate_packets", Value::int(packet_count as i64)),
        ("preroll", Value::int(extra as i64)),
        ("reference_packets", Value::int(reference.len() as i64)),
        ("first_difference", first),
    ]))
}

/// The listed cases and the differences each must show: open cases and the cases
/// of every other list of the registry, with the `differences` their entry declares.
/// A case with two causes is listed in two lists and must show the differences of
/// both entries, and no other.
fn declared(previous: &Value, cases: &[Case]) -> Result<BTreeMap<String, Vec<String>>> {
    use crate::harness::registry::Form;
    let mut listed: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let differences = |entry: &Value, what: &str| -> Result<BTreeSet<String>> {
        let kinds = list(entry, "differences")
            .map_err(|_| format!("{what} declares no differences"))?
            .iter()
            .map(|kind| kind.as_str().map(str::to_owned))
            .collect::<Option<BTreeSet<_>>>()
            .ok_or_else(|| format!("{what}: a difference is not text"))?;
        if kinds.is_empty() {
            return Err(format!("{what} declares no differences"));
        }
        Ok(kinds)
    };
    for entry in previous
        .get("open_cases")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        let id = text(entry, "id")?;
        listed.insert(id.to_owned(), differences(entry, id)?);
    }
    // The lists that group cases, without the `differences` the registry does not
    // know, and scoped by the gate's targets.
    let mut declarations = vec![(
        "required_targets".to_owned(),
        Value::Array(crate::gate::TARGETS.into_iter().map(Value::str).collect()),
    )];
    for form in Form::GROUPS {
        let entries = previous
            .get(form.list())
            .and_then(Value::as_array)
            .unwrap_or_default();
        let entries = entries.iter().map(|entry| match entry {
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .filter(|(key, _)| key != "differences")
                    .cloned()
                    .collect(),
            ),
            other => other.clone(),
        });
        declarations.push((form.list().to_owned(), Value::Array(entries.collect())));
    }
    let manifest = Value::object([(
        "cases",
        Value::Array(
            cases
                .iter()
                .map(|case| Value::object([("id", Value::str(&case.id))]))
                .collect(),
        ),
    )]);
    let open = report::open(
        &Value::Object(declarations),
        &manifest,
        &report::gate_target()?,
    )?;
    for form in Form::GROUPS {
        for entry in previous
            .get(form.list())
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            let kinds = differences(entry, form.list())?;
            for id in list(entry, "cases")? {
                let id = id.as_str().ok_or("a listed case is not an id")?;
                if open.contains(id) {
                    listed
                        .entry(id.to_owned())
                        .or_default()
                        .extend(kinds.clone());
                }
            }
        }
    }
    Ok(listed
        .into_iter()
        .map(|(id, kinds)| (id, kinds.into_iter().collect()))
        .collect())
}

fn verdict(
    report: &Value,
    declared: Option<&Vec<String>>,
    pinned: Option<&Value>,
) -> Option<String> {
    let kinds: Vec<&str> = report
        .get("differences")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    match declared {
        None if kinds.is_empty() => None,
        None => Some(format!("unlisted difference {kinds:?}")),
        Some(_) if kinds.is_empty() => Some("listed but passes".into()),
        Some(expected) if kinds != *expected => {
            Some(format!("differs by {kinds:?}, declared {expected:?}"))
        }
        Some(_) if pinned != report.get("candidate_sha256") => {
            Some("candidate output differs from its pin".into())
        }
        Some(_) => None,
    }
}

pub(super) fn main(root: &Path, args: &[String]) -> Result<()> {
    let (record, candidate, output, selected) = match args {
        [record, worker, flag, selected @ ..]
            if record == "--record" && flag == "--cases" && !selected.is_empty() =>
        {
            (true, root.join(worker), None, selected)
        }
        [record, worker] if record == "--record" => (true, root.join(worker), None, &[][..]),
        [worker, path] if worker != "--record" => {
            (false, root.join(worker), Some(root.join(path)), &[][..])
        }
        _ => return Err(USAGE.into()),
    };
    let file = root.join(MANIFEST);
    let previous = if file.exists() {
        json::read(&file)?
    } else {
        Value::Null
    };
    let cases = cases(root)?;
    let mut listed = declared(&previous, &cases)?;
    let mut pins = Vec::new();
    let mut candidate_pins = Vec::new();
    let mut reports = Vec::new();
    let mut bad = Vec::new();
    let mut as_listed = 0;
    let mut pending: BTreeSet<_> = selected.iter().map(String::as_str).collect();
    for case in cases {
        let declared = listed.remove(&case.id);
        if !selected.is_empty() && !pending.remove(case.id.as_str()) {
            pins.push((
                case.id.clone(),
                get(get(&previous, "pins")?, &case.id)?.clone(),
            ));
            if declared.is_some() {
                let pin = get(get(&previous, "candidate_pins")?, &case.id)?.clone();
                candidate_pins.push((case.id, pin));
            }
            continue;
        }
        if sha256::file_hex(&case.input)
            .map_err(|error| format!("{}: {error}", case.input.display()))?
            != case.sha
        {
            return Err(format!("{}: input pin mismatch", case.id));
        }
        let reference = root.join(if case.matroska {
            "target/harness/ffmpeg-worker"
        } else {
            "target/harness/ffmpeg-isobmff-worker"
        });
        let native = reference_seek(root, &reference, &case)?;
        if native.code != 0 {
            return Err(format!(
                "{}: reference exited {}: {}",
                case.id,
                native.code,
                String::from_utf8_lossy(&native.stderr)
            ));
        }
        let (landing, packets) = reference_value(&case, &native.stdout)?;
        let pin = Value::str(sha256::hex(
            json::to_python(
                &Value::Array(vec![landing.clone(), Value::Array(packets.clone())]),
                None,
            )
            .as_bytes(),
        ));
        if !record && previous.get("pins").and_then(|rows| rows.get(&case.id)) != Some(&pin) {
            return Err(format!(
                "{}: native seek trace differs from its pin",
                case.id
            ));
        }
        let ours = run(root, &candidate, "any-seek-trace", &case, &case.candidate)?;
        let extra = previous
            .get("playback_preroll")
            .and_then(|cases| cases.get(&case.id))
            .map_or(Ok(0), |extra| {
                usize::try_from(number(extra)?)
                    .map_err(|_| "invalid expected preroll count".to_owned())
            })?;
        let mut report = compare(root, &reference, &case, &ours, &landing, &packets, extra)?;
        let output = get(&report, "candidate_sha256")?.clone();
        let pinned = if record {
            pins.push((case.id.clone(), pin));
            if declared.is_some() {
                candidate_pins.push((case.id.clone(), output.clone()));
            }
            Some(&output)
        } else {
            previous
                .get("candidate_pins")
                .and_then(|rows| rows.get(&case.id))
        };
        match verdict(&report, declared.as_ref(), pinned) {
            Some(problem) => bad.push(format!("{}: {problem}", case.id)),
            None if declared.is_some() => as_listed += 1,
            None => (),
        }
        validation::put(&mut report, "listed", Value::Bool(declared.is_some()))?;
        reports.push(report);
    }
    if !pending.is_empty() {
        return Err(format!("unknown selected seek case IDs: {pending:?}"));
    }
    if !listed.is_empty() {
        return Err(format!(
            "listed cases that are not seek cases: {:?}",
            listed.keys().collect::<Vec<_>>()
        ));
    }
    if record {
        let mut recorded = if previous == Value::Null {
            Value::object([
                ("schema", Value::int(1)),
                ("open_cases", Value::Array(Vec::new())),
                ("open_unmeasured", Value::Array(Vec::new())),
            ])
        } else {
            previous
        };
        validation::put(&mut recorded, "pins", Value::Object(pins))?;
        validation::put(
            &mut recorded,
            "candidate_pins",
            Value::Object(candidate_pins),
        )?;
        json::write(&file, &recorded)?;
        println!("recorded {}", file.display());
        if !bad.is_empty() {
            println!(
                "cases that do not match their listing:\n  {}",
                bad.join("\n  ")
            );
        }
        return Ok(());
    }
    let output = output.ok_or(USAGE)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let total = reports.len();
    let passed = reports
        .iter()
        .filter(|row| row.get("status") == Some(&Value::str("pass")))
        .count();
    json::write(&output, &Value::Array(reports))?;
    println!(
        "{passed}/{total} facade container seek cases matched; {as_listed} differ exactly as listed"
    );
    if !bad.is_empty() {
        return Err(format!(
            "seek cases that do not match their listing:\n  {}",
            bad.join("\n  ")
        ));
    }
    Ok(())
}
