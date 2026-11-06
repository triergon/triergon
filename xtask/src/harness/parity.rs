//! Correctness of one case: the candidate and reference traces must agree, the
//! candidate's source view must match the independent oracles, short reads must
//! not change the result, and seeks must land on the reference's key frames.
//! Every raw worker output and oracle expectation is written next to the result.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::diff::differences;
use super::oracle::{self, Kind};
use super::worker::{self, DEFAULT_TIMEOUT, arg, data, ok, status};
use crate::json::Value;

/// The workers and files of one parity run.
pub struct Run<'a> {
    pub root: &'a Path,
    pub candidate: &'a Path,
    pub reference: &'a Path,
    pub input: &'a Path,
    pub raw: PathBuf,
}

/// An approved, input-bound comparison rule (see `comparison-rules.json`).
pub struct Comparison<'a> {
    pub rule: &'a Value,
    pub native_reference: &'a Path,
}

impl Run<'_> {
    fn exec(&self, worker: &Path, op: &str, extra: &[OsString]) -> Value {
        let mut argv = vec![
            worker.as_os_str().to_owned(),
            op.into(),
            self.input.as_os_str().to_owned(),
        ];
        argv.extend_from_slice(extra);
        worker::execute(&argv, DEFAULT_TIMEOUT)
    }

    fn candidate(&self, op: &str, extra: &[OsString]) -> Value {
        self.exec(self.candidate, op, extra)
    }

    fn write(&self, name: &str, value: &Value) -> Result<(), String> {
        crate::corpus::write_json(&self.raw.join(format!("{name}.json")), value)
    }

    fn oracle(&self, kind: Kind) -> Result<Result<Value, String>, String> {
        oracle::run(self.root, kind, self.input)
    }
}

fn error_list(message: &str) -> Value {
    Value::Array(vec![Value::object([("error", Value::str(message))])])
}

fn status_of(pass: bool) -> Value {
    Value::str(if pass { "pass" } else { "mismatch" })
}

fn set(result: &mut Value, key: &str, value: Value) {
    if let Value::Object(members) = result {
        match members.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => members.push((key.to_owned(), value)),
        }
    }
}

/// The packet view of the content oracle: codec private data and frame payloads.
pub fn projected_content(trace: &Value) -> Option<Value> {
    let private = trace
        .get("tracks")?
        .as_array()?
        .iter()
        .map(|t| t.get("private").cloned())
        .collect::<Option<Vec<_>>>()?;
    let frames = trace
        .get("packets")?
        .as_array()?
        .iter()
        .map(|p| {
            Some(Value::object([
                ("track", p.get("track")?.clone()),
                ("payload", p.get("payload")?.clone()),
            ]))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Value::object([
        ("private", Value::Array(private)),
        ("frames", Value::Array(frames)),
    ]))
}

/// The raw native output and the candidate's storage view may differ only on
/// the reviewed paths, and the native output must be the reviewed one.
pub(super) fn review_native_difference(
    storage: &Value,
    native: &Value,
    rule: &Value,
) -> (Value, Value, Value) {
    let delta = differences(storage, native);
    let actual = crate::sha256::hex(crate::json::canonical(native).as_bytes());
    let expected = rule
        .get("raw_native_output_sha256")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let paths: Vec<Value> = delta
        .iter()
        .filter_map(|d| d.get("path").cloned())
        .collect();
    let reviewed = !expected.is_empty()
        && actual == expected
        && rule
            .get("raw_native_difference_paths")
            .is_some_and(|p| super::diff::equal(p, &Value::Array(paths)));
    let disposition = Value::object([
        (
            "status",
            Value::str(if reviewed {
                "reviewed"
            } else {
                "unexpected-native-difference"
            }),
        ),
        ("rule", rule.get("id").cloned().unwrap_or(Value::Null)),
    ]);
    (Value::Array(delta), disposition, Value::str(actual))
}

pub fn parity(
    run: &Run<'_>,
    sequential_required: bool,
    comparison: Option<&Comparison<'_>>,
    source_content: bool,
    prefix: &str,
) -> Result<Value, String> {
    let trace = format!("{prefix}trace");
    let storage = run.candidate(&trace, &[]);
    let left = if comparison.is_some() {
        run.candidate("codec-trace", &[])
    } else {
        storage.clone()
    };
    let right = run.exec(
        run.reference,
        if comparison.is_some() {
            "codec-trace"
        } else {
            &trace
        },
        &[],
    );
    std::fs::create_dir_all(&run.raw).map_err(|e| format!("{}: {e}", run.raw.display()))?;
    run.write("candidate", &left)?;
    run.write("reference", &right)?;
    run.write("source-frame-candidate", &storage)?;
    if !(ok(&left) && ok(&right) && ok(&storage)) {
        return Ok(Value::object([
            ("status", Value::str("worker-failure")),
            ("candidate", Value::str(status(&left))),
            ("reference", Value::str(status(&right))),
        ]));
    }
    let diff = differences(data(&left), data(&right));
    let (mut native_diff, mut disposition, mut native_sha) =
        (Value::Array(vec![]), Value::Null, Value::Null);
    if let Some(comparison) = comparison {
        let native = run.exec(comparison.native_reference, "trace", &[]);
        run.write("raw-native-reference", &native)?;
        if !ok(&native) {
            return Ok(Value::object([
                ("status", Value::str("worker-failure")),
                ("reference", Value::str("raw native operation failed")),
            ]));
        }
        (native_diff, disposition, native_sha) =
            review_native_difference(data(&storage), data(&native), comparison.rule);
    }
    let (mut content_status, mut content_diff) = (Value::str("not-required"), Value::Array(vec![]));
    if source_content || comparison.is_some() {
        let expected = match (
            run.oracle(Kind::Content)?,
            projected_content(data(&storage)),
        ) {
            (Ok(expected), Some(actual)) => {
                let delta = differences(&actual, &expected);
                content_status = status_of(delta.is_empty());
                content_diff = Value::Array(delta);
                expected
            }
            (Err(error), _) => {
                content_status = Value::str("oracle-error");
                content_diff = error_list(&error);
                Value::Null
            }
            (Ok(_), None) => {
                content_status = Value::str("oracle-error");
                content_diff = error_list("'tracks'");
                Value::Null
            }
        };
        run.write("source-content-oracle", &expected)?;
    }
    let source = run.candidate("trace-source", &[]);
    let (source_status, source_diff, oracle) = match run.oracle(Kind::Timing)? {
        Ok(oracle) => {
            let delta = differences(data(&source), &oracle);
            (
                status_of(ok(&source) && delta.is_empty()),
                Value::Array(delta),
                oracle,
            )
        }
        Err(error) => (Value::str("oracle-error"), error_list(&error), Value::Null),
    };
    run.write("source-candidate", &source)?;
    run.write("source-oracle", &oracle)?;
    let mut streamed = Vec::new();
    let chunks: &[u32] = if sequential_required {
        &[1, 7, 4096]
    } else {
        &[]
    };
    for &chunk in chunks {
        let size = [arg(chunk)];
        let short = run.candidate(&format!("{prefix}trace-short"), &size);
        let mut mismatch = !ok(&short) || !differences(data(&short), data(&storage)).is_empty();
        if comparison.is_some() {
            let projected = run.candidate("codec-trace-short", &size);
            mismatch |= !ok(&projected) || !differences(data(&projected), data(&left)).is_empty();
        }
        let exact = run.candidate("trace-source-short", &size);
        mismatch |= !ok(&exact) || !differences(data(&exact), &oracle).is_empty();
        streamed.push(Value::object([
            ("chunk", Value::int(chunk)),
            ("status", status_of(!mismatch)),
        ]));
    }
    let reference = data(&right);
    let packets = reference
        .get("packets")
        .and_then(Value::as_array)
        .unwrap_or_default();
    let video = reference
        .get("tracks")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .position(|t| {
            t.get("kind")
                .is_some_and(|k| super::diff::equal(k, &Value::int(1)))
        });
    let mut seeks = Vec::new();
    if let Some(video) = video {
        for target in [0i64, 250_000_000, 1_500_000_000, 20_000_000_000] {
            let pts = |p: &Value| {
                p.get("pts_ns").and_then(|v| match v {
                    Value::Number(n) => n.as_i128(),
                    _ => None,
                })
            };
            let mut best: Option<&Value> = None;
            for p in packets {
                let on_video = p
                    .get("track")
                    .is_some_and(|t| super::diff::equal(t, &Value::int(video as i64)));
                let key = p.get("key").is_some_and(Value::truthy);
                if on_video
                    && key
                    && pts(p).is_some_and(|t| t <= i128::from(target))
                    && best.is_none_or(|b| pts(p) > pts(b))
                {
                    best = Some(p);
                }
            }
            let Some(expected) = best else {
                seeks.push(Value::object([
                    ("target_ns", Value::int(target)),
                    ("status", Value::str("no-previous-keyframe")),
                ]));
                continue;
            };
            let actual = run.candidate("seek", &[arg(target)]);
            let field = |k: &str| expected.get(k).cloned().unwrap_or(Value::Null);
            let wanted = Value::object([
                ("actual_ns", field("pts_ns")),
                ("track", field("track")),
                ("pts_ns", field("pts_ns")),
                ("key", field("key")),
                ("payload", field("payload")),
            ]);
            let delta = differences(data(&actual), &wanted);
            seeks.push(Value::object([
                ("target_ns", Value::int(target)),
                ("status", status_of(ok(&actual) && delta.is_empty())),
                ("differences", Value::Array(delta)),
            ]));
        }
    }
    let field_is = |v: &Value, key: &str, wanted: &[&str]| {
        v.get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| wanted.contains(&s))
    };
    let mut passed = source_status.as_str() == Some("pass")
        && matches!(content_status.as_str(), Some("pass" | "not-required"))
        && diff.is_empty()
        && streamed.iter().all(|s| field_is(s, "status", &["pass"]))
        && seeks
            .iter()
            .all(|s| field_is(s, "status", &["pass", "no-previous-keyframe"]));
    if comparison.is_some() {
        passed &= field_is(&disposition, "status", &["reviewed"]);
    }
    let bytes: usize = packets
        .iter()
        .map(|p| {
            p.get("payload")
                .and_then(Value::as_str)
                .map_or(0, |s| s.len() / 2)
        })
        .sum();
    Ok(Value::object([
        ("status", status_of(passed)),
        ("differences", Value::Array(diff)),
        ("source_timing_status", source_status),
        ("source_timing_differences", source_diff),
        ("source_content_status", content_status),
        ("source_content_differences", content_diff),
        (
            "comparison_rule",
            comparison
                .and_then(|c| c.rule.get("id").cloned())
                .unwrap_or(Value::Null),
        ),
        ("raw_native_differences", native_diff),
        ("raw_native_disposition", disposition),
        ("raw_native_output_sha256", native_sha),
        ("short_reads", Value::Array(streamed)),
        ("seeks_after_eof", Value::Array(seeks)),
        ("packets", Value::int(packets.len() as i64)),
        ("bytes", Value::int(bytes as i64)),
        ("sequential_required", Value::Bool(sequential_required)),
    ]))
}

pub fn catalogue_parity(
    run: &Run<'_>,
    sequential_required: bool,
    comparison: Option<&Comparison<'_>>,
    content: bool,
) -> Result<Value, String> {
    let mut result = parity(run, sequential_required, comparison, content, "catalogue-")?;
    let actual = run.candidate("catalogue-source", &[]);
    let colour = run.candidate("metadata-source", &[]);
    let (expected, colour_expected, delta, colour_delta, state, colour_state) =
        match (run.oracle(Kind::Catalogue)?, run.oracle(Kind::Colour)?) {
            (Ok(expected), Ok(colour_expected)) => {
                let delta = differences(data(&actual), &expected);
                let colour_delta = differences(data(&colour), &colour_expected);
                let state = status_of(ok(&actual) && delta.is_empty());
                let colour_state = status_of(ok(&colour) && colour_delta.is_empty());
                (
                    expected,
                    colour_expected,
                    Value::Array(delta),
                    Value::Array(colour_delta),
                    state,
                    colour_state,
                )
            }
            (Err(error), _) | (Ok(_), Err(error)) => {
                let errors = error_list(&error);
                let failed = Value::str("oracle-error");
                (
                    Value::Null,
                    Value::Null,
                    errors.clone(),
                    errors,
                    failed.clone(),
                    failed,
                )
            }
        };
    run.write("catalogue-source", &actual)?;
    run.write("catalogue-oracle", &expected)?;
    run.write("colour-source", &colour)?;
    run.write("colour-oracle", &colour_expected)?;
    let mismatch = state.as_str() != Some("pass") || colour_state.as_str() != Some("pass");
    set(&mut result, "source_catalogue_status", state);
    set(&mut result, "source_catalogue_differences", delta);
    set(&mut result, "source_colour_status", colour_state);
    set(&mut result, "source_colour_differences", colour_delta);
    if mismatch {
        set(&mut result, "status", Value::str("mismatch"));
    }
    Ok(result)
}

pub fn colour_parity(
    run: &Run<'_>,
    sequential_required: bool,
    comparison: Option<&Comparison<'_>>,
    content: bool,
) -> Result<Value, String> {
    let mut result = parity(run, sequential_required, comparison, content, "")?;
    let left = run.candidate("metadata-trace", &[]);
    let right = run.exec(run.reference, "metadata-trace", &[]);
    let source = run.candidate("metadata-source", &[]);
    let (oracle, delta, source_status) = match run.oracle(Kind::Colour)? {
        Ok(oracle) => {
            let delta = differences(data(&source), &oracle);
            let state = status_of(ok(&source) && delta.is_empty());
            (oracle, Value::Array(delta), state)
        }
        Err(error) => (Value::Null, error_list(&error), Value::str("oracle-error")),
    };
    let native_delta = differences(data(&left), data(&right));
    let native_status = status_of(ok(&left) && ok(&right) && native_delta.is_empty());
    let mut shorts = Vec::new();
    for chunk in [1u32, 7, 4096] {
        let actual = run.candidate("metadata-trace-short", &[arg(chunk)]);
        let pass = ok(&actual) && differences(data(&actual), data(&left)).is_empty();
        shorts.push(Value::object([
            ("chunk", Value::int(chunk)),
            ("status", status_of(pass)),
        ]));
    }
    run.write("colour-candidate", &left)?;
    run.write("colour-native", &right)?;
    run.write("colour-source", &source)?;
    run.write("colour-oracle", &oracle)?;
    let mismatch = source_status.as_str() != Some("pass")
        || native_status.as_str() != Some("pass")
        || shorts
            .iter()
            .any(|s| s.get("status").and_then(Value::as_str) != Some("pass"));
    set(&mut result, "source_colour_status", source_status);
    set(&mut result, "source_colour_differences", delta);
    set(&mut result, "colour_reference_status", native_status);
    set(
        &mut result,
        "colour_reference_differences",
        Value::Array(native_delta),
    );
    set(&mut result, "colour_short_reads", Value::Array(shorts));
    if mismatch {
        set(&mut result, "status", Value::str("mismatch"));
    }
    Ok(result)
}

pub fn seek_parity(
    run: &Run<'_>,
    sequential_required: bool,
    comparison: Option<&Comparison<'_>>,
    content: bool,
) -> Result<Value, String> {
    let mut result = parity(run, sequential_required, comparison, content, "")?;
    let mut rows = Vec::new();
    for target in [
        0i64,
        250_000_000,
        2_500_000_000,
        7_500_000_000,
        15_500_000_000,
        25_000_000_000,
    ] {
        for after_eof in [false, true] {
            let mut extra = vec![arg(target)];
            if after_eof {
                extra.push("after-eof".into());
            }
            let left = run.candidate("seek-trace", &extra);
            let right = run.exec(run.reference, "seek-trace", &extra);
            let delta = differences(data(&left), data(&right));
            let pass = ok(&left) && ok(&right) && delta.is_empty();
            rows.push(Value::object([
                ("target_ns", Value::int(target)),
                (
                    "state",
                    Value::str(if after_eof { "after-eof" } else { "fresh" }),
                ),
                ("status", status_of(pass)),
                ("differences", Value::Array(delta)),
                ("candidate", left),
                ("reference", right),
            ]));
        }
    }
    run.write("seek-comparisons", &Value::Array(rows.clone()))?;
    let summary: Vec<Value> = rows
        .iter()
        .map(|row| match row {
            Value::Object(m) => Value::Object(
                m.iter()
                    .filter(|(k, _)| k != "candidate" && k != "reference")
                    .cloned()
                    .collect(),
            ),
            other => other.clone(),
        })
        .collect();
    let mismatch = rows
        .iter()
        .any(|r| r.get("status").and_then(Value::as_str) != Some("pass"));
    set(&mut result, "playback_seeks", Value::Array(summary));
    if mismatch {
        set(&mut result, "status", Value::str("mismatch"));
    }
    Ok(result)
}

/// Fold parity: candidate and reference `{prefix}fold` both reach the end of input and
/// both match the case's independently recorded packet count, byte count and fold.
pub fn feed_parity(
    run: &Run<'_>,
    prefix: &str,
    arguments: &[OsString],
    expected: &Value,
) -> Result<Value, String> {
    let mode = format!("{prefix}fold");
    let left = run.candidate(&mode, arguments);
    let right = run.exec(run.reference, &mode, arguments);
    let work = |outcome: &Value| {
        Value::object(
            ["packets", "bytes", "digest"]
                .map(|key| (key, data(outcome).get(key).cloned().unwrap_or(Value::Null))),
        )
    };
    let complete =
        |outcome: &Value| ok(outcome) && data(outcome).get("eof") == Some(&Value::Bool(true));
    let (candidate, reference) = (work(&left), work(&right));
    let pass = complete(&left)
        && complete(&right)
        && super::diff::equal(&candidate, expected)
        && super::diff::equal(&reference, expected);
    run.write(
        &mode,
        &Value::object([("candidate", left), ("reference", right)]),
    )?;
    Ok(Value::object([
        ("status", status_of(pass)),
        ("source_timing_status", status_of(pass)),
        ("expected", expected.clone()),
        ("candidate", candidate),
        ("reference", reference),
    ]))
}
