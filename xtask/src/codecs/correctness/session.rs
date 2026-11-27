//! Session grading over packet clocks and the existing decoder references.
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::json::Value;
use crate::process::{Stop, Text, run_text};
use crate::python;

use super::{Grade, PROBE_TIMEOUT, audio_format};

pub(in crate::codecs) const CASES: &str = "harness/codecs/session-cases.json";

pub(in crate::codecs) struct RegisteredRefusal<'a> {
    pub(in crate::codecs) sha256: &'a str,
    pub(in crate::codecs) reason: &'a str,
    detail: &'a str,
    candidate: i128,
    /// The reference's picture count; `None` where it does not decode the refused
    /// feature either, so that what it emits past the candidate's pictures is not a
    /// reference and is not pinned.
    reference: Option<i128>,
}

pub(in crate::codecs) fn registered_refusals(
    manifest: &Value,
) -> Result<BTreeMap<&str, RegisteredRefusal<'_>>, String> {
    let mut refusals = BTreeMap::new();
    let Some(groups) = manifest.get(crate::codecs::summary::REFUSALS) else {
        return Ok(refusals);
    };
    for group in groups
        .as_array()
        .ok_or("open_unimplemented must be an array")?
    {
        if group.get("kind").and_then(Value::as_str)
            == Some(crate::codecs::summary::VIDEO_REFUSAL_KIND)
        {
            continue;
        }
        let line = |key| {
            group
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty() && !value.contains('\n'))
                .ok_or_else(|| format!("registered session refusal needs {key}"))
        };
        let reason = line("reason")?;
        let detail = line("detail")?;
        line("clause")?;
        let reference_decodes = crate::codecs::summary::reference_decodes(group)?;
        let cases = group
            .get("cases")
            .and_then(Value::as_array)
            .filter(|cases| !cases.is_empty())
            .ok_or("registered session refusal needs cases")?;
        for case in cases {
            let id = case
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| id.starts_with("session-h264:") || id.starts_with("session-hevc:"))
                .ok_or("registered refusal needs a video session case")?;
            let sha256 = case
                .get("sha256")
                .and_then(Value::as_str)
                .filter(|sha| {
                    sha.len() == 64
                        && sha
                            .bytes()
                            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                })
                .ok_or_else(|| format!("{id}: registered refusal needs an input SHA-256"))?;
            let pictures = |key| {
                case.get(key)
                    .and_then(python::int)
                    .ok_or_else(|| format!("{id}: registered refusal needs {key}"))
            };
            let candidate = pictures("candidate_pictures")?;
            let reference = if reference_decodes {
                Some(pictures("reference_pictures")?)
            } else if case.get("reference_pictures").is_some() {
                return Err(format!(
                    "{id}: a reference that does not decode the feature pins no picture count"
                ));
            } else {
                None
            };
            if candidate < 0
                || reference.is_some_and(|reference| reference <= candidate)
                || refusals
                    .insert(
                        id,
                        RegisteredRefusal {
                            sha256,
                            reason,
                            detail,
                            candidate,
                            reference,
                        },
                    )
                    .is_some()
            {
                return Err(format!("{id}: invalid or duplicate registered refusal"));
            }
        }
    }
    Ok(refusals)
}

impl RegisteredRefusal<'_> {
    pub(in crate::codecs) fn matches(&self, row: &Value, decoder: &Value, output: &Value) -> bool {
        let number =
            |section: &Value, key, expected| section.get(key) == Some(&Value::int(expected));
        row.get("sha256").and_then(Value::as_str) == Some(self.sha256)
            && !row.get("open_case").is_some_and(Value::truthy)
            && decoder.get("status").and_then(Value::as_str) == Some("fail")
            && number(decoder, "ours_blocks", self.candidate)
            && self
                .reference
                .is_none_or(|reference| number(decoder, "reference_frames", reference))
            && number(decoder, "first_difference", self.candidate)
            && output.get("status").and_then(Value::as_str) == Some("failed-exit")
            && number(output, "pictures_ours", self.candidate)
            && self
                .reference
                .is_none_or(|reference| number(output, "pictures_ref", reference))
            && number(output, "pictures_equal", self.candidate)
            && number(output, "ours_rc", 1)
            && number(output, "reference_rc", 0)
            && number(output, "ours_partial_bytes", 0)
            && number(output, "reference_partial_bytes", 0)
            && output
                .get("ours_stderr_tail")
                .and_then(Value::as_str)
                .and_then(|tail| tail.lines().last())
                == Some(format!("Error: Unsupported({:?})", self.detail).as_str())
    }
}

/// Both decoders explicitly report the same mid-stream FLAC format refusal.
/// The reference device command can return success after reporting it.
pub(super) fn flac_depth_refusal(ours: &Text, reference: &Text) -> bool {
    ours.code > 0
        && ours.stderr.lines().last().is_some_and(|line| {
            line == "Error: Unsupported(\"sample size change within a stream\")"
        })
        && reference
            .stderr
            .contains("switching bps mid-stream is not supported")
}

/// Count the reference decoder's explicit STREAMINFO block-limit refusals.
pub(super) fn flac_block_refusals(reference: &Text) -> usize {
    reference
        .stderr
        .lines()
        .zip(reference.stderr.lines().skip(2))
        // The stream-discovery decoder also prints size diagnostics. Only
        // count those followed by the input stream's packet refusal.
        .filter(|(_, outcome)| outcome.contains("Error submitting packet to decoder: Invalid data"))
        .filter_map(|(line, _)| line.split_once("] blocksize "))
        .filter_map(|(_, values)| values.split_once(" > "))
        .filter(|(actual, maximum)| {
            actual
                .parse::<u32>()
                .ok()
                .zip(maximum.parse::<u32>().ok())
                .is_some_and(|(actual, maximum)| actual > maximum)
        })
        .count()
}

pub(super) fn lane(base: &str) -> Option<&'static str> {
    Some(match base {
        "aac" => "session-aac",
        "ac3" => "session-ac3",
        "eac3" => "session-eac3",
        "dts" => "session-dts",
        "flac" => "session-flac",
        "mp1" | "mp2" | "mp3" => "session-mp3",
        "vorbis" => "session-vorbis",
        "opus" => "session-opus",
        "h264" => "session-h264",
        "hevc" => "session-hevc",
        _ => return None,
    })
}

pub(super) fn base(lane: &str, vector: &Value) -> Result<String, String> {
    if let Some(base) = vector.get("base_lane").and_then(Value::as_str) {
        return Ok(base.to_owned());
    }
    let base = vector
        .get("lane")
        .and_then(Value::as_str)
        .ok_or("no vector lane")?;
    if self::lane(base) != Some(lane) {
        return Err(format!("{lane}: no session media lane for {base}"));
    }
    Ok(base.to_owned())
}

fn video(lane: &str) -> bool {
    matches!(lane, "h264" | "hevc")
}

fn metadata(
    a: &Grade,
    path: &str,
    lane: &str,
    rows: &str,
) -> Result<(Vec<Value>, (i128, i128)), Stop> {
    let format = if video(lane) {
        Some(lane)
    } else {
        audio_format(lane)?
    };
    let mut args = [a.ffprobe.as_str(), "-v", "error", "-threads", "1"]
        .map(String::from)
        .to_vec();
    if let Some(format) = format {
        args.extend(["-f", format].map(String::from));
    }
    args.extend(["-select_streams", if video(lane) { "v:0" } else { "a:0" }].map(String::from));
    let entries = if rows == "packets" {
        "stream=time_base:packet=pos,size,pts"
    } else {
        "stream=time_base:frame=pts,nb_samples"
    };
    args.extend(
        [
            &format!("-show_{rows}"),
            "-show_entries",
            entries,
            "-of",
            "json",
            path,
        ]
        .map(String::from),
    );
    let result = run_text(&args, PROBE_TIMEOUT)?;
    if result.code != 0 {
        return Err(format!("ffprobe {rows}: {}", result.stderr).into());
    }
    let doc =
        python::loads(&result.stdout).ok_or_else(|| format!("invalid ffprobe {rows} JSON"))?;
    let base = doc
        .get("streams")
        .and_then(Value::as_array)
        .and_then(|rows| rows.first())
        .and_then(|stream| stream.get("time_base"))
        .and_then(Value::as_str)
        .and_then(|base| base.split_once('/'))
        .and_then(|(n, d)| Some((n.parse::<i128>().ok()?, d.parse::<i128>().ok()?)))
        .filter(|&(n, d)| n > 0 && d > 0)
        .ok_or_else(|| format!("ffprobe {rows} returned no valid time base"))?;
    let rows = doc
        .get(rows)
        .and_then(Value::as_array)
        .unwrap_or_default()
        .to_vec();
    Ok((rows, base))
}

fn ns(ticks: i128, (n, d): (i128, i128)) -> Option<i128> {
    // Match Time::to_nanoseconds(NearestAway) without using candidate code.
    let scaled = ticks.checked_mul(n)?.checked_mul(1_000_000_000)?;
    let remainder = scaled % d;
    let adjust = remainder.abs() >= d / 2 + d % 2;
    Some(scaled / d + i128::from(adjust) * scaled.signum())
}

fn pts(row: &Value, base: (i128, i128)) -> Result<Value, String> {
    match row.get("pts") {
        None | Some(Value::Null) => Ok(Value::Null),
        Some(v) => python::int(v)
            .and_then(|value| ns(value, base))
            .map(Value::int)
            .ok_or_else(|| "invalid or unrepresentable packet timestamp".to_owned()),
    }
}

pub(super) fn args(
    a: &Grade,
    lane: &str,
    path: &str,
    output: &str,
    tmp: &Path,
    vector: &Value,
    form: &str,
) -> Result<Vec<String>, Stop> {
    let mut args = vec![
        python::path(&a.bin.join("session_decode")),
        path.to_owned(),
        output.to_owned(),
        "--lane".into(),
        lane.into(),
        "--form".into(),
        form.into(),
    ];
    if video(lane) || lane == "dts" {
        let (packets, base) = metadata(a, path, lane, "packets")?;
        let mut text = String::new();
        use std::fmt::Write;
        for packet in packets {
            let number = |field| {
                packet
                    .get(field)
                    .and_then(python::int)
                    .ok_or_else(|| format!("missing packet {field}"))
            };
            let timestamp = match pts(&packet, base)? {
                Value::Null => "-".into(),
                value => python::str(&value),
            };
            writeln!(text, "{} {} {timestamp}", number("pos")?, number("size")?)
                .map_err(|e| e.to_string())?;
        }
        let packets = tmp.join("packets.txt");
        fs::write(&packets, text).map_err(|e| e.to_string())?;
        args.extend(["--packets".into(), python::path(&packets)]);
    }
    append_options(&mut args, vector);
    Ok(args)
}

pub(super) fn append_options(args: &mut Vec<String>, vector: &Value) {
    for (field, option) in [("on_refusal", "--on-refusal"), ("reset_at", "--reset-at")] {
        if let Some(value) = vector.get(field).and_then(Value::as_str) {
            args.extend([option.into(), value.into()]);
        }
    }
}

pub(super) fn reset_audio_format(lane: &str, path: &str) -> Result<Option<&'static str>, String> {
    let suffix = Path::new(path).extension().and_then(|value| value.to_str());
    match (lane, suffix) {
        ("dts" | "opus", Some("mka" | "mkv")) => Ok(Some("matroska")),
        ("opus", Some("opus" | "ogg")) => Ok(Some("ogg")),
        _ => audio_format(lane),
    }
}

pub(super) fn reference_args(
    a: &Grade,
    lane: &str,
    path: &str,
    output: &str,
    vector: &Value,
    form: &str,
) -> Option<Vec<String>> {
    let reset = vector.get("reset_at")?.as_str()?;
    let track_reset = vector.get("lane").and_then(Value::as_str) == Some("track-reset");
    let format = if video(lane) {
        lane
    } else if track_reset {
        if lane == "flac" && vector.get("independent_frames") == Some(&Value::Bool(true)) {
            // RFC 9639 §§6 and 9.2 put all predictor state in each frame.
            // The ordinary natively decoded integer stream grades the full
            // reset output; the reset worker's float interface cannot emit it.
            return None;
        }
        reset_audio_format(lane, path).ok()??
    } else {
        audio_format(lane).ok()??
    };
    Some(
        [
            python::path(&a.bin.join("reference-session")),
            format.into(),
            path.into(),
            output.into(),
            form.into(),
            reset.into(),
        ]
        .into(),
    )
}

pub(super) fn timeline(
    a: &Grade,
    path: &str,
    lane: &str,
    diagnostics: &[Value],
    reset_reference: Option<&[Value]>,
) -> Result<Value, Stop> {
    let reference = if let Some(rows) = reset_reference {
        rows.to_vec()
    } else {
        let (frames, base) = metadata(a, path, lane, "frames")?;
        frames
            .iter()
            .map(|v| {
                Ok(Value::object([
                    ("pts_ns", pts(v, base)?),
                    (
                        "nb_samples",
                        v.get("nb_samples")
                            .cloned()
                            .unwrap_or_else(|| Value::int(0)),
                    ),
                ]))
            })
            .collect::<Result<Vec<_>, String>>()?
    };
    let candidate: Vec<_> = diagnostics
        .iter()
        .filter_map(|v| {
            Some(Value::object([
                ("pts_ns", v.get("pts_ns")?.clone()),
                ("nb_samples", v.get("nb_samples")?.clone()),
            ]))
        })
        .collect();
    let first_difference = candidate
        .iter()
        .zip(&reference)
        .position(|(a, b)| a != b)
        .or_else(|| {
            (candidate.len() != reference.len()).then_some(candidate.len().min(reference.len()))
        });
    let row = |rows: &[Value]| {
        first_difference
            .and_then(|i| rows.get(i))
            .cloned()
            .unwrap_or(Value::Null)
    };
    Ok(Value::object([
        (
            "status",
            Value::str(if first_difference.is_some() {
                "fail"
            } else {
                "pass"
            }),
        ),
        ("ours_blocks", Value::int(candidate.len() as u64)),
        ("reference_frames", Value::int(reference.len() as u64)),
        (
            "first_difference",
            first_difference.map_or(Value::Null, |index| Value::int(index as u64)),
        ),
        ("ours_at_difference", row(&candidate)),
        ("reference_at_difference", row(&reference)),
    ]))
}

pub(super) fn video_timeline(
    a: &Grade,
    path: &str,
    lane: &str,
    vector: &Value,
    output: Option<&Value>,
) -> Result<Value, Stop> {
    let base = base(lane, vector)?;
    let output_rows = |key| {
        output
            .and_then(|row| row.get(key))
            .and_then(Value::as_array)
    };
    let clock = timeline(
        a,
        path,
        &base,
        output_rows("timeline").unwrap_or_default(),
        vector
            .get("reset_at")
            .and_then(|_| output_rows("reference_timeline")),
    )?;
    if let Some(output) = output
        && output.get("status").and_then(Value::as_str) == Some("reference-produced-nothing")
        && clock.get("reference_frames").and_then(python::int) == Some(0)
        && output
            .get("ours_stderr_tail")
            .and_then(Value::as_str)
            .is_some_and(|error| {
                error.contains("Error: Unsupported(") || error.contains("Error: Invalid(")
            })
        && output
            .get("reference_stderr_tail")
            .and_then(Value::as_str)
            .is_some_and(|error| !error.is_empty())
    {
        return Ok(Value::object([
            ("status", Value::str("ffmpeg-refused")),
            ("timeline", clock),
        ]));
    }
    Ok(clock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_refusal_requires_the_same_error_and_complete_prefix() {
        let manifest = crate::json::parse(
            r#"{"open_unimplemented":[{"basis":"unimplemented","reference":"decodes","reason":"SP/SI slices","clause":"H.264 §8.6.2",
                "detail":"SP/SI slices (Extended profile)",
                "cases":[{"id":"session-h264:switching.h264",
                    "sha256":"0000000000000000000000000000000000000000000000000000000000000000",
                    "candidate_pictures":6,"reference_pictures":400}]}]}"#,
        )
        .unwrap();
        let refusals = registered_refusals(&manifest).unwrap();
        let refusal = &refusals["session-h264:switching.h264"];
        let row = Value::object([("sha256", Value::str(refusal.sha256))]);
        let decoder = crate::json::parse(
            r#"{"status":"fail","ours_blocks":6,"reference_frames":400,"first_difference":6}"#,
        )
        .unwrap();
        let output = r#"{"status":"failed-exit","pictures_ours":6,"pictures_ref":400,
            "pictures_equal":6,"ours_rc":1,"reference_rc":0,"ours_partial_bytes":0,
            "reference_partial_bytes":0,"ours_stderr_tail":"Error: Unsupported(\"SP/SI slices (Extended profile)\")\n"}"#;
        assert!(refusal.matches(&row, &decoder, &crate::json::parse(output).unwrap()));
        for changed in [
            output.replace("Unsupported", "Invalid"),
            output.replace("\"pictures_equal\":6", "\"pictures_equal\":5"),
            output.replace("\"reference_rc\":0", "\"reference_rc\":1"),
        ] {
            assert!(!refusal.matches(&row, &decoder, &crate::json::parse(&changed).unwrap()));
        }
    }

    #[test]
    fn a_reference_that_does_not_decode_the_feature_pins_no_picture_count() {
        let group = |fields: &str| {
            crate::json::parse(&format!(
                r#"{{"open_unimplemented":[{{{fields}"reason":"slice groups","clause":"H.264 §8.2.2",
                "detail":"flexible macroblock ordering (slice groups)",
                "cases":[{{"id":"session-h264:groups.264",
                    "sha256":"0000000000000000000000000000000000000000000000000000000000000000",
                    "candidate_pictures":103}}]}}]}}"#
            ))
            .unwrap()
        };
        assert!(registered_refusals(&group("")).is_err());
        assert!(registered_refusals(&group(r#""basis":"unimplemented","#)).is_err());
        assert!(
            registered_refusals(&group(r#""basis":"unimplemented","reference":"decodes","#))
                .is_err()
        );
        let manifest = group(r#""basis":"unimplemented","reference":"unimplemented","#);
        let refusals = registered_refusals(&manifest).unwrap();
        let refusal = &refusals["session-h264:groups.264"];
        let row = Value::object([("sha256", Value::str(refusal.sha256))]);
        let rows = |reference: u32, equal: u32| {
            let decoder = crate::json::parse(&format!(
                r#"{{"status":"fail","ours_blocks":103,"reference_frames":{reference},"first_difference":103}}"#
            ))
            .unwrap();
            let output = crate::json::parse(&format!(
                r#"{{"status":"failed-exit","pictures_ours":103,"pictures_ref":{reference},
                "pictures_equal":{equal},"ours_rc":1,"reference_rc":0,"ours_partial_bytes":0,
                "reference_partial_bytes":0,"ours_stderr_tail":"Error: Unsupported(\"flexible macroblock ordering (slice groups)\")\n"}}"#
            ))
            .unwrap();
            refusal.matches(&row, &decoder, &output)
        };
        assert!(rows(305, 103));
        assert!(rows(290, 103));
        assert!(!rows(305, 102));
    }

    #[test]
    fn timestamp_projection_rounds_both_signs_and_refuses_overflow() {
        assert_eq!(ns(1, (1, 48_000)), Some(20_833));
        assert_eq!(ns(-3, (1, 48_000)), Some(-62_500));
        assert_eq!(ns(1, (1, 2_000_000_000)), Some(1));
        assert_eq!(ns(-1, (1, 2_000_000_000)), Some(-1));
        assert_eq!(ns(i128::MAX, (2, 1)), None);
    }
}
