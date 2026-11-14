//! Paired codec memory collection under the shared observer
//! ([`crate::harness::memory`]), as `capture qualify` measures memory.
//!
//! For every workload, both workers first run the operation's `-memory` mode once
//! without the observer; their work (output units and bytes) must be equal, and each
//! measured run must then reproduce its own. The observer is admitted for the selected
//! workers, then 30 randomly ordered sets of the four runs (candidate and reference,
//! heap and RSS) are measured. The memory gate of docs/quality.md#gates holds when the
//! paired 95% upper bounds of peak live heap (`heap_ratio_upper_95`) and total resident
//! peak RSS (`total_rss_ratio_upper_95`) are both below 1.0, on native AArch64 Linux, on
//! the gate build, with at least 30 pairs. Every raw observation is written.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::correctness::file_sha;
use crate::harness::{gate_build, memory, worker};
use crate::json::{self, Value};
use crate::python;

pub const PAIRS: usize = 30;
/// After timing's seed and its two retries (`super::timing::SEED` onwards).
pub const SEED: u64 = super::timing::SEED + 3;
const WORK_TIMEOUT: Duration = Duration::from_secs(600);

/// The arguments of `codecs memory`.
pub struct Options {
    /// The repository: observer, build identity and policy.
    pub repository: PathBuf,
    pub candidate: PathBuf,
    pub reference: PathBuf,
    pub workloads: PathBuf,
    pub root: PathBuf,
    pub out: PathBuf,
    pub pairs: usize,
    pub seed: u64,
    pub only: Option<String>,
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

/// Whether the platform can produce a memory gate result.
fn gate_platform() -> bool {
    cfg!(all(
        target_os = "linux",
        target_arch = "aarch64",
        target_env = "gnu"
    ))
}

/// The memory verdict of one collection: both paired upper bounds below 1.0 on an
/// admitted observation of enough pairs, on the gate platform and build.
pub fn verdict(measured: &Value, pairs: usize, platform: bool, gate_build: bool) -> Value {
    let status = measured
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("invalid-memory-observation");
    let admitted = status == "qualified" && pairs >= PAIRS && platform && gate_build;
    let below = |name: &str| {
        measured
            .get(name)
            .and_then(Value::as_f64)
            .is_some_and(|ratio| ratio < 1.0)
    };
    let heap = admitted && below("heap_ratio_upper_95");
    let rss = admitted && below("total_rss_ratio_upper_95");
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

pub fn run(options: &Options) -> Result<(), String> {
    let manifest = crate::checks::read_json(&options.workloads)
        .map_err(|e| format!("{}: {e}", options.workloads.display()))?;
    let workloads = manifest
        .get("workloads")
        .and_then(Value::as_array)
        .ok_or("workloads file without workloads")?
        .to_vec();
    let parity = super::aac_access_units::measurement_parity(
        &options.repository,
        &options.candidate,
        &options.reference,
        &options.workloads,
        &options.root,
        &manifest,
    )?;
    fs::create_dir_all(&options.out).map_err(|e| format!("{}: {e}", options.out.display()))?;
    let refusals =
        gate_build::pair_refusals(&options.repository, &options.candidate, &options.reference);
    for reason in &refusals {
        println!("no gate result: {reason}");
    }
    if !gate_platform() {
        println!("no gate result: memory gates are measured on native AArch64 Linux with glibc");
    }
    let only: Option<Vec<&str>> = options.only.as_deref().map(|o| o.split(',').collect());
    let mut identity = Value::object([
        (
            "candidate_sha256",
            Value::str(file_sha(&options.candidate)?),
        ),
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
        ("utc", Value::str(python::utc_now())),
    ]);
    if let Some(parity) = &parity
        && let Value::Object(fields) = &mut identity
    {
        fields.push(("parity_sha256".into(), Value::str(&parity.sha256)));
    }
    write(&options.out.join("identity.json"), &identity)?;
    let mut summary = Vec::new();
    for w in &workloads {
        let id = text(w, "id")?;
        if only.as_ref().is_some_and(|o| !o.contains(&id)) {
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
        let measured =
            if Some(file_sha(&input)?.as_str()) != w.get("sha256").and_then(Value::as_str) {
                Value::object([("status", Value::str("input-hash-mismatch"))])
            } else {
                match (
                    work(&options.candidate, operation, &input),
                    work(&options.reference, operation, &input),
                ) {
                    (Ok(candidate), Ok(reference))
                        if crate::harness::diff::equal(&candidate, &reference) =>
                    {
                        let expected =
                            Value::object([("candidate", candidate), ("reference", reference)]);
                        record.push(("expected_outputs".into(), expected.clone()));
                        let arguments = [format!("{operation}-memory"), python::path(&input)];
                        let workers = memory::WorkerPaths {
                            candidate: &options.candidate,
                            reference: &options.reference,
                            registered: false,
                        };
                        memory::collect_one(memory::Job::new(&workers, [input.clone()], |slot| {
                            memory::collect_command(
                                slot,
                                &options.repository,
                                &options.candidate,
                                &options.reference,
                                &input,
                                memory::Pairs::gate(options.pairs, memory::CEILING),
                                options.seed,
                                &arguments,
                                &expected,
                            )
                        }))?
                    }
                    (Ok(candidate), Ok(reference)) => Value::object([
                        ("status", Value::str("unequal-memory-work")),
                        ("candidate_work", candidate),
                        ("reference_work", reference),
                    ]),
                    (candidate, reference) => Value::object([
                        ("status", Value::str("worker-failure")),
                        ("candidate", candidate.err().map_or(Value::Null, Value::str)),
                        ("reference", reference.err().map_or(Value::Null, Value::str)),
                    ]),
                }
            };
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
        );
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
    println!("DONE");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measured(heap: f64, rss: f64) -> Value {
        Value::object([
            ("status", Value::str("qualified")),
            ("heap_ratio_upper_95", Value::float(heap)),
            ("total_rss_ratio_upper_95", Value::float(rss)),
        ])
    }

    fn gates(value: &Value) -> (bool, bool, bool) {
        let flag = |k: &str| value.get(k).is_some_and(Value::truthy);
        (flag("heap_gate"), flag("rss_gate"), flag("memory_gate"))
    }

    #[test]
    fn both_paired_upper_bounds_must_be_below_one() {
        assert_eq!(
            gates(&verdict(&measured(0.5, 0.9), 30, true, true)),
            (true, true, true)
        );
        assert_eq!(
            gates(&verdict(&measured(0.5, 1.0), 30, true, true)),
            (true, false, false)
        );
        assert_eq!(
            gates(&verdict(&measured(1.2, 0.9), 30, true, true)),
            (false, true, false)
        );
    }

    #[test]
    fn no_gate_without_admission_pairs_platform_or_gate_build() {
        let failed = Value::object([("status", Value::str("observer-unqualified"))]);
        assert_eq!(
            gates(&verdict(&failed, 30, true, true)),
            (false, false, false)
        );
        let good = measured(0.5, 0.5);
        assert_eq!(
            gates(&verdict(&good, 29, true, true)),
            (false, false, false)
        );
        assert_eq!(
            gates(&verdict(&good, 30, false, true)),
            (false, false, false)
        );
        assert_eq!(
            gates(&verdict(&good, 30, true, false)),
            (false, false, false)
        );
        assert_eq!(
            verdict(&Value::Null, 30, true, true)
                .get("memory_status")
                .and_then(Value::as_str),
            Some("invalid-memory-observation")
        );
    }
}
