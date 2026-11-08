//! `cargo xtask report`: run every corpus case of one feature contract through
//! the candidate and its selected reference worker, then grade the report.
//! Completed cases are checkpointed so an interrupted run keeps its evidence;
//! a checkpoint is never an acceptance.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::acceptance::acceptance;
use super::comparison::reference_summary;
use super::gate_build;
use super::parity::{self, Comparison, Run};
use super::provenance::source_identity_error;
use super::timing::{Operation, benchmark};
use super::validation::{self, selected_operation};
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
    /// Memory pairs when different from the timing pair count.
    pub memory_pairs: Option<usize>,
    /// Timed iterations per pair member; the contract's `iterations` when unset.
    pub iterations: Option<u32>,
    pub seed: u64,
    /// Time and measure the candidate against this worker of another build instead
    /// of the reference. Correctness is still compared with the reference.
    pub baseline_worker: Option<PathBuf>,
}

impl Options {
    pub fn defaults(root: &Path) -> Self {
        Options {
            corpus: root.join(".research/fixtures"),
            manifest: root.join("harness/features/matroska-demux/corpus.json"),
            contract: root.join("harness/features/matroska-demux/contract.json"),
            output: root.join(".research/harness-report"),
            fate_cache: None,
            baseline_worker: None,
            additional_reports: Vec::new(),
            require_acceptance: false,
            require_slice_acceptance: false,
            benchmark: false,
            memory: false,
            pairs: 30,
            memory_pairs: None,
            iterations: None,
            seed: 280,
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

/// Manifests of third-party inputs whose licences have been reviewed. Each lists
/// `sources` with a `licence` and the `sha256` of every file a source provides.
const REVIEWED_INPUTS: [&str; 1] = ["harness/fonts/corpus.json"];

fn reviewed_inputs(root: &Path) -> Result<Vec<Value>, String> {
    REVIEWED_INPUTS
        .iter()
        .map(|path| read_json(&root.join(path)).map_err(|e| format!("{path}: {e}")))
        .collect()
}

/// Whether a case's input may be measured: authored synthetic data, or a
/// `reviewed-licence` file whose hash a reviewed input manifest records under the
/// licence the case names. Any other rights value is unreviewed.
fn rights_reviewed(reviewed: &[Value], case: &Value) -> bool {
    fn list<'a>(value: &'a Value, key: &str) -> &'a [Value] {
        value.get(key).and_then(Value::as_array).unwrap_or_default()
    }
    match s(case, "rights") {
        Some("authored-synthetic") => true,
        Some("reviewed-licence") => {
            let (Some(licence), Some(sha)) = (s(case, "licence"), s(case, "sha256")) else {
                return false;
            };
            reviewed
                .iter()
                .flat_map(|manifest| list(manifest, "sources"))
                .filter(|source| s(source, "licence") == Some(licence))
                .flat_map(|source| list(source, "files"))
                .any(|file| s(file, "sha256") == Some(sha))
        }
        _ => false,
    }
}

/// The arguments a prefix worker takes after a case's input.
pub fn case_arguments(case: &Value) -> Vec<std::ffi::OsString> {
    case.get("arguments")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(Into::into)
        .collect()
}

/// The directory a corpus manifest records for its cases' files, relative to the
/// repository root.
pub fn recorded_corpus(root: &Path, manifest: &Value) -> Option<PathBuf> {
    s(manifest, "directory").map(|directory| root.join(directory))
}

/// A registered feature whose cases run through a prefix worker
/// (`worker_operation`): one input and the case's arguments per run.
pub struct PrefixFeature {
    pub id: String,
    pub prefix: String,
    pub candidate: String,
    /// The comparison reference's name in the build identity.
    pub reference: String,
    pub contract: PathBuf,
    pub manifest: PathBuf,
    pub cases: Vec<Value>,
    pub corpus: PathBuf,
}

impl PrefixFeature {
    /// Loads a prefix feature; `corpus` overrides the manifest's `directory`.
    pub fn load(
        root: &Path,
        contract: &Path,
        manifest: &Path,
        corpus: Option<&Path>,
    ) -> Result<Self, String> {
        let document = read_json(contract)?;
        let text = |key: &str| {
            s(&document, key)
                .map(str::to_owned)
                .ok_or_else(|| format!("{}: no {key}", contract.display()))
        };
        let id = text("id")?;
        let listing = read_json(manifest)?;
        let corpus = match corpus {
            Some(corpus) => corpus.to_path_buf(),
            None => recorded_corpus(root, &listing).ok_or_else(|| {
                format!(
                    "{}: no corpus directory; pass --corpus or record `directory`",
                    manifest.display()
                )
            })?,
        };
        Ok(Self {
            prefix: text("worker_operation")?,
            candidate: text("candidate")?,
            reference: text("reference")?,
            cases: listing
                .get("cases")
                .and_then(Value::as_array)
                .unwrap_or_default()
                .to_vec(),
            contract: contract.to_path_buf(),
            manifest: manifest.to_path_buf(),
            corpus,
            id,
        })
    }

    /// Every registered feature (`harness/features/<id>`) whose contract runs
    /// `candidate` through a worker operation prefix, in id order.
    pub fn registered(root: &Path, candidate: &str) -> Result<Vec<Self>, String> {
        let features = root.join("harness/features");
        let mut ids: Vec<String> = fs::read_dir(&features)
            .map_err(|e| format!("{}: {e}", features.display()))?
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        ids.sort();
        let mut found = Vec::new();
        for id in ids {
            let contract = features.join(&id).join("contract.json");
            let document = read_json(&contract)?;
            if document.get("worker_operation").is_some()
                && s(&document, "candidate") == Some(candidate)
            {
                let manifest = features.join(&id).join("corpus.json");
                found.push(Self::load(root, &contract, &manifest, None)?);
            }
        }
        Ok(found)
    }

    /// The built candidate and reference, checked against the build identity.
    pub fn workers(&self, root: &Path) -> Result<(PathBuf, PathBuf), String> {
        let identity = read_json(&root.join("target/harness/identity.json"))?;
        let candidate = validation::candidate_worker(root, &identity, &self.candidate)?;
        let record = super::comparison::reference_named(&identity, &self.reference)?;
        let reference = root.join(s(&record, "path").unwrap_or_default());
        if Some(super::linux::file_sha(&reference)?.as_str()) != s(&record, "sha256") {
            return Err(format!(
                "{} changed since build identity; rebuild",
                self.reference
            ));
        }
        Ok((candidate, reference))
    }

    /// A case's input file, which may not escape the corpus directory.
    pub fn input(&self, case: &Value) -> Result<PathBuf, String> {
        let file = s(case, "file").unwrap_or_default();
        if file.is_empty()
            || Path::new(file)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(format!("{}: input escapes corpus", self.id));
        }
        Ok(self.corpus.join(file))
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
    if options.baseline_worker.is_some()
        && (options.require_acceptance || options.require_slice_acceptance)
    {
        return Err("a baseline comparison is not an acceptance run".into());
    }
    let identity = read_json(&root.join("target/harness/identity.json"))?;
    let pinned = gate_build::pinned_release(root)?;
    println!("{}", gate_build::describe(&identity, &pinned));
    let refusals = gate_build::refusals(&identity, &pinned);
    if !refusals.is_empty() && (options.require_acceptance || options.require_slice_acceptance) {
        return Err(format!("not the gate build: {}", refusals.join("; ")));
    }
    let (candidate, native) = validation::current_workers(root, &identity)?;
    let contract = read_json(&options.contract)?;
    let candidate_name = s(&contract, "candidate")
        .unwrap_or("media-worker")
        .to_owned();
    let candidate = if candidate_name == "media-worker" {
        candidate
    } else {
        validation::candidate_worker(root, &identity, &candidate_name)?
    };
    // Workers that take one input per operation and print one fold: the contract
    // names the mode prefix, and each case may add arguments after the input.
    let worker_operation = s(&contract, "worker_operation").map(str::to_owned);
    let iterations = options
        .iterations
        .or_else(|| {
            contract
                .get("iterations")
                .and_then(Value::as_f64)
                .map(|n| n as u32)
        })
        .unwrap_or(200);
    if (options.require_acceptance || options.require_slice_acceptance)
        && !validation::iterations_valid(&contract, &Value::int(iterations))
    {
        return Err("qualification iterations do not match the contract".into());
    }
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
    fs::create_dir_all(&options.output).map_err(|e| e.to_string())?;
    let target = host_target();
    let coverage = fate_coverage(
        root,
        &read_json(&root.join("harness/fate/index.json"))?,
        &read_json(&root.join("harness/fate/map.json"))?,
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
                ("cpus", gate_build::cpus()),
            ]),
        ),
        ("fate_coverage", Value::Array(coverage.clone())),
        ("additional_cases", Value::Array(additional.clone())),
    ];
    let corpus = options
        .corpus
        .canonicalize()
        .map_err(|e| format!("{}: {e}", options.corpus.display()))?;
    let reviewed = reviewed_inputs(root)?;
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
        let selected = selected_operation(case, &contract, &identity)?;
        let rule = selected.rule;
        if let Some(name) = selected.reference_worker {
            row.push(("reference_worker", Value::str(name)));
        }
        let reference = root.join(s(&selected.reference, "path").unwrap_or_default());
        row.push(("reference", reference_summary(&selected.reference)));
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
        } else if !rights_reviewed(&reviewed, case) {
            Value::object([("status", Value::str("rights-unreviewed"))])
        } else if let Some(prefix) = &worker_operation {
            let arguments = case_arguments(case);
            row.push(("input_sha256", Value::str(sha)));
            row.push(("candidate_worker", Value::str(&candidate_name)));
            row.push(("operation", selected.operation));
            let run = Run {
                root,
                candidate: &candidate,
                reference: &reference,
                input: &path,
                raw: options.output.join(&id),
            };
            timed = Some((
                reference.clone(),
                path.clone(),
                Operation {
                    prefix: prefix.clone(),
                    arguments: arguments.clone(),
                },
            ));
            parity::feed_parity(
                &run,
                prefix,
                &arguments,
                case.get("expected").unwrap_or(&Value::Null),
            )?
        } else {
            row.push(("input_sha256", Value::str(sha)));
            let seek = case
                .get("seek_target_ns")
                .filter(|v| **v != Value::Null)
                .cloned();
            let catalogue = contract_id.as_deref() == Some("matroska-segment-metadata");
            let colour = contract_id.as_deref() == Some("matroska-colour");
            row.push(("operation", selected.operation));
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
            let content = contract_id.as_deref() == Some("matroska-zlib");
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
            let reference = options.baseline_worker.clone().unwrap_or(reference);
            if options.benchmark {
                let timing = benchmark(
                    &candidate,
                    &reference,
                    &input,
                    options.pairs,
                    iterations,
                    options.seed,
                    &operation,
                )?;
                row.push(("timing", timing));
            }
            if options.memory {
                let memory = super::memory::collect(
                    root,
                    &candidate,
                    &reference,
                    &input,
                    options.memory_pairs.unwrap_or(options.pairs),
                    options.seed,
                    &operation,
                )?;
                row.push(("memory", memory));
            }
        }
        rows.push(object(row));
        let mut partial = base.clone();
        partial.extend([
            ("collection_status", Value::str("partial")),
            ("cases", Value::Array(rows.clone())),
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
    Ok(i32::from(!all_pass))
}

#[cfg(test)]
mod tests {
    use super::{PrefixFeature, recorded_corpus, reviewed_inputs, rights_reviewed};
    use crate::json::{Value, parse};
    use std::path::Path;

    #[test]
    fn rights_are_synthetic_or_a_recorded_reviewed_licence() {
        let reviewed = [parse(
            r#"{"sources": [{"licence": "OFL-1.1", "files": [{"path": "a.ttf", "sha256": "aa"}]}]}"#,
        )
        .unwrap()];
        let case = |text: &str| parse(text).unwrap();
        assert!(rights_reviewed(
            &reviewed,
            &case(r#"{"rights": "authored-synthetic"}"#)
        ));
        let font = r#"{"rights": "reviewed-licence", "licence": "OFL-1.1", "sha256": "aa"}"#;
        assert!(rights_reviewed(&reviewed, &case(font)));
        for unreviewed in [
            r#"{"rights": "reviewed-licence", "licence": "Apache-2.0", "sha256": "aa"}"#,
            r#"{"rights": "reviewed-licence", "licence": "OFL-1.1", "sha256": "bb"}"#,
            r#"{"rights": "reviewed-licence", "sha256": "aa"}"#,
            r#"{"rights": "OFL-1.1", "licence": "OFL-1.1", "sha256": "aa"}"#,
            r#"{"sha256": "aa"}"#,
        ] {
            assert!(
                !rights_reviewed(&reviewed, &case(unreviewed)),
                "{unreviewed}"
            );
        }
    }

    #[test]
    fn the_font_feature_is_a_registered_prefix_feature() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let features = PrefixFeature::registered(root, "font-worker").unwrap();
        let [font] = &features[..] else {
            panic!("one font feature");
        };
        assert_eq!(
            (font.id.as_str(), font.prefix.as_str()),
            ("font-outlines", "font-")
        );
        assert_eq!(font.corpus, root.join(".research/fonts/files"));
        assert_eq!(font.cases.len(), 47);
        assert!(
            font.input(&parse(r#"{"file": "../x.ttf"}"#).unwrap())
                .is_err()
        );
        assert_eq!(recorded_corpus(root, &Value::Object(vec![])), None);
        let reviewed = reviewed_inputs(root).unwrap();
        assert!(
            font.cases
                .iter()
                .all(|case| rights_reviewed(&reviewed, case))
        );
    }
}
