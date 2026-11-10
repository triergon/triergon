//! Capture diagnostics on native Linux.
//!
//! For every required case in `harness/capture/cases.json`:
//! 1. generate the RGBA input (`harness/capture/generate_inputs.py`, run with
//!    the numpy Python named by `TRIERGON_PYTHON`, else `python3` on `PATH`)
//!    and record its SHA-256;
//! 2. correctness: `cargo xtask capture compare` (conversion, mux, encoder and
//!    pipeline rules of `harness/capture/contract.json`);
//! 3. runtime: one session of the shared paired runner
//!    ([`crate::harness::timing::benchmark`]) over `bench` runs of
//!    `--iterations` sessions each, graded by its validator and against the
//!    runtime gate of `harness/policy.json`. Every run must reproduce its
//!    worker's checked output;
//! 4. memory: admit both workers to the shared observer and run separate heap
//!    and RSS measurements. Each worker must reproduce its checked output.
//!    Peak live heap must be lower in every pair. The upper paired 95% bounds
//!    for heap and total resident peak RSS must both be below 1.0.
//!
//! The machine must be otherwise idle: the timing session rejects a run during
//! which a tracked process used CPU and retries it, and a CPU-contention check
//! runs before every memory pair. Contaminated runs are kept, never used. Every
//! raw observation is written.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::codecs::correctness::file_sha;
use std::time::Duration;

use crate::harness::gate_build;
use crate::harness::memory;
use crate::harness::policy;
use crate::harness::timing::{self, GATE_PAIRS, Operation, Work};
use crate::harness::worker::data;
use crate::json::{self, Value};
use crate::process::exit_code;
use crate::{python, sha256};

/// How long one capture session may take in a timed run, on the slowest
/// target.
const SESSION_TIMEOUT: Duration = Duration::from_secs(60);
/// Offsets from `--seed`: the timing session and its two retries, then the
/// `--enable-small` comparison and its retries, then memory.
const SMALL_CONFIG_SEED: u64 = 3;
const MEMORY_SEED: u64 = 6;

/// The case fields every worker mode after `convert` takes, in order.
const SESSION: [&str; 5] = ["width", "height", "flip", "frames", "bitrate"];

/// The arguments of `capture qualify`.
pub struct Options {
    pub candidate: String,
    pub reference: String,
    pub reference_small: Option<String>,
    pub oracle_bin: String,
    pub observer: String,
    pub cases: PathBuf,
    pub only: Vec<String>,
    pub inputs: PathBuf,
    pub out: PathBuf,
    pub pairs: usize,
    pub iterations: usize,
    pub memory_pairs: usize,
    pub seed: u64,
}

/// Source files whose digest identifies a qualification run.
const SOURCE_TREES: [&str; 5] = [
    "crates/capture",
    "crates/capture-worker",
    "harness/capture",
    "xtask/src/capture",
    "xtask/src/harness",
];
const SOURCE_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "xtask/src/numpy.rs",
    "xtask/src/pyrandom.rs",
    "xtask/src/python.rs",
    "xtask/src/stats.rs",
    "harness/allocations-linux.c",
    "harness/policy.json",
    "harness/portable-memory-check.c",
    "harness/reference.c",
    "harness/reference_capture.h",
    "harness/rss-bound-api.h",
];

fn write(path: &Path, text: String) -> Result<(), String> {
    fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

fn case_args(case: &Value, keys: &[&str]) -> Vec<String> {
    keys.iter()
        .map(|k| python::str(case.get(k).unwrap_or(&Value::Null)))
        .collect()
}

/// `bench IN.RGBA ITERATIONS W H FLIP FRAMES BITRATE`: frames are the shared
/// work, the stream's byte count and digest each encoder's own.
fn operation(case: &Value, iterations: u32) -> Operation {
    Operation {
        prefix: String::new(),
        arguments: case_args(case, &SESSION)
            .into_iter()
            .map(OsString::from)
            .collect(),
        candidate_options: vec![],
        work: Work {
            fields: vec![],
            own: vec!["bytes".to_owned(), "digest".to_owned()],
        },
        timeout: SESSION_TIMEOUT * iterations,
    }
}

/// Whether each worker's warm-up printed the stream of that worker's
/// correctness run: its digest, and its byte count once per session. The
/// validator has shown that every pair repeats the warm-up's work, and each run
/// fails if its sessions' streams differ.
fn outputs_match(timing: &Value, outputs: &Value) -> bool {
    let Some(sessions) = timing.get("iterations").and_then(python::int) else {
        return false;
    };
    let warmups = timing
        .get("warmups")
        .and_then(Value::as_array)
        .unwrap_or_default();
    ["candidate", "reference"].into_iter().all(|role| {
        let warmup = warmups
            .iter()
            .find(|w| w.get("worker").and_then(Value::as_str) == Some(role))
            .and_then(|w| w.get("result"))
            .map(data);
        let expected = || -> Option<(i128, i128)> {
            let output = outputs.get(role)?;
            let digest = u64::from_str_radix(output.get("fnv1a64")?.as_str()?, 16).ok()?;
            let bytes = python::int(output.get("bytes")?)?;
            Some((bytes.checked_mul(sessions)?, i128::from(digest)))
        };
        warmup
            .zip(expected())
            .is_some_and(|(run, (bytes, digest))| {
                run.get("bytes").and_then(python::int) == Some(bytes)
                    && run.get("digest").and_then(python::int) == Some(digest)
            })
    })
}

/// Keep a measurement but pass nothing: the run was not on the gate build.
fn withhold_gates(timed: &mut Value) {
    if let Value::Object(fields) = timed {
        for (key, value) in fields {
            if key == "gate_pass" || key == "target_pass" {
                *value = Value::Bool(false);
            }
        }
    }
}

/// The ratio interval of a runtime result, in the shared record or, for
/// results written before it, beside the samples.
pub(super) fn ratio(runtime: &Value) -> Option<&Value> {
    runtime.get("timing").unwrap_or(runtime).get("ratio")
}

/// One timing session of `case`, graded: the shared result fields, whether
/// every run reproduced its worker's checked output, and the gate verdicts,
/// which need [`GATE_PAIRS`] pairs.
#[allow(clippy::too_many_arguments)] // The workers, input, gate and paired timing settings.
fn runtime(
    gate: policy::Runtime,
    candidate: &str,
    reference: &str,
    case: &Value,
    path: &Path,
    pairs: usize,
    iterations: u32,
    seed: u64,
    expected_outputs: Option<&Value>,
) -> Result<Value, String> {
    let graded = timing::measure(
        Path::new(candidate),
        Path::new(reference),
        path,
        pairs,
        iterations,
        seed,
        &operation(case, iterations),
    )?;
    let deterministic = graded.interval.is_ok()
        && expected_outputs.is_none_or(|outputs| outputs_match(&graded.record, outputs));
    let passes = |bound: f64| {
        graded.interval.as_ref().is_ok_and(|interval| {
            deterministic && pairs >= GATE_PAIRS && interval.upper_95 <= bound
        })
    };
    let (gate_pass, target_pass) = (passes(gate.ceiling), passes(gate.target));
    let mut fields = graded.into_fields();
    fields.extend([
        ("deterministic_output".into(), Value::Bool(deterministic)),
        ("gate_pass".into(), Value::Bool(gate_pass)),
        ("target_pass".into(), Value::Bool(target_pass)),
    ]);
    Ok(Value::Object(fields))
}

/// `platform.uname()._asdict()`.
fn uname() -> Value {
    let field = |flag: &str| {
        Command::new("uname")
            .arg(flag)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default()
    };
    let processor = field("-p");
    Value::object([
        ("system", Value::str(field("-s"))),
        ("node", Value::str(field("-n"))),
        ("release", Value::str(field("-r"))),
        ("version", Value::str(field("-v"))),
        ("machine", Value::str(field("-m"))),
        (
            "processor",
            Value::str(if processor == "unknown" {
                String::new()
            } else {
                processor
            }),
        ),
    ])
}

fn source_manifest(root: &Path) -> Result<Vec<(String, Value)>, String> {
    let mut manifest = Vec::new();
    for top in SOURCE_TREES {
        let mut files = Vec::new();
        collect(&root.join(top), &mut files);
        files.sort();
        for file in files {
            if file.components().any(|c| c.as_os_str() == "__pycache__") {
                continue;
            }
            let relative = file.strip_prefix(root).unwrap_or(&file);
            manifest.push((python::path(relative), Value::str(file_sha(&file)?)));
        }
    }
    for file in SOURCE_FILES {
        manifest.push(((*file).to_owned(), Value::str(file_sha(&root.join(file))?)));
    }
    Ok(manifest)
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            collect(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// The Python with numpy that generates the capture inputs.
fn python_with_numpy() -> String {
    std::env::var("TRIERGON_PYTHON").unwrap_or_else(|_| "python3".into())
}

pub fn run(root: &Path, a: &Options) -> Result<(), String> {
    fs::create_dir_all(&a.out).map_err(|e| format!("{}: {e}", a.out.display()))?;
    let cases =
        crate::checks::read_json(&a.cases).map_err(|e| format!("{}: {e}", a.cases.display()))?;
    let mut cases: Vec<Value> = cases
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("cases file without cases")?
        .to_vec();
    if !a.only.is_empty() {
        cases.retain(|c| {
            c.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| a.only.iter().any(|o| o == id))
        });
    }
    let manifest = source_manifest(root)?;
    let digest = sha256::hex(
        json::to_python(&python::sort_keys(&Value::Object(manifest.clone())), None).as_bytes(),
    );
    let sha = |path: &str| file_sha(Path::new(path)).map(Value::str);
    let observer = sha(&a.observer)?;
    if observer != Value::str(file_sha(&root.join("target/harness/allocations.so"))?) {
        return Err("capture requires the observer built in target/harness".into());
    }
    let gate = policy::Runtime::load(root)?;
    let refusals =
        gate_build::pair_refusals(root, Path::new(&a.candidate), Path::new(&a.reference));
    for reason in &refusals {
        println!("no gate result: {reason}");
    }
    let identity = Value::object([
        ("source_digest", Value::str(digest)),
        ("source_manifest", Value::Object(manifest)),
        ("build_refusals", Value::strings(refusals.clone())),
        ("cpus", gate_build::cpus()),
        ("candidate_sha256", sha(&a.candidate)?),
        ("reference_sha256", sha(&a.reference)?),
        (
            "reference_small_sha256",
            match &a.reference_small {
                Some(path) => sha(path)?,
                None => Value::Null,
            },
        ),
        ("observer_sha256", observer),
        ("cases_sha256", Value::str(file_sha(&a.cases)?)),
        ("host", uname()),
        ("utc", Value::float(python::time())),
    ]);
    write(
        &a.out.join("identity.json"),
        json::to_python(&identity, Some(1)),
    )?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut summary = Vec::new();
    for case in &cases {
        let text = |key: &str| python::str(case.get(key).unwrap_or(&Value::Null));
        let id = text("id");
        let cdir = a.out.join(&id);
        fs::create_dir_all(&cdir).map_err(|e| format!("{}: {e}", cdir.display()))?;
        let path = a.inputs.join(format!(
            "{}-{}x{}-{}.rgba",
            text("content"),
            text("width"),
            text("height"),
            text("frames")
        ));
        let path_text = python::path(&path);
        if !path.exists() {
            let generator = python::path(&root.join("harness/capture/generate_inputs.py"));
            let python = python_with_numpy();
            let status = Command::new(&python)
                .args([
                    generator,
                    text("content"),
                    text("width"),
                    text("height"),
                    text("frames"),
                ])
                .arg(&path_text)
                .status()
                .map_err(|e| format!("{python}: {e}"))?;
            if !status.success() {
                return Err(format!("input generation failed: {status}"));
            }
        }
        let mut record = vec![
            ("case".to_owned(), case.clone()),
            ("input".to_owned(), Value::str(path_text.as_str())),
            ("input_sha256".to_owned(), Value::str(file_sha(&path)?)),
        ];
        let compare = Command::new(&exe)
            .args([
                "capture",
                "compare",
                "--candidate",
                &a.candidate,
                "--reference",
                &a.reference,
            ])
            .args([
                "--ffmpeg-bin",
                &a.oracle_bin,
                "--case",
                &json::to_python(case, None),
            ])
            .args([
                "--input",
                &path_text,
                "--out",
                &python::path(&cdir.join("correctness")),
            ])
            .output()
            .map_err(|e| format!("{}: {e}", exe.display()))?;
        let code = exit_code(compare.status);
        record.push(("correctness_exit".into(), Value::int(code)));
        let correctness = if code == 0 {
            python::loads(&python::text(&compare.stdout)).ok_or("compare printed no summary")?
        } else {
            Value::object([(
                "error",
                Value::str(python::tail(&python::text(&compare.stderr), 2000)),
            )])
        };
        record.push(("correctness_summary".into(), correctness.clone()));
        let outputs = correctness.get("outputs");
        let iterations = u32::try_from(a.iterations).map_err(|e| e.to_string())?;
        let mut timed = runtime(
            gate,
            &a.candidate,
            &a.reference,
            case,
            &path,
            a.pairs,
            iterations,
            a.seed,
            outputs,
        )?;
        if !refusals.is_empty() {
            withhold_gates(&mut timed);
        }
        let runtime_gate = outputs.is_some() && timed.get("gate_pass").is_some_and(Value::truthy);
        let runtime_target = timed.get("target_pass").is_some_and(Value::truthy);
        let runtime_upper = ratio(&timed)
            .and_then(|ratio| ratio.get("upper_95"))
            .and_then(Value::as_f64)
            .filter(|bound| bound.is_finite());
        record.push(("runtime".into(), timed));
        if let Some(small) = &a.reference_small {
            record.push((
                "runtime_vs_small_config".into(),
                runtime(
                    gate,
                    &a.candidate,
                    small,
                    case,
                    &path,
                    10.max(a.pairs / 3),
                    iterations,
                    a.seed + SMALL_CONFIG_SEED,
                    None,
                )?,
            ));
        }
        let measured = if let Some(outputs) = outputs {
            let mut args = vec!["memory".to_owned()];
            args.extend(case_args(case, &SESSION));
            args.push(path_text);
            memory::collect_command(
                root,
                Path::new(&a.candidate),
                Path::new(&a.reference),
                &path,
                a.memory_pairs,
                a.seed + MEMORY_SEED,
                &args,
                outputs,
            )?
        } else {
            Value::object([("status", Value::str("missing-correctness-output"))])
        };
        let memory_status = measured
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("invalid-memory-observation");
        let admitted = memory_status == "qualified"
            && a.memory_pairs >= 30
            && cfg!(all(
                target_os = "linux",
                any(target_arch = "aarch64", target_arch = "arm"),
                target_env = "gnu"
            ));
        let lower = |name| {
            measured
                .get(name)
                .and_then(Value::as_f64)
                .is_some_and(|ratio| ratio < 1.0)
        };
        let heap_gate = admitted
            && lower("heap_ratio_upper_95")
            && measured
                .get("heap_lower_in_each_pair")
                .is_some_and(Value::truthy);
        let rss_gate = admitted && lower("total_rss_ratio_upper_95");
        let memory_status = Value::str(memory_status);
        record.push(("memory".into(), measured));
        let flag = |key: &str| correctness.get(key).is_some_and(Value::truthy);
        let pipeline = flag("pipeline");
        let verdict = vec![
            ("conversion".to_owned(), Value::Bool(flag("conversion"))),
            ("mux".to_owned(), Value::Bool(flag("mux"))),
            ("encoder".to_owned(), Value::Bool(flag("encoder"))),
            ("pipeline_parity".to_owned(), Value::Bool(pipeline)),
            ("runtime_gate".to_owned(), Value::Bool(runtime_gate)),
            (
                "runtime_target_0_80".to_owned(),
                Value::Bool(runtime_target),
            ),
            ("heap_gate".to_owned(), Value::Bool(heap_gate)),
            ("rss_gate".to_owned(), Value::Bool(rss_gate)),
            ("memory_status".to_owned(), memory_status),
            (
                "all_gates".to_owned(),
                Value::Bool(pipeline && runtime_gate && heap_gate && rss_gate),
            ),
        ];
        record.push(("verdict".into(), Value::Object(verdict.clone())));
        write(
            &cdir.join("result.json"),
            json::to_python(&Value::Object(record), Some(1)),
        )?;
        let mut line = vec![("id".to_owned(), Value::str(id.as_str()))];
        line.extend(verdict);
        line.push((
            "runtime_upper95".into(),
            runtime_upper.map_or(Value::Null, |bound| Value::float(python::round(bound, 4))),
        ));
        line.push(("correctness".into(), correctness));
        let line = Value::Object(line);
        println!("{}", json::to_python(&line, None));
        summary.push(line);
    }
    write(
        &a.out.join("summary.json"),
        json::to_python(&Value::Array(summary), Some(1)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warmups(candidate_digest: u64) -> Value {
        let run = |digest: u64| {
            Value::object([
                ("status", Value::str("ok")),
                (
                    "data",
                    Value::object([("bytes", Value::int(188)), ("digest", Value::int(digest))]),
                ),
            ])
        };
        Value::object([(
            "warmups",
            Value::Array(vec![
                Value::object([
                    ("worker", Value::str("candidate")),
                    ("result", run(candidate_digest)),
                ]),
                Value::object([("worker", Value::str("reference")), ("result", run(0xff))]),
            ]),
        )])
    }

    #[test]
    fn timed_outputs_must_be_each_workers_checked_stream() {
        let outputs = json::parse(
            r#"{"candidate": {"bytes": 188, "fnv1a64": "00000000000000ab"},
                "reference": {"bytes": 188, "fnv1a64": "00000000000000ff"}}"#,
        )
        .unwrap();
        assert!(outputs_match(&warmups(0xab), &outputs));
        assert!(!outputs_match(&warmups(0xff), &outputs));
        assert!(!outputs_match(&warmups(0xab), &Value::Object(vec![])));
    }
}
