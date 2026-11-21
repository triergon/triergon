//! Exact comparisons of a prefix worker's trace with independent specification rows.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{comparison, features, manifest, oracle, report, validation, worker};
use crate::json::{self, Value};
use crate::sha256;

type R<T> = Result<T, String>;

const USAGE: &str = "usage: cargo xtask oracle-compare [--contract FILE] [--output DIR]";
const PIN: &str = "trace_sha256";

pub fn kind(document: &Value) -> R<oracle::Kind> {
    if document.get("reference").is_some() || document.get("runner").is_some() {
        return Err("a trace oracle contract cannot also name a reference or runner".into());
    }
    oracle::Kind::trace(validation::text(document, "oracle")?)
}

fn split_trace<'a>(raw: &'a [u8], operation: &str, fields: &[String]) -> R<&'a [u8]> {
    let body = raw.strip_suffix(b"\n").ok_or("missing trace line ending")?;
    let split = body
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |i| i + 1);
    let summary =
        json::parse_observations(std::str::from_utf8(&body[split..]).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    comparison::work_summary(&summary, fields)?;
    if summary.get("operation").and_then(Value::as_str) != Some(operation)
        || summary.get("schema") != Some(&Value::int(1))
        || summary.get("iterations") != Some(&Value::int(1))
        || summary.get("eof") != Some(&Value::Bool(true))
    {
        return Err("trace worker returned another operation or protocol".into());
    }
    Ok(&raw[..split])
}

fn case_input(root: &Path, contract: &Value, corpus: &Path, case: &Value) -> R<PathBuf> {
    let file = validation::text(case, "file")?;
    validation::hash(case, "sha256")?;
    let input = corpus.join(file);
    let reviewed = report::reviewed_inputs(root)?;
    if !report::rights_reviewed(&reviewed, case) {
        return Err("rights-unreviewed".into());
    }
    if let Some(problem) = report::input_problem(
        corpus,
        case,
        contract.get("worker_operation").and_then(Value::as_str),
    )? {
        return Err(problem.into());
    }
    Ok(input)
}

pub(super) fn corpus_directory(root: &Path, contract: &Value, listing: &Value) -> Option<PathBuf> {
    contract
        .get("corpus_directory")
        .and_then(Value::as_str)
        .map(|p| root.join(p))
        .or_else(|| report::recorded_corpus(root, listing))
}

fn oracle_trace(root: &Path, contract: &Value, corpus: &Path, case: &Value) -> R<String> {
    let input = case_input(root, contract, corpus, case)?;
    oracle::trace(
        root,
        kind(contract)?,
        &input,
        &report::resolved_case_arguments(corpus, case, report::case_arguments(case))?,
        worker::contract_parity_timeout(contract)?,
    )
}

pub fn record(root: &Path, contract: &Value, listing: &mut Value) -> R<()> {
    let corpus = corpus_directory(root, contract, listing).ok_or("no corpus directory")?;
    let Value::Object(members) = listing else {
        return Err("expected a manifest object".into());
    };
    let Some((_, Value::Array(cases))) = members.iter_mut().find(|(key, _)| key == "cases") else {
        return Err("manifest needs cases".into());
    };
    for case in cases {
        let trace = oracle_trace(root, contract, &corpus, case)?;
        validation::put(
            case,
            "expected",
            Value::object([(PIN, Value::str(sha256::hex(trace.as_bytes())))]),
        )?;
    }
    Ok(())
}

pub(super) enum CaseOutput<'a> {
    /// Retain the raw oracle and candidate outputs in the case's report directory.
    Files(&'a Path),
    /// Record the candidate's arguments, exit code and raw stdout digest.
    Calls(&'a mut Vec<Value>),
}

pub(super) struct TraceOracle<'a> {
    contract: &'a Value,
    oracle: oracle::Kind,
    mode: String,
    operation: &'a str,
    fields: Vec<String>,
    timeout: Duration,
}

impl<'a> TraceOracle<'a> {
    pub(super) fn new(contract: &'a Value) -> R<Self> {
        Ok(Self {
            contract,
            oracle: kind(contract)?,
            mode: format!("{}trace", validation::text(contract, "worker_operation")?),
            operation: validation::text(contract, "operation")?,
            fields: comparison::work_fields(contract)?,
            timeout: worker::contract_parity_timeout(contract)?,
        })
    }

    pub(super) fn compare_case(
        &self,
        root: &Path,
        corpus: &Path,
        case: &Value,
        candidate: &Path,
        output: CaseOutput<'_>,
    ) -> R<Value> {
        let input = case_input(root, self.contract, corpus, case)?;
        let declared = report::case_arguments(case);
        let arguments = report::resolved_case_arguments(corpus, case, declared.clone())?;
        let run_candidate = || -> R<worker::Finished> {
            let mut command = vec![
                candidate.as_os_str().to_owned(),
                self.mode.as_str().into(),
                input.as_os_str().to_owned(),
            ];
            command.extend_from_slice(&arguments);
            worker::run_in(&command, &[], Some(root), self.timeout)?
                .ok_or("candidate timed out".into())
        };
        // Trace identities include the candidate call even if the oracle pin fails.
        let (files, recorded) = match output {
            CaseOutput::Files(path) => (Some(path), None),
            CaseOutput::Calls(calls) => (
                None,
                Some(run_candidate().inspect(|result| {
                    let argv = [
                        self.mode.as_str().into(),
                        input
                            .file_name()
                            .unwrap_or(input.as_os_str())
                            .to_string_lossy(),
                    ]
                    .into_iter()
                    .chain(declared.iter().map(|argument| argument.to_string_lossy()));
                    calls.push(Value::object([
                        ("argv", Value::strings(argv)),
                        ("exit", Value::int(result.code)),
                        ("stdout_sha256", Value::str(sha256::hex(&result.stdout))),
                    ]));
                })),
            ),
        };
        let expected = pin(case)?;
        let reference = oracle::trace(root, self.oracle, &input, &arguments, self.timeout)?;
        let observed = sha256::hex(reference.as_bytes());
        if let Some(path) = files {
            report::create_output(path)?;
            fs::write(path.join("oracle.trace"), &reference).map_err(|e| e.to_string())?;
        }
        if observed != expected {
            return Err(format!(
                "oracle trace changed: expected {expected}, got {observed}"
            ));
        }
        let result = recorded.unwrap_or_else(run_candidate)?;
        if let Some(path) = files {
            fs::write(path.join("candidate.stdout"), &result.stdout).map_err(|e| e.to_string())?;
            fs::write(path.join("candidate.stderr"), &result.stderr).map_err(|e| e.to_string())?;
        }
        if result.code != 0 {
            return Err(format!("candidate exited {}", result.code));
        }
        let actual = split_trace(&result.stdout, self.operation, &self.fields)?;
        case_input(root, self.contract, corpus, case)?;
        let exact = actual == reference.as_bytes();
        let within_one_ulp = matches!(self.oracle, oracle::Kind::Trace("windows"))
            && crate::codecs::dsp::windows_match(actual, reference.as_bytes());
        let status = if exact || within_one_ulp {
            "pass"
        } else {
            "mismatch"
        };
        Ok(Value::object([
            ("status", Value::str(status)),
            ("oracle_trace_sha256", Value::str(observed)),
            ("candidate_trace_sha256", Value::str(sha256::hex(actual))),
        ]))
    }
}

pub fn compare(root: &Path, file: &Path, output: &Path) -> R<i32> {
    let contract = json::read(file)?;
    let contract_sha = super::linux::file_sha(file)?;
    let oracle = TraceOracle::new(&contract)?;
    let id = validation::text(&contract, "id")?;
    let candidate_name = validation::text(&contract, "candidate")?;
    let target = report::gate_target()?;
    let manifest_file = file.with_file_name("corpus.json");
    let (listing, manifest_sha) = manifest::read(root, &manifest_file)?;
    manifest::validate_required(&contract, &listing)?;
    let corpus = corpus_directory(root, &contract, &listing).ok_or("no corpus directory")?;
    let open = report::open_cases(&contract, &listing, &target)?;
    let identity = json::read(&root.join("target/harness/identity.json"))?;
    let candidate = validation::candidate_worker(root, &identity, candidate_name)?;
    let binary_sha = super::linux::file_sha(&candidate)?;
    report::create_output(output)?;
    let cases = listing
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("manifest needs cases")?;
    let mut rows = Vec::new();
    let mut execution_failed = false;
    for case in cases {
        let case_id = validation::text(case, "id")?;
        if Path::new(case_id).components().count() != 1
            || !matches!(
                Path::new(case_id).components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            return Err(format!("invalid oracle case id: {case_id}"));
        }
        let parity = match oracle.compare_case(
            root,
            &corpus,
            case,
            &candidate,
            CaseOutput::Files(&output.join(case_id)),
        ) {
            Ok(parity) => {
                println!("{case_id} {}", worker::status(&parity));
                parity
            }
            Err(error) => {
                execution_failed = true;
                println!("{case_id} error: {error}");
                Value::object([
                    ("status", Value::str("error")),
                    ("reason", Value::str(error)),
                ])
            }
        };
        rows.push(Value::object([
            ("id", Value::str(case_id)),
            (
                "input_sha256",
                case.get("sha256").cloned().unwrap_or(Value::Null),
            ),
            ("parity", parity),
        ]));
    }
    validation::candidate_worker(root, &identity, candidate_name)?;
    if super::linux::file_sha(file)? != contract_sha
        || manifest::read(root, &manifest_file)?.1 != manifest_sha
    {
        return Err("oracle contract or corpus changed during comparison".into());
    }
    // Operational failures cannot satisfy an open parity difference.
    let passed = report::ratchet(id, &rows, &open) && !execution_failed;
    let record = Value::object([
        ("schema", Value::int(1)),
        ("contract_id", Value::str(id)),
        ("target", Value::str(&target)),
        ("contract_sha256", Value::str(contract_sha)),
        ("corpus_manifest_sha256", Value::str(manifest_sha)),
        ("candidate_sha256", Value::str(binary_sha)),
        ("cases", Value::Array(rows)),
    ]);
    report::publish_json(&output.join("report.json"), &manifest::canonical(&record))?;
    Ok(i32::from(!passed))
}

pub(super) fn pin(case: &Value) -> R<&str> {
    validation::hash(validation::field(case, "expected")?, PIN)
}

pub fn main(root: &Path, args: &[String]) -> R<i32> {
    let (mut contract, mut output) = (None, None);
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let field = match flag.as_str() {
            "--contract" => &mut contract,
            "--output" => &mut output,
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(0);
            }
            _ => return Err(USAGE.into()),
        };
        if field.is_some() {
            return Err(USAGE.into());
        }
        *field = args.next().map(|path| root.join(path));
        if field.is_none() {
            return Err(USAGE.into());
        }
    }
    let output = output.unwrap_or_else(|| {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        root.join(format!(
            ".research/oracle-reports/{}-{stamp}",
            std::process::id()
        ))
    });
    if let Some(contract) = contract {
        return compare(root, &contract, &output);
    }
    let mut count = 0;
    let mut code = 0;
    for contract in features::required(root)?
        .into_iter()
        .filter(|c| c.document.get("oracle").is_some())
    {
        count += 1;
        code = code.max(compare(root, &contract.path, &output.join(&contract.id))?);
    }
    if count == 0 {
        return Err("no required oracle contracts".into());
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_line_is_removed_without_rewriting_trace_rows() {
        let raw = b"3  7\n8\n{\"schema\":1,\"operation\":\"inspect\",\"iterations\":1,\"packets\":2,\"bytes\":0,\"digest\":8,\"eof\":true}\n";
        let fields = comparison::work_fields(&Value::Null).unwrap();
        assert_eq!(split_trace(raw, "inspect", &fields).unwrap(), b"3  7\n8\n");
        assert!(split_trace(raw, "another-operation", &fields).is_err());
        assert!(split_trace(&raw[..raw.len() - 1], "inspect", &fields).is_err());
    }

    #[test]
    fn trace_pins_require_sha256() {
        let valid = Value::object([(
            "expected",
            Value::object([(PIN, Value::str("a".repeat(64)))]),
        )]);
        assert!(pin(&valid).is_ok());
        assert!(pin(&Value::Null).is_err());
    }
}
