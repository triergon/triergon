//! `cargo xtask report`: run every corpus case of one feature contract through
//! the candidate and its selected reference worker, then grade the report.
//! Completed cases are checkpointed so an interrupted run keeps its evidence;
//! a checkpoint is never an acceptance.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::acceptance::acceptance;
use super::comparison::{reference_record, reference_summary, rule_for};
use super::parity::{self, Comparison, Run};
use super::provenance::source_identity_error;
use super::timing::{Operation, benchmark};
use crate::checks::read_json;
use crate::corpus::write_json;
use crate::json::Value;
use crate::sha256;

pub struct Options {
    pub corpus: PathBuf,
    pub manifest: PathBuf,
    pub contract: PathBuf,
    pub output: PathBuf,
    pub fate_cache: Option<PathBuf>,
    pub additional_reports: Vec<PathBuf>,
    pub require_acceptance: bool,
    pub require_slice_acceptance: bool,
    pub benchmark: bool,
    pub memory: bool,
    pub pairs: usize,
    pub iterations: u32,
    pub seed: u64,
    pub reuse_report: Option<PathBuf>,
}

impl Options {
    pub fn defaults(root: &Path) -> Self {
        Options {
            corpus: root.join(".research/fixtures"),
            manifest: root.join("harness/corpus.json"),
            contract: root.join("harness/slice.json"),
            output: root.join(".research/harness-report"),
            fate_cache: None,
            additional_reports: Vec::new(),
            require_acceptance: false,
            require_slice_acceptance: false,
            benchmark: false,
            memory: false,
            pairs: 30,
            iterations: 200,
            seed: 280,
            reuse_report: None,
        }
    }
}

fn digest(path: &Path) -> Result<String, String> {
    Ok(sha256::hex(
        &fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?,
    ))
}

fn s<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn command_output(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// The Rust target triple this host qualifies for.
pub fn host_target() -> String {
    let (system, machine) = (
        command_output("uname", &["-s"]),
        command_output("uname", &["-m"]),
    );
    match (system.as_str(), machine.as_str()) {
        ("Darwin", "arm64") => "aarch64-apple-darwin".into(),
        ("Linux", "x86_64") => "x86_64-unknown-linux-gnu".into(),
        ("Linux", "aarch64") => "aarch64-unknown-linux-gnu".into(),
        _ => format!("unqualified-{machine}"),
    }
}

/// One-, five- and fifteen-minute load averages.
pub fn load_average() -> Value {
    let text = fs::read_to_string("/proc/loadavg")
        .unwrap_or_else(|_| command_output("sysctl", &["-n", "vm.loadavg"]));
    let values: Vec<Value> = text
        .split_whitespace()
        .filter_map(|w| w.trim_matches(['{', '}']).parse::<f64>().ok())
        .take(3)
        .map(Value::float)
        .collect();
    Value::Array(values)
}

fn platform_name() -> String {
    ["-s", "-r", "-m"]
        .map(|flag| command_output("uname", &[flag]))
        .join("-")
}

/// Upstream FATE identities with their mapping, enabled state, cached asset and
/// any upstream reference run of the pinned revision.
pub fn fate_coverage(
    root: &Path,
    index: &Value,
    mapping: &Value,
    cache: Option<&Path>,
) -> Result<Vec<Value>, String> {
    let rules = mapping
        .get("rules")
        .and_then(Value::as_array)
        .unwrap_or_default();
    let reference = root.join(".research/reference");
    let build = read_json(&reference.join("build.json")).ok();
    let enabled: Vec<String> = fs::read_to_string(reference.join("fate-list.txt"))
        .map(|t| t.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();
    let mut receipts: Vec<(String, Value)> = Vec::new();
    if let Ok(entries) = fs::read_dir(&reference) {
        let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with("fate-") && name.ends_with(".json") {
                let receipt = read_json(&path)?;
                if let Some(case) = s(&receipt, "case") {
                    receipts.retain(|(k, _)| k != case);
                    receipts.push((case.to_owned(), receipt));
                }
            }
        }
    }
    let mut rows = Vec::new();
    for case in index
        .get("cases")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        let id = s(case, "id").unwrap_or_default();
        let mut row = vec![
            ("id".to_owned(), Value::str(id)),
            ("status".to_owned(), Value::str("unmapped")),
            (
                "definitions".to_owned(),
                case.get("definition_files").cloned().unwrap_or(Value::Null),
            ),
            (
                "enabled_in_reference_configuration".to_owned(),
                Value::Bool(enabled.iter().any(|e| e == id)),
            ),
        ];
        if let Some(rule) = rules.iter().find(|r| s(r, "id") == Some(id)) {
            row[1].1 = rule.get("status").cloned().unwrap_or(Value::Null);
            row.push((
                "operation".into(),
                rule.get("operation").cloned().unwrap_or(Value::Null),
            ));
            row.push((
                "sample".into(),
                rule.get("sample").cloned().unwrap_or(Value::Null),
            ));
            if let Some(cache) = cache {
                let sample = s(rule, "sample").unwrap_or_default();
                let path = cache.join(sample);
                if Path::new(sample)
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
                {
                    return Err("sample escapes cache".into());
                }
                let asset = if path.is_file() {
                    Value::object([
                        ("status", Value::str("present-external-rights-unreviewed")),
                        ("sha256", Value::str(digest(&path)?)),
                    ])
                } else {
                    Value::object([("status", Value::str("missing"))])
                };
                row.push(("asset".into(), asset));
            }
            if let Some((_, receipt)) = receipts.iter().find(|(k, _)| k == id)
                && let Some(build) = &build
                && receipt.get("revision").is_some()
                && receipt.get("revision") == build.get("revision")
            {
                row.push(("upstream_reference_execution".into(), receipt.clone()));
            }
        }
        rows.push(Value::Object(row));
    }
    Ok(rows)
}

/// Earlier rows by case, including unvisited rows an interrupted run kept.
pub fn measurement_cache(report: &Value) -> Result<Vec<(String, Value)>, String> {
    let mut cache: Vec<(String, Value)> = Vec::new();
    for key in ["cached_cases", "cases"] {
        for row in report
            .get(key)
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            let id = s(row, "id").unwrap_or_default().to_owned();
            if cache.iter().any(|(k, _)| *k == id) {
                return Err("duplicate measurement-cache case identities".into());
            }
            cache.push((id, row.clone()));
        }
    }
    Ok(cache)
}

/// A first clean or terminal earlier measurement of an unchanged operation, if
/// it may be kept; `None` permits a new observation.
pub fn retained_timing(
    measurement: &Value,
    pairs: usize,
    iterations: u32,
    minimum_pairs: i128,
) -> Result<Option<Value>, String> {
    if !measurement.truthy() || s(measurement, "status") == Some("contaminated") {
        return Ok(None);
    }
    if s(measurement, "status") != Some("measured") {
        return Ok(Some(measurement.clone()));
    }
    let old_pairs = match measurement.get("pairs") {
        Some(Value::Number(n)) => n.as_i128(),
        _ => None,
    };
    let same = old_pairs == Some(pairs as i128)
        && measurement
            .get("iterations")
            .is_some_and(|i| super::diff::equal(i, &Value::int(iterations)));
    if same {
        return Ok(Some(measurement.clone()));
    }
    if old_pairs.is_some_and(|old| 0 < old && old < minimum_pairs && minimum_pairs <= pairs as i128)
    {
        // A short diagnostic screen is not a complete gate run.
        return Ok(None);
    }
    Err("completed clean measurement cannot be replaced by changing collection settings; change the workers before requalification".into())
}

fn same_field(a: &Value, b: &Value, key: &str) -> bool {
    match (a.get(key), b.get(key)) {
        (Some(x), Some(y)) => super::diff::equal(x, y),
        (None, None) => true,
        _ => false,
    }
}

fn save_json(path: &Path, value: &Value) -> Result<(), String> {
    let temporary = path.with_extension("tmp");
    write_json(&temporary, value)?;
    fs::rename(&temporary, path).map_err(|e| format!("{}: {e}", path.display()))
}

fn object(pairs: Vec<(&str, Value)>) -> Value {
    Value::Object(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

/// Run the report; returns the process exit code (0 all cases pass, 1 a case
/// failed, 2 a required acceptance was not reached).
pub fn run(root: &Path, options: &Options) -> Result<i32, String> {
    let candidate = root.join("target/release/media-worker");
    let native = root.join("target/harness/ffmpeg-worker");
    let identity = read_json(&root.join("target/harness/identity.json"))?;
    let binaries = Value::object([
        ("candidate", Value::str(digest(&candidate)?)),
        ("reference", Value::str(digest(&native)?)),
    ]);
    if !identity
        .get("binaries")
        .is_some_and(|b| super::diff::equal(b, &binaries))
    {
        return Err("adapter changed since build identity; rebuild".into());
    }
    let harness_dir = root
        .join("target/harness")
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if let Some(Value::Object(references)) = identity.get("comparison_references") {
        for (_, record) in references {
            let worker = root
                .join(s(record, "path").unwrap_or_default())
                .canonicalize()
                .map_err(|e| e.to_string())?;
            if !worker.starts_with(&harness_dir)
                || Some(digest(&worker)?.as_str()) != s(record, "sha256")
            {
                return Err("comparison reference changed since build identity; rebuild".into());
            }
        }
    }
    if let Some(error) = source_identity_error(root, &identity) {
        return Err(format!("{error}; rebuild"));
    }
    let contract = read_json(&options.contract)?;
    let contract_sha = digest(&options.contract)?;
    let contract_id = s(&contract, "id").map(str::to_owned);
    let mut additional = Vec::new();
    for path in &options.additional_reports {
        let receipt = read_json(path)?;
        let fresh = receipt
            .get("identity")
            .is_some_and(|i| super::diff::equal(i, &identity))
            && s(&receipt, "contract_sha256") == Some(contract_sha.as_str());
        if !fresh {
            return Err("additional vector receipt has stale source/contract identity".into());
        }
        additional.push(receipt.get("case").cloned().unwrap_or(Value::Null));
    }
    let (mut reusable, mut previous_sha, mut previous_instrumentation) = (Vec::new(), None, None);
    if let Some(reuse) = &options.reuse_report {
        let previous = read_json(reuse)?;
        let output = fs::canonicalize(&options.output).unwrap_or_else(|_| options.output.clone());
        if reuse
            .parent()
            .and_then(|p| fs::canonicalize(p).ok())
            .as_ref()
            == Some(&output)
        {
            return Err("measurement reuse needs a different output directory to preserve the previous report and raw observations".into());
        }
        let old = previous.get("identity").cloned().unwrap_or(Value::Null);
        let same = same_field(&old, &identity, "binaries")
            && s(&previous, "contract_sha256") == Some(contract_sha.as_str())
            && same_field(&old, &identity, "comparison_references");
        if !same {
            return Err("measurement reuse requires identical workers and feature contract".into());
        }
        reusable = measurement_cache(&previous)?;
        previous_instrumentation = old.get("instrumentation").cloned();
        previous_sha = Some(digest(reuse)?);
    }
    fs::create_dir_all(&options.output).map_err(|e| e.to_string())?;
    let target = host_target();
    let coverage = fate_coverage(
        root,
        &read_json(&root.join("research/fate-index.json"))?,
        &read_json(&root.join("harness/fate-map.json"))?,
        options.fate_cache.as_deref(),
    )?;
    let base = vec![
        ("schema", Value::int(1)),
        ("target", Value::str(&target)),
        ("identity", identity.clone()),
        ("contract_sha256", Value::str(&contract_sha)),
        (
            "contract_id",
            contract.get("id").cloned().unwrap_or(Value::Null),
        ),
        (
            "corpus_manifest_sha256",
            Value::str(digest(&options.manifest)?),
        ),
        (
            "environment",
            Value::object([
                ("platform", Value::str(platform_name())),
                ("load", load_average()),
            ]),
        ),
        ("fate_coverage", Value::Array(coverage.clone())),
        ("additional_cases", Value::Array(additional.clone())),
    ];
    let corpus = options
        .corpus
        .canonicalize()
        .map_err(|e| format!("{}: {e}", options.corpus.display()))?;
    let mut rows: Vec<Value> = Vec::new();
    for case in read_json(&options.manifest)?
        .get("cases")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        let id = s(case, "id").unwrap_or_default().to_owned();
        let mut timed: Option<(PathBuf, PathBuf, Operation)> = None;
        let mut row = vec![
            ("id", Value::str(&id)),
            (
                "profile",
                case.get("profile").cloned().unwrap_or(Value::Null),
            ),
        ];
        let rule = rule_for(case, &identity, contract_id.as_deref())?;
        let selected = reference_record(&identity, rule)?;
        let reference = root.join(s(&selected, "path").unwrap_or_default());
        row.push(("reference", reference_summary(&selected)));
        let file = s(case, "file").unwrap_or_default();
        let path = corpus.join(file);
        if Path::new(file)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("input escapes corpus".into());
        }
        let sha = s(case, "sha256").unwrap_or_default();
        let parity = if !path.is_file() {
            Value::object([("status", Value::str("missing-sample"))])
        } else if digest(&path)? != sha {
            Value::object([("status", Value::str("input-hash-mismatch"))])
        } else if s(case, "rights") != Some("authored-synthetic") {
            Value::object([("status", Value::str("rights-unreviewed"))])
        } else {
            row.push(("input_sha256", Value::str(sha)));
            let seek = case
                .get("seek_target_ns")
                .filter(|v| **v != Value::Null)
                .cloned();
            let catalogue = contract_id.as_deref() == Some("K5-segment-metadata-v1");
            let colour = contract_id.as_deref() == Some("K4-colour-hdr-v1");
            let view = if catalogue {
                Value::str("catalogue-source-and-native-v1")
            } else if colour {
                Value::str("colour-source-and-ffmpeg9-v1")
            } else if let Some(rule) = rule {
                rule.get("view").cloned().unwrap_or(Value::Null)
            } else {
                Value::str("source-frames-ffmpeg9-clock-v1")
            };
            let name = if seek.is_none() {
                "container-demux-v1"
            } else {
                "container-seek-preroll-v1"
            };
            row.push((
                "operation",
                Value::object([
                    ("name", Value::str(name)),
                    ("seek_target_ns", seek.clone().unwrap_or(Value::Null)),
                    ("view", view),
                ]),
            ));
            let run = Run {
                root,
                candidate: &candidate,
                reference: &reference,
                input: &path,
                raw: options.output.join(&id),
            };
            let comparison = rule.map(|rule| Comparison {
                rule,
                native_reference: &native,
            });
            let content = contract_id.as_deref() == Some("K2-bounded-zlib-v1");
            let prefix = if catalogue {
                "catalogue-"
            } else if colour {
                "metadata-"
            } else if rule.is_some() {
                "codec-"
            } else if seek.is_some() {
                "seek-"
            } else {
                ""
            };
            let arguments = match (&seek, catalogue || colour || rule.is_some()) {
                (Some(seek), false) => vec![crate::json::to_python(seek, None).into()],
                _ => vec![],
            };
            timed = Some((
                reference.clone(),
                path.clone(),
                Operation {
                    prefix: prefix.into(),
                    arguments,
                },
            ));
            if catalogue {
                parity::catalogue_parity(&run, true, comparison.as_ref(), content)?
            } else if colour {
                parity::colour_parity(&run, true, comparison.as_ref(), content)?
            } else if seek.is_none() {
                parity::parity(&run, true, comparison.as_ref(), content, "")?
            } else {
                parity::seek_parity(&run, true, comparison.as_ref(), content)?
            }
        };
        println!("{id} {}", s(&parity, "status").unwrap_or_default());
        let passed = s(&parity, "status") == Some("pass");
        row.push(("parity", parity));
        if let (true, Some((reference, input, operation))) = (passed, timed) {
            let current = object(row.clone());
            let mut old = reusable
                .iter()
                .find(|(k, _)| *k == id)
                .map(|(_, v)| v.clone())
                .unwrap_or(Value::Object(vec![]));
            if !["input_sha256", "operation", "reference"]
                .iter()
                .all(|k| same_field(&old, &current, k))
            {
                old = Value::Object(vec![]);
            }
            if options.benchmark {
                let minimum = match contract.get("minimum_pairs") {
                    Some(Value::Number(n)) => n.as_i128().unwrap_or(i128::MAX),
                    _ => i128::MAX,
                };
                let earlier = old.get("timing").cloned().unwrap_or(Value::Object(vec![]));
                let timing =
                    match retained_timing(&earlier, options.pairs, options.iterations, minimum)? {
                        Some(kept) => kept,
                        None => benchmark(
                            &candidate,
                            &reference,
                            &input,
                            options.pairs,
                            options.iterations,
                            options.seed,
                            &operation,
                        )?,
                    };
                row.push(("timing", timing));
            }
            if options.memory {
                let minimum = match contract.get("minimum_pairs") {
                    Some(Value::Number(n)) => n.as_i128().unwrap_or(i128::MAX),
                    _ => i128::MAX,
                };
                let earlier = old.get("memory").cloned().unwrap_or(Value::Object(vec![]));
                let kept = super::memory::retained_memory(
                    &earlier,
                    options.pairs,
                    minimum,
                    previous_instrumentation.as_ref(),
                    identity.get("instrumentation"),
                )?;
                let memory = match kept {
                    Some(kept) => kept,
                    None => super::memory::collect(
                        root,
                        &candidate,
                        &reference,
                        &input,
                        options.pairs,
                        options.seed,
                        &operation,
                    )?,
                };
                row.push(("memory", memory));
            }
            if old.truthy()
                && let (Some(sha), Some(reuse)) = (&previous_sha, &options.reuse_report)
            {
                let path = fs::canonicalize(reuse)
                    .unwrap_or_else(|_| reuse.clone())
                    .display()
                    .to_string();
                row.push(("reused_evidence_sha256", Value::str(sha)));
                row.push((
                    "previous_measurement_report",
                    Value::object([("path", Value::str(path)), ("sha256", Value::str(sha))]),
                ));
            }
        }
        rows.push(object(row));
        let mut partial = base.clone();
        partial.extend([
            ("collection_status", Value::str("partial")),
            ("cases", Value::Array(rows.clone())),
            (
                "cached_cases",
                Value::Array(
                    reusable
                        .iter()
                        .filter(|(k, _)| !rows.iter().any(|r| s(r, "id") == Some(k.as_str())))
                        .map(|(_, v)| v.clone())
                        .collect(),
                ),
            ),
            (
                "acceptance",
                Value::object([
                    ("accepted", Value::Bool(false)),
                    ("slice_accepted", Value::Bool(false)),
                    ("slice_reasons", Value::strings(["collection incomplete"])),
                    ("reasons", Value::strings(["collection incomplete"])),
                ]),
            ),
        ]);
        save_json(&options.output.join("partial.json"), &object(partial))?;
    }
    let mut verdict = acceptance(
        root,
        &contract,
        &rows,
        &identity,
        &target,
        &coverage,
        &additional,
    );
    if let Some(error) = source_identity_error(root, &identity)
        && let Value::Object(fields) = &mut verdict
    {
        for (key, value) in fields.iter_mut() {
            match (key.as_str(), value) {
                ("accepted" | "slice_accepted", v) => *v = Value::Bool(false),
                ("reasons" | "slice_reasons", Value::Array(list)) => list.push(Value::str(&error)),
                _ => {}
            }
        }
    }
    let mut report = base;
    report.extend([
        ("collection_status", Value::str("complete")),
        ("cases", Value::Array(rows.clone())),
        ("acceptance", verdict.clone()),
    ]);
    save_json(&options.output.join("report.json"), &object(report))?;
    let reasons = |key: &str| {
        verdict
            .get(key)
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("; ")
    };
    let accepted = verdict.get("accepted") == Some(&Value::Bool(true));
    let slice = verdict.get("slice_accepted") == Some(&Value::Bool(true));
    println!(
        "{}",
        if accepted {
            "ACCEPTED".to_owned()
        } else {
            format!("NOT ACCEPTED: {}", reasons("reasons"))
        }
    );
    let label = contract_id.unwrap_or_default();
    println!(
        "{}",
        if slice {
            format!("{label} SLICE ACCEPTED")
        } else {
            format!("{label} SLICE NOT ACCEPTED: {}", reasons("slice_reasons"))
        }
    );
    if (options.require_slice_acceptance && !slice) || (options.require_acceptance && !accepted) {
        return Ok(2);
    }
    let all_pass = !rows.is_empty()
        && rows
            .iter()
            .all(|r| r.get("parity").and_then(|p| s(p, "status")) == Some("pass"));
    Ok(if all_pass { 0 } else { 1 })
}
