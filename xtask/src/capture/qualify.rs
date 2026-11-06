//! Capture diagnostics on native Linux.
//!
//! For every required case in `harness/capture/cases.json`:
//! 1. generate the RGBA input (`harness/capture/generate_inputs.py`, run with
//!    the numpy Python named by `TRIERGON_PYTHON`, else `python3` on `PATH`)
//!    and record its SHA-256;
//! 2. correctness: `cargo xtask capture compare` (conversion, mux, encoder and
//!    pipeline rules of `harness/capture/contract.json`);
//! 3. runtime: paired, randomly ordered worker processes; each sample is the
//!    median of `--iterations` in-process sessions after one discarded warm-up
//!    session; the ratio interval is the bootstrap of the median paired ratio.
//!    Gate: upper 95% bound ≤ 1.20 (target 0.80);
//! 4. memory: the glibc observer (`harness/allocations-linux.c`) in separate
//!    heap and RSS runs, alternating order. The RSS diagnostic reports growth
//!    during the operation. Capture qualification requires observer admission
//!    and total resident peak RSS, so memory is recorded as diagnostic only and
//!    the memory gates are not met.
//!
//! The machine must be otherwise idle: a CPU-contention check runs before
//! every timing and memory pair, and contaminated pairs wait and are retried,
//! never silently used. Every raw observation is written.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::codecs::correctness::file_sha;
use crate::harness::timing::competing_builds;
use crate::json::{self, Value};
use crate::process::{check_output, exit_code};
use crate::pyrandom::PyRandom;
use crate::{python, sha256, stats};

const RUNTIME_GATE: f64 = 1.2;
const RUNTIME_TARGET: f64 = 0.80;
const RESAMPLES: usize = 4000;

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
const SOURCE_TREES: [&str; 3] = ["crates/capture", "harness/capture", "xtask/src/capture"];
const SOURCE_FILES: [&str; 9] = [
    "Cargo.toml",
    "Cargo.lock",
    "xtask/src/harness/timing.rs",
    "xtask/src/numpy.rs",
    "xtask/src/pyrandom.rs",
    "xtask/src/python.rs",
    "xtask/src/stats.rs",
    "harness/allocations-linux.c",
    "harness/rss-bound-api.h",
];

fn write(path: &Path, text: String) -> Result<(), String> {
    fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Wait until no competing build uses CPU; each wait is logged.
fn wait_idle(log: &mut Vec<Value>) -> Result<(), String> {
    for _ in 0..20 {
        let busy = competing_builds()?;
        if busy.is_empty() {
            return Ok(());
        }
        log.push(Value::object([
            ("contaminants", Value::strings(busy)),
            ("utc", Value::float(python::time())),
        ]));
        std::thread::sleep(Duration::from_secs(3));
    }
    Err(format!(
        "machine not idle: {}",
        python::repr(log.last().expect("logged"))
    ))
}

fn number(value: &Value) -> Result<f64, String> {
    value
        .as_f64()
        .ok_or_else(|| format!("not a number: {}", json::to_python(value, None)))
}

/// `statistics.median`: the middle item itself for an odd count (an integer
/// stays an integer), the mean of the two middle items otherwise.
pub fn median(values: &[Value]) -> Result<Value, String> {
    let mut sorted: Vec<(f64, &Value)> = values
        .iter()
        .map(|v| Ok((number(v)?, v)))
        .collect::<Result<_, String>>()?;
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    let n = sorted.len();
    if n == 0 {
        return Err("no median for empty data".into());
    }
    if n % 2 == 1 {
        return Ok(sorted[n / 2].1.clone());
    }
    let (a, b) = (sorted[n / 2 - 1].1, sorted[n / 2].1);
    let sum = match (a, b) {
        (Value::Number(x), Value::Number(y)) if x.is_integer() && y.is_integer() => {
            (x.as_i128().unwrap_or(0) + y.as_i128().unwrap_or(0)) as f64
        }
        _ => number(a)? + number(b)?,
    };
    Ok(Value::float(sum / 2.0))
}

fn interval(pairs: &[(f64, f64)], seed: u64) -> Result<Value, String> {
    let interval = stats::ratio_interval(pairs, seed, RESAMPLES)?;
    Ok(crate::codecs::timing::interval_value(&interval))
}

fn upper(interval: &Value) -> f64 {
    interval
        .get("upper_95")
        .and_then(Value::as_f64)
        .unwrap_or(f64::NAN)
}

fn case_args(case: &Value, keys: &[&str]) -> Vec<String> {
    keys.iter()
        .map(|k| python::str(case.get(k).unwrap_or(&Value::Null)))
        .collect()
}

fn time_sample(worker: &str, case: &Value, path: &str, iterations: usize) -> Result<Value, String> {
    let mut argv = vec![worker.to_owned(), "time".into()];
    argv.extend(case_args(
        case,
        &["width", "height", "flip", "frames", "bitrate"],
    ));
    argv.extend([path.to_owned(), (iterations + 1).to_string()]);
    let out = check_output(&argv)?;
    let rows = python::splitlines(&out)
        .into_iter()
        .map(|line| python::loads(line).ok_or_else(|| format!("{worker}: not JSON: {line}")))
        .collect::<Result<Vec<_>, _>>()?;
    let field = |row: &Value, key: &str| {
        row.get(key)
            .cloned()
            .ok_or_else(|| format!("{worker}: time row without {key}"))
    };
    let first = rows.first().ok_or("worker printed no time rows")?;
    let measured = rows[1..]
        .iter()
        .map(|r| field(r, "ns"))
        .collect::<Result<Vec<_>, _>>()?;
    let mut hashes: Vec<Value> = Vec::new();
    for row in &rows {
        let hash = field(row, "fnv1a64")?;
        if !hashes.iter().any(|h| crate::harness::diff::equal(h, &hash)) {
            hashes.push(hash);
        }
    }
    hashes.sort_by(|a, b| match (a, b) {
        (Value::String(x), Value::String(y)) => x.cmp(y),
        _ => number(a)
            .unwrap_or(f64::NAN)
            .total_cmp(&number(b).unwrap_or(f64::NAN)),
    });
    Ok(Value::object([
        ("warmup_ns", field(first, "ns")?),
        ("ns", Value::Array(measured.clone())),
        ("median_ns", median(&measured)?),
        ("hashes", Value::Array(hashes)),
        ("bytes", field(first, "bytes")?),
    ]))
}

fn shuffled(rng: &mut PyRandom) -> [&'static str; 2] {
    let mut order = ["candidate", "reference"];
    rng.shuffle(&mut order);
    order
}

fn runtime(
    candidate: &str,
    reference: &str,
    case: &Value,
    path: &str,
    pairs: usize,
    iterations: usize,
    seed: u64,
) -> Result<Value, String> {
    let mut rng = PyRandom::new(seed);
    let mut contamination = Vec::new();
    let mut samples = Vec::new();
    for i in 0..pairs {
        wait_idle(&mut contamination)?;
        let order = shuffled(&mut rng);
        let mut pair = vec![
            ("index".to_owned(), Value::int(i as i64)),
            ("order".to_owned(), Value::strings(order)),
        ];
        for role in order {
            let worker = if role == "candidate" {
                candidate
            } else {
                reference
            };
            pair.push((
                role.to_owned(),
                time_sample(worker, case, path, iterations)?,
            ));
        }
        samples.push(Value::Object(pair));
    }
    let medians = |role: &str| -> Vec<Value> {
        samples
            .iter()
            .map(|p| {
                p.get(role)
                    .and_then(|r| r.get("median_ns"))
                    .cloned()
                    .unwrap_or(Value::Null)
            })
            .collect()
    };
    let (c, r) = (medians("candidate"), medians("reference"));
    let pairs_ns = c
        .iter()
        .zip(&r)
        .map(|(a, b)| Ok((number(a)?, number(b)?)))
        .collect::<Result<Vec<_>, String>>()?;
    let ratio = interval(&pairs_ns, 0)?;
    let stable = samples.iter().all(|p| {
        ["candidate", "reference"].iter().all(|role| {
            p.get(role)
                .and_then(|s| s.get("hashes"))
                .and_then(Value::as_array)
                .is_some_and(|h| h.len() == 1)
        })
    });
    let upper_95 = upper(&ratio);
    Ok(Value::object([
        ("pairs", Value::int(pairs as i64)),
        ("iterations", Value::int(iterations as i64)),
        ("seed", Value::int(seed)),
        ("samples", Value::Array(samples.clone())),
        ("ratio", ratio),
        (
            "candidate_median_ms",
            Value::float(number(&median(&c)?)? / 1e6),
        ),
        (
            "reference_median_ms",
            Value::float(number(&median(&r)?)? / 1e6),
        ),
        ("deterministic_output", Value::Bool(stable)),
        ("contamination_waits", Value::Array(contamination)),
        ("gate_pass", Value::Bool(upper_95 <= RUNTIME_GATE)),
        ("target_pass", Value::Bool(upper_95 <= RUNTIME_TARGET)),
    ]))
}

fn observe(
    worker: &str,
    case: &Value,
    path: &str,
    observer: &str,
    mode: &str,
    receipt: &Path,
) -> Result<Value, String> {
    let mut argv: Vec<OsString> = vec![worker.into(), "memory".into()];
    argv.extend(
        case_args(case, &["width", "height", "flip", "frames", "bitrate"])
            .into_iter()
            .map(OsString::from),
    );
    argv.push(path.into());
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .env("LD_PRELOAD", observer)
        .env("TRIERGON_ALLOCATION_REPORT", python::path(receipt))
        .env("TRIERGON_MEMORY_MODE", mode)
        .output()
        .map_err(|e| format!("{worker}: {e}"))?;
    let data = if receipt.exists() {
        let text =
            fs::read_to_string(receipt).map_err(|e| format!("{}: {e}", receipt.display()))?;
        python::loads(&text).ok_or_else(|| format!("{}: not JSON", receipt.display()))?
    } else {
        Value::Null
    };
    Ok(Value::object([
        ("exit", Value::int(exit_code(output.status))),
        (
            "stdout",
            Value::str(python::tail(&python::text(&output.stdout), 400)),
        ),
        (
            "stderr",
            Value::str(python::tail(&python::text(&output.stderr), 400)),
        ),
        ("receipt", data),
    ]))
}

fn receipt_int(receipt: &Value, key: &str) -> Result<i128, String> {
    receipt
        .get(key)
        .and_then(python::int)
        .ok_or_else(|| format!("receipt without {key}"))
}

fn valid(observation: &Value, mode: &str) -> Result<bool, String> {
    if !observation
        .get("exit")
        .is_some_and(|e| python::equals_int(e, 0))
    {
        return Ok(false);
    }
    let d = observation.get("receipt").unwrap_or(&Value::Null);
    if matches!(d, Value::Null) {
        return Ok(false);
    }
    let field = |key: &str| d.get(key).ok_or_else(|| format!("receipt without {key}"));
    Ok(field("status")?.as_str() == Some("measured")
        && field("mode")?.as_str() == Some(mode)
        && python::equals_int(field("error_bits")?, 0))
}

#[allow(clippy::too_many_arguments)] // The memory run's case, workers and settings.
fn memory(
    candidate: &str,
    reference: &str,
    case: &Value,
    path: &str,
    observer: &str,
    pairs: usize,
    out: &Path,
    seed: u64,
) -> Result<Value, String> {
    let mut rng = PyRandom::new(seed);
    let mut contamination = Vec::new();
    let mut runs: Vec<(&str, Vec<Value>)> = vec![("heap", Vec::new()), ("rss", Vec::new())];
    for (mode, list) in &mut runs {
        for i in 0..pairs {
            wait_idle(&mut contamination)?;
            let order = shuffled(&mut rng);
            let mut pair = vec![
                ("index".to_owned(), Value::int(i as i64)),
                ("order".to_owned(), Value::strings(order)),
            ];
            for role in order {
                let receipt = out.join(format!("{mode}-{i}-{role}.json"));
                if receipt.exists() {
                    fs::remove_file(&receipt).map_err(|e| e.to_string())?;
                }
                let worker = if role == "candidate" {
                    candidate
                } else {
                    reference
                };
                pair.push((
                    role.to_owned(),
                    observe(worker, case, path, observer, mode, &receipt)?,
                ));
            }
            list.push(Value::Object(pair));
        }
    }
    let (heap, rss) = (runs[0].1.clone(), runs[1].1.clone());
    let all_valid = |list: &[Value], mode: &str| -> Result<bool, String> {
        for p in list {
            for role in ["candidate", "reference"] {
                if !valid(p.get(role).unwrap_or(&Value::Null), mode)? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    };
    let heap_ok = all_valid(&heap, "heap")?;
    let rss_ok = all_valid(&rss, "rss")?;
    let receipt = |p: &Value, role: &str| {
        p.get(role)
            .and_then(|o| o.get("receipt"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let mut result = vec![
        ("pairs".to_owned(), Value::int(pairs as i64)),
        ("heap".to_owned(), Value::Array(heap.clone())),
        ("rss".to_owned(), Value::Array(rss.clone())),
        (
            "contamination_waits".to_owned(),
            Value::Array(contamination),
        ),
        ("heap_valid".to_owned(), Value::Bool(heap_ok)),
        ("rss_valid".to_owned(), Value::Bool(rss_ok)),
    ];
    if heap_ok {
        let peaks = |role: &str| -> Result<Vec<i128>, String> {
            heap.iter()
                .map(|p| receipt_int(&receipt(p, role), "heap_peak_bytes"))
                .collect()
        };
        let (c, r) = (peaks("candidate")?, peaks("reference")?);
        let as_values = |v: &[i128]| Value::Array(v.iter().map(|&x| Value::int(x)).collect());
        result.push((
            "heap_peak_bytes".into(),
            Value::object([("candidate", as_values(&c)), ("reference", as_values(&r))]),
        ));
        let pairs: Vec<(f64, f64)> = c
            .iter()
            .zip(&r)
            .map(|(&a, &b)| (a as f64, b as f64))
            .collect();
        result.push(("heap_ratio".into(), interval(&pairs, 0)?));
    }
    if rss_ok {
        let (mut bound, mut observed) = (Vec::new(), Vec::new());
        for p in &rss {
            let (cd, rd) = (receipt(p, "candidate"), receipt(p, "reference"));
            let page = receipt_int(&cd, "page_size")?;
            let candidate_upper = receipt_int(&cd, "rss_peak_upper_bound_bytes")?
                - receipt_int(&cd, "rss_start")?
                + 2 * page;
            let reference_lower =
                receipt_int(&rd, "rss_peak_observed")? - receipt_int(&rd, "rss_start")? - 2 * page;
            bound.push((candidate_upper, reference_lower));
            observed.push((
                receipt_int(&cd, "rss_peak_observed")? - receipt_int(&cd, "rss_start")? + 1,
                receipt_int(&rd, "rss_peak_observed")? - receipt_int(&rd, "rss_start")? + 1,
            ));
        }
        let pairs_value = |v: &[(i128, i128)]| {
            Value::Array(
                v.iter()
                    .map(|&(a, b)| Value::Array(vec![Value::int(a), Value::int(b)]))
                    .collect(),
            )
        };
        let floats = |v: &[(i128, i128)]| -> Vec<(f64, f64)> {
            v.iter().map(|&(a, b)| (a as f64, b as f64)).collect()
        };
        result.push(("rss_bound_pairs".into(), pairs_value(&bound)));
        result.push(("rss_observed_growth_pairs".into(), pairs_value(&observed)));
        if bound.iter().all(|&(a, b)| a > 0 && b > 0) {
            result.push(("rss_bound_ratio".into(), interval(&floats(&bound), 0)?));
        } else {
            result.push(("rss_bound_ratio".into(), Value::Null));
        }
        result.push((
            "rss_observed_ratio".into(),
            interval(&floats(&observed), 0)?,
        ));
    }
    Ok(Value::Object(result))
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
        manifest.push((file.to_owned(), Value::str(file_sha(&root.join(file))?)));
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
    let identity = Value::object([
        ("source_digest", Value::str(digest)),
        ("source_manifest", Value::Object(manifest)),
        ("candidate_sha256", sha(&a.candidate)?),
        ("reference_sha256", sha(&a.reference)?),
        (
            "reference_small_sha256",
            match &a.reference_small {
                Some(path) => sha(path)?,
                None => Value::Null,
            },
        ),
        ("observer_sha256", sha(&a.observer)?),
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
        let timed = runtime(
            &a.candidate,
            &a.reference,
            case,
            &path_text,
            a.pairs,
            a.iterations,
            a.seed,
        )?;
        let runtime_gate = timed.get("gate_pass").is_some_and(Value::truthy);
        let runtime_target = timed.get("target_pass").is_some_and(Value::truthy);
        let runtime_upper = upper(timed.get("ratio").unwrap_or(&Value::Null));
        record.push(("runtime".into(), timed));
        if let Some(small) = &a.reference_small {
            record.push((
                "runtime_vs_small_config".into(),
                runtime(
                    &a.candidate,
                    small,
                    case,
                    &path_text,
                    10.max(a.pairs / 3),
                    a.iterations,
                    a.seed + 1,
                )?,
            ));
        }
        let mdir = cdir.join("memory");
        fs::create_dir_all(&mdir).map_err(|e| format!("{}: {e}", mdir.display()))?;
        let measured = memory(
            &a.candidate,
            &a.reference,
            case,
            &path_text,
            &a.observer,
            a.memory_pairs,
            &mdir,
            a.seed + 2,
        )?;
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
            ("heap_gate".to_owned(), Value::Bool(false)),
            ("rss_gate".to_owned(), Value::Bool(false)),
            ("memory_status".to_owned(), Value::str("diagnostic-only")),
            ("all_gates".to_owned(), Value::Bool(false)),
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
            Value::float(python::round(runtime_upper, 4)),
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

    #[test]
    fn median_keeps_python_types() {
        // python3 -c "import statistics as s; print(s.median([3, 1, 2]), s.median([4, 1, 3, 2]), s.median([1.5, 2]))"
        let v = |t: &str| json::parse(t).unwrap();
        assert_eq!(
            json::to_python(&median(v("[3, 1, 2]").as_array().unwrap()).unwrap(), None),
            "2"
        );
        assert_eq!(
            json::to_python(
                &median(v("[4, 1, 3, 2]").as_array().unwrap()).unwrap(),
                None
            ),
            "2.5"
        );
        assert_eq!(
            json::to_python(&median(v("[1.5, 2]").as_array().unwrap()).unwrap(), None),
            "1.75"
        );
    }
}
