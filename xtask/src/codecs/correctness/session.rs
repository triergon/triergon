//! Session grading over packet clocks and the existing decoder references.
use std::fs;
use std::path::Path;

use crate::json::Value;
use crate::process::{Stop, run_text};
use crate::python;

use super::{Grade, PROBE_TIMEOUT, audio_format};

pub(super) const CASES: &str = "harness/codecs/session-cases.json";

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

pub(super) fn reference_args(
    a: &Grade,
    lane: &str,
    path: &str,
    output: &str,
    vector: &Value,
    form: &str,
) -> Option<Vec<String>> {
    let reset = vector.get("reset_at")?.as_str()?;
    let format = if video(lane) {
        lane
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_projection_rounds_both_signs_and_refuses_overflow() {
        assert_eq!(ns(1, (1, 48_000)), Some(20_833));
        assert_eq!(ns(-3, (1, 48_000)), Some(-62_500));
        assert_eq!(ns(1, (1, 2_000_000_000)), Some(1));
        assert_eq!(ns(-1, (1, 2_000_000_000)), Some(-1));
        assert_eq!(ns(i128::MAX, (2, 1)), None);
    }
}
