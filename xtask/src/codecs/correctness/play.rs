//! Grades the facade example's native packet trace and presentation outputs.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

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

fn audio(ours: &Path, reference: &Path, limit: i64) -> Result<(Value, bool), String> {
    let a = std::fs::read(ours).map_err(|err| err.to_string())?;
    let b = std::fs::read(reference).map_err(|err| err.to_string())?;
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
            ("reference_sha256", Value::str(sha256::hex(&b))),
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
    let path = python::path(path);
    let worker = python::path(grade.codec_workers.of("play"));
    let ours = Run::execute(&worker, &path, &tmp.join("ours"), vector)?;
    let reference = Run::execute(&grade.reference, &path, &tmp.join("reference"), vector)?;
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
    let (video, exact_video) = frames(ov, rv)?;
    let (audio, exact_audio) = audio(oa, ra, grade.rules.lsb)?;
    let (packets, exact_packets) = packets(op, rp)?;
    let ours_time = Timeline::parse(&ours.output.stdout)?;
    let reference_time = Timeline::parse(&reference.output.stdout)?;
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
