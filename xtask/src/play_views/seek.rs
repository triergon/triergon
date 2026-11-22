//! Compares facade seek receipts and packet suffixes with the existing native trace operations.

use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
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

fn reference_seek(
    root: &Path,
    worker: &Path,
    case: &Case,
    scans: &mut BTreeMap<PathBuf, Value>,
) -> Result<worker::Finished> {
    if case.matroska {
        return run(root, worker, "seek-trace", case, &case.reference);
    }
    let scan = match scans.entry(case.input.clone()) {
        Entry::Occupied(scan) => scan.into_mut(),
        Entry::Vacant(scan) => {
            let native = run(root, worker, "open-trace", case, &case.reference[4..])?;
            let mut scan_rows = rows(&native.stdout)?;
            if native.code != 0 || scan_rows.len() != 1 {
                return Err(format!("{}: incomplete native source scan", case.id));
            }
            let complete = scan_rows.pop().ok_or("missing native source scan")?;
            if get(&complete, "confirmed_eof")? != &Value::Bool(true) {
                return Err(format!("{}: native source scan did not reach EOF", case.id));
            }
            scan.insert(complete)
        }
    };
    let [track, pts, ..] = case.reference.as_slice() else {
        return Err("missing reference seek request".into());
    };
    let track: i128 = track.parse().map_err(|_| "invalid reference track")?;
    let target: i128 = pts.parse().map_err(|_| "invalid reference target")?;
    let (mut first, mut best) = (None, None);
    for packet in list(scan, "packets")? {
        if number(get(packet, "pid")?)? != track || number(get(packet, "flags")?)? & 1 == 0 {
            continue;
        }
        let (Some(pts), Some(dts)) = (
            python::int(get(packet, "pts")?),
            python::int(get(packet, "dts")?),
        ) else {
            continue;
        };
        first.get_or_insert(dts);
        if pts <= target && best.is_none_or(|(previous, _)| pts >= previous) {
            best = Some((pts, dts));
        }
    }
    // The facade asks for a presentation time. The native backward seek takes
    // a decode tick. Seek to the DTS of the sync sample chosen on the source timeline.
    let dts = best
        .map(|(_, dts)| dts)
        .or(first)
        .ok_or("no timed reference sync sample")?;
    let mut arguments = case.reference.clone();
    arguments[1] = dts.to_string();
    run(root, worker, "seek-trace", case, &arguments)
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
    let receipt = rows.iter().find_map(|row| row.get("receipt"));
    let received = rows
        .iter()
        .filter(|row| row.get("payload").is_some())
        .map(|row| packet(row, false, case.matroska))
        .collect::<Result<Vec<_>>>()?;
    let packet_count = received.len();
    let received = if extra != 0 {
        matroska_preroll(root, worker, case, received, reference, extra)?
    } else {
        received
    };
    let receipt_matches =
        if let Some([requested, actual, discard]) = receipt.and_then(Value::as_array) {
            let target = case.reference[usize::from(!case.matroska)]
                .parse::<i64>()
                .map_err(|_| "invalid reference target")?;
            let expected_discard = if case.matroska { requested } else { actual };
            *requested == Value::int(target) && actual == landing && discard == expected_discard
        } else {
            false
        };
    let first_diff = received
        .iter()
        .map(Some)
        .chain(std::iter::once(None))
        .zip(reference.iter().map(Some).chain(std::iter::once(None)))
        .enumerate()
        .find(|(_, (a, b))| a != b)
        .map_or(Value::Null, |(index, (a, b))| {
            // Complete packet payloads are consumed above. Keep each first differing
            // packet's fields and payload hash in the summary.
            let row = |row: Option<&Value>| {
                row.and_then(Value::as_array).map_or(Value::Null, |fields| {
                    let mut fields = fields.to_vec();
                    if let Some(Value::String(payload)) = fields.last_mut() {
                        *payload = sha256::hex(payload.as_bytes());
                    }
                    Value::Array(fields)
                })
            };
            Value::object([
                ("index", Value::int(index as i64)),
                ("candidate", row(a)),
                ("reference", row(b)),
            ])
        });
    let pass = receipt_matches && first_diff == Value::Null;
    Ok(Value::object([
        ("id", Value::str(&case.id)),
        ("status", Value::str(if pass { "pass" } else { "fail" })),
        ("candidate_exit", Value::int(ours.code)),
        (
            "candidate_error",
            Value::str(String::from_utf8_lossy(&ours.stderr)),
        ),
        ("candidate_receipt", receipt.cloned().unwrap_or(Value::Null)),
        ("reference_landing", landing.clone()),
        ("candidate_packets", Value::int(packet_count as i64)),
        ("reference_packets", Value::int(reference.len() as i64)),
        ("first_difference", first_diff),
    ]))
}

pub(super) fn main(root: &Path, args: &[String]) -> Result<()> {
    let (record, candidate, output, selected) = match args {
        [record] if record == "--record" => (true, PathBuf::new(), None, &[][..]),
        [record, flag, selected @ ..]
            if record == "--record" && flag == "--cases" && !selected.is_empty() =>
        {
            (true, PathBuf::new(), None, selected)
        }
        [worker, path] => (false, root.join(worker), Some(root.join(path)), &[][..]),
        _ => {
            return Err(
                "usage: cargo xtask play-views seek --record [--cases ID...] | WORKER OUTPUT"
                    .into(),
            );
        }
    };
    let file = root.join(MANIFEST);
    let previous = if file.exists() {
        json::read(&file)?
    } else {
        Value::Null
    };
    let open = previous
        .get("open_cases")
        .and_then(Value::as_array)
        .unwrap_or_default();
    let mut open_ids = open
        .iter()
        .map(|row| text(row, "id").map(str::to_owned))
        .collect::<Result<BTreeSet<_>>>()?;
    let cases = cases(root)?;
    if let Some(deviations) = previous.get("open_deviations") {
        let declarations = Value::object([
            ("open_deviations", deviations.clone()),
            (
                "required_targets",
                Value::Array(crate::gate::TARGETS.into_iter().map(Value::str).collect()),
            ),
        ]);
        let manifest = Value::object([(
            "cases",
            Value::Array(
                cases
                    .iter()
                    .map(|case| Value::object([("id", Value::str(&case.id))]))
                    .collect(),
            ),
        )]);
        for id in report::open_cases(&declarations, &manifest, &report::gate_target()?)? {
            if !open_ids.insert(id.clone()) {
                return Err(format!("{id}: listed as both an open case and a deviation"));
            }
        }
    }
    let mut pins = Vec::new();
    let mut reports = Vec::new();
    let mut bad = Vec::new();
    let mut pending: BTreeSet<_> = selected.iter().map(String::as_str).collect();
    let mut scans = BTreeMap::new();
    for case in cases {
        if !selected.is_empty() && !pending.remove(case.id.as_str()) {
            let pin = get(get(&previous, "pins")?, &case.id)?.clone();
            pins.push((case.id, pin));
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
        let native = reference_seek(root, &reference, &case, &mut scans)?;
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
        if record {
            pins.push((case.id, pin));
            continue;
        }
        if previous.get("pins").and_then(|rows| rows.get(&case.id)) != Some(&pin) {
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
        let report = compare(root, &reference, &case, &ours, &landing, &packets, extra)?;
        let pass = text(&report, "status")? == "pass";
        let listed = open_ids.remove(case.id.as_str());
        if pass == listed {
            bad.push(case.id.clone());
        }
        reports.push(report);
    }
    if record {
        if !pending.is_empty() {
            return Err(format!("unknown selected seek case IDs: {pending:?}"));
        }
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
        json::write(&file, &recorded)?;
        println!("recorded {}", file.display());
    } else if let Some(output) = output {
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let total = reports.len();
        let passed = reports
            .iter()
            .filter(|row| row.get("status") == Some(&Value::str("pass")))
            .count();
        json::write(&output, &Value::Array(reports))?;
        println!("{passed}/{total} facade container seek cases matched");
        if !bad.is_empty() || !open_ids.is_empty() {
            return Err(format!(
                "unregistered, closed or absent seek cases: {bad:?} {open_ids:?}"
            ));
        }
    }
    Ok(())
}
