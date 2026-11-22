//! Grade format-changing files as consecutive segments of native PCM.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::json::Value;
use crate::process::{Stop, run_text};
use crate::python;

use super::{Grade, RUN_TIMEOUT, channel_stats, json_lines};

pub(super) fn selected<'a>(lane: &'a str, vector: &Value) -> Option<&'a str> {
    let base = lane.strip_prefix("session-").unwrap_or(lane);
    (matches!(base, "flac" | "mp3") && vector.get("native_frames") == Some(&Value::Bool(true)))
        .then_some(base)
}

pub(super) fn build(bin: &Path, ffmpeg: &str) -> Result<PathBuf, String> {
    let root = crate::root();
    let libs = Path::new(ffmpeg)
        .parent()
        .and_then(Path::parent)
        .ok_or("native frame reference needs the pinned FFmpeg install path")?
        .join("lib/pkgconfig");
    let flags = Command::new("pkg-config")
        .env("PKG_CONFIG_LIBDIR", libs)
        .args([
            "--static",
            "--cflags",
            "--libs",
            "libavformat",
            "libavcodec",
            "libavfilter",
            "libavutil",
            "libswscale",
            "libswresample",
        ])
        .output()
        .map_err(|e| format!("pkg-config for native audio: {e}"))?;
    if !flags.status.success() {
        return Err(String::from_utf8_lossy(&flags.stderr).into_owned());
    }
    let output = bin.join("native-audio-reference");
    let built = Command::new(std::env::var_os("CC").unwrap_or_else(|| "cc".into()))
        .args(["-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(root.join("xtask/src/codecs/native_audio.c"))
        .args(crate::harness::build::shell_words(
            &String::from_utf8_lossy(&flags.stdout),
        )?)
        .arg("-o")
        .arg(&output)
        .output()
        .map_err(|e| format!("compile native audio reference: {e}"))?;
    if !built.status.success() {
        return Err(String::from_utf8_lossy(&built.stderr).into_owned());
    }
    Ok(output)
}

#[derive(PartialEq, Eq)]
struct Format {
    rate: u64,
    layout: String,
    channels: usize,
}

struct Segment {
    format: Format,
    samples: usize,
}

fn segments(diagnostics: &[Value], rate_field: &str, samples_field: &str) -> Option<Vec<Segment>> {
    let mut segments: Vec<Segment> = Vec::new();
    for row in diagnostics
        .iter()
        .filter(|row| row.get(samples_field).is_some())
    {
        let number = |key| u64::try_from(python::int(row.get(key)?)?).ok();
        let format = Format {
            rate: number(rate_field)?,
            layout: row.get("layout")?.as_str()?.to_owned(),
            channels: usize::try_from(number("channels")?).ok()?,
        };
        let samples = usize::try_from(number(samples_field)?).ok()?;
        if format.rate == 0 || format.channels == 0 || samples == 0 {
            return None;
        }
        if let Some(last) = segments.last_mut()
            && last.format == format
        {
            last.samples = last.samples.checked_add(samples)?;
        } else {
            segments.push(Segment { format, samples });
        }
    }
    (segments.len() > 1).then_some(segments)
}

fn compare(
    a: &Grade,
    lane: &str,
    ours: &[u8],
    reference: &[u8],
    candidate_segments: &[Segment],
    reference_segments: &[Segment],
) -> Result<Vec<Value>, String> {
    let mut rows = Vec::new();
    let mut offset = 0usize;
    for (ours_segment, ref_segment) in candidate_segments.iter().zip(reference_segments) {
        let Segment { format, samples } = ours_segment;
        if *format != ref_segment.format || *samples != ref_segment.samples {
            break;
        }
        let end = samples
            .checked_mul(format.channels)
            .and_then(|n| n.checked_mul(size_of::<f32>()))
            .and_then(|n| n.checked_add(offset))
            .ok_or("native PCM segment length overflow")?;
        let (Some(c), Some(r)) = (ours.get(offset..end), reference.get(offset..end)) else {
            break;
        };
        let Some((rms, abs)) = channel_stats(c, r, format.channels as i128)? else {
            break;
        };
        let (worst_rms, worst_abs) = (python::max(&rms), python::max(&abs));
        let pass = if a.rules.exact.iter().any(|exact| exact == lane) {
            worst_abs == 0.0
        } else {
            worst_rms <= a.rules.rms && worst_abs <= a.rules.abs
        };
        rows.push(Value::object([
            ("rate", Value::int(format.rate)),
            ("layout", Value::str(format.layout.as_str())),
            ("samples", Value::int(*samples as u64)),
            ("worst_abs", Value::float(worst_abs)),
            ("worst_rms", Value::float(worst_rms)),
            ("matches", Value::Bool(pass)),
        ]));
        offset = end;
    }
    if offset != ours.len() || offset != reference.len() {
        rows.push(Value::object([("matches", Value::Bool(false))]));
    }
    Ok(rows)
}

pub(super) fn grade(
    a: &Grade,
    lane: &str,
    path: &str,
    tmp: &Path,
    executable: &Path,
    session: Option<&Value>,
) -> Result<(Value, Value), Stop> {
    let path_in = |name| python::path(&tmp.join(name));
    let (ours_pcm, ref_pcm) = (path_in("ours.f32"), path_in("ref.f32"));
    let (ours_device, ref_device) = (path_in("ours.s16"), path_in("ref.s16"));
    let reference = run_text(
        &[
            python::path(executable),
            lane.into(),
            path.into(),
            ref_pcm.clone(),
            ref_device.clone(),
        ],
        RUN_TIMEOUT,
    )?;
    let ours_args = if let Some(vector) = session {
        super::session::args(a, lane, path, &ours_pcm, tmp, vector, "planar")?
    } else {
        vec![
            a.example(lane)?,
            path.into(),
            ours_pcm.clone(),
            "--native-frames".into(),
        ]
    };
    let ours = run_text(&ours_args, RUN_TIMEOUT)?;
    let ours_rows = json_lines(&ours.stderr);
    let reference_rows = json_lines(&reference.stderr);
    let candidate = if session.is_some() {
        segments(&ours_rows, "sample_rate", "nb_samples")
    } else {
        segments(&ours_rows, "rate", "samples")
    };
    let expected = segments(&reference_rows, "rate", "samples");
    let read = |file: &str| fs::read(file).unwrap_or_default();
    let mut complete = false;
    let mut checked = Vec::new();
    if let (Some(candidate), Some(expected)) = (&candidate, &expected) {
        checked = compare(
            a,
            lane,
            &read(&ours_pcm),
            &read(&ref_pcm),
            candidate,
            expected,
        )?;
        complete = checked.len() == expected.len()
            && candidate.len() == expected.len()
            && checked
                .iter()
                .all(|row| row.get("matches") == Some(&Value::Bool(true)));
    }
    let status = |pass| Value::str(if pass { "pass" } else { "fail" });
    let timeline = session
        .map(|_| super::session::timeline(a, path, lane, &ours_rows, None))
        .transpose()?;
    let same_timeline = timeline
        .as_ref()
        .is_none_or(|v| v.get("status").and_then(Value::as_str) == Some("pass"));
    let decoder = Value::object([
        (
            "status",
            status(
                complete
                    && same_timeline
                    && ours.code == 0
                    && reference.code == 0
                    && ours_rows
                        .last()
                        .and_then(|row| row.get("errors"))
                        .and_then(python::int)
                        == Some(0),
            ),
        ),
        (
            "candidate_segments",
            Value::int(candidate.as_ref().map_or(0, Vec::len) as u64),
        ),
        (
            "reference_segments",
            Value::int(expected.as_ref().map_or(0, Vec::len) as u64),
        ),
        ("segments", Value::Array(checked)),
        ("timeline", timeline.unwrap_or(Value::Null)),
        (
            "candidate_tail",
            Value::str(python::tail(&ours.stderr, 500)),
        ),
        (
            "reference_tail",
            Value::str(python::tail(&reference.stderr, 500)),
        ),
    ]);
    let device_args = if let Some(vector) = session {
        super::session::args(a, lane, path, &ours_device, tmp, vector, "device")?
    } else {
        let operation = super::operation(lane).ok_or_else(|| super::unknown(lane))?;
        vec![
            python::path(a.codec_workers.of(operation)),
            format!("{operation}-raw"),
            path.into(),
            ours_device.clone(),
        ]
    };
    let device = run_text(&device_args, RUN_TIMEOUT)?;
    let (c, r) = (read(&ours_device), read(&ref_device));
    let max = c
        .chunks_exact(2)
        .zip(r.chunks_exact(2))
        .map(|(c, r)| i16::from_le_bytes([c[0], c[1]]).abs_diff(i16::from_le_bytes([r[0], r[1]])))
        .max();
    let output = Value::object([
        (
            "status",
            status(
                device.code == 0
                    && reference.code == 0
                    && c.len() == r.len()
                    && c.len() % 4 == 0
                    && max.is_some_and(|n| i64::from(n) <= a.rules.lsb),
            ),
        ),
        ("samples_ours", Value::int((c.len() / 4) as u64)),
        ("samples_ref", Value::int((r.len() / 4) as u64)),
        ("max_abs_lsb", max.map_or(Value::Null, Value::int)),
        (
            "candidate_tail",
            Value::str(python::tail(&device.stderr, 500)),
        ),
    ]);
    Ok((decoder, output))
}
