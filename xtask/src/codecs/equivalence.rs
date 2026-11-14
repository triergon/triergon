//! Exact decoder-output identities of the codec crates.
//!
//! Runs each codec's `decode_<codec>` example over the local conformance
//! vectors and the `codec-worker` raw operation over the decoder workloads,
//! and records the SHA-256 of every output file with the exit status and the
//! digest of stderr. Refactors of decoders, bit readers and sessions must
//! reproduce the record exactly; this is equivalence evidence only, never
//! parity with or performance against FFmpeg.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::codecs::correctness::file_sha;
use crate::harness::worker;
use crate::json::{self, Value};
use crate::process::TempDir;
use crate::{python, sha256};

const TIMEOUT: Duration = Duration::from_secs(600);

/// The codec crates and the conformance folders their example decodes.
const DECODERS: [(&str, &[&str]); 5] = [
    ("aac", &["aac"]),
    ("ac3", &["ac3", "eac3"]),
    ("dca", &["dts"]),
    ("h264", &["h264-conformance"]),
    ("hevc", &["hevc-conformance"]),
];

/// One recorded run: its record name, command line and output file (if any).
pub struct Job {
    pub name: String,
    pub argv: Vec<String>,
    pub output: Option<PathBuf>,
}

/// Run the jobs on `threads` threads; results in job order.
pub fn run_jobs(jobs: &[Job], threads: usize, entry: fn(&Job) -> Value) -> Vec<(String, Value)> {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<Value>>> = Mutex::new(vec![None; jobs.len()]);
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1).min(jobs.len().max(1)) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= jobs.len() {
                        break;
                    }
                    let value = entry(&jobs[i]);
                    results.lock().expect("results")[i] = Some(value);
                }
            });
        }
    });
    let results = results.into_inner().expect("results");
    jobs.iter()
        .zip(results)
        .map(|(job, value)| (job.name.clone(), value.expect("every job ran")))
        .collect()
}

/// Hash and remove a job's output file.
pub fn take_output(output: &Path) -> Value {
    if !output.exists() {
        return Value::Null;
    }
    let digest = file_sha(output).map_or(Value::Null, Value::str);
    let _ = fs::remove_file(output);
    digest
}

fn decoder_entry(job: &Job) -> Value {
    let argv: Vec<OsString> = job.argv.iter().map(OsString::from).collect();
    let (exit, stderr) = match worker::run(&argv, &[], TIMEOUT) {
        Ok(Some(done)) => (Value::int(done.code), Value::str(sha256::hex(&done.stderr))),
        Ok(None) => (Value::str("timeout"), Value::Null),
        Err(error) => panic!("{error}"),
    };
    let output = job.output.as_deref().map_or(Value::Null, take_output);
    Value::object([
        ("exit", exit),
        ("output_sha256", output),
        ("stderr_sha256", stderr),
    ])
}

/// `cargo rustc --release --locked ARGS...` for the worker target in `root`, linked
/// under the gate build's link policy; ARGS select one binary or example; `quiet`
/// captures its output.
pub fn cargo_build(root: &Path, args: &[&str], quiet: bool) -> Result<(), String> {
    let mut command = Command::new("cargo");
    command
        .args(["rustc", "--release", "--locked"])
        .args(crate::harness::gate_build::target_args()?)
        .args(args)
        .arg("--")
        .args(crate::harness::gate_build::worker_link_arguments()?)
        .env("CARGO_INCREMENTAL", "0")
        .current_dir(root);
    let status = if quiet {
        let output = command.output().map_err(|e| format!("cargo: {e}"))?;
        output.status
    } else {
        command.status().map_err(|e| format!("cargo: {e}"))?
    };
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo rustc {}: {status}", args.join(" ")))
    }
}

/// Write the record and, with `compare`, check it against an earlier one.
/// Returns the exit code: 1 when the records differ.
pub fn finish(
    output: &Path,
    record: Value,
    summary: &str,
    compare: Option<&Path>,
) -> Result<i32, String> {
    fs::write(output, json::to_python(&record, Some(1)) + "\n")
        .map_err(|e| format!("{}: {e}", output.display()))?;
    println!("{summary}");
    let Some(compare) = compare else {
        return Ok(0);
    };
    let before =
        crate::checks::read_json(compare).map_err(|e| format!("{}: {e}", compare.display()))?;
    let empty = Value::Object(Vec::new());
    let before = before.get("outputs").unwrap_or(&empty);
    let after = record.get("outputs").unwrap_or(&empty);
    let keys = |v: &Value| -> Vec<String> {
        match v {
            Value::Object(m) => m.iter().map(|(k, _)| k.clone()).collect(),
            _ => Vec::new(),
        }
    };
    let mut names = keys(before);
    names.extend(keys(after));
    names.sort();
    names.dedup();
    let changed: Vec<String> = names
        .into_iter()
        .filter(|k| match (before.get(k), after.get(k)) {
            (Some(a), Some(b)) => !crate::harness::diff::equal(a, b),
            (None, None) => false,
            _ => true,
        })
        .collect();
    if !changed.is_empty() {
        println!(
            "NOT EQUIVALENT {} {}",
            changed.len(),
            python::repr_strings(&changed[..changed.len().min(20)])
        );
        return Ok(1);
    }
    println!("byte-identical to {}", python::path(compare));
    Ok(0)
}

/// Every file below `dir`, sorted, as `sorted(Path.rglob('*'))` with `is_file()`.
fn files(dir: &Path) -> Vec<PathBuf> {
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                visit(&path, out);
            }
            out.push(path);
        }
    }
    let mut out = Vec::new();
    visit(dir, &mut out);
    out.sort();
    out.retain(|p| p.is_file());
    out
}

/// `codecs equivalence --output F [--compare F] [--jobs N]`.
pub fn run(
    root: &Path,
    output: &Path,
    compare: Option<&Path>,
    threads: usize,
) -> Result<i32, String> {
    cargo_build(root, &["-p", "codec-worker"], false)?;
    let suite = root.join(".research/fate-samples");
    let examples = root
        .join(crate::harness::gate_build::release_dir()?)
        .join("examples");
    let scratch = TempDir::new(None)?;
    let mut jobs = Vec::new();
    for (codec, folders) in DECODERS {
        let example = format!("decode_{}", if codec == "dca" { "dts" } else { codec });
        cargo_build(
            root,
            &[
                "--example",
                &example,
                "-p",
                &format!("triergon-codec-{codec}"),
            ],
            true,
        )?;
        let copy = python::path(&examples.join(&example));
        for folder in folders {
            for vector in files(&suite.join(folder)) {
                let out = scratch.0.join(format!("{codec}-{}.out", jobs.len()));
                let relative = vector.strip_prefix(&suite).unwrap_or(&vector);
                let mut argv = vec![copy.clone(), python::path(&vector), python::path(&out)];
                if codec == "hevc" {
                    argv.push("--quiet".into());
                }
                jobs.push(Job {
                    name: format!("{codec}/{}", python::path(relative)),
                    argv,
                    output: Some(out),
                });
            }
        }
    }
    let workloads = crate::checks::read_json(&root.join("harness/codecs/workloads.json"))?;
    for workload in workloads
        .get("workloads")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        let field = |key: &str| {
            workload
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("workload without {key}"))
        };
        let id = field("id")?;
        let out = scratch.0.join(format!("worker-{id}.out"));
        jobs.push(Job {
            name: format!("worker/{id}"),
            argv: vec![
                python::path(
                    &root
                        .join(crate::harness::gate_build::release_dir()?)
                        .join("codec-worker"),
                ),
                format!("{}-raw", field("operation")?),
                python::path(&root.join(field("path")?)),
                python::path(&out),
            ],
            output: Some(out),
        });
    }
    let mut record = run_jobs(&jobs, threads, decoder_entry);
    record.sort_by(|a, b| a.0.cmp(&b.0));
    let passed = record
        .iter()
        .filter(|(_, r)| r.get("exit").is_some_and(|e| python::equals_int(e, 0)))
        .count();
    let summary = format!("{} decoder outputs; {passed} exit 0", record.len());
    finish(
        output,
        Value::object([
            ("schema", Value::int(1)),
            ("outputs", Value::Object(record)),
        ]),
        &summary,
        compare,
    )
}
