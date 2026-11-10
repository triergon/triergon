//! Paired codec timing collection.
//!
//! Each workload is one session of the shared paired runner
//! ([`crate::harness::timing::benchmark`]: 30 randomised pairs, seed 280, the
//! benchmark lock, recorded warmups, the contamination watch and the
//! kernel-module guard) graded by its validator. Equal work for decoders means
//! equal output picture/block counts and byte counts; each worker's output
//! digest must stay the same across its runs, and whether the two digests agree
//! is recorded (they agree only where the outputs are bit-exact, which the
//! correctness grading checks separately). Each claim key is measured once.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::harness::comparison::BASE_WORK_FIELDS;
use crate::harness::gate_build;
use crate::harness::policy;
use crate::harness::timing::{self, Expected, Operation};
use crate::harness::worker::data;
use crate::json::{self, Value};
use crate::{python, sha256};

const PAIRS: usize = 30;
const SEED: u64 = 280;
const WORKER_TIMEOUT: Duration = Duration::from_secs(600);
/// `kind` of a claim key. A ledger may hold claims of other kinds, recorded in
/// other formats; those never stand in for a claim of this one.
const KIND: &str = "codec-time-session";

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

/// The timed operation of a workload: `OP-bench FILE ITERATIONS`, whose digest
/// is each worker's own.
fn operation(name: &str) -> Operation {
    Operation {
        prefix: format!("{name}-"),
        arguments: vec![],
        candidate_options: vec![],
        work_fields: BASE_WORK_FIELDS.map(str::to_owned).into(),
        own_work: vec!["digest".to_owned()],
        timeout: WORKER_TIMEOUT,
    }
}

/// Whether both workers printed the same output digest in every pair.
fn digests_equal(timing: &Value) -> bool {
    timing
        .get("samples")
        .and_then(Value::as_array)
        .is_some_and(|samples| {
            samples.iter().all(|sample| {
                let digest =
                    |role: &str| sample.get(role).map(|outcome| data(outcome).get("digest"));
                digest("candidate") == digest("reference")
            })
        })
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
    let gate = policy::Runtime::load(&options.repository)?;
    let refusals =
        gate_build::pair_refusals(&options.repository, &options.candidate, &options.reference);
    for reason in &refusals {
        println!("no gate result: {reason}");
    }
    let hashes = Value::object([
        ("candidate", Value::str(sha(&options.candidate)?)),
        ("reference", Value::str(sha(&options.reference)?)),
    ]);
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
        let name = get_str(w, "operation").ok_or("workload without operation")?;
        let iterations = w
            .get("iterations")
            .and_then(python::int)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or("workload without iterations")?;
        let claim = Value::object([
            ("workers", hashes.clone()),
            ("input", input_sha.clone()),
            ("operation", Value::str(name)),
            ("iterations", Value::int(iterations)),
            ("kind", Value::str(KIND)),
        ]);
        let key = sha256::hex(json::to_python(&python::sort_keys(&claim), None).as_bytes());
        if ledger.get(&key).is_some() {
            println!("SKIP {id}");
            continue;
        }
        let mut entry = Value::object([
            ("label", Value::str(options.label.as_str())),
            ("id", Value::str(id.as_str())),
            ("status", Value::str("claimed")),
            ("seed", Value::int(SEED)),
            ("utc", Value::str(python::utc_now())),
        ]);
        set(&mut ledger, &key, entry.clone());
        write_ledger(&options.ledger, &ledger)?;
        let operation = operation(name);
        let timing = timing::benchmark(
            &options.candidate,
            &options.reference,
            &path,
            PAIRS,
            iterations,
            SEED,
            &operation,
        )?;
        let measured = get_str(&timing, "status");
        let graded = (measured == Some("measured")).then(|| {
            timing::validate(
                &timing,
                &Expected {
                    worker_hashes: &hashes,
                    input_sha256: Some(&input_sha),
                    minimum_pairs: PAIRS as i128,
                    work_fields: &operation.work_fields,
                    own_work: &operation.own_work,
                    require_linux: cfg!(target_os = "linux"),
                    module_absence: true,
                },
            )
        });
        let state = match &graded {
            Some(Err(_)) => "invalid-timing",
            _ => measured.unwrap_or("timing-attempt-failed"),
        }
        .to_owned();
        let mut result = vec![
            ("id".to_owned(), Value::str(id.as_str())),
            ("key".to_owned(), Value::str(key.as_str())),
            ("label".to_owned(), Value::str(options.label.as_str())),
            ("operation".to_owned(), Value::str(name)),
            (
                "build_refusals".to_owned(),
                Value::strings(refusals.clone()),
            ),
            ("cpus".to_owned(), gate_build::cpus()),
        ];
        let (mut ratio, mut delivery, mut digest_equal) = (None, None, None);
        match graded {
            Some(Ok(interval)) => {
                let equal = digests_equal(&timing);
                let gate_build = refusals.is_empty();
                let passes = gate_build && interval.upper_95 <= gate.ceiling;
                result.extend([
                    ("digest_equal".to_owned(), Value::Bool(equal)),
                    ("delivery".to_owned(), Value::Bool(passes)),
                    (
                        "optimization".to_owned(),
                        Value::Bool(gate_build && interval.upper_95 <= gate.target),
                    ),
                ]);
                ratio = Some((timing::interval_value(&interval), interval.upper_95));
                delivery = Some(passes);
                digest_equal = Some(equal);
            }
            Some(Err(error)) => result.push(("invalid".to_owned(), Value::str(error))),
            None => {}
        }
        result.push(("status".to_owned(), Value::str(state.as_str())));
        result.push(("timing".to_owned(), timing));
        let raw = options.out.join(format!("{}.json", id.replace('/', "__")));
        if raw.exists() {
            return Err(format!("{} exists", raw.display()));
        }
        write(
            &raw,
            json::to_python(&Value::Object(result), Some(1)) + "\n",
        )?;
        set(&mut entry, "status", Value::str(state.as_str()));
        set(&mut entry, "raw", Value::str(python::path(&raw)));
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
