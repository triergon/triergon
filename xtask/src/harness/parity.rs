//! Correctness of one case: the candidate and reference traces must agree, the
//! candidate's source view must match the independent oracles, short reads must
//! not change the result, and seeks must land on the reference's key frames.
//! Every raw worker output and oracle expectation is written next to the result.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::diff::differences;
use super::oracle::{self, Kind};
use super::worker::{self, arg, data, ok, status};
use crate::json::Value;

/// The workers and files of one parity run.
pub struct Run<'a> {
    pub root: &'a Path,
    pub candidate: &'a Path,
    pub reference: &'a Path,
    pub input: &'a Path,
    pub raw: PathBuf,
    /// How long one worker run may take.
    pub timeout: Duration,
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
        worker::execute(&argv, self.timeout)
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

fn same_preroll_packet(packet: &Value, source: &Value) -> bool {
    let (Value::Object(fields), Value::Object(source_fields)) = (packet, source) else {
        return false;
    };
    fields.len() == source_fields.len()
        && fields.iter().all(|(key, observed)| {
            source.get(key).is_some_and(|expected| {
                // Start padding is applied once at the beginning of the source.
                key == "trim_start_samples" || super::validation::same(observed, expected)
            })
        })
}

// Normal playback can recover earlier audio and subtitles for decoding. Use the
// reference read from the start to check each added packet's source and order.
fn playback_preroll_matches(
    actual: &Value,
    seek: &Value,
    source: &Value,
    target_ns: i64,
) -> Result<(), &'static str> {
    let integer = |field: Option<&Value>| match field {
        Some(Value::Number(number)) => number.as_i128(),
        _ => None,
    };
    let track_info = seek.get("tracks").ok_or("missing seek tracks")?;
    let tracks = track_info.as_array().ok_or("invalid seek tracks")?;
    if !actual
        .get("tracks")
        .is_some_and(|actual| super::validation::same(actual, track_info))
    {
        return Err("playback seek tracks differ");
    }
    let wanted = seek
        .get("packets")
        .and_then(Value::as_array)
        .ok_or("missing reference seek packets")?;
    let source = source
        .get("packets")
        .and_then(Value::as_array)
        .ok_or("missing sequential reference packets")?;
    let packets = actual
        .get("packets")
        .and_then(Value::as_array)
        .ok_or("missing playback seek packets")?;
    let track_index = |packet: &Value| {
        integer(packet.get("track"))
            .and_then(|number| usize::try_from(number).ok())
            .filter(|&track| track < tracks.len())
            .ok_or("invalid playback seek track")
    };
    let mut first_wanted = vec![None; tracks.len()];
    for packet in wanted {
        first_wanted[track_index(packet)?].get_or_insert(packet);
    }
    // The first required packet for a track ends its preroll range in the
    // sequential trace, even when that packet also has a clock before the seek.
    let mut first_position = vec![None; tracks.len()];
    for (position, packet) in source.iter().enumerate() {
        let track = track_index(packet)?;
        if first_position[track].is_none()
            && first_wanted[track].is_some_and(|expected| same_preroll_packet(expected, packet))
        {
            first_position[track] = Some(position);
        }
    }
    let mut wanted = wanted.iter().peekable();
    let mut earlier = source.iter().enumerate();
    let mut emitted = vec![false; tracks.len()];
    let mut preroll_clock = vec![None; tracks.len()];
    for packet in packets {
        let track = track_index(packet)?;
        if wanted
            .peek()
            .is_some_and(|expected| super::validation::same(packet, expected))
        {
            emitted[track] = true;
            wanted.next();
            continue;
        }
        if emitted[track] || !matches!(integer(tracks[track].get("kind")), Some(2 | 17)) {
            return Err("playback seeks must keep the complete reference packet sequence");
        }
        let timed = match packet.get("pts_ns") {
            Some(Value::Null) => false,
            pts if integer(pts).is_some_and(|pts| pts < i128::from(target_ns)) => true,
            _ => return Err("extra playback packet lacks a preroll clock before the target"),
        };
        let end = match first_wanted[track] {
            Some(_) => first_position[track].ok_or("reference seek packet missing from source")?,
            None => source.len(),
        };
        if integer(packet.get("trim_start_samples")) != Some(0) {
            return Err("extra playback preroll reapplies initial trim");
        }
        let (position, _) = earlier
            .by_ref()
            .take_while(|(position, _)| *position < end)
            .find(|(_, original)| same_preroll_packet(packet, original))
            .ok_or("extra playback preroll differs from the sequential reference")?;
        if timed {
            preroll_clock[track] = Some(position);
        } else {
            let anchor = preroll_clock[track].ok_or("untimestamped preroll has no source clock")?;
            if source[anchor + 1..position].iter().any(|original| {
                track_index(original) == Ok(track) && integer(original.get("pts_ns")).is_some()
            }) {
                return Err("untimestamped preroll skipped its source clock");
            }
        }
    }
    if wanted.next().is_some() {
        return Err("playback seek lost a reference packet");
    }
    Ok(())
}

#[test]
fn playback_seek_keeps_the_full_reference_sequence() {
    let packet = |track, pts, start_trim, payload| {
        Value::object([
            ("track", Value::int(track)),
            ("pts_ns", pts),
            ("key", Value::Bool(true)),
            ("trim_start_samples", Value::int(start_trim)),
            ("payload", Value::str(payload)),
        ])
    };
    let trace = |packets| {
        Value::object([
            (
                "tracks",
                Value::Array(
                    [1, 2, 17]
                        .map(|kind| Value::object([("kind", Value::int(kind))]))
                        .to_vec(),
                ),
            ),
            ("packets", Value::Array(packets)),
        ])
    };
    let video = packet(0, Value::int(20), 0, "video");
    let audio = packet(1, Value::int(21), 0, "audio");
    let seek = trace(vec![video.clone(), audio.clone()]);
    let mut integer_key = audio.clone();
    super::validation::put(&mut integer_key, "key", Value::int(1)).unwrap();
    let earlier = packet(1, Value::int(18), 0, "preroll");
    let untimed = packet(1, Value::Null, 0, "same lace");
    let source = trace(vec![
        packet(1, Value::int(18), 312, "preroll"),
        untimed.clone(),
        video.clone(),
        audio.clone(),
    ]);
    let valid = trace(vec![
        earlier.clone(),
        untimed.clone(),
        video.clone(),
        audio.clone(),
    ]);
    assert_eq!(playback_preroll_matches(&valid, &seek, &source, 22), Ok(()));
    assert!(playback_preroll_matches(&valid, &seek, &source, 18).is_err());
    for packets in [
        vec![untimed.clone(), video.clone(), audio.clone()],
        vec![audio.clone(), video.clone(), audio.clone()],
        vec![video.clone(), earlier.clone()],
        vec![earlier.clone(), audio.clone(), video.clone()],
        vec![video.clone(), audio.clone(), earlier.clone()],
        vec![video.clone(), integer_key],
        vec![packet(1, Value::int(18), 0, "other bytes"), video, audio],
    ] {
        assert!(playback_preroll_matches(&trace(packets), &seek, &source, 22).is_err());
    }

    let first = packet(1, Value::int(18), 0, "earlier audio");
    let later = packet(1, Value::int(27), 0, "later audio");
    let later_tail = packet(1, Value::Null, 0, "later lace");
    let required = packet(1, Value::int(30), 0, "reference audio");
    let source = trace(vec![
        first.clone(),
        later,
        later_tail.clone(),
        required.clone(),
    ]);
    let seek = trace(vec![required.clone()]);
    let invalid = trace(vec![first, later_tail, required]);
    assert!(playback_preroll_matches(&invalid, &seek, &source, 22).is_err());
}

pub fn seek_parity(
    run: &Run<'_>,
    sequential_required: bool,
    comparison: Option<&Comparison<'_>>,
    content: bool,
) -> Result<Value, String> {
    const TARGETS_NS: [i64; 6] = [
        0,
        250_000_000,
        2_500_000_000,
        7_500_000_000,
        15_500_000_000,
        25_000_000_000,
    ];
    let mut result = parity(run, sequential_required, comparison, content, "")?;
    let source = crate::checks::read_json(&run.raw.join("reference.json"))?;
    let mut rows = Vec::new();
    for target in TARGETS_NS {
        for after_eof in [false, true] {
            let mut extra = vec![arg(target)];
            if after_eof {
                extra.push("after-eof".into());
            }
            let left = run.candidate("seek-trace", &extra);
            let right = run.exec(run.reference, "seek-trace", &extra);
            let playback = run.candidate("playback-seek-trace", &extra);
            let playback_check = if ok(&playback) && ok(&right) && ok(&source) {
                playback_preroll_matches(data(&playback), data(&right), data(&source), target)
            } else {
                Err("playback or reference seek worker failed")
            };
            let delta = differences(data(&left), data(&right));
            let pass = ok(&left) && ok(&right) && delta.is_empty() && playback_check.is_ok();
            rows.push(Value::object([
                ("target_ns", Value::int(target)),
                (
                    "state",
                    Value::str(if after_eof { "after-eof" } else { "fresh" }),
                ),
                ("status", status_of(pass)),
                ("differences", Value::Array(delta)),
                ("playback_status", status_of(playback_check.is_ok())),
                (
                    "playback_error",
                    playback_check.err().map_or(Value::Null, Value::str),
                ),
                ("candidate", left),
                ("reference", right),
                ("playback_candidate", playback),
            ]));
        }
    }
    run.write("seek-comparisons", &Value::Array(rows.clone()))?;
    let summary: Vec<Value> = rows
        .iter()
        .map(|row| match row {
            Value::Object(m) => Value::Object(
                m.iter()
                    .filter(|(k, _)| {
                        !matches!(k.as_str(), "candidate" | "reference" | "playback_candidate")
                    })
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

/// Fold parity: both workers match the independently pinned packet count, byte
/// count, fold and completion shape (EOF, or an explicit zero-output refusal).
///
/// A reference that flags its fold host-dependent (its result went through a step
/// C leaves to the target) passes only in a feature pinned per target
/// (`pinned_per_target`), on a case whose pin for this target carries the same
/// flag; elsewhere the case fails as `host-dependent-reference`.
pub fn feed_parity(
    run: &Run<'_>,
    prefix: &str,
    arguments: &[OsString],
    expected: &Value,
    work_fields: &[String],
    per_target: bool,
) -> Result<Value, String> {
    let mode = format!("{prefix}fold");
    let left = run.candidate(&mode, arguments);
    let right = run.exec(run.reference, &mode, arguments);
    let result = feed_result(&left, &right, expected, work_fields, per_target);
    run.write(
        &mode,
        &Value::object([("candidate", left), ("reference", right)]),
    )?;
    Ok(result)
}

pub(super) fn pins_zero_output_refusal(expected: &Value) -> bool {
    expected.get("eof") == Some(&Value::Bool(false))
        && ["packets", "bytes", "digest"]
            .iter()
            .all(|key| expected.get(key) == Some(&Value::int(0)))
}

pub(super) fn reports_zero_output_refusal(output: &Value) -> bool {
    pins_zero_output_refusal(output) && output.get("refusal").and_then(Value::as_str).is_some()
}

fn feed_result(
    left: &Value,
    right: &Value,
    expected: &Value,
    work_fields: &[String],
    per_target: bool,
) -> Value {
    use crate::generators::{HOST_DEPENDENT, host_dependent as flagged};
    let work = |outcome: &Value| super::comparison::work_summary(data(outcome), work_fields);
    // A refusal is a separately pinned shape, never a successful EOF. Require
    // explicit zero coverage and a refusal on both sides; texts remain diagnostic.
    let refusal = expected.get("eof") == Some(&Value::Bool(false));
    let complete = |outcome: &Value| {
        ok(outcome)
            && if refusal {
                pins_zero_output_refusal(expected) && reports_zero_output_refusal(data(outcome))
            } else {
                data(outcome).get("eof") == Some(&Value::Bool(true))
            }
    };
    let (candidate, reference) = (work(left), work(right));
    let (reference_flag, pinned_flag) = (flagged(data(right)), flagged(expected));
    let mut pinned = expected.clone();
    if let Value::Object(members) = &mut pinned {
        members.retain(|(key, _)| key != HOST_DEPENDENT && key != "eof");
    }
    let pass = complete(left)
        && complete(right)
        && reference_flag == pinned_flag
        && super::comparison::work_summary(&pinned, work_fields).is_ok()
        && candidate
            .as_ref()
            .is_ok_and(|c| super::diff::equal(c, &pinned))
        && reference
            .as_ref()
            .is_ok_and(|r| super::diff::equal(r, &pinned));
    let mut result = vec![
        ("status", status_of(pass)),
        ("source_timing_status", status_of(pass)),
        ("expected", expected.clone()),
        ("candidate", candidate.unwrap_or(Value::Null)),
        ("reference", reference.unwrap_or(Value::Null)),
        // How the reference run ended and how long it took, against the run limit.
        (
            "reference_run",
            Value::object([
                (
                    "status",
                    right.get("status").cloned().unwrap_or(Value::Null),
                ),
                (
                    "elapsed_ns",
                    right
                        .get("process_elapsed_ns")
                        .cloned()
                        .unwrap_or(Value::Null),
                ),
            ]),
        ),
    ];
    if reference_flag && !per_target {
        let status = Value::str("host-dependent-reference");
        result[0].1 = status.clone();
        result[1].1 = status;
        result.push((
            "reason",
            Value::str(
                "the reference's result went through a step C leaves to the target; \
                 pin the case per target in a feature whose contract says pinned_per_target",
            ),
        ));
    }
    Value::object(result)
}

#[cfg(test)]
mod feed_tests {
    use super::*;

    fn outcome(work: &Value) -> Value {
        let Value::Object(mut data) = work.clone() else {
            panic!("work object")
        };
        data.push(("eof".into(), Value::Bool(true)));
        Value::object([("status", Value::str("ok")), ("data", Value::Object(data))])
    }

    #[test]
    fn refusal_requires_pinned_zero_coverage_and_both_refusals() {
        let fields = ["packets", "bytes", "digest"];
        let mut expected = Value::object(fields.map(|key| (key, Value::int(0))));
        let Value::Object(members) = &mut expected else {
            unreachable!()
        };
        members.push(("eof".into(), Value::Bool(false)));
        let refused = |eof, refusal: bool, packets| {
            Value::object([
                ("status", Value::str("ok")),
                (
                    "data",
                    Value::object([
                        ("packets", Value::int(packets)),
                        ("bytes", Value::int(0)),
                        ("digest", Value::int(0)),
                        ("eof", Value::Bool(eof)),
                        (
                            "refusal",
                            if refusal {
                                Value::str("unsupported")
                            } else {
                                Value::Null
                            },
                        ),
                    ]),
                ),
            ])
        };
        let good = refused(false, true, 0);
        assert_eq!(
            feed_result(&good, &good, &expected, &[], false).get("status"),
            Some(&Value::str("pass"))
        );
        for bad in [
            refused(true, true, 0),
            refused(false, false, 0),
            refused(false, true, 1),
        ] {
            for (left, right) in [(&bad, &good), (&good, &bad)] {
                assert_eq!(
                    feed_result(left, right, &expected, &[], false).get("status"),
                    Some(&Value::str("mismatch"))
                );
            }
        }
        // An old successful zero-packet pin must still require EOF=true.
        let success = Value::object(fields.map(|key| (key, Value::int(0))));
        assert_eq!(
            feed_result(&good, &good, &success, &[], false).get("status"),
            Some(&Value::str("mismatch"))
        );
    }

    #[test]
    fn all_framing_expectations_use_both_digests() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let contract =
            crate::checks::read_json(&root.join("harness/features/packet-framing/contract.json"))
                .unwrap();
        let manifest =
            crate::checks::read_json(&root.join("harness/features/packet-framing/corpus.json"))
                .unwrap();
        let fields = super::super::comparison::work_fields(&contract).unwrap();
        let rows = manifest.get("cases").and_then(Value::as_array).unwrap();
        assert_eq!(rows.len(), 135);
        for row in rows {
            let expected = row.get("expected").unwrap();
            let result = feed_result(
                &outcome(expected),
                &outcome(expected),
                expected,
                &fields,
                false,
            );
            assert_eq!(
                result.get("status"),
                Some(&Value::str("pass")),
                "{:?}",
                row.get("id")
            );
            assert_eq!(result.get("candidate"), Some(expected));
            for changed in [Some(Value::int(0)), None] {
                let Value::Object(mut work) = expected.clone() else {
                    unreachable!()
                };
                work.retain(|(k, _)| k != "digest_b");
                if let Some(changed) = changed {
                    work.push(("digest_b".into(), changed));
                }
                let changed = Value::Object(work);
                for (a, b) in [
                    (outcome(&changed), outcome(expected)),
                    (outcome(expected), outcome(&changed)),
                    (outcome(&changed), outcome(&changed)),
                ] {
                    assert_eq!(
                        feed_result(&a, &b, expected, &fields, false).get("status"),
                        Some(&Value::str("mismatch"))
                    );
                }
            }
        }
    }

    #[test]
    fn ordinary_feed_projection_remains_exactly_three_fields() {
        let expected = Value::object([
            ("packets", Value::int(1)),
            ("bytes", Value::int(2)),
            ("digest", Value::int(3)),
        ]);
        let mut extra = expected.clone();
        let Value::Object(fields) = &mut extra else {
            unreachable!()
        };
        fields.push(("digest_b".into(), Value::int(4)));
        let result = feed_result(&outcome(&extra), &outcome(&extra), &expected, &[], false);
        assert_eq!(result.get("status"), Some(&Value::str("pass")));
        assert_eq!(result.get("candidate"), Some(&expected));
        assert_eq!(result.get("reference"), Some(&expected));
    }

    #[test]
    fn the_reference_run_s_status_and_elapsed_time_are_recorded() {
        let work = Value::object([
            ("packets", Value::int(1)),
            ("bytes", Value::int(2)),
            ("digest", Value::int(3)),
        ]);
        let mut timed = outcome(&work);
        if let Value::Object(members) = &mut timed {
            members.push(("process_elapsed_ns".into(), Value::int(1_250_000_000)));
        }
        let result = feed_result(&outcome(&work), &timed, &work, &[], false);
        assert_eq!(result.get("status"), Some(&Value::str("pass")));
        assert_eq!(
            result.get("reference_run"),
            Some(&Value::object([
                ("status", Value::str("ok")),
                ("elapsed_ns", Value::int(1_250_000_000)),
            ]))
        );
        let timeout = Value::object([("status", Value::str("timeout"))]);
        let result = feed_result(&outcome(&work), &timeout, &work, &[], false);
        assert_eq!(result.get("status"), Some(&Value::str("mismatch")));
        assert_eq!(
            result.get("reference_run"),
            Some(&Value::object([
                ("status", Value::str("timeout")),
                ("elapsed_ns", Value::Null),
            ]))
        );
    }

    #[test]
    fn a_host_dependent_reference_passes_only_where_declared_and_pinned() {
        let work = Value::object([
            ("packets", Value::int(1)),
            ("bytes", Value::int(2)),
            ("digest", Value::int(3)),
        ]);
        let mut flagged = work.clone();
        if let Value::Object(members) = &mut flagged {
            members.push(("host_dependent".into(), Value::Bool(true)));
        }
        let status = |reference: &Value, expected: &Value, declared: bool| {
            feed_result(
                &outcome(&work),
                &outcome(reference),
                expected,
                &[],
                declared,
            )
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned)
        };
        let pass = Some("pass".to_owned());
        assert_eq!(status(&flagged, &flagged, true), pass);
        assert_eq!(status(&work, &work, false), pass);
        assert_eq!(status(&work, &work, true), pass);
        // A flag the pin does not carry, or the other way round, is a mismatch.
        assert_eq!(status(&work, &flagged, true), Some("mismatch".into()));
        assert_eq!(status(&flagged, &work, true), Some("mismatch".into()));
        // A feature required on every host fails a flagged reference outright.
        for expected in [&flagged, &work] {
            assert_eq!(
                status(&flagged, expected, false),
                Some("host-dependent-reference".into())
            );
        }
    }
}
