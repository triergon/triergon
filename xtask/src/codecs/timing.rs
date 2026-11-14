//! Paired codec timing collection.
//!
//! Each workload is one session of the shared paired runner, graded by its
//! validator ([`crate::harness::timing::measure`]: 30 randomised pairs on seed
//! 280, retried on 281 and 282, the benchmark lock, recorded warmups, the
//! contamination watch and the kernel-module guard). Equal work for decoders means
//! equal output picture/block counts and byte counts; each worker's output
//! digest must stay the same across its runs, and whether the two digests agree
//! is recorded (they agree only where the outputs are bit-exact, which the
//! correctness grading checks separately). Each claim key is measured once.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::harness::gate_build;
use crate::harness::policy;
use crate::harness::timing::{self, GATE_PAIRS, Operation, Pairs, Work};
use crate::harness::worker::data;
use crate::json::{self, Value};
use crate::{python, sha256};

/// The session's seed; its retries take the next two.
pub const SEED: u64 = 280;
const WORKER_TIMEOUT: Duration = Duration::from_secs(600);
/// Separate claims for records collected with the shared timing session.
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
    /// Let a gate session stop at an early look (`--no-early-stop` clears it).
    pub early_stop: bool,
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

fn write_claims(path: &Path, claims: &Value) -> Result<(), String> {
    let pending = path.with_extension(format!("{}.part", std::process::id()));
    write(
        &pending,
        json::to_python(&python::sort_keys(claims), Some(1)) + "\n",
    )?;
    fs::rename(&pending, path).map_err(|e| format!("{}: {e}", path.display()))
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
        work: Work {
            fields: vec![],
            own: vec!["digest".to_owned()],
        },
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

fn iterations(workload: &Value) -> Result<u32, String> {
    let count = match workload.get("iterations") {
        Some(Value::Number(number)) => number.as_i128(),
        _ => None,
    };
    count
        .and_then(|count| u32::try_from(count).ok())
        .filter(|count| *count > 0)
        .ok_or_else(|| "workload iterations must be a positive integer".into())
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
    let timing_lock = timing::lock()?;
    let claims_path = &options.ledger;
    let mut claims = if claims_path.exists() {
        python::loads(
            &fs::read_to_string(claims_path)
                .map_err(|e| format!("{}: {e}", claims_path.display()))?,
        )
        .ok_or_else(|| format!("{}: not JSON", claims_path.display()))?
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
    let mut unmeasured = Vec::new();
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
        let iterations = iterations(w).map_err(|error| format!("{id}: {error}"))?;
        let claim = Value::object([
            ("workers", hashes.clone()),
            ("input", input_sha.clone()),
            ("operation", Value::str(name)),
            ("iterations", Value::int(iterations)),
            ("kind", Value::str(KIND)),
        ]);
        let key = sha256::hex(json::to_python(&python::sort_keys(&claim), None).as_bytes());
        if let Some(previous) = claims.get(&key) {
            if get_str(previous, "status") == Some("claimed") {
                return Err(format!("{id}: prior timing attempt did not finish"));
            }
            println!("SKIP {id}");
            continue;
        }
        let raw = options.out.join(format!("{}.json", id.replace('/', "__")));
        if raw
            .try_exists()
            .map_err(|e| format!("{}: {e}", raw.display()))?
        {
            return Err(format!("{} exists", raw.display()));
        }
        let mut entry = Value::object([
            ("label", Value::str(options.label.as_str())),
            ("id", Value::str(id.as_str())),
            ("status", Value::str("claimed")),
            ("seed", Value::int(SEED)),
            ("utc", Value::str(python::utc_now())),
        ]);
        let mut result_file = None;
        let graded = timing_lock.measure(
            &options.candidate,
            &options.reference,
            &path,
            Pairs::gate(GATE_PAIRS, gate.ceiling).early_stop(options.early_stop),
            iterations,
            SEED,
            &operation(name),
            || {
                let file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&raw)
                    .map_err(|e| format!("{}: {e}", raw.display()))?;
                set(&mut claims, &key, entry.clone());
                if let Err(error) = write_claims(claims_path, &claims) {
                    drop(file);
                    fs::remove_file(&raw)
                        .map_err(|e| format!("{error}. {}: {e}", raw.display()))?;
                    return Err(error);
                }
                result_file = Some(file);
                Ok(())
            },
        );
        let Some(mut result_file) = result_file else {
            return Err(format!(
                "{id}: timing setup failed: {}",
                json::to_python(&graded?.record, None)
            ));
        };
        let graded = graded?;
        let parity_matches = parity.as_ref().is_none_or(|parity| {
            parity.matches(
                graded.record.get("worker_hashes"),
                &graded.record,
                Some(&input_sha),
            )
        });
        let state = graded.status().to_owned();
        // The seed of the attempt the session kept.
        let seed = graded
            .record
            .get("seed")
            .cloned()
            .unwrap_or(Value::int(SEED));
        set(&mut entry, "seed", seed.clone());
        let mut result = vec![
            ("id".to_owned(), Value::str(id.as_str())),
            ("key".to_owned(), Value::str(key.as_str())),
            ("label".to_owned(), Value::str(options.label.as_str())),
            ("operation".to_owned(), Value::str(name)),
            ("workers".to_owned(), hashes.clone()),
            ("input_sha256".to_owned(), input_sha),
            ("iterations".to_owned(), Value::int(iterations)),
            ("seed".to_owned(), seed),
            (
                "build_refusals".to_owned(),
                Value::strings(refusals.clone()),
            ),
            ("cpus".to_owned(), gate_build::cpus()),
        ];
        if let Some(parity) = &parity {
            result.push(("parity_sha256".to_owned(), Value::str(&parity.sha256)));
            result.push(("parity_matches".to_owned(), Value::Bool(parity_matches)));
        }
        let (mut ratio, mut delivery, mut digest_equal) = (None, None, None);
        if let Ok(interval) = &graded.interval {
            let equal = digests_equal(&graded.record);
            let gate_build = refusals.is_empty() && parity_matches;
            let passes = gate_build && interval.upper_95 <= gate.ceiling;
            result.extend([
                ("digest_equal".to_owned(), Value::Bool(equal)),
                ("delivery".to_owned(), Value::Bool(passes)),
                (
                    "optimization".to_owned(),
                    Value::Bool(gate_build && interval.upper_95 <= gate.target),
                ),
            ]);
            ratio = Some((timing::interval_value(interval), interval.upper_95));
            delivery = Some(passes);
            digest_equal = Some(equal);
        }
        result.extend(graded.into_fields());
        let result = json::to_python(&Value::Object(result), Some(1)) + "\n";
        result_file
            .write_all(result.as_bytes())
            .and_then(|()| result_file.sync_data())
            .map_err(|e| format!("{}: {e}", raw.display()))?;
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
        set(&mut claims, &key, entry);
        write_claims(claims_path, &claims)?;
        if state != "measured" {
            unmeasured.push(id.clone());
        }
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
    super::memory::measured_all("timing", &unmeasured)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workload_iterations_require_a_positive_integer() {
        for count in ["0", "-1", "8.5", "8.0", "\"8\"", "4294967296"] {
            let workload = python::loads(&format!("{{\"iterations\":{count}}}")).unwrap();
            assert!(iterations(&workload).is_err(), "accepted {count}");
        }
        let workload = Value::object([("iterations", Value::int(8))]);
        assert_eq!(iterations(&workload), Ok(8));
    }
}
