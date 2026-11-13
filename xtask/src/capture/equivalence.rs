//! Exact capture-pipeline output identities.
//!
//! Runs `capture-worker convert`, `recon` and `pipeline` over deterministic
//! RGBA inputs at the geometry, orientation and bit rate of every case in
//! `harness/capture/cases.json`, and records the SHA-256 of each output with
//! the exit status and the digests of stdout and stderr. Refactors of the
//! converter, encoder, multiplexer and session must reproduce the record
//! exactly. The inputs are generated here (seeded noise, a scrolled texture, a
//! scene cut and a static screen, from CPython's `random` sequence); they are
//! equivalence inputs only, not the qualification inputs of
//! `harness/capture/generate_inputs.py`, and this is never parity or
//! performance evidence.

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::codecs::correctness::file_sha;
use crate::codecs::equivalence::{Job, cargo_build, finish, run_jobs, take_output};
use crate::harness::worker;
use crate::json::Value;
use crate::process::TempDir;
use crate::pyrandom::PyRandom;
use crate::{python, sha256};

/// One 30-picture GOP plus the start of the next.
const FRAMES: usize = 40;

/// A smooth texture: separable low-frequency ramps with a little noise.
fn texture(width: usize, height: usize, seed: u64) -> Vec<u8> {
    let mut rng = PyRandom::new(seed);
    let cols: Vec<usize> = (0..width)
        .map(|x| (x * 7 + rng.below(8) as usize) & 0xFF)
        .collect();
    let mut out = vec![0u8; width * height * 4];
    for y in 0..height {
        let base = (y * 3) & 0xFF;
        let row = &mut out[y * width * 4..(y + 1) * width * 4];
        for (x, &c) in cols.iter().enumerate() {
            row[4 * x] = ((c + base) & 0xFF) as u8;
            row[4 * x + 1] = ((c * 2 + base / 2) & 0xFF) as u8;
            row[4 * x + 2] = ((255 - c + base) & 0xFF) as u8;
        }
        for x in 0..width {
            row[4 * x + 3] = rng.byte();
        }
    }
    out
}

/// The `FRAMES` input pictures of one content class.
pub fn frames(content: &str, width: usize, height: usize, seed: u64) -> Vec<u8> {
    let size = width * height * 4;
    match content {
        "noise" => {
            let mut rng = PyRandom::new(seed);
            (0..FRAMES).flat_map(|_| rng.bytes(size)).collect()
        }
        "ui_static" => texture(width, height, seed).repeat(FRAMES),
        "ui_transition" => {
            let a = texture(width, height, seed);
            let mut b = texture(width, height, seed + 1);
            b.reverse();
            let mut out = a.repeat(FRAMES / 3);
            out.extend(b.repeat(FRAMES - FRAMES / 3));
            out
        }
        _ => {
            let canvas = texture(width, height + 6 * FRAMES, seed);
            let step = if content == "ui_scroll" { 6 } else { 2 };
            (0..FRAMES)
                .flat_map(|k| {
                    let start = k * step * width * 4;
                    canvas[start..start + size].to_vec()
                })
                .collect()
        }
    }
}

fn capture_entry(job: &Job) -> Value {
    let argv: Vec<OsString> = job.argv.iter().map(OsString::from).collect();
    let done = match worker::run(&argv, &[], Duration::from_secs(600)) {
        Ok(Some(done)) => done,
        Ok(None) => panic!("{} timed out", job.argv.join(" ")),
        Err(error) => panic!("{error}"),
    };
    let mut entry = vec![
        ("exit".to_owned(), Value::int(done.code)),
        (
            "stdout_sha256".to_owned(),
            Value::str(sha256::hex(&done.stdout)),
        ),
        (
            "stderr_sha256".to_owned(),
            Value::str(sha256::hex(&done.stderr)),
        ),
    ];
    if let Some(output) = &job.output {
        entry.push(("output_sha256".into(), take_output(output)));
    }
    Value::Object(entry)
}

/// `capture equivalence --output F [--compare F] [--jobs N]`.
pub fn run(
    root: &Path,
    output: &Path,
    compare: Option<&Path>,
    threads: usize,
) -> Result<i32, String> {
    cargo_build(root, &["--bin", "capture-worker"], false)?;
    let worker = python::path(
        &root
            .join(crate::harness::gate_build::release_dir()?)
            .join("capture-worker"),
    );
    let cases = crate::checks::read_json(&root.join("harness/capture/cases.json"))?;
    let scratch = TempDir::new(None)?;
    let mut record: Vec<(String, Value)> = Vec::new();
    for (index, case) in cases
        .get("cases")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let field = |key: &str| {
            case.get(key)
                .cloned()
                .ok_or_else(|| format!("case without {key}"))
        };
        let size = |key: &str| -> Result<usize, String> {
            field(key).and_then(|v| {
                python::int(&v)
                    .map(|n| n as usize)
                    .ok_or(format!("{key} is not a number"))
            })
        };
        let id = python::str(&field("id")?);
        let content = python::str(&field("content")?);
        let (w, h) = (size("width")?, size("height")?);
        let path = scratch.0.join(format!("{id}.rgba"));
        fs::write(&path, frames(&content, w, h, 280 + index as u64)).map_err(|e| e.to_string())?;
        let geometry = [
            python::str(&field("width")?),
            python::str(&field("height")?),
            python::str(&field("flip")?),
            FRAMES.to_string(),
        ];
        let rate = python::str(&field("bitrate")?);
        let input = python::path(&path);
        let out = |kind: &str| scratch.0.join(format!("{id}.{kind}"));
        let job = |kind: &str, operation: &str, with_rate: bool, extension: &str| {
            let mut argv = vec![worker.clone(), operation.to_owned()];
            argv.extend(geometry.iter().cloned());
            if with_rate {
                argv.push(rate.clone());
            }
            argv.extend([input.clone(), python::path(&out(extension))]);
            Job {
                name: format!("{id}/{kind}"),
                argv,
                output: Some(out(extension)),
            }
        };
        let jobs = [
            job("convert", "convert", false, "yuv"),
            job("recon", "recon", true, "recon"),
            job("pipeline", "pipeline", true, "ts"),
        ];
        for (name, entry) in run_jobs(&jobs, threads, capture_entry) {
            match record.iter_mut().find(|(k, _)| *k == name) {
                Some(slot) => slot.1 = entry,
                None => record.push((name, entry)),
            }
        }
        record.push((
            format!("{id}/input"),
            Value::object([("sha256", Value::str(file_sha(&path)?))]),
        ));
        fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    record.sort_by(|a, b| a.0.cmp(&b.0));
    let passed = record
        .iter()
        .filter(|(_, r)| r.get("exit").is_some_and(|e| python::equals_int(e, 0)))
        .count();
    let summary = format!("{} records; {passed} exit 0", record.len());
    finish(
        output,
        Value::object([
            ("schema", Value::int(1)),
            ("frames", Value::int(FRAMES as i64)),
            ("outputs", Value::Object(record)),
        ]),
        &summary,
        compare,
    )
}
