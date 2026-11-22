//! Compares facade seek receipts and packet suffixes with the existing native trace operations.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::harness::worker;
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

fn compare(
    case: &Case,
    ours: &worker::Finished,
    landing: &Value,
    reference: &[Value],
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
        ("candidate_packets", Value::int(received.len() as i64)),
        ("reference_packets", Value::int(reference.len() as i64)),
        ("first_difference", first_diff),
    ]))
}

pub(super) fn main(root: &Path, args: &[String]) -> Result<()> {
    let (record, candidate, output) = match args {
        [record] if record == "--record" => (true, PathBuf::new(), None),
        [worker, path] => (false, root.join(worker), Some(root.join(path))),
        _ => return Err("usage: cargo xtask play-views seek --record | WORKER OUTPUT".into()),
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
        .map(|row| text(row, "id"))
        .collect::<Result<BTreeSet<_>>>()?;
    let mut pins = Vec::new();
    let mut reports = Vec::new();
    let mut bad = Vec::new();
    for case in cases(root)? {
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
        let native = run(root, &reference, "seek-trace", &case, &case.reference)?;
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
        let report = compare(&case, &ours, &landing, &packets)?;
        let pass = text(&report, "status")? == "pass";
        let listed = open_ids.remove(case.id.as_str());
        if pass == listed {
            bad.push(case.id.clone());
        }
        reports.push(report);
    }
    if record {
        json::write(
            &file,
            &Value::object([
                ("schema", Value::int(1)),
                ("pins", Value::Object(pins)),
                ("open_cases", Value::Array(open.to_vec())),
                (
                    "open_unmeasured",
                    previous
                        .get("open_unmeasured")
                        .cloned()
                        .unwrap_or_else(|| Value::Array(Vec::new())),
                ),
            ]),
        )?;
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
