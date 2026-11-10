//! `cargo xtask report`: run every corpus case of one feature contract through
//! the candidate and its selected reference worker, then grade the report.
//! Completed cases are checkpointed so an interrupted run keeps its evidence;
//! a checkpoint is never an acceptance.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::acceptance::acceptance;
use super::comparison::reference_summary;
use super::gate_build;
use super::parity::{self, Comparison, Run};
use super::provenance::source_identity_error;
use super::timing::{Operation, benchmark};
use super::validation::{self, selected_operation};
use super::worker;
use crate::checks::read_json;
use crate::corpus::write_json;
use crate::json::{self, Value};
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
    sha256::file_hex(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn measure_pinned(
    check: impl Fn() -> Result<Option<&'static str>, String>,
    measure: impl FnOnce() -> Result<Value, String>,
) -> Result<Value, String> {
    if let Some(status) = check()? {
        return Ok(Value::object([("status", Value::str(status))]));
    }
    let result = measure()?;
    match check() {
        Ok(None) => Ok(result),
        Ok(Some(status)) => Ok(Value::object([
            ("status", Value::str(status)),
            ("invalidated_measurement", result),
        ])),
        Err(error) => Ok(Value::object([
            ("status", Value::str("input-check-failed")),
            ("input_error", Value::str(error)),
            ("invalidated_measurement", result),
        ])),
    }
}

fn s<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn entity_input_failure(
    corpus: &Path,
    case: &Value,
    ordered_transport: bool,
) -> Result<Option<&'static str>, String> {
    let entities = case.get("entities");
    if entities.is_none() && !ordered_transport {
        return Ok(None);
    }
    let args = case_arguments(case);
    let [_, _, args @ ..] = args.as_slice() else {
        return Err("ordered transport input needs packet size and program".into());
    };
    let args = match args {
        [namespace, remaining @ ..] if namespace == "--dvb" || namespace == "--atsc" => remaining,
        _ => args,
    };
    let Some(entities) = entities else {
        return if args.is_empty() {
            Ok(None)
        } else {
            Err("worker entity argument has no input pin".into())
        };
    };
    let entities = entities.as_array().ok_or("expected an entity list")?;
    let [primary, following @ ..] = entities else {
        return Err("empty entity list".into());
    };
    let file = s(case, "file").ok_or("case without input path")?;
    if s(primary, "file") != Some(file) || s(primary, "sha256") != s(case, "sha256") {
        return Err("first entity does not match the case input".into());
    }
    let mut args = args.iter();
    for entity in following {
        let Some(name) = args.next() else {
            return Err("ordered entity has no worker argument".into());
        };
        let discontinuity = name == "--discontinuity";
        if entity
            .get("discontinuity")
            .is_some_and(|declared| declared != &Value::Bool(discontinuity))
        {
            return Err("entity discontinuity does not match its worker argument".into());
        }
        let name = if discontinuity {
            args.next().ok_or("discontinuity without an entity")?
        } else {
            name
        };
        let relative = Path::new(name);
        if name.to_str().is_none_or(|name| {
            name.is_empty() || name.starts_with('-') || name.contains(['/', '\\'])
        }) || !matches!(
            relative.components().next(),
            Some(std::path::Component::Normal(_))
        ) || s(entity, "file").map(Path::new)
            != Some(Path::new(file).with_file_name(relative).as_path())
        {
            return Err("ordered entity does not match its worker argument".into());
        }
    }
    if args.next().is_some() {
        return Err("worker entity argument has no input pin".into());
    }
    for entity in following {
        let file = Path::new(s(entity, "file").ok_or("entity without input path")?);
        if file.as_os_str().is_empty()
            || file
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("entity input escapes corpus".into());
        }
        let path = corpus.join(file);
        if !path.is_file() {
            return Ok(Some("missing-entity"));
        }
        if digest(&path)? != s(entity, "sha256").ok_or("entity without input hash")? {
            return Ok(Some("entity-hash-mismatch"));
        }
    }
    Ok(None)
}

fn command_output(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// Targets a qualification run may name.
const QUALIFIED_TARGETS: [&str; 4] = [
    "aarch64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
    "armv7-unknown-linux-gnueabihf",
];

/// The Rust target triple this host qualifies for: the host triple of the
/// toolchain that builds the workers (`rustc -vV`). A 32-bit ARM user space on an
/// AArch64 kernel reports `aarch64` or `armv8l` from `uname -m`, so the machine
/// name cannot tell it apart from a 64-bit one. Any other target, such as ARMv6 or
/// `thumbv7neon`, is reported as unqualified.
pub fn host_target() -> String {
    static HOST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HOST.get_or_init(|| {
        let version = command_output("rustc", &["-vV"]);
        let host = version.lines().find_map(|line| line.strip_prefix("host: "));
        qualified(host.unwrap_or("unknown"))
    })
    .clone()
}

fn qualified(target: &str) -> String {
    if QUALIFIED_TARGETS.contains(&target) {
        target.to_owned()
    } else {
        format!("unqualified-{target}")
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

/// Manifests of third-party inputs that may be measured: ones whose licences have
/// been reviewed, which list `sources` with a `licence` and the `sha256` of every
/// file a source provides, and the fetched upstream samples compared locally only,
/// which list `samples` with their `sha256`.
const REVIEWED_INPUTS: [&str; 2] = ["harness/fonts/corpus.json", "harness/fate/samples.json"];

fn reviewed_inputs(root: &Path) -> Result<Vec<Value>, String> {
    REVIEWED_INPUTS
        .iter()
        .map(|path| read_json(&root.join(path)).map_err(|e| format!("{path}: {e}")))
        .collect()
}

/// Whether a case's input may be measured: authored synthetic data, a
/// `reviewed-licence` file whose hash a reviewed input manifest records under the
/// licence the case names, or a `fetched-not-redistributed` sample whose hash a
/// manifest of fetched samples records. Any other rights value is unreviewed.
fn rights_reviewed(reviewed: &[Value], case: &Value) -> bool {
    fn list<'a>(value: &'a Value, key: &str) -> &'a [Value] {
        value.get(key).and_then(Value::as_array).unwrap_or_default()
    }
    match s(case, "rights") {
        Some("authored-synthetic") => true,
        Some("fetched-not-redistributed") => s(case, "sha256").is_some_and(|sha| {
            reviewed
                .iter()
                .flat_map(|manifest| list(manifest, "samples"))
                .any(|sample| s(sample, "sha256") == Some(sha))
        }),
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

/// The work a case must fold on `target`: its `expected` pin, or, for a case
/// pinned per target, its pin for `target`; `None` when it has none there.
pub fn expected_for<'a>(case: &'a Value, target: &str) -> Option<&'a Value> {
    case.get("expected").or_else(|| {
        case.get(crate::generators::EXPECTED_BY_TARGET)
            .and_then(|pins| pins.get(target))
    })
}

/// A case without a pin for `target`, run and reported, neither passing nor
/// failing: `unpinned`, with the contract's reason when it gives one.
fn unpinned(mut ran: Value, contract: &Value, id: &str, target: &str) -> Value {
    let reason = crate::generators::unpinned(contract, id)
        .into_iter()
        .find(|(t, _)| *t == target)
        .map_or_else(
            || format!("no pin for {target}; reported, not compared"),
            |(_, why)| format!("not pinned for {target}, reported: {why}"),
        );
    if let Value::Object(members) = &mut ran {
        members.retain(|(key, _)| key != "status" && key != "reason");
        members.insert(0, ("status".into(), Value::str("unpinned")));
        members.push(("reason".into(), Value::str(reason)));
    }
    ran
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
    /// The work values the contract compares.
    pub work_fields: Vec<String>,
    /// How long one worker run may take (the contract's `timeout_seconds`).
    pub timeout: Duration,
    /// Whether the contract pins its cases per target.
    pub per_target: bool,
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
        let (listing, _) = super::manifest::read(root, manifest)?;
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
            work_fields: super::comparison::work_fields(&document)?,
            timeout: worker::contract_timeout(&document)
                .map_err(|e| format!("{}: {e}", contract.display()))?,
            per_target: crate::generators::pinned_per_target(&document),
            id,
        })
    }

    /// Every registered feature (`harness/features/<id>`) whose contract runs
    /// `candidate` through a worker operation prefix, in id order.
    pub fn registered(root: &Path, candidate: &str) -> Result<Vec<Self>, String> {
        super::features::discover(root)?
            .iter()
            .filter(|contract| {
                contract.document.get("worker_operation").is_some()
                    && contract.candidate() == Some(candidate)
            })
            .map(|contract| Self::load(root, &contract.path, &contract.manifest(), None))
            .collect()
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

/// Stream canonical report JSON, retaining at most one case from the iterator.
/// Publish a complete report without replacing an existing destination.
pub(crate) fn write_case_report(
    output: &Path,
    header: &Value,
    rows: impl Iterator<Item = Result<Value, String>>,
) -> Result<(), String> {
    let Value::Object(fields) = header else {
        return Err("report header must be an object".into());
    };
    let mut fields: BTreeMap<_, _> = fields
        .iter()
        .map(|(key, value)| (key.as_str(), value))
        .collect();
    let placeholder = Value::Null;
    fields.insert("cases", &placeholder);
    let temporary = output.join("report.json.tmp");
    let destination = output.join("report.json");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    let result = (|| -> std::io::Result<()> {
        let mut writer = BufWriter::new(file);
        let mut rows = rows;
        write!(writer, "{{")?;
        for (index, (key, value)) in fields.iter().enumerate() {
            let separator = if index == 0 { "" } else { "," };
            write!(writer, "{separator}{}:", json::canonical(&Value::str(*key)))?;
            if *key == "cases" {
                write!(writer, "[")?;
                for (index, row) in rows.by_ref().enumerate() {
                    let row = row.map_err(std::io::Error::other)?;
                    let separator = if index == 0 { "" } else { "," };
                    write!(writer, "{separator}{}", json::canonical(&row))?;
                }
                write!(writer, "]")?;
            } else {
                write!(writer, "{}", json::canonical(value))?;
            }
        }
        writeln!(writer, "}}")?;
        writer.flush()?;
        drop(writer);
        fs::hard_link(&temporary, &destination)
    })();
    let _ = fs::remove_file(&temporary);
    result.map_err(|error| format!("{}: {error}", destination.display()))
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
    let work_fields = super::comparison::work_fields(&contract)?;
    let timeout = worker::contract_timeout(&contract)?;
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
    let (manifest, manifest_sha) = super::manifest::read(root, &options.manifest)?;
    let open = open_cases(&contract, &manifest, &host_target())?;
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
        ("corpus_manifest_sha256", Value::str(&manifest_sha)),
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
    for case in manifest
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
        } else if let Some(status) = entity_input_failure(
            &corpus,
            case,
            matches!(worker_operation.as_deref(), Some("ts-" | "ts-au-")),
        )? {
            Value::object([("status", Value::str(status))])
        } else if let Some(prefix) = &worker_operation {
            let arguments = case_arguments(case);
            row.push(("input_sha256", Value::str(sha)));
            if let Some(entities) = case.get("entities") {
                row.push(("input_entities", entities.clone()));
            }
            row.push(("candidate_worker", Value::str(&candidate_name)));
            row.push(("operation", selected.operation));
            let run = Run {
                root,
                candidate: &candidate,
                reference: &reference,
                input: &path,
                raw: options.output.join(&id),
                timeout,
            };
            timed = Some((
                reference.clone(),
                path.clone(),
                Operation {
                    prefix: prefix.clone(),
                    arguments: arguments.clone(),
                    candidate_options: vec![],
                    work_fields: work_fields.clone(),
                    timeout,
                },
            ));
            let per_target = crate::generators::pinned_per_target(&contract);
            let expected = expected_for(case, &target);
            let ran = parity::feed_parity(
                &run,
                prefix,
                &arguments,
                expected.unwrap_or(&Value::Null),
                &work_fields,
                per_target,
            )?;
            if expected.is_some() {
                ran
            } else {
                unpinned(ran, &contract, &id, &target)
            }
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
                timeout,
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
                (Some(seek), false) => vec![json::to_python(seek, None).into()],
                _ => vec![],
            };
            timed = Some((
                reference.clone(),
                path.clone(),
                Operation {
                    prefix: prefix.into(),
                    arguments,
                    candidate_options: vec![],
                    work_fields: work_fields.clone(),
                    timeout,
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
        match s(&parity, "reason") {
            Some(reason) => println!(
                "{id} {}: {reason}",
                s(&parity, "status").unwrap_or_default()
            ),
            None => println!("{id} {}", s(&parity, "status").unwrap_or_default()),
        }
        let passed = s(&parity, "status") == Some("pass");
        row.push(("parity", parity));
        if let (true, Some((reference, input, operation))) = (passed, timed) {
            let reference = options.baseline_worker.clone().unwrap_or(reference);
            let check_inputs = || {
                if digest(&input)? != sha {
                    return Ok(Some("input-hash-mismatch"));
                }
                entity_input_failure(&corpus, case, false)
            };
            if options.benchmark {
                let timing = measure_pinned(check_inputs, || {
                    benchmark(
                        &candidate,
                        &reference,
                        &input,
                        options.pairs,
                        iterations,
                        options.seed,
                        &operation,
                    )
                })?;
                row.push(("timing", timing));
            }
            if options.memory {
                let memory = measure_pinned(check_inputs, || {
                    super::memory::collect(
                        root,
                        &candidate,
                        &reference,
                        &input,
                        options.memory_pairs.unwrap_or(options.pairs),
                        options.seed,
                        &operation,
                    )
                })?;
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
    Ok(i32::from(!ratchet(&label, &rows, &open)))
}

/// A contract's `open_cases` on `target`: case ids that fail today, each tied to
/// one of its `pending_module_requirements`. An entry with `targets` is open on
/// those of the contract's `required_targets` only, and must pass elsewhere.
/// Every id must be a case of the manifest.
pub(super) fn open_cases(
    contract: &Value,
    manifest: &Value,
    target: &str,
) -> Result<Vec<String>, String> {
    let list = |value: &Value, key: &str| -> Vec<Value> {
        value
            .get(key)
            .and_then(Value::as_array)
            .unwrap_or_default()
            .to_vec()
    };
    let pending = list(contract, "pending_module_requirements");
    let cases = list(manifest, "cases");
    let mut open = Vec::new();
    for entry in list(contract, "open_cases") {
        let (Some(id), Some(requirement)) = (s(&entry, "id"), entry.get("requirement")) else {
            return Err("an open case needs an id and a requirement".into());
        };
        if !pending.contains(requirement) {
            return Err(format!("open case {id}: its requirement is not pending"));
        }
        if !cases.iter().any(|case| s(case, "id") == Some(id)) {
            return Err(format!("open case {id}: not a case of the manifest"));
        }
        if let Some(scope) = entry.get("targets") {
            let required = list(contract, "required_targets");
            let scope = scope.as_array().unwrap_or_default();
            if scope.is_empty() || scope.iter().any(|t| !required.contains(t)) {
                return Err(format!(
                    "open case {id}: its targets must be some of the required targets"
                ));
            }
            if !scope.iter().any(|t| t.as_str() == Some(target)) {
                continue;
            }
        }
        open.push(id.to_owned());
    }
    Ok(open)
}

/// Prints how many cases pass and which listed open cases still fail. Holds when
/// every other case passes and every open case still fails: a listed case that
/// starts to pass must leave the list, so the list only shrinks.
pub(super) fn ratchet(label: &str, rows: &[Value], open: &[String]) -> bool {
    let passed = |row: &Value| row.get("parity").and_then(|p| s(p, "status")) == Some("pass");
    let listed = |row: &Value| open.iter().any(|id| s(row, "id") == Some(id));
    let ids = |keep: &dyn Fn(&Value) -> bool| -> Vec<&str> {
        rows.iter()
            .filter(|row| keep(row))
            .filter_map(|row| s(row, "id"))
            .collect()
    };
    let unpinned = |row: &Value| row.get("parity").and_then(|p| s(p, "status")) == Some("unpinned");
    let failing = ids(&|row| !passed(row) && !listed(row) && !unpinned(row));
    let reported = ids(&unpinned);
    let still_open = ids(&|row| !passed(row) && listed(row));
    let closed = ids(&|row| passed(row) && listed(row));
    println!(
        "{label}: {} of {} cases pass",
        rows.iter().filter(|row| passed(row)).count(),
        rows.len()
    );
    if !still_open.is_empty() {
        println!(
            "{label}: {} open failures, as listed: {}",
            still_open.len(),
            still_open.join(", ")
        );
    }
    if !reported.is_empty() {
        println!(
            "{label}: {} cases not pinned for this target, reported only: {}",
            reported.len(),
            reported.join(", ")
        );
    }
    if !failing.is_empty() {
        println!("{label}: failing: {}", failing.join(", "));
    }
    if !closed.is_empty() {
        println!(
            "{label}: listed open cases now pass; remove them from open_cases: {}",
            closed.join(", ")
        );
    }
    !rows.is_empty() && failing.is_empty() && closed.is_empty()
}

#[cfg(test)]
mod prefix_tests {
    use super::{
        PrefixFeature, entity_input_failure, measure_pinned, open_cases, ratchet, read_json,
        recorded_corpus, reviewed_inputs, rights_reviewed, s,
    };
    use crate::json::{Value, parse};
    use std::path::Path;

    #[test]
    fn ordered_input_hashes_are_checked_before_running_the_workers() {
        let root = crate::walk::scratch("ordered-input-hashes");
        std::fs::create_dir_all(&root).unwrap();
        let input = b"known entity bytes";
        let hash = crate::sha256::hex(input);
        let row = format!(
            r#"{{"file":"first.ts","sha256":"{hash}","arguments":["188","1","next.ts"],
                 "entities":[{{"file":"first.ts","sha256":"{hash}"}},
                             {{"file":"next.ts","sha256":"{hash}"}}]}}"#
        );
        let case = parse(&row).unwrap();
        let unpinned = parse(r#"{"arguments":["188","1","next.ts"]}"#).unwrap();
        assert_eq!(
            entity_input_failure(&root, &unpinned, true).unwrap_err(),
            "worker entity argument has no input pin"
        );
        let mismatched =
            parse(&row.replace(r#"["188","1","next.ts"]"#, r#"["188","1","different.ts"]"#))
                .unwrap();
        assert_eq!(
            entity_input_failure(&root, &mismatched, true).unwrap_err(),
            "ordered entity does not match its worker argument"
        );
        assert_eq!(
            entity_input_failure(&root, &case, true).unwrap(),
            Some("missing-entity")
        );
        std::fs::write(root.join("next.ts"), b"other entity bytes").unwrap();
        assert_eq!(
            entity_input_failure(&root, &case, true).unwrap(),
            Some("entity-hash-mismatch")
        );
        std::fs::write(root.join("next.ts"), input).unwrap();
        assert_eq!(entity_input_failure(&root, &case, true).unwrap(), None);
        for namespace in ["--dvb", "--atsc"] {
            let with_namespace = parse(&row.replace(
                r#"["188","1","next.ts"]"#,
                &format!(r#"["188","1","{namespace}","next.ts"]"#),
            ))
            .unwrap();
            assert_eq!(
                entity_input_failure(&root, &with_namespace, true).unwrap(),
                None
            );
        }
        let measured = measure_pinned(
            || entity_input_failure(&root, &case, true),
            || {
                std::fs::write(root.join("next.ts"), b"changed while measuring").unwrap();
                Ok(Value::object([("status", Value::str("measured"))]))
            },
        )
        .unwrap();
        assert_eq!(s(&measured, "status"), Some("entity-hash-mismatch"));
        assert_eq!(
            measured
                .get("invalidated_measurement")
                .and_then(|v| s(v, "status")),
            Some("measured")
        );
        let measured = std::cell::Cell::new(false);
        let result = measure_pinned(
            || {
                if measured.get() {
                    Err("input unreadable".into())
                } else {
                    Ok(None)
                }
            },
            || {
                measured.set(true);
                Ok(Value::object([("status", Value::str("measured"))]))
            },
        )
        .unwrap();
        assert_eq!(s(&result, "status"), Some("input-check-failed"));
        assert!(result.get("invalidated_measurement").is_some());
        std::fs::remove_file(root.join("next.ts")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn rights_are_synthetic_or_a_recorded_reviewed_licence() {
        let reviewed = [
            parse(
                r#"{"sources": [{"licence": "OFL-1.1", "files": [{"path": "a.ttf", "sha256": "aa"}]}]}"#,
            )
            .unwrap(),
            parse(r#"{"samples": [{"path": "fate/a.ts", "sha256": "cc"}]}"#).unwrap(),
        ];
        let case = |text: &str| parse(text).unwrap();
        assert!(rights_reviewed(
            &reviewed,
            &case(r#"{"rights": "authored-synthetic"}"#)
        ));
        let font = r#"{"rights": "reviewed-licence", "licence": "OFL-1.1", "sha256": "aa"}"#;
        assert!(rights_reviewed(&reviewed, &case(font)));
        let fetched = r#"{"rights": "fetched-not-redistributed", "sha256": "cc"}"#;
        assert!(rights_reviewed(&reviewed, &case(fetched)));
        for unreviewed in [
            r#"{"rights": "fetched-not-redistributed", "sha256": "aa"}"#,
            r#"{"rights": "fetched-not-redistributed", "source": "fate-suite/a.ts"}"#,
            r#"{"rights": "reviewed-licence", "licence": "OFL-1.1", "sha256": "cc"}"#,
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
    fn every_subtitle_case_may_be_measured() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let reviewed = reviewed_inputs(root).unwrap();
        for feature in ["pgs", "dvb", "dvd", "cc", "teletext"] {
            let path = root.join(format!("harness/features/subtitle-{feature}/corpus.json"));
            let manifest = read_json(&path).unwrap();
            for case in manifest.get("cases").and_then(Value::as_array).unwrap() {
                assert!(
                    rights_reviewed(&reviewed, case),
                    "{}",
                    s(case, "id").unwrap()
                );
            }
        }
    }

    #[test]
    fn the_font_features_are_registered_prefix_features() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let features = PrefixFeature::registered(root, "font-worker").unwrap();
        let found: Vec<(&str, &str, usize)> = features
            .iter()
            .map(|f| (f.id.as_str(), f.prefix.as_str(), f.cases.len()))
            .collect();
        assert_eq!(
            found,
            [
                ("font-hinting-modes", "font-sized-", 954),
                ("font-hinting-none", "font-sized-", 159),
                ("font-outlines", "font-", 53),
            ]
        );
        let reviewed = reviewed_inputs(root).unwrap();
        for feature in &features {
            assert_eq!(feature.corpus, root.join(".research/fonts/files"));
            assert!(
                feature
                    .cases
                    .iter()
                    .all(|case| rights_reviewed(&reviewed, case))
            );
        }
        assert!(
            features[0]
                .input(&parse(r#"{"file": "../x.ttf"}"#).unwrap())
                .is_err()
        );
        assert_eq!(recorded_corpus(root, &Value::Object(vec![])), None);
    }

    #[test]
    fn open_cases_only_shrink() {
        let rows = |pass: &[(&str, bool)]| -> Vec<Value> {
            pass.iter()
                .map(|(id, ok)| {
                    let status = if *ok { "pass" } else { "mismatch" };
                    parse(&format!(
                        r#"{{"id": "{id}", "parity": {{"status": "{status}"}}}}"#
                    ))
                    .unwrap()
                })
                .collect()
        };
        let open = vec!["b".to_owned()];
        assert!(ratchet("f", &rows(&[("a", true), ("b", false)]), &open));
        assert!(
            !ratchet("f", &rows(&[("a", false), ("b", false)]), &open),
            "unlisted failure"
        );
        assert!(
            !ratchet("f", &rows(&[("a", true), ("b", true)]), &open),
            "listed case passes"
        );
        assert!(ratchet("f", &rows(&[("a", true)]), &[]));
        let contract = |requirement: &str| {
            parse(&format!(
                r#"{{"pending_module_requirements": ["x"], "open_cases": [{{"id": "b", "requirement": "{requirement}"}}]}}"#
            ))
            .unwrap()
        };
        let manifest = parse(r#"{"cases": [{"id": "a"}, {"id": "b"}]}"#).unwrap();
        assert_eq!(open_cases(&contract("x"), &manifest, "t").unwrap(), open);
        assert!(
            open_cases(&contract("y"), &manifest, "t").is_err(),
            "requirement not pending"
        );
        let other = parse(r#"{"cases": [{"id": "a"}]}"#).unwrap();
        assert!(
            open_cases(&contract("x"), &other, "t").is_err(),
            "unknown case"
        );
        // An open case scoped to targets is open there only.
        let scoped = |targets: &str| {
            parse(&format!(
                r#"{{"required_targets": ["t", "u"], "pending_module_requirements": ["x"],
                    "open_cases": [{{"id": "b", "requirement": "x", "targets": {targets}}}]}}"#
            ))
            .unwrap()
        };
        assert_eq!(
            open_cases(&scoped(r#"["t"]"#), &manifest, "t").unwrap(),
            open
        );
        assert!(
            open_cases(&scoped(r#"["t"]"#), &manifest, "u")
                .unwrap()
                .is_empty()
        );
        for wrong in ["[]", r#"["v"]"#] {
            assert!(
                open_cases(&scoped(wrong), &manifest, "t").is_err(),
                "{wrong}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn only_the_listed_targets_qualify() {
        assert_eq!(
            qualified("armv7-unknown-linux-gnueabihf"),
            "armv7-unknown-linux-gnueabihf"
        );
        for other in [
            "arm-unknown-linux-gnueabihf",
            "armv6-unknown-linux-gnueabihf",
            "thumbv7neon-unknown-linux-gnueabihf",
            "aarch64-unknown-linux-musl",
        ] {
            assert_eq!(qualified(other), format!("unqualified-{other}"));
        }
        let host = host_target();
        assert!(
            QUALIFIED_TARGETS.contains(&host.as_str()) || host.starts_with("unqualified-"),
            "{host}"
        );
    }

    struct Output(PathBuf);

    impl Output {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "triergon-case-report-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Output {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn case_report_preserves_raw_rows_and_number_precision() {
        let output = Output::new();
        let header = Value::object([
            (
                "z",
                Value::object([("b", Value::int(2)), ("a", Value::int(1))]),
            ),
            ("cases", Value::strings(["replace this value"])),
            ("a", Value::str("complete")),
        ]);
        let raw = [
            r#"{"id":"one","timing":{"samples":[{"digest":18446744073709551615,"elapsed_ns":9007199254740993,"ratio":0.123456789012345678901234567890}]}}"#,
            r#"{"id":"two","memory":{"samples":[{"low":-170141183460469231731687303715884105728,"bytes":170141183460469231731687303715884105727}]},"proof":{"valid":true,"source":"input"}}"#,
        ];
        write_case_report(
            &output.0,
            &header,
            raw.iter()
                .map(|text| json::parse_strict(text).map_err(|error| error.to_string())),
        )
        .unwrap();
        let expected = Value::object([
            ("z", header.get("z").unwrap().clone()),
            (
                "cases",
                Value::Array(
                    raw.iter()
                        .map(|text| json::parse_strict(text).unwrap())
                        .collect(),
                ),
            ),
            ("a", Value::str("complete")),
        ]);
        let text = fs::read_to_string(output.0.join("report.json")).unwrap();
        assert_eq!(text, format!("{}\n", json::canonical(&expected)));
        for number in [
            "18446744073709551615",
            "9007199254740993",
            "0.123456789012345678901234567890",
            "-170141183460469231731687303715884105728",
        ] {
            assert!(text.contains(number));
        }
        assert!(!output.0.join("report.json.tmp").exists());
    }

    #[test]
    fn case_report_iteration_failure_is_not_published() {
        let output = Output::new();
        let rows = [Ok(Value::int(1)), Err("raw case unavailable".to_owned())]
            .into_iter()
            .chain(std::iter::once_with(|| {
                panic!("iteration continued after error")
            }));
        let error = write_case_report(&output.0, &Value::Object(Vec::new()), rows).unwrap_err();
        assert!(error.contains("raw case unavailable"));
        assert!(!output.0.join("report.json").exists());
        assert!(!output.0.join("report.json.tmp").exists());
    }

    #[test]
    fn case_report_keeps_existing_destination() {
        let output = Output::new();
        let destination = output.0.join("report.json");
        fs::write(&destination, b"existing report\n").unwrap();
        assert!(
            write_case_report(
                &output.0,
                &Value::object([("schema", Value::int(1))]),
                std::iter::once(Ok(Value::int(2))),
            )
            .is_err()
        );
        assert_eq!(fs::read(&destination).unwrap(), b"existing report\n");
        assert!(!output.0.join("report.json.tmp").exists());
    }

    #[test]
    fn case_report_keeps_existing_temporary_file() {
        let output = Output::new();
        let temporary = output.0.join("report.json.tmp");
        fs::write(&temporary, b"in progress\n").unwrap();
        assert!(
            write_case_report(
                &output.0,
                &Value::object([("schema", Value::int(1))]),
                std::iter::empty(),
            )
            .is_err()
        );
        assert_eq!(fs::read(&temporary).unwrap(), b"in progress\n");
        assert!(!output.0.join("report.json").exists());
    }
}
