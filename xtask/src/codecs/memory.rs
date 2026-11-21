//! Paired codec memory collection under the shared observer
//! ([`crate::harness::memory`]), as `capture qualify` measures memory.
//!
//! For every workload, both workers first run the operation's `-memory` mode once
//! without the observer; their work (output units and bytes) must be equal, and each
//! measured run must then reproduce its own. The observer is admitted for the selected
//! workers, then 30 randomly ordered sets of the four runs (candidate and reference,
//! heap and RSS) are measured. The memory gate of docs/quality.md#gates holds when the
//! paired 95% upper bounds of peak live heap (`heap_ratio_upper_95`) and total resident
//! peak RSS (`total_rss_ratio_upper_95`) are both below the ceilings of
//! `harness/policy.json`, on a gate target's native Linux, on the gate build, with at
//! least [`PAIRS`] pairs. Every raw observation is written.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::correctness::file_sha;
use crate::harness::{gate_build, memory, policy, worker};
use crate::json::{self, Value};
use crate::python;

pub const PAIRS: usize = crate::harness::timing::GATE_PAIRS;
/// After timing's seed and its two retries (`super::timing::SEED` onwards).
pub const SEED: u64 = super::timing::SEED + 3;
const WORK_TIMEOUT: Duration = Duration::from_secs(600);

/// The arguments of `codecs memory`.
pub struct Options {
    /// The repository: observer, build identity and policy.
    pub repository: PathBuf,
    pub candidates: super::Candidates,
    pub reference: PathBuf,
    pub workloads: PathBuf,
    pub root: PathBuf,
    pub out: PathBuf,
    pub pairs: usize,
    pub seed: u64,
    pub only: Option<String>,
    /// Part I of N (zero-based I): every Nth workload from the Ith.
    pub part: Option<(usize, usize)>,
    /// Let a gate session stop at an early look (`--no-early-stop` clears it).
    pub early_stop: bool,
}

/// The work one unobserved `-memory` run reports: its last stdout line.
fn work(worker: &Path, operation: &str, input: &Path) -> Result<Value, String> {
    let argv: Vec<OsString> = vec![
        worker.into(),
        format!("{operation}-memory").into(),
        input.into(),
    ];
    let finished = worker::run(&argv, &[], WORK_TIMEOUT)?
        .ok_or_else(|| format!("{}: timed out", worker.display()))?;
    if finished.code != 0 {
        return Err(format!(
            "{} exited {}: {}",
            worker.display(),
            finished.code,
            python::tail(&python::text(&finished.stderr), 300)
        ));
    }
    let stdout = python::text(&finished.stdout);
    python::splitlines(&stdout)
        .last()
        .and_then(|line| python::loads(line))
        .filter(|v| matches!(v, Value::Object(_)))
        .ok_or_else(|| format!("{}: no work line", worker.display()))
}

/// Whether the platform can produce a codec gate result: native Linux with glibc, on
/// AArch64 or 32-bit ARM, the two gate targets' hosts.
pub fn gate_platform() -> bool {
    cfg!(all(
        target_os = "linux",
        any(target_arch = "aarch64", target_arch = "arm"),
        target_env = "gnu"
    ))
}

/// The gate target whose bounds a collection on this platform decides, where it
/// decides any ([`gate_platform`]).
pub fn gate_target() -> Result<Option<&'static str>, String> {
    if gate_platform() {
        gate_build::worker_target().map(Some)
    } else {
        Ok(None)
    }
}

/// The memory verdict of one collection: both paired upper bounds below their
/// ceilings on an admitted observation of enough pairs, on the gate platform and build.
pub fn verdict(
    measured: &Value,
    pairs: usize,
    platform: bool,
    gate_build: bool,
    ceilings: policy::Memory,
) -> Value {
    let status = measured
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("invalid-memory-observation");
    let admitted = status == "qualified" && pairs >= PAIRS && platform && gate_build;
    let below = |name: &str, ceiling: f64| {
        measured
            .get(name)
            .and_then(Value::as_f64)
            .is_some_and(|ratio| ratio < ceiling)
    };
    let heap = admitted && below("heap_ratio_upper_95", ceilings.heap);
    let rss = admitted && below("total_rss_ratio_upper_95", ceilings.total_rss);
    Value::object([
        ("memory_status", Value::str(status)),
        ("admitted", Value::Bool(admitted)),
        ("heap_gate", Value::Bool(heap)),
        ("rss_gate", Value::Bool(rss)),
        ("memory_gate", Value::Bool(heap && rss)),
    ])
}

fn write(path: &Path, value: &Value) -> Result<(), String> {
    fs::write(path, json::to_python(value, Some(1)) + "\n")
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("workload without {key}"))
}

/// How long one observer run of `manifest`'s workloads may take on `target`: its
/// `memory_timeout_seconds` for the target, a whole number of seconds from 1 to
/// [`worker::MAXIMUM_TIMEOUT_SECONDS`], else [`worker::DEFAULT_TIMEOUT`].
pub fn run_limit(manifest: &Value, target: &str) -> Result<Duration, String> {
    let Some(declared) = manifest
        .get("memory_timeout_seconds")
        .and_then(|limits| limits.get(target))
    else {
        return Ok(worker::DEFAULT_TIMEOUT);
    };
    match declared {
        Value::Number(number) => number.as_i128(),
        _ => None,
    }
    .and_then(|seconds| u64::try_from(seconds).ok())
    .filter(|seconds| (1..=worker::MAXIMUM_TIMEOUT_SECONDS).contains(seconds))
    .map(Duration::from_secs)
    .ok_or_else(|| {
        format!(
            "memory_timeout_seconds for {target} must be a whole number of seconds \
                 from 1 to {}",
            worker::MAXIMUM_TIMEOUT_SECONDS
        )
    })
}

pub fn run(options: &Options) -> Result<(), String> {
    let ceilings = policy::Memory::load(&options.repository)?;
    let manifest = crate::checks::read_json(&options.workloads)
        .map_err(|e| format!("{}: {e}", options.workloads.display()))?;
    let limit = run_limit(&manifest, &crate::harness::report::gate_target()?)?;
    let workloads = manifest
        .get("workloads")
        .and_then(Value::as_array)
        .ok_or("workloads file without workloads")?
        .to_vec();
    let parity = super::aac_access_units::measurement_parity(
        &options.repository,
        &options.candidates,
        &options.reference,
        &options.workloads,
        &options.root,
        &manifest,
    )?;
    fs::create_dir_all(&options.out).map_err(|e| format!("{}: {e}", options.out.display()))?;
    let candidates: Vec<&Path> = options
        .candidates
        .roles()
        .into_iter()
        .map(|(_, worker)| worker)
        .collect();
    let refusals =
        gate_build::workers_refusals(&options.repository, &candidates, &options.reference);
    for reason in &refusals {
        println!("no gate result: {reason}");
    }
    if !gate_platform() {
        println!(
            "no gate result: memory gates are measured on native Linux with glibc, AArch64 or 32-bit ARM"
        );
    }
    let only: Option<Vec<&str>> = options.only.as_deref().map(|o| o.split(',').collect());
    let mut identity = Value::object([
        ("candidate_sha256", Value::str(file_sha(candidates[0])?)),
        (
            "reference_sha256",
            Value::str(file_sha(&options.reference)?),
        ),
        (
            "workloads_sha256",
            Value::str(file_sha(&options.workloads)?),
        ),
        ("build_refusals", Value::strings(refusals.clone())),
        ("cpus", gate_build::cpus()),
        ("pairs", Value::int(options.pairs as i128)),
        ("seed", Value::int(options.seed)),
        ("run_limit_seconds", Value::int(i128::from(limit.as_secs()))),
        ("utc", Value::str(python::utc_now())),
    ]);
    let by_operation = options
        .candidates
        .by_operation()
        .map(|(operation, worker)| Ok((operation, Value::str(file_sha(worker)?))))
        .collect::<Result<Vec<_>, String>>()?;
    if let Value::Object(fields) = &mut identity {
        if !by_operation.is_empty() {
            fields.push((
                "candidate_sha256_by_operation".into(),
                Value::object(by_operation),
            ));
        }
        if let Some(parity) = &parity {
            fields.push(("parity_sha256".into(), Value::str(&parity.sha256)));
        }
    }
    write(&options.out.join("identity.json"), &identity)?;
    // Every workload's unobserved work first, then the memory of all of them in one
    // collection, side by side, each slot measuring its own copies of the gate
    // build's workers.
    let mut planned = Vec::new();
    for (index, w) in workloads.iter().enumerate() {
        let id = text(w, "id")?;
        if only.as_ref().is_some_and(|o| !o.contains(&id)) || !in_part(index, options.part) {
            continue;
        }
        let operation = text(w, "operation")?;
        let input = options.root.join(text(w, "path")?);
        let mut record = vec![
            ("id".to_owned(), Value::str(id)),
            ("operation".to_owned(), Value::str(operation)),
            (
                "input_sha256".to_owned(),
                w.get("sha256").cloned().unwrap_or(Value::Null),
            ),
        ];
        let ready = if Some(file_sha(&input)?.as_str()) != w.get("sha256").and_then(Value::as_str) {
            Err(Value::object([(
                "status",
                Value::str("input-hash-mismatch"),
            )]))
        } else {
            match (
                work(options.candidates.of(operation), operation, &input),
                work(&options.reference, operation, &input),
            ) {
                (Ok(candidate), Ok(reference))
                    if crate::harness::diff::equal(&candidate, &reference) =>
                {
                    let expected =
                        Value::object([("candidate", candidate), ("reference", reference)]);
                    record.push(("expected_outputs".into(), expected.clone()));
                    Ok((
                        expected,
                        [format!("{operation}-memory"), python::path(&input)],
                    ))
                }
                (Ok(candidate), Ok(reference)) => Err(Value::object([
                    ("status", Value::str("unequal-memory-work")),
                    ("candidate_work", candidate),
                    ("reference_work", reference),
                ])),
                (candidate, reference) => Err(Value::object([
                    ("status", Value::str("worker-failure")),
                    ("candidate", candidate.err().map_or(Value::Null, Value::str)),
                    ("reference", reference.err().map_or(Value::Null, Value::str)),
                ])),
            }
        };
        planned.push((w, id, record, input, ready));
    }
    let pairs =
        memory::Pairs::gate(options.pairs, ceilings.ceiling()).early_stop(options.early_stop);
    let mut jobs = Vec::new();
    let mut collected = Vec::new();
    for (index, (w, _, _, input, ready)) in planned.iter().enumerate() {
        let Ok((expected, arguments)) = ready else {
            continue;
        };
        let workers = memory::WorkerPaths {
            candidate: options.candidates.of(text(w, "operation")?),
            reference: &options.reference,
            registered: refusals.is_empty(),
        };
        jobs.push(memory::Job::new(&workers, [input.clone()], move |slot| {
            memory::collect_command(
                slot,
                &options.repository,
                workers,
                input,
                pairs,
                options.seed,
                arguments,
                expected,
                limit,
            )
        }));
        collected.push(index);
    }
    let mut memories: Vec<Option<Value>> = vec![None; planned.len()];
    for (index, memory) in collected.into_iter().zip(memory::collect_all(jobs)?) {
        memories[index] = Some(memory?);
    }
    let mut summary = Vec::new();
    let mut unmeasured = Vec::new();
    let mut bounds = Vec::new();
    for ((w, id, mut record, _, ready), memory) in planned.into_iter().zip(memories) {
        let measured = match ready {
            Err(refused) => refused,
            Ok(_) => memory.ok_or("a codec memory collection did not run")?,
        };
        if measured.get("status").and_then(Value::as_str) != Some("qualified") {
            unmeasured.push(id.to_owned());
        }
        let parity_matches = parity.as_ref().is_none_or(|parity| {
            parity.matches(
                measured
                    .get("certificate")
                    .and_then(|certificate| certificate.get("worker_hashes")),
                &measured,
                w.get("sha256"),
            )
        });
        if parity.is_some() {
            record.push(("parity_matches".into(), Value::Bool(parity_matches)));
        }
        let verdict = verdict(
            &measured,
            options.pairs,
            gate_platform(),
            refusals.is_empty() && parity_matches,
            ceilings,
        );
        if measured.get("status").and_then(Value::as_str) == Some("qualified") {
            let held = verdict.get("memory_gate").is_some_and(Value::truthy);
            bounds.push((id.to_owned(), held));
        }
        let bound = |key: &str| measured.get(key).cloned().unwrap_or(Value::Null);
        let mut line = vec![("id".to_owned(), Value::str(id))];
        line.extend(match &verdict {
            Value::Object(fields) => fields.clone(),
            _ => Vec::new(),
        });
        line.push(("heap_ratio_upper_95".into(), bound("heap_ratio_upper_95")));
        line.push((
            "total_rss_ratio_upper_95".into(),
            bound("total_rss_ratio_upper_95"),
        ));
        line.push((
            "heap_lower_in_each_pair".into(),
            bound("heap_lower_in_each_pair"),
        ));
        record.push(("memory".into(), measured));
        record.push(("verdict".into(), verdict));
        write(
            &options.out.join(format!("{}.json", id.replace('/', "__"))),
            &Value::Object(record),
        )?;
        let line = Value::Object(line);
        println!("CODECMEM {}", json::to_python(&line, None));
        summary.push(line);
    }
    write(&options.out.join("summary.json"), &Value::Array(summary))?;
    let name = list_name(&options.workloads);
    let (lines, failed) = super::bounds(&manifest, &name, "memory", &bounds, gate_target()?)?;
    for line in &lines {
        println!("{line}");
    }
    println!("DONE");
    measured_all("memory", &unmeasured)?;
    if failed {
        return Err(format!("{name} memory: a gated bound failed"));
    }
    Ok(())
}

/// A workload list's name, from its file: `workloads` or `file-workloads`.
pub fn list_name(workloads: &Path) -> String {
    workloads.file_stem().map_or_else(
        || "workloads".into(),
        |stem| stem.to_string_lossy().into_owned(),
    )
}

/// Whether the workload at `index` is in `part` (zero-based I of N): every Nth
/// workload from the Ith; every workload without a part.
pub fn in_part(index: usize, part: Option<(usize, usize)>) -> bool {
    part.is_none_or(|(i, n)| index % n == i)
}

/// A collection measures every workload it selects, or it fails naming those it did
/// not, after writing every record.
pub fn measured_all(what: &str, unmeasured: &[String]) -> Result<(), String> {
    if unmeasured.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{what} not measured for {} workloads: {}",
            unmeasured.len(),
            unmeasured.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests;
