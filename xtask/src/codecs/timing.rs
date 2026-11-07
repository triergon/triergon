//! Paired codec timing collection.
//!
//! The shared timing primitives (worker execution, CPU snapshots, competing
//! build detection, the benchmark lock, the kernel-module guard and the
//! bootstrap ratio interval) with 30 randomised pairs and seed 280. Equal work
//! for decoders means equal output picture/block counts and byte counts; the
//! output digest is recorded for both sides (equal only where the outputs are
//! bit-exact, which the correctness grading checks separately). Each claim key
//! is measured once; results stay first-clean, apart from the declared
//! once-only recollection of contaminated claims on seed 281.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::harness::acceptance::active_cpu;
use crate::harness::gate_build;
use crate::harness::timing::{
    TimedWorkers, competing_builds, cpu_snapshot, lock, timing_module_state,
};
use crate::harness::worker::{self, data, status};
use crate::json::{self, Value};
use crate::pyrandom::PyRandom;
use crate::{python, sha256, stats};

const PAIRS: usize = 30;
const SEED: u64 = 280;
const RESAMPLES: usize = 4000;
const WORKER_TIMEOUT: Duration = Duration::from_secs(600);
/// Gate: upper 95% bound of the candidate/reference time ratio.
const DELIVERY: f64 = 1.25;
/// Optimisation target for the same bound.
const OPTIMIZATION: f64 = 0.80;
/// `kind` of a claim key; keys of ledgers written before the Rust runner
/// differ.
const KIND: &str = "codec-time";

/// The arguments of `codecs time`.
pub struct Options {
    /// The repository whose recorded build the candidate is held against.
    pub repository: PathBuf,
    pub candidate: PathBuf,
    pub reference: PathBuf,
    pub workloads: PathBuf,
    pub root: PathBuf,
    pub ledger: PathBuf,
    pub out: PathBuf,
    pub label: String,
    pub only: Option<String>,
    pub recollect_contaminated: Option<String>,
}

fn sha(path: &Path) -> Result<String, String> {
    Ok(sha256::hex(
        &fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?,
    ))
}

fn write(path: &Path, text: String) -> Result<(), String> {
    fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

fn set(object: &mut Value, key: &str, value: Value) {
    if let Value::Object(members) = object {
        match members.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => members.push((key.to_owned(), value)),
        }
    }
}

fn write_ledger(path: &Path, ledger: &Value) -> Result<(), String> {
    write(
        path,
        json::to_python(&python::sort_keys(ledger), Some(1)) + "\n",
    )
}

fn get_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// The four work counts of one pair: candidate packets and bytes, reference
/// packets and bytes.
fn work(sample: &Value) -> Result<[Value; 4], String> {
    let field = |side: &str, key: &str| {
        data(sample.get(side).unwrap_or(&Value::Null))
            .get(key)
            .cloned()
            .ok_or_else(|| format!("{side} result without {key}"))
    };
    Ok([
        field("candidate", "packets")?,
        field("candidate", "bytes")?,
        field("reference", "packets")?,
        field("reference", "bytes")?,
    ])
}

fn equal(a: &Value, b: &Value) -> bool {
    crate::harness::diff::equal(a, b)
}

pub fn run(options: &Options) -> Result<(), String> {
    let workloads = crate::checks::read_json(&options.workloads)
        .map_err(|e| format!("{}: {e}", options.workloads.display()))?;
    let workloads = workloads
        .get("workloads")
        .and_then(Value::as_array)
        .ok_or("workloads file without workloads")?
        .to_vec();
    let mut ledger = if options.ledger.exists() {
        python::loads(
            &fs::read_to_string(&options.ledger)
                .map_err(|e| format!("{}: {e}", options.ledger.display()))?,
        )
        .ok_or_else(|| format!("{}: not JSON", options.ledger.display()))?
    } else {
        Value::Object(Vec::new())
    };
    fs::create_dir_all(&options.out).map_err(|e| format!("{}: {e}", options.out.display()))?;
    let refusals =
        gate_build::pair_refusals(&options.repository, &options.candidate, &options.reference);
    for reason in &refusals {
        println!("no gate result: {reason}");
    }
    let hashes = Value::object([
        ("candidate", Value::str(sha(&options.candidate)?)),
        ("reference", Value::str(sha(&options.reference)?)),
    ]);
    let module = timing_module_state();
    if !matches!(
        get_str(&module, "status"),
        Some("module-absent" | "not-applicable")
    ) {
        return Err(format!(
            "the RSS kernel module must be absent while timing: {}",
            json::to_python(&module, None)
        ));
    }
    let only: Option<Vec<&str>> = options.only.as_deref().map(|o| o.split(',').collect());
    for w in &workloads {
        let id = get_str(w, "id").ok_or("workload without id")?.to_owned();
        if only.as_ref().is_some_and(|o| !o.contains(&id.as_str())) {
            continue;
        }
        let path = options
            .root
            .join(get_str(w, "path").ok_or("workload without path")?);
        let input_sha = w.get("sha256").cloned().unwrap_or(Value::Null);
        if input_sha.as_str() != Some(sha(&path)?.as_str()) {
            return Err(format!("input hash differs from the workload: {id}"));
        }
        let operation = w.get("operation").cloned().unwrap_or(Value::Null);
        let iterations = w.get("iterations").cloned().unwrap_or(Value::Null);
        let claim = Value::object([
            ("workers", hashes.clone()),
            ("input", input_sha.clone()),
            ("operation", operation.clone()),
            ("iterations", iterations.clone()),
            ("kind", Value::str(KIND)),
        ]);
        let key = sha256::hex(json::to_python(&python::sort_keys(&claim), None).as_bytes());
        let (mut previous, mut seed) = (None, SEED);
        if let Some(old) = ledger.get(&key) {
            let recollect = options
                .recollect_contaminated
                .as_deref()
                .is_some_and(|label| {
                    get_str(old, "label") == Some(label)
                        && get_str(old, "status") == Some("contaminated")
                        && old.get("contaminated_previous").is_none()
                });
            if !recollect {
                println!("SKIP {id}");
                continue;
            }
            previous = Some(old.clone());
            seed = SEED + 1;
        }
        let mut entry = Value::object([
            ("label", Value::str(options.label.as_str())),
            ("id", Value::str(id.as_str())),
            ("status", Value::str("claimed")),
            ("seed", Value::int(seed)),
            ("utc", Value::str(python::utc_now())),
        ]);
        if let Some(previous) = previous {
            set(&mut entry, "contaminated_previous", previous);
        }
        set(&mut ledger, &key, entry);
        write_ledger(&options.ledger, &ledger)?;
        let mut rng = PyRandom::new(seed);
        let mut samples = Vec::new();
        let mut state = "measured".to_owned();
        let operation_name = format!("{}-bench", operation.as_str().unwrap_or_default());
        let argv = |worker: &str| -> Vec<OsString> {
            vec![
                worker.into(),
                operation_name.as_str().into(),
                python::path(&path).into(),
                python::str(&iterations).into(),
            ]
        };
        {
            let timed = TimedWorkers::new(&options.candidate, &options.reference)?;
            let workers = [
                ("candidate", python::path(&timed.candidate)),
                ("reference", python::path(&timed.reference)),
            ];
            let _lock = lock()?;
            if !competing_builds()?.is_empty() {
                state = "contaminated".into();
            } else {
                for (name, worker) in &workers {
                    let warm = worker::execute(&argv(worker), WORKER_TIMEOUT);
                    if status(&warm) != "ok" {
                        state = format!("warmup-failed:{name}");
                    }
                }
                let pairs = if state == "measured" { PAIRS } else { 0 };
                for index in 0..pairs {
                    let mut order = [0usize, 1];
                    rng.shuffle(&mut order);
                    let before = cpu_snapshot()?;
                    let mut row = vec![
                        ("pair".to_owned(), Value::int(index as i64)),
                        (
                            "order".to_owned(),
                            Value::strings(order.map(|i| workers[i].0)),
                        ),
                        ("cpu_before".to_owned(), before.clone()),
                    ];
                    for i in order {
                        let (name, worker) = &workers[i];
                        row.push((
                            (*name).to_owned(),
                            worker::execute(&argv(worker), WORKER_TIMEOUT),
                        ));
                    }
                    let after = cpu_snapshot()?;
                    row.push(("cpu_after".to_owned(), after.clone()));
                    samples.push(Value::Object(row));
                    if !active_cpu(&before, &after, false).is_empty()
                        || get_str(&timing_module_state(), "status") == Some("module-loaded")
                    {
                        state = "contaminated".into();
                        break;
                    }
                }
            }
        }
        let mut result = vec![
            ("id".to_owned(), Value::str(id.as_str())),
            ("key".to_owned(), Value::str(key.as_str())),
            ("label".to_owned(), Value::str(options.label.as_str())),
            ("seed".to_owned(), Value::int(seed)),
            ("workers".to_owned(), hashes.clone()),
            ("input_sha256".to_owned(), input_sha.clone()),
            ("operation".to_owned(), operation.clone()),
            ("iterations".to_owned(), iterations.clone()),
            ("status".to_owned(), Value::str(state.as_str())),
            ("samples".to_owned(), Value::Array(samples.clone())),
            (
                "build_refusals".to_owned(),
                Value::strings(refusals.clone()),
            ),
            ("cpus".to_owned(), gate_build::cpus()),
        ];
        let mut ratio = None;
        let (mut delivery, mut digest_equal) = (None, None);
        if state == "measured" {
            let failed = samples.iter().any(|r| {
                ["candidate", "reference"]
                    .iter()
                    .any(|name| status(r.get(name).unwrap_or(&Value::Null)) != "ok")
            });
            if failed {
                state = "worker-failure".into();
            } else {
                // The set of work tuples, in Python's set iteration order:
                // the recorded `work` is the first tuple it yields.
                let tuples = samples.iter().map(work).collect::<Result<Vec<_>, _>>()?;
                let integers: Option<Vec<[i128; 4]>> = tuples
                    .iter()
                    .map(|t| {
                        let mut out = [0; 4];
                        for (slot, v) in out.iter_mut().zip(t) {
                            *slot = match v {
                                Value::Number(n) => n.as_i128()?,
                                _ => return None,
                            };
                        }
                        Some(out)
                    })
                    .collect();
                let seen: Vec<[Value; 4]> = if let Some(integers) = integers {
                    python::set_order(&integers, |t| python::hash_tuple(t))
                        .into_iter()
                        .map(|t| t.map(Value::int))
                        .collect()
                } else {
                    let mut seen: Vec<[Value; 4]> = Vec::new();
                    for tuple in tuples {
                        if !seen
                            .iter()
                            .any(|s| s.iter().zip(&tuple).all(|(a, b)| equal(a, b)))
                        {
                            seen.push(tuple);
                        }
                    }
                    seen
                };
                let [c_pk, c_by, r_pk, r_by] = seen[0].clone();
                let equal_work = seen.len() == 1 && equal(&c_pk, &r_pk) && equal(&c_by, &r_by);
                let digests_equal = samples.iter().all(|r| {
                    let digest = |side: &str| {
                        data(r.get(side).unwrap_or(&Value::Null))
                            .get("digest")
                            .cloned()
                            .unwrap_or(Value::Null)
                    };
                    equal(&digest("candidate"), &digest("reference"))
                });
                digest_equal = Some(digests_equal);
                result.push(("equal_work".into(), Value::Bool(equal_work)));
                result.push(("digest_equal".into(), Value::Bool(digests_equal)));
                result.push((
                    "work".into(),
                    Value::object([
                        ("candidate", Value::Array(vec![c_pk, c_by])),
                        ("reference", Value::Array(vec![r_pk, r_by])),
                    ]),
                ));
                if equal_work {
                    let elapsed = |r: &Value, side: &str| {
                        data(r.get(side).unwrap_or(&Value::Null))
                            .get("elapsed_ns")
                            .and_then(Value::as_f64)
                            .ok_or_else(|| format!("{side} result without elapsed_ns"))
                    };
                    let pairs = samples
                        .iter()
                        .map(|r| Ok((elapsed(r, "candidate")?, elapsed(r, "reference")?)))
                        .collect::<Result<Vec<_>, String>>()?;
                    let interval = stats::ratio_interval(&pairs, seed, RESAMPLES)?;
                    let value = interval_value(&interval);
                    result.push(("ratio".into(), value.clone()));
                    let gate_build = refusals.is_empty();
                    let passes = gate_build && interval.upper_95 <= DELIVERY;
                    result.push(("delivery".into(), Value::Bool(passes)));
                    result.push((
                        "optimization".into(),
                        Value::Bool(gate_build && interval.upper_95 <= OPTIMIZATION),
                    ));
                    ratio = Some((value, interval.upper_95));
                    delivery = Some(passes);
                } else {
                    state = "unequal-work".into();
                }
            }
            set_field(&mut result, "status", Value::str(state.as_str()));
        }
        let name = format!(
            "{}{}.json",
            id.replace('/', "__"),
            if seed == SEED {
                String::new()
            } else {
                format!("-seed{seed}")
            }
        );
        let raw = options.out.join(name);
        if raw.exists() {
            return Err(format!("{} exists", raw.display()));
        }
        write(
            &raw,
            json::to_python(&Value::Object(result), Some(1)) + "\n",
        )?;
        let raw_text = python::path(&raw);
        let mut entry = ledger.get(&key).cloned().unwrap_or(Value::Null);
        set(&mut entry, "status", Value::str(state.as_str()));
        set(&mut entry, "raw", Value::str(raw_text));
        set(&mut entry, "raw_sha256", Value::str(sha(&raw)?));
        set(
            &mut entry,
            "ratio",
            ratio.as_ref().map_or(Value::Null, |(v, _)| v.clone()),
        );
        set(
            &mut entry,
            "delivery",
            delivery.map_or(Value::Null, Value::Bool),
        );
        set(
            &mut entry,
            "digest_equal",
            digest_equal.map_or(Value::Null, Value::Bool),
        );
        set(&mut ledger, &key, entry);
        write_ledger(&options.ledger, &ledger)?;
        println!(
            "CODECTIME {id} {state} {} {} {}",
            ratio
                .as_ref()
                .map_or_else(|| "None".to_owned(), |(_, upper)| python::float(*upper)),
            delivery.map_or("None", |d| if d { "True" } else { "False" }),
            if digest_equal == Some(true) {
                "digest_equal"
            } else {
                ""
            }
        );
    }
    println!("DONE");
    Ok(())
}

fn set_field(fields: &mut [(String, Value)], key: &str, value: Value) {
    if let Some(slot) = fields.iter_mut().find(|(k, _)| k == key) {
        slot.1 = value;
    }
}

/// A ratio interval as the records store it.
pub fn interval_value(interval: &stats::Interval) -> Value {
    Value::object([
        ("median", Value::float(interval.median)),
        ("lower_95", Value::float(interval.lower_95)),
        ("upper_95", Value::float(interval.upper_95)),
    ])
}
