//! Compare two builds of the same public-session workload with the shared paired runner.
use crate::harness::{comparison, memory, report, timing, worker};
use crate::json::Value;
use crate::{checks, corpus, json, sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

const USAGE: &str = "usage: cargo xtask session-queues check|time|memory BASE_WORKER CANDIDATE_WORKER NEW_OUTPUT_DIR";
const STAGES: [&str; 4] = ["cycle", "starved", "full", "drain-full"];
const TIMEOUT: Duration = Duration::from_secs(120);
const SEED: u64 = 280;
const FIELDS: [&str; 7] = [
    "units_per_pass",
    "queue_slots",
    "queue_bytes",
    "max_unit_bytes",
    "distinct_sizes",
    "adjacent_equal_sizes",
    "input_digest",
];

fn command(
    binary: &Path,
    mode: &str,
    input: &Path,
    case: &str,
    stage: &str,
) -> Result<Value, String> {
    let argv = [
        binary.into(),
        mode.into(),
        input.into(),
        case.into(),
        stage.into(),
    ];
    let result = worker::execute(&argv, TIMEOUT);
    if worker::ok(&result) {
        Ok(worker::data(&result).clone())
    } else {
        Err(json::to_python(&result, None))
    }
}

struct Row {
    id: String,
    input: PathBuf,
    stage: &'static str,
    fold: Value,
}

fn check(
    root: &Path,
    output: &Path,
    workers: memory::WorkerPaths<'_>,
    cases: &[Value],
) -> Result<Vec<Row>, String> {
    let mut rows = Vec::new();
    for case in cases {
        let text = |key| {
            case.get(key)
                .and_then(Value::as_str)
                .ok_or("invalid session fixture")
        };
        let id = text("id")?;
        let input = root.join(text("file")?);
        if sha(&input)? != text("sha256")? {
            return Err(format!("{id}: fixture hash mismatch"));
        }
        for role in [workers.reference, workers.candidate] {
            let pattern = command(role, "queue-trace", &input, id, "cycle")?;
            if case.get("sizes").is_none() || pattern.get("sizes") != case.get("sizes") {
                return Err(format!("{id}: access-unit lengths changed"));
            }
        }
        for stage in STAGES {
            let base = command(workers.reference, "queue-fold", &input, id, stage)?;
            let candidate = command(workers.candidate, "queue-fold", &input, id, stage)?;
            let expected = case
                .get(if stage == "full" { "starved" } else { stage })
                .ok_or("missing expected output extent")?;
            corpus::write_json(
                &output.join(format!("{id}-{stage}-work.json")),
                &Value::object([("baseline", base.clone()), ("candidate", candidate.clone())]),
            )?;
            if base != candidate
                || base.get("input_digest") != case.get("input_digest")
                || ["bytes", "packets"]
                    .iter()
                    .any(|k| expected.get(k).is_none() || base.get(k) != expected.get(k))
            {
                return Err(format!(
                    "{id}-{stage}: unequal work or unexpected output extent"
                ));
            }
            rows.push(Row {
                id: id.to_owned(),
                input: input.clone(),
                stage,
                fold: base,
            });
        }
    }
    Ok(rows)
}

fn sha(path: &Path) -> Result<String, String> {
    sha256::file_hex(path).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    if args == ["--help"] {
        println!("{USAGE}");
        return Ok(());
    }
    let [mode, baseline, candidate, dest] = args else {
        return Err(USAGE.into());
    };
    if !matches!(mode.as_str(), "check" | "time" | "memory") {
        return Err(USAGE.into());
    }
    let executable = |p: &str| Path::new(p).canonicalize().map_err(|e| format!("{p}: {e}"));
    let (baseline, candidate) = (executable(baseline)?, executable(candidate)?);
    let workers = memory::WorkerPaths {
        candidate: &candidate,
        reference: &baseline,
        registered: false,
    };
    let output = Path::new(dest);
    report::create_output(output)?;
    let manifest = root.join("harness/session-queues/cases.json");
    let identity = Value::object([
        ("baseline_sha256", Value::str(sha(&baseline)?)),
        ("candidate_sha256", Value::str(sha(&candidate)?)),
        ("manifest_sha256", Value::str(sha(&manifest)?)),
        ("target", Value::str(report::gate_target()?)),
        ("mode", Value::str(mode)),
    ]);
    corpus::write_json(&output.join("identity.json"), &identity)?;
    let manifest = checks::read_json(&manifest)?;
    let cases = manifest
        .as_array()
        .filter(|c| !c.is_empty())
        .ok_or("empty workload list")?;
    let rows = check(root, output, workers, cases)?;
    let mut failed = Vec::new();
    for row in &rows {
        let operation = timing::Operation {
            prefix: "queue-".into(),
            arguments: vec![row.id.as_str().into(), row.stage.into()],
            candidate_options: vec![],
            work: timing::Work {
                fields: FIELDS.map(str::to_owned).into(),
                own: vec![],
            },
            timeout: TIMEOUT,
        };
        let (result, ok) = match mode.as_str() {
            "time" if row.stage == "cycle" => {
                let mut timed = timing::measure(
                    &candidate,
                    &baseline,
                    &row.input,
                    timing::Pairs::fixed(timing::GATE_PAIRS),
                    1,
                    SEED,
                    &operation,
                )?;
                if timed.interval.is_ok() {
                    let expected = comparison::work_summary(&row.fold, &operation.work.fields)?;
                    let samples = timed
                        .record
                        .get("samples")
                        .and_then(Value::as_array)
                        .ok_or("missing timed pairs")?;
                    for pair in samples {
                        for role in ["candidate", "reference"] {
                            let observed = pair
                                .get(role)
                                .map(worker::data)
                                .ok_or("missing timed worker")?;
                            if comparison::work_summary(observed, &operation.work.fields)?
                                != expected
                            {
                                timed.interval =
                                    Err("timed work differs from the output check".into());
                            }
                        }
                    }
                }
                let ok = timed.status() == "measured";
                (Value::Object(timed.into_fields()), ok)
            }
            "memory" if row.stage != "drain-full" => {
                let args = [
                    "queue-memory".to_owned(),
                    row.input.display().to_string(),
                    row.id.clone(),
                    row.stage.to_owned(),
                ];
                let expected = Value::object([
                    ("candidate", row.fold.clone()),
                    ("reference", row.fold.clone()),
                ]);
                let job = memory::Job::new(&workers, [row.input.clone()], |slot| {
                    memory::collect_command(
                        slot,
                        root,
                        workers,
                        &row.input,
                        timing::Pairs::fixed(10),
                        SEED,
                        &args,
                        &expected,
                        TIMEOUT,
                    )
                });
                let result = memory::collect_one(job)?;
                let ok = result.get("status").and_then(Value::as_str) == Some("qualified");
                (result, ok)
            }
            _ => continue,
        };
        let name = format!("{}-{}-{mode}", row.id, row.stage);
        corpus::write_json(&output.join(format!("{name}.json")), &result)?;
        if !ok {
            failed.push(name);
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("incomplete measurements: {}", failed.join(", ")))
    }
}
