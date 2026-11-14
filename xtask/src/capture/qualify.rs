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
use std::time::Duration;

use crate::codecs::correctness::file_sha;
use crate::harness::gate_build;
use crate::harness::memory;
use crate::harness::policy;
use crate::harness::timing::{self, GATE_PAIRS, Operation, Pairs, Work};
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
    /// Let a gate session stop at an early look (`--no-early-stop` clears it).
    pub early_stop: bool,
    /// Which half of the qualification this run does.
    pub stage: Stage,
}

/// `capture qualify --stage`: correctness and timing, the memory of cases a
/// timing run recorded, or both in one run.
pub enum Stage {
    Both,
    Timing,
    Memory { timing: PathBuf },
}

impl Stage {
    fn name(&self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Timing => "timing",
            Self::Memory { .. } => "memory",
        }
    }
}

/// The key of a qualification run as a gate step: its settings, the cases and
/// their input generator, the workers and the oracle decoders.
pub fn gate_key(root: &Path, a: &Options) -> Result<crate::harness::reuse::Key, String> {
    let settings = Value::object([
        ("only", Value::strings(a.only.clone())),
        ("pairs", Value::int(a.pairs as i64)),
        ("iterations", Value::int(a.iterations as i64)),
        ("memory_pairs", Value::int(a.memory_pairs as i64)),
        ("seed", Value::int(a.seed)),
        ("early_stop", Value::Bool(a.early_stop)),
        (
            "kernel_bound",
            std::env::var("TRIERGON_RSS_KERNEL_BOUND").map_or(Value::Null, Value::str),
        ),
        ("stage", Value::str(a.stage.name())),
    ]);
    let generator = root.join("harness/capture/generate_inputs.py");
    let oracle = Path::new(&a.oracle_bin);
    let (ffmpeg, ffprobe) = (oracle.join("ffmpeg"), oracle.join("ffprobe"));
    let mut workers = vec![
        ("candidate", Path::new(&a.candidate)),
        ("reference", Path::new(&a.reference)),
        ("ffmpeg", ffmpeg.as_path()),
        ("ffprobe", ffprobe.as_path()),
    ];
    if let Some(small) = &a.reference_small {
        workers.push(("reference-small", Path::new(small)));
    }
    let mut files = vec![a.cases.clone(), generator, PathBuf::from(&a.observer)];
    if let Stage::Memory { timing } = &a.stage {
        // A memory run measures what its timing run recorded.
        let mut recorded = vec![timing.join("identity.json")];
        if let Ok(entries) = fs::read_dir(timing) {
            for entry in entries.flatten() {
                let record = entry.path().join(TIMING_RECORD);
                if record.is_file() {
                    recorded.push(record);
                }
            }
        }
        recorded.sort();
        files.extend(recorded);
    }
    let files: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
    crate::harness::reuse::Key::new(root, "capture qualify", settings, &files, &workers)
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
    pairs: Pairs,
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
            deterministic && pairs.planned >= GATE_PAIRS && interval.upper_95 <= bound
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

/// A case once its correctness and timing are known, before its memory: its
/// record, which `--stage timing` writes as `ID/timing.json`.
struct MeasuredCase {
    id: String,
    input: PathBuf,
    record: Vec<(String, Value)>,
}

impl MeasuredCase {
    /// The case's correctness summary, which holds both workers' outputs.
    fn correctness(&self) -> &Value {
        self.field("correctness_summary")
    }

    fn field(&self, key: &str) -> &Value {
        self.record
            .iter()
            .find_map(|(k, v)| (k == key).then_some(v))
            .unwrap_or(&Value::Null)
    }

    /// The observed worker arguments of the case's memory runs, reading `input`.
    fn memory_arguments(&self, input: &Path) -> Vec<String> {
        let mut arguments = vec!["memory".to_owned()];
        arguments.extend(case_args(self.field("case"), &SESSION));
        arguments.push(python::path(input));
        arguments
    }

    /// A copy of the case's input at `directory`, with the hash its timing recorded.
    fn copy_input(&self, directory: &Path) -> Result<PathBuf, String> {
        let name = self
            .input
            .file_name()
            .ok_or("an input without a file name")?;
        let copy = directory.join(&self.id).join(name);
        fs::create_dir_all(copy.parent().expect("under the directory"))
            .map_err(|e| format!("{}: {e}", directory.display()))?;
        fs::copy(&self.input, &copy).map_err(|e| format!("{}: {e}", copy.display()))?;
        if file_sha(&copy).ok().as_deref() != self.field("input_sha256").as_str() {
            return Err(format!("{}: differs from the timed input", copy.display()));
        }
        Ok(copy)
    }

    /// The case `--stage timing` recorded in `timing/ID/timing.json`.
    fn read(timing: &Path, case: &Value) -> Result<Self, String> {
        let id = python::str(case.get("id").unwrap_or(&Value::Null));
        let path = timing.join(&id).join(TIMING_RECORD);
        let Value::Object(record) =
            crate::checks::read_json(&path).map_err(|e| format!("{}: {e}", path.display()))?
        else {
            return Err(format!("{}: not a timing record", path.display()));
        };
        let measured = Self {
            id,
            input: PathBuf::new(),
            record,
        };
        if !crate::harness::diff::equal(measured.field("case"), case) {
            return Err(format!("{}: records another case", path.display()));
        }
        let input = PathBuf::from(python::str(measured.field("input")));
        if file_sha(&input).ok().as_deref() != measured.field("input_sha256").as_str() {
            return Err(format!(
                "{}: its input changed since timing",
                input.display()
            ));
        }
        Ok(Self { input, ..measured })
    }
}

/// Each case's record after `--stage timing`.
const TIMING_RECORD: &str = "timing.json";

/// The per-case input copies of a collection that runs cases side by side, under
/// the run's inputs directory; removed with this value.
struct InputCopies(PathBuf);

impl Drop for InputCopies {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Generate the case's input if needed, compare both workers' outputs and time
/// them.
fn correctness_and_timing(
    root: &Path,
    a: &Options,
    exe: &Path,
    gate: policy::Runtime,
    refusals: &[String],
    case: &Value,
) -> Result<MeasuredCase, String> {
    let text = |key: &str| python::str(case.get(key).unwrap_or(&Value::Null));
    let id = text("id");
    let directory = a.out.join(&id);
    fs::create_dir_all(&directory).map_err(|e| format!("{}: {e}", directory.display()))?;
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
        let python = python::interpreter();
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
    let compare = Command::new(exe)
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
            &python::path(&directory.join("correctness")),
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
        Pairs::gate(a.pairs, gate.ceiling).early_stop(a.early_stop),
        iterations,
        a.seed,
        outputs,
    )?;
    if !refusals.is_empty() {
        withhold_gates(&mut timed);
    }
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
                Pairs::fixed(10.max(a.pairs / 3)),
                iterations,
                a.seed + SMALL_CONFIG_SEED,
                None,
            )?,
        ));
    }
    Ok(MeasuredCase {
        id,
        input: path,
        record,
    })
}

/// Grade a case with its memory, write its result and return its summary line.
fn finish_case(a: &Options, case: MeasuredCase, measured: Value) -> Result<Value, String> {
    let correctness = case.correctness().clone();
    let timed = case.field("runtime");
    let runtime_gate =
        correctness.get("outputs").is_some() && timed.get("gate_pass").is_some_and(Value::truthy);
    let runtime_target = timed.get("target_pass").is_some_and(Value::truthy);
    // The validator's interval: a refused record has none.
    let runtime_upper = timed
        .get("ratio")
        .and_then(|ratio| ratio.get("upper_95"))
        .and_then(Value::as_f64)
        .filter(|bound| bound.is_finite());
    let MeasuredCase { id, mut record, .. } = case;
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
    let directory = a.out.join(&id);
    fs::create_dir_all(&directory).map_err(|e| format!("{}: {e}", directory.display()))?;
    write(
        &directory.join("result.json"),
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
    Ok(line)
}

/// Collect the memory of every case in one collection, side by side, each slot
/// measuring its own copies of the gate build's workers, and finish each case.
fn memory_stage(
    root: &Path,
    a: &Options,
    refusals: &[String],
    measured_cases: Vec<MeasuredCase>,
) -> Result<Vec<Value>, String> {
    let workers = memory::WorkerPaths {
        candidate: Path::new(&a.candidate),
        reference: Path::new(&a.reference),
        registered: refusals.is_empty(),
    };
    let pairs = Pairs::gate(a.memory_pairs, memory::CEILING).early_stop(a.early_stop);
    // Cases of one generated input would share its file and so run one after
    // another; side by side, each measures its own copy, of the same bytes.
    let side_by_side = gate_build::allowed_cpu_list().is_none_or(|cpus| cpus.len() > 1);
    let copies = side_by_side.then(|| {
        InputCopies(
            a.inputs
                .join(format!(".memory-copies-{}", std::process::id())),
        )
    });
    let mut inputs = Vec::new();
    for measured in &measured_cases {
        inputs.push(match (&copies, measured.correctness().get("outputs")) {
            (Some(copies), Some(_)) => measured.copy_input(&copies.0)?,
            _ => measured.input.clone(),
        });
    }
    let arguments: Vec<Vec<String>> = measured_cases
        .iter()
        .zip(&inputs)
        .map(|(measured, input)| measured.memory_arguments(input))
        .collect();
    let mut jobs = Vec::new();
    let mut collected = Vec::new();
    for (index, ((measured, args), path)) in measured_cases
        .iter()
        .zip(&arguments)
        .zip(&inputs)
        .enumerate()
    {
        let Some(outputs) = measured.correctness().get("outputs") else {
            continue;
        };
        jobs.push(memory::Job::new(&workers, [path.clone()], move |slot| {
            memory::collect_command(
                slot,
                root,
                workers,
                path,
                pairs,
                a.seed + MEMORY_SEED,
                args,
                outputs,
            )
        }));
        collected.push(index);
    }
    let mut memories: Vec<Option<Value>> = vec![None; measured_cases.len()];
    for (index, memory) in collected.into_iter().zip(memory::collect_all(jobs)?) {
        memories[index] = Some(memory?);
    }
    drop(copies);
    measured_cases
        .into_iter()
        .zip(memories)
        .map(|(measured, memory)| {
            let memory = memory.unwrap_or_else(|| {
                Value::object([("status", Value::str("missing-correctness-output"))])
            });
            finish_case(a, measured, memory)
        })
        .collect()
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
        ("stage", Value::str(a.stage.name())),
        ("utc", Value::float(python::time())),
    ]);
    write(
        &a.out.join("identity.json"),
        json::to_python(&identity, Some(1)),
    )?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // Every case's correctness and timing first, then the memory of all cases in
    // one collection, so timing never runs beside memory. `--stage` runs either
    // half alone: timing on one CPU, memory on the CPUs its slots may use.
    let measured_cases = match &a.stage {
        Stage::Memory { timing } => {
            let recorded = timing.join("identity.json");
            let recorded = crate::checks::read_json(&recorded)
                .map_err(|e| format!("{}: {e}", recorded.display()))?;
            let unstamped = |identity: &Value| match identity {
                Value::Object(fields) => Value::Object(
                    fields
                        .iter()
                        // What differs between the two runs by design.
                        .filter(|(k, _)| !["utc", "stage", "cpus"].contains(&k.as_str()))
                        .cloned()
                        .collect(),
                ),
                other => other.clone(),
            };
            if !crate::harness::diff::equal(&unstamped(&recorded), &unstamped(&identity)) {
                return Err(format!(
                    "{}: timed other sources, workers, cases or host",
                    timing.display()
                ));
            }
            cases
                .iter()
                .map(|case| MeasuredCase::read(timing, case))
                .collect::<Result<Vec<_>, _>>()?
        }
        Stage::Timing | Stage::Both => {
            let mut measured = Vec::new();
            for case in &cases {
                let case = correctness_and_timing(root, a, &exe, gate, &refusals, case)?;
                if matches!(a.stage, Stage::Timing) {
                    write(
                        &a.out.join(&case.id).join(TIMING_RECORD),
                        json::to_python(&Value::Object(case.record.clone()), Some(1)),
                    )?;
                }
                measured.push(case);
            }
            measured
        }
    };
    if matches!(a.stage, Stage::Timing) {
        return Ok(());
    }
    let summary = memory_stage(root, a, &refusals, measured_cases)?;
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
                    Value::object([("bytes", Value::int(376)), ("digest", Value::int(digest))]),
                ),
            ])
        };
        Value::object([
            // Two sessions of a 188-byte stream.
            ("iterations", Value::int(2)),
            (
                "warmups",
                Value::Array(vec![
                    Value::object([
                        ("worker", Value::str("candidate")),
                        ("result", run(candidate_digest)),
                    ]),
                    Value::object([("worker", Value::str("reference")), ("result", run(0xff))]),
                ]),
            ),
        ])
    }

    #[test]
    fn a_memory_run_reads_only_the_cases_its_timing_run_recorded() {
        let dir = crate::walk::scratch("capture-timing-records");
        let input = dir.join("noise-64x48-2.rgba");
        fs::create_dir_all(dir.join("timing/noise")).unwrap();
        fs::write(&input, b"pixels").unwrap();
        let case = json::parse(
            r#"{"id": "noise", "content": "noise", "width": 64, "height": 48, "frames": 2}"#,
        )
        .unwrap();
        let record = Value::object([
            ("case", case.clone()),
            ("input", Value::str(python::path(&input))),
            ("input_sha256", Value::str(file_sha(&input).unwrap())),
            (
                "correctness_summary",
                Value::object([("pipeline", Value::Bool(true))]),
            ),
        ]);
        fs::write(
            dir.join("timing/noise").join(TIMING_RECORD),
            json::to_python(&record, Some(1)),
        )
        .unwrap();
        let read = MeasuredCase::read(&dir.join("timing"), &case).unwrap();
        assert_eq!(read.input, input);
        assert_eq!(
            read.memory_arguments(&input).last(),
            Some(&python::path(&input))
        );
        let copies = InputCopies(dir.join("copies"));
        let copy = read.copy_input(&copies.0).unwrap();
        assert_eq!(fs::read(&copy).unwrap(), b"pixels");
        assert_eq!(copy, dir.join("copies/noise/noise-64x48-2.rgba"));
        drop(copies);
        assert!(
            !dir.join("copies").exists(),
            "copies are removed with the run"
        );
        let other = json::parse(r#"{"id": "noise", "content": "bars"}"#).unwrap();
        assert!(MeasuredCase::read(&dir.join("timing"), &other).is_err());
        fs::write(&input, b"changed").unwrap();
        let error = MeasuredCase::read(&dir.join("timing"), &case)
            .err()
            .unwrap();
        assert!(error.contains("changed since timing"), "{error}");
        fs::remove_dir_all(dir).unwrap();
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
