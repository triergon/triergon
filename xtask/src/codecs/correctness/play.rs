//! Grades the facade example's native packet trace and presentation outputs.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use crate::harness::oracle::{self, Kind};
use crate::json::Value;
use crate::process::{Stop, Text, run_text};
use crate::{python, sha256};

use super::{CANVAS, Grade, RUN_TIMEOUT};

pub(super) const MANIFEST: &str = "harness/codecs/play-manifest.json";

pub(super) fn lane(name: &str) -> bool {
    matches!(name, "play-mkv" | "play-mp4" | "play-ts")
}

struct Run {
    output: Text,
    paths: [PathBuf; 3],
}

impl Run {
    fn execute(worker: &str, path: &str, directory: &Path, vector: &Value) -> Result<Self, Stop> {
        std::fs::create_dir(directory).map_err(|error| error.to_string())?;
        let paths = ["video.rgba", "audio.s16", "packets.txt"].map(|name| directory.join(name));
        let mut argv = vec![worker.to_owned(), "play-raw".into(), path.to_owned()];
        argv.extend(paths.iter().map(|path| python::path(path)));
        if let Some(tracks) = vector.get("tracks").and_then(Value::as_array) {
            let [video, audio] = tracks else {
                return Err("play track selection needs two entries".to_owned().into());
            };
            argv.push("1920x1080".into());
            for track in [video, audio] {
                argv.push(if track == &Value::Null {
                    "-".into()
                } else {
                    python::int(track)
                        .and_then(|index| u32::try_from(index).ok())
                        .ok_or("play track index must be nonnegative")
                        .map_err(str::to_owned)?
                        .to_string()
                });
            }
        }
        if let Some(namespace) = vector.get("namespace").and_then(Value::as_str) {
            if argv.len() != 9 || namespace != "atsc" {
                return Err("ATSC play needs explicit audio and video track indices"
                    .to_owned()
                    .into());
            }
            argv.push(namespace.to_owned());
        }
        Ok(Self {
            output: run_text(&argv, RUN_TIMEOUT)?,
            paths,
        })
    }
}

#[derive(Default, PartialEq, Eq)]
struct Timeline {
    pictures: Vec<String>,
    audio: Vec<(u32, usize)>,
}

impl Timeline {
    fn parse(text: &str) -> Result<Self, String> {
        let mut timeline = Self::default();
        for line in text.lines() {
            let fields: Vec<_> = line.split_ascii_whitespace().collect();
            match fields.as_slice() {
                ["v", pts] if *pts == "-" || pts.parse::<i64>().is_ok() => {
                    timeline.pictures.push((*pts).to_owned());
                }
                ["a", rate, count] => {
                    let rate = rate.parse().map_err(|_| "invalid sample rate")?;
                    let count = count.parse().map_err(|_| "invalid sample count")?;
                    if let Some((previous_rate, samples)) = timeline.audio.last_mut()
                        && *previous_rate == rate
                    {
                        *samples += count;
                    } else {
                        timeline.audio.push((rate, count));
                    }
                }
                _ => return Err(format!("invalid play output row {line:?}")),
            }
        }
        Ok(timeline)
    }
}

fn open(path: &Path) -> Result<(BufReader<File>, u64), String> {
    let file = File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let size = file.metadata().map_err(|err| err.to_string())?.len();
    Ok((BufReader::new(file), size))
}

fn frames(ours: &Path, reference: &Path) -> Result<(Value, bool), String> {
    let (mut ours, a) = open(ours)?;
    let (mut reference, b) = open(reference)?;
    let mut x = vec![0; CANVAS];
    let mut y = vec![0; CANVAS];
    let (an, bn) = (a / CANVAS as u64, b / CANVAS as u64);
    let mut equal = 0;
    let mut first = Value::Null;
    for index in 0..an.min(bn) {
        ours.read_exact(&mut x).map_err(|err| err.to_string())?;
        reference
            .read_exact(&mut y)
            .map_err(|err| err.to_string())?;
        if x == y {
            equal += 1;
        } else if first == Value::Null {
            first = Value::object([
                ("picture", Value::int(index as i64)),
                ("ours_sha256", Value::str(sha256::hex(&x))),
                ("reference_sha256", Value::str(sha256::hex(&y))),
            ]);
        }
    }
    let pass = equal > 0 && an == equal && bn == equal && a == b && a % CANVAS as u64 == 0;
    Ok((
        Value::object([
            ("pictures_ours", Value::int(an as i64)),
            ("pictures_ref", Value::int(bn as i64)),
            ("pictures_equal", Value::int(equal as i64)),
            ("bytes_ours", Value::int(a as i64)),
            ("bytes_ref", Value::int(b as i64)),
            ("first_diff", first),
        ]),
        pass,
    ))
}

/// Compare the candidate's native Matroska clock with the independent wire
/// reading. Compute the part of reference AAC output that precedes time zero.
fn matroska_trim(
    root: &Path,
    input: &Path,
    packets: &Path,
    selected_audio: Option<i128>,
    reference: &str,
) -> Result<usize, String> {
    let source = oracle::run(root, Kind::Timing, input)??;
    let source = source
        .get("packets")
        .and_then(Value::as_array)
        .ok_or("missing Matroska oracle packets")?;
    let trace = std::fs::read_to_string(packets).map_err(|error| error.to_string())?;
    let mut trace = trace.lines();
    let mut selected = Vec::new();
    for packet in source {
        let track = packet
            .get("track")
            .and_then(python::int)
            .ok_or("missing Matroska oracle track")?;
        let time = |field| -> Result<Option<i128>, String> {
            let value = packet
                .get(field)
                .ok_or("missing Matroska oracle clock field")?;
            if value == &Value::Null {
                return Ok(None);
            }
            let num = value
                .get("numerator")
                .and_then(python::int)
                .ok_or("missing Matroska clock numerator")?;
            let den = value
                .get("denominator")
                .and_then(python::int)
                .filter(|&den| den > 0)
                .ok_or("invalid Matroska clock denominator")?;
            Ok(Some(num.div_euclid(den)))
        };
        let pts = time("pts")?;
        let duration = time("duration")?;
        let text = |value: Option<i128>| value.map_or_else(|| "-".to_owned(), |v| v.to_string());
        let prefix = format!("{track} 1/1000000000 {} - {} ", text(pts), text(duration));
        if !trace.next().is_some_and(|row| row.starts_with(&prefix)) {
            return Err(format!(
                "Matroska packet differs from the source clock: {prefix}"
            ));
        }
        if selected_audio == Some(track) {
            selected.push((pts, duration));
        }
    }
    if trace.next().is_some() {
        return Err("candidate has packets after the Matroska source trace".into());
    }
    if selected.is_empty() {
        return Ok(0);
    }

    let (reference_blocks, rate) = reference.lines().fold((0, None), |(count, rate), row| {
        let mut fields = row.split_ascii_whitespace();
        if fields.next() == Some("a") {
            (
                count + 1,
                rate.or_else(|| fields.next().and_then(|rate| rate.parse::<u32>().ok())),
            )
        } else {
            (count, rate)
        }
    });
    let whole = selected
        .iter()
        .take_while(|&&(pts, duration)| {
            pts.zip(duration)
                .is_some_and(|(pts, duration)| pts < 0 && pts + duration <= 0)
        })
        .count();
    if selected.len() - whole != reference_blocks {
        return Err("reference AAC block count disagrees with Matroska preroll".into());
    }
    let first_pts = selected
        .get(whole)
        .and_then(|(pts, _)| *pts)
        .unwrap_or_default();
    if first_pts >= 0 {
        return Ok(0);
    }
    let rate = i128::from(rate.ok_or("missing reference AAC sample rate")?);
    usize::try_from((-first_pts * rate + 500_000_000) / 1_000_000_000)
        .map_err(|error| error.to_string())
}

fn audio(
    ours: &Path,
    reference: &Path,
    prefix: usize,
    limit: i64,
) -> Result<(Value, bool), String> {
    let a = std::fs::read(ours).map_err(|err| err.to_string())?;
    let b = std::fs::read(reference).map_err(|err| err.to_string())?;
    let b = b
        .get(prefix.checked_mul(4).ok_or("stereo PCM prefix overflow")?..)
        .ok_or("source trim exceeds reference stereo PCM")?;
    let mut worst = 0;
    let mut differing = 0;
    for (x, y) in a.as_chunks::<2>().0.iter().zip(b.as_chunks::<2>().0) {
        let delta = (i32::from(i16::from_le_bytes(*x)) - i32::from(i16::from_le_bytes(*y))).abs();
        worst = worst.max(delta);
        differing += i64::from(delta != 0);
    }
    Ok((
        Value::object([
            ("samples_ours", Value::int((a.len() / 4) as i64)),
            ("samples_ref", Value::int((b.len() / 4) as i64)),
            ("max_abs_lsb", Value::int(worst)),
            ("differing_samples", Value::int(differing)),
            ("ours_sha256", Value::str(sha256::hex(&a))),
            ("reference_sha256", Value::str(sha256::hex(b))),
            ("reference_leading_discard", Value::int(prefix as i64)),
        ]),
        a.len() == b.len() && a.len() % 4 == 0 && i64::from(worst) <= limit,
    ))
}

fn packets(ours: &Path, reference: &Path) -> Result<(Value, bool), String> {
    let a = std::fs::read_to_string(ours).map_err(|err| err.to_string())?;
    let b = std::fs::read_to_string(reference).map_err(|err| err.to_string())?;
    let pass = a == b && !a.is_empty();
    let first = a
        .lines()
        .map(Some)
        .chain(std::iter::once(None))
        .zip(b.lines().map(Some).chain(std::iter::once(None)))
        .enumerate()
        .find(|(_, (a, b))| a != b)
        .map_or(Value::Null, |(index, (a, b))| {
            Value::object([
                ("index", Value::int(index as i64)),
                ("ours", a.map_or(Value::Null, Value::str)),
                ("reference", b.map_or(Value::Null, Value::str)),
            ])
        });
    Ok((
        Value::object([
            ("packets_ours", Value::int(a.lines().count() as i64)),
            ("packets_ref", Value::int(b.lines().count() as i64)),
            ("ours_sha256", Value::str(sha256::hex(a.as_bytes()))),
            ("reference_sha256", Value::str(sha256::hex(b.as_bytes()))),
            ("first_diff", first),
        ]),
        pass,
    ))
}

pub(super) fn grade(grade: &Grade, path: &Path, tmp: &Path, vector: &Value) -> Result<Value, Stop> {
    let worker = python::path(grade.codec_workers.of("play"));
    let ours = Run::execute(&worker, &python::path(path), &tmp.join("ours"), vector)?;
    let reference = Run::execute(
        &grade.reference,
        &python::path(path),
        &tmp.join("reference"),
        vector,
    )?;
    let expected_exit = vector
        .get("candidate_exit")
        .and_then(python::int)
        .unwrap_or(0);
    if i128::from(ours.output.code) != expected_exit || reference.output.code != 0 {
        return Err(Stop::Fatal(format!(
            "unexpected worker exit, candidate {} (expected {expected_exit}), reference {}: {} / {}",
            ours.output.code,
            reference.output.code,
            python::tail(&ours.output.stderr, 300),
            python::tail(&reference.output.stderr, 300),
        )));
    }
    let [ov, oa, op] = &ours.paths;
    let [rv, ra, rp] = &reference.paths;
    let prefix = if vector.get("lane").and_then(Value::as_str) == Some("play-mkv") {
        let selected_audio = vector
            .get("tracks")
            .and_then(Value::as_array)
            .and_then(|tracks| tracks.get(1))
            .and_then(python::int);
        matroska_trim(
            &grade.root,
            path,
            op,
            selected_audio,
            &reference.output.stdout,
        )?
    } else {
        0
    };
    let (video, exact_video) = frames(ov, rv)?;
    let (audio, exact_audio) = audio(oa, ra, prefix, grade.rules.lsb)?;
    let (packets, exact_packets) = packets(op, rp)?;
    let ours_time = Timeline::parse(&ours.output.stdout)?;
    let mut reference_time = Timeline::parse(&reference.output.stdout)?;
    if prefix != 0 {
        let (_, samples) = reference_time
            .audio
            .first_mut()
            .ok_or_else(|| "missing reference PCM timeline".to_owned())?;
        *samples = samples
            .checked_sub(prefix)
            .ok_or_else(|| "source trim exceeds reference PCM timeline".to_owned())?;
    }
    let exact_timeline = ours_time == reference_time;
    let pass = ours.output.code == 0
        && reference.output.code == 0
        && exact_video
        && exact_audio
        && exact_packets
        && exact_timeline;
    Ok(Value::object([
        ("status", Value::str(if pass { "pass" } else { "fail" })),
        ("video", video),
        ("audio", audio),
        ("packets", packets),
        ("timeline_equal", Value::Bool(exact_timeline)),
        ("ours_timeline", Value::str(ours.output.stdout)),
        ("reference_timeline", Value::str(reference.output.stdout)),
        ("ours_rc", Value::int(ours.output.code)),
        ("reference_rc", Value::int(reference.output.code)),
        (
            "ours_stderr_tail",
            Value::str(python::tail(&ours.output.stderr, 300)),
        ),
        (
            "reference_stderr_tail",
            Value::str(python::tail(&reference.output.stderr, 300)),
        ),
    ]))
}
