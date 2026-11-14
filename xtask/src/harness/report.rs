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

use super::acceptance::acceptance_iter;
use super::comparison::reference_summary;
use super::gate_build;
use super::parity::{self, Comparison, Run};
use super::policy::Runtime;
use super::provenance::source_identity_error;
use super::reuse;
use super::timing::{Operation, Pairs, Work, benchmark};
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
    /// Measure even when a cited evidence set holds this collection's result.
    pub full: bool,
    /// Let a gate session stop at an early look; `--no-early-stop` runs every pair.
    pub early_stop: bool,
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
            full: false,
            early_stop: true,
        }
    }

    /// Timed iterations per pair member: the override, else the contract's.
    fn iterations(&self, contract: &Value) -> u32 {
        self.iterations
            .or_else(|| {
                contract
                    .get("iterations")
                    .and_then(Value::as_f64)
                    .map(|n| n as u32)
            })
            .unwrap_or(200)
    }
}

fn digest(path: &Path) -> Result<String, String> {
    sha256::file_hex(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn measure_pinned(
    check: impl Fn() -> Result<Option<&'static str>, String>,
    measure: impl FnOnce() -> Result<Value, String>,
) -> Result<Value, String> {
    match check() {
        Ok(None) => (),
        Ok(Some(status)) => return Ok(Value::object([("status", Value::str(status))])),
        Err(error) => {
            return Ok(Value::object([
                ("status", Value::str("input-check-failed")),
                ("input_error", Value::str(error)),
            ]));
        }
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
    auxiliary_input_failure(
        corpus,
        following,
        "entity",
        "missing-entity",
        "entity-hash-mismatch",
    )
}

fn auxiliary_input_failure(
    corpus: &Path,
    inputs: &[Value],
    kind: &str,
    missing: &'static str,
    changed: &'static str,
) -> Result<Option<&'static str>, String> {
    for input in inputs {
        let file = Path::new(s(input, "file").ok_or_else(|| format!("{kind} without input path"))?);
        if file.as_os_str().is_empty()
            || file
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(format!("{kind} input escapes corpus"));
        }
        let path = corpus.join(file);
        if !path.is_file() {
            return Ok(Some(missing));
        }
        if digest(&path)?
            != s(input, "sha256").ok_or_else(|| format!("{kind} without input hash"))?
        {
            return Ok(Some(changed));
        }
    }
    Ok(None)
}

fn additional_input_failure(
    corpus: &Path,
    case: &Value,
    prefix: Option<&str>,
) -> Result<Option<&'static str>, String> {
    let failure = entity_input_failure(corpus, case, matches!(prefix, Some("ts-" | "ts-au-")))?;
    if failure.is_some() {
        return Ok(failure);
    }
    let Some(companions) = case.get("companions") else {
        return Ok(None);
    };
    auxiliary_input_failure(
        corpus,
        companions.as_array().ok_or("expected a companion list")?,
        "companion",
        "missing-companion",
        "companion-hash-mismatch",
    )
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
    gate_build::ARM32_TARGET,
];

/// The Rust target this host qualifies: the one its workers are compiled for
/// ([`gate_build::worker_target`]), from the host triple of the toolchain
/// that builds them (`rustc -vV`). A 32-bit ARM user space on an AArch64 kernel
/// reports `aarch64` or `armv8l` from `uname -m`, so the machine name cannot tell
/// it apart from a 64-bit one. Any other target, such as ARMv6 or A32, is reported
/// as unqualified.
pub fn gate_target() -> Result<String, String> {
    gate_build::worker_target().map(qualified)
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

/// Rebase DVD palette options onto the same corpus used to check companion pins.
pub(super) fn resolved_case_arguments(
    corpus: &Path,
    case: &Value,
    mut arguments: Vec<std::ffi::OsString>,
) -> Result<Vec<std::ffi::OsString>, String> {
    const OPTION: &str = "opt:ifo_palette=";
    for argument in &mut arguments {
        let Some(declared) = argument.to_str().and_then(|text| text.strip_prefix(OPTION)) else {
            continue;
        };
        let relative = Path::new(declared)
            .strip_prefix(".research/subtitle-corpus")
            .map_err(|_| "IFO palette input is outside the subtitle corpus")?;
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err("IFO palette input escapes corpus".into());
        }
        let path = corpus.join(relative);
        let pinned = case
            .get("companions")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .any(|pin| s(pin, "file").map(Path::new) == Some(relative));
        if !pinned && path.try_exists().map_err(|error| error.to_string())? {
            return Err(format!(
                "IFO palette input has no pin: {}",
                relative.display()
            ));
        }
        let mut resolved = std::ffi::OsString::from(OPTION);
        resolved.push(&path);
        *argument = resolved;
    }
    Ok(arguments)
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
    pub fn registered(
        root: &Path,
        candidate: &str,
        corpora: &BTreeMap<String, PathBuf>,
    ) -> Result<Vec<Self>, String> {
        let contracts: Vec<_> = super::features::discover(root)?
            .into_iter()
            .filter(|contract| {
                contract.document.get("worker_operation").is_some()
                    && contract.candidate() == Some(candidate)
            })
            .collect();
        if let Some(id) = corpora
            .keys()
            .find(|id| !contracts.iter().any(|contract| contract.id == id.as_str()))
        {
            return Err(format!("{candidate} has no registered prefix feature {id}"));
        }
        contracts
            .iter()
            .map(|contract| {
                let corpus = corpora.get(&contract.id).map(PathBuf::as_path);
                Self::load(root, &contract.path, &contract.manifest(), corpus)
            })
            .collect()
    }

    /// The built candidate and reference, checked against the build identity.
    pub fn workers(&self, root: &Path) -> Result<(PathBuf, PathBuf), String> {
        let identity = read_json(&root.join("target/harness/identity.json"))?;
        let candidate = validation::candidate_worker(root, &identity, &self.candidate)?;
        Ok((candidate, self.reference_worker(root, &identity)?))
    }

    pub(super) fn reference_worker(
        &self,
        root: &Path,
        identity: &Value,
    ) -> Result<PathBuf, String> {
        let record = super::comparison::reference_named(identity, &self.reference)?;
        let reference = root.join(validation::text(&record, "path")?);
        if super::linux::file_sha(&reference)? != validation::hash(&record, "sha256")? {
            return Err(format!(
                "{} changed since build identity; rebuild",
                self.reference
            ));
        }
        Ok(reference)
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

    pub(super) fn verified_input(&self, case: &Value) -> Result<PathBuf, String> {
        let path = self.input(case)?;
        if digest(&path)? != validation::hash(case, "sha256")? {
            return Err(format!("input changed: {}", path.display()));
        }
        if let Some(status) = additional_input_failure(&self.corpus, case, Some(&self.prefix))? {
            let id = s(case, "id").unwrap_or_default();
            return Err(format!("{}/{id}: {status}", self.id));
        }
        Ok(path)
    }
}

/// A checkpoint of the rows collected so far; never an acceptance.
fn partial(base: &[(&str, Value)], rows: &[Value]) -> Value {
    let mut partial = base.to_vec();
    partial.extend([
        ("collection_status", Value::str("partial")),
        ("cases", Value::Array(rows.to_vec())),
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
    object(partial)
}

/// Why a measured case's inputs no longer match their pins, if they do not.
fn input_failure(
    corpus: &Path,
    case: &Value,
    input: &Path,
    sha: &str,
    prefix: Option<&str>,
) -> Result<Option<&'static str>, String> {
    if digest(input)? != sha {
        return Ok(Some("input-hash-mismatch"));
    }
    let failure = additional_input_failure(corpus, case, prefix)?;
    if failure.is_none() && prefix.is_some() {
        resolved_case_arguments(corpus, case, case_arguments(case))?;
    }
    Ok(failure)
}

/// Why a case cannot be measured from `corpus`, before anything runs: a missing
/// or changed input, ordered entity or companion.
pub(super) fn input_problem(
    corpus: &Path,
    case: &Value,
    prefix: Option<&str>,
) -> Result<Option<&'static str>, String> {
    let file = s(case, "file").ok_or("case without input path")?;
    if Path::new(file)
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err("input escapes corpus".into());
    }
    let path = corpus.join(file);
    if !path.is_file() {
        return Ok(Some("missing-sample"));
    }
    input_failure(
        corpus,
        case,
        &path,
        s(case, "sha256").unwrap_or_default(),
        prefix,
    )
}

/// Every corpus file a case reads: its input, ordered entities and companions.
fn case_files(corpus: &Path, case: &Value) -> Result<Vec<PathBuf>, String> {
    let mut files = vec![corpus.join(s(case, "file").ok_or("case without input path")?)];
    for key in ["entities", "companions"] {
        for input in case.get(key).and_then(Value::as_array).unwrap_or_default() {
            files.push(corpus.join(s(input, "file").ok_or("input without a path")?));
        }
    }
    Ok(files)
}

fn save_checkpoint(path: &Path, value: &Value) -> Result<(), String> {
    let temporary = path.with_extension("tmp");
    write_json(&temporary, value)?;
    fs::rename(&temporary, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Publish a complete JSON document without replacing another result.
pub(crate) fn publish_json(path: &Path, text: &str) -> Result<(), String> {
    publish(path, |writer| writer.write_all(text.as_bytes()))
}

fn publish(
    destination: &Path,
    write: impl FnOnce(&mut BufWriter<fs::File>) -> std::io::Result<()>,
) -> Result<(), String> {
    let temporary = destination.with_extension("json.tmp");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    let result = (|| -> std::io::Result<()> {
        let mut writer = BufWriter::new(file);
        write(&mut writer)?;
        writer.flush()?;
        drop(writer);
        fs::hard_link(&temporary, destination)
    })();
    let _ = fs::remove_file(&temporary);
    result.map_err(|error| format!("{}: {error}", destination.display()))
}

fn store_measurement(
    output: &Path,
    case: usize,
    kind: &str,
    measurement: &Value,
) -> Result<Value, String> {
    let name = format!("measurements/{case:04}-{kind}.json");
    let path = output.join(&name);
    let data = json::observations(measurement) + "\n";
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut file| file.write_all(data.as_bytes()))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Value::object([
        ("raw", Value::str(name)),
        ("raw_sha256", Value::str(sha256::hex(data.as_bytes()))),
    ]))
}

/// Stream canonical report JSON for callers with a canonical output format.
pub(crate) fn write_case_report(
    output: &Path,
    header: &Value,
    rows: impl Iterator<Item = Result<Value, String>>,
) -> Result<(), String> {
    stream_report(output, header, rows, true)
}

/// Stream the cases in their recorded order, including each observer's run order.
pub(crate) fn write_observed_case_report(
    output: &Path,
    header: &Value,
    rows: impl Iterator<Item = Result<Value, String>>,
) -> Result<(), String> {
    stream_report(output, header, rows, false)
}

/// Retain at most one case and publish without replacing an existing report.
fn stream_report(
    output: &Path,
    header: &Value,
    rows: impl Iterator<Item = Result<Value, String>>,
    canonical: bool,
) -> Result<(), String> {
    let Value::Object(fields) = header else {
        return Err("report header must be an object".into());
    };
    let mut fields: Vec<_> = fields
        .iter()
        .filter(|(key, _)| key != "cases")
        .map(|(key, value)| (key.as_str(), value))
        .collect();
    fields.push(("cases", &Value::Null));
    let serialize = if canonical {
        json::canonical
    } else {
        json::observations
    };
    if canonical {
        fields.sort_unstable_by_key(|(key, _)| *key);
    }
    publish(&output.join("report.json"), |writer| {
        let mut rows = rows;
        write!(writer, "{{")?;
        for (index, (key, value)) in fields.into_iter().enumerate() {
            let separator = if index == 0 { "" } else { "," };
            write!(writer, "{separator}{}:", json::canonical(&Value::str(key)))?;
            if key == "cases" {
                write!(writer, "[")?;
                for (index, row) in rows.by_ref().enumerate() {
                    let row = row.map_err(std::io::Error::other)?;
                    let separator = if index == 0 { "" } else { "," };
                    write!(writer, "{separator}{}", serialize(&row))?;
                }
                write!(writer, "]")?;
            } else {
                write!(writer, "{}", serialize(value))?;
            }
        }
        writeln!(writer, "}}")
    })
}

fn object(pairs: Vec<(&str, Value)>) -> Value {
    Value::Object(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

fn reject_controls(proof: &mut Value, error: &str) -> Result<(), String> {
    validation::put(proof, "passed", Value::Bool(false))?;
    validation::put(proof, "error", Value::str(error))
}

/// Reserve a new directory before emitting any report, traces or observations.
pub(crate) fn create_output(path: &Path) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    fs::create_dir(path)
        .map_err(|error| format!("{}: {error}; use a new output directory", path.display()))
}

fn reserve_case_outputs(output: &Path, cases: &[Value]) -> Result<(), String> {
    let mut directories = Vec::with_capacity(cases.len());
    for case in cases {
        let id = s(case, "id").unwrap_or_default();
        let relative = Path::new(id);
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(format!(
                "case id {id:?} does not name a relative output directory"
            ));
        }
        directories.push(relative);
    }
    // A case may name another case's parent directory. Reserve the parent
    // first without changing the order of the report rows.
    directories.sort_unstable_by_key(|&path| (path.components().count(), path));
    for relative in directories {
        create_output(&output.join(relative))?;
    }
    Ok(())
}

/// Run the report; returns 0 for matching parity, 1 for a failed case and 2
/// when required acceptance was withheld.
pub fn run(root: &Path, options: &Options) -> Result<i32, String> {
    create_output(&options.output)?;
    match gate_key(root, options, &[])? {
        Some(key) => reuse::step(root, &options.output, &key, options.full, || {
            collect(root, options, None)
        }),
        None => collect(root, options, None),
    }
}

/// The key of a gate collection: timing or memory against the reference, not a
/// comparison with another build. It binds every worker the run uses, and any
/// `documents` beyond the contract and manifest that define its cases.
pub(crate) fn gate_key(
    root: &Path,
    options: &Options,
    documents: &[PathBuf],
) -> Result<Option<reuse::Key>, String> {
    let mode = match (options.benchmark, options.memory) {
        _ if options.baseline_worker.is_some() => return Ok(None),
        (false, false) => return Ok(None),
        (true, false) => "report --benchmark",
        (false, true) => "report --memory",
        (true, true) => "report --benchmark --memory",
    };
    let identity = read_json(&root.join("target/harness/identity.json"))?;
    let contract = read_json(&options.contract)?;
    let (manifest, _) = super::manifest::read(root, &options.manifest)?;
    let (media, native) = validation::current_workers(root, &identity)?;
    let candidate = match s(&contract, "candidate").unwrap_or("media-worker") {
        "media-worker" => media.clone(),
        name => validation::candidate_worker(root, &identity, name)?,
    };
    let mut workers = vec![
        ("candidate".to_owned(), candidate),
        ("media-worker".to_owned(), media),
        ("ffmpeg-worker".to_owned(), native),
    ];
    for case in manifest
        .get("cases")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        let selected = selected_operation(case, &contract, &identity)?;
        for record in [&selected.reference, &selected.measured] {
            let path = root.join(s(record, "path").unwrap_or_default());
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !workers.iter().any(|(known, _)| *known == name) {
                workers.push((name, path));
            }
        }
    }
    let settings = Value::object([
        ("pairs", Value::int(options.pairs as i64)),
        (
            "memory_pairs",
            Value::int(options.memory_pairs.unwrap_or(options.pairs) as i64),
        ),
        ("iterations", Value::int(options.iterations(&contract))),
        ("seed", Value::int(options.seed)),
        (
            "require_acceptance",
            Value::Bool(options.require_acceptance),
        ),
        (
            "require_slice_acceptance",
            Value::Bool(options.require_slice_acceptance),
        ),
        ("fate_cache", Value::Bool(options.fate_cache.is_some())),
        ("early_stop", Value::Bool(options.early_stop)),
        (
            "kernel_bound",
            std::env::var("TRIERGON_RSS_KERNEL_BOUND").map_or(Value::Null, Value::str),
        ),
    ]);
    let shared = [
        "harness/fate/index.json",
        "harness/fate/map.json",
        "harness/module-gates.json",
        "harness/comparison-rules.json",
    ]
    .map(|path| root.join(path));
    let files: Vec<&Path> = [options.contract.as_path(), options.manifest.as_path()]
        .into_iter()
        .chain(shared.iter().chain(documents).map(PathBuf::as_path))
        .chain(options.additional_reports.iter().map(PathBuf::as_path))
        .collect();
    let workers: Vec<(&str, &Path)> = workers
        .iter()
        .map(|(name, path)| (name.as_str(), path.as_path()))
        .collect();
    reuse::Key::new(root, mode, settings, &files, &workers).map(Some)
}

/// Collect inside the directory reserved by the report command.
pub(crate) fn collect(
    root: &Path,
    options: &Options,
    controls: Option<&Value>,
) -> Result<i32, String> {
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
    let iterations = options.iterations(&contract);
    if (options.require_acceptance || options.require_slice_acceptance)
        && !validation::iterations_valid(&contract, &Value::int(iterations))
    {
        return Err("qualification iterations do not match the contract".into());
    }
    let contract_sha = digest(&options.contract)?;
    let (manifest, manifest_sha) = super::manifest::read(root, &options.manifest)?;
    let open = open_cases(&contract, &manifest, &gate_target()?)?;
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
    let measures = options.benchmark || options.memory;
    if measures {
        let samples = options.output.join("measurements");
        fs::create_dir(&samples).map_err(|e| format!("{}: {e}", samples.display()))?;
    }
    let target = gate_target()?;
    let coverage = fate_coverage(
        root,
        &read_json(&root.join("harness/fate/index.json"))?,
        &read_json(&root.join("harness/fate/map.json"))?,
        options.fate_cache.as_deref(),
    )?;
    let mut base = vec![
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
    if measures {
        base.push(("case_storage", Value::str("sha256-files")));
    }
    if let Some(proof) = controls {
        base.push(("functional_controls", proof.clone()));
    }
    let validate_controls = |header: &Value| {
        if contract.get("functional_controls").is_some() {
            crate::progressive_bench::validate_report_controls(
                root,
                &options.output.join("report.json"),
                header,
                &contract,
                &manifest,
            )
        } else {
            Ok(())
        }
    };
    let header = object(base.clone());
    if measures || controls.is_some() {
        validate_controls(&header)?;
    }
    let corpus = options
        .corpus
        .canonicalize()
        .map_err(|e| format!("{}: {e}", options.corpus.display()))?;
    let reviewed = reviewed_inputs(root)?;
    let cases = manifest
        .get("cases")
        .and_then(Value::as_array)
        .unwrap_or_default();
    reserve_case_outputs(&options.output, cases)?;
    let mut rows: Vec<Value> = Vec::new();
    let mut memory_jobs = Vec::new();
    // A gate comparison stops early once its bound is settled below the ceiling;
    // a comparison with another build runs every pair.
    let planned_memory = options.memory_pairs.unwrap_or(options.pairs);
    let (timing_pairs, memory_pairs) = match options.baseline_worker {
        None => (
            Pairs::gate(options.pairs, Runtime::load(root)?.ceiling).early_stop(options.early_stop),
            Pairs::gate(planned_memory, super::memory::CEILING).early_stop(options.early_stop),
        ),
        Some(_) => (Pairs::fixed(options.pairs), Pairs::fixed(planned_memory)),
    };
    for case in cases {
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
        if let Some(name) = selected.measurement_worker {
            row.push(("measurement_worker", Value::str(name)));
            row.push((
                "measurement_reference",
                reference_summary(&selected.measured),
            ));
        }
        let measured = root.join(s(&selected.measured, "path").unwrap_or_default());
        if (options.benchmark || options.memory) && options.baseline_worker.is_none() {
            super::comparison::measurable(&selected.measured)?;
        }
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
        } else if let Some(status) =
            additional_input_failure(&corpus, case, worker_operation.as_deref())?
        {
            Value::object([("status", Value::str(status))])
        } else if let Some(prefix) = &worker_operation {
            let arguments = resolved_case_arguments(&corpus, case, case_arguments(case))?;
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
                measured.clone(),
                path.clone(),
                Operation {
                    prefix: prefix.clone(),
                    arguments: arguments.clone(),
                    candidate_options: vec![],
                    work: Work {
                        fields: work_fields.clone(),
                        own: vec![],
                    },
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
                measured.clone(),
                path.clone(),
                Operation {
                    prefix: prefix.into(),
                    arguments,
                    candidate_options: vec![],
                    work: Work {
                        fields: work_fields.clone(),
                        own: vec![],
                    },
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
        if let (true, Some((measured, input, operation))) = (passed, timed) {
            let reference = options.baseline_worker.clone().unwrap_or(measured);
            let prefix = worker_operation.as_deref();
            if options.benchmark {
                let timing = measure_pinned(
                    || input_failure(&corpus, case, &input, sha, prefix),
                    || {
                        benchmark(
                            &candidate,
                            &reference,
                            &input,
                            timing_pairs,
                            iterations,
                            options.seed,
                            &operation,
                        )
                    },
                )?;
                row.push((
                    "timing",
                    store_measurement(&options.output, rows.len(), "timing", &timing)?,
                ));
            }
            if options.memory {
                let index = rows.len();
                let registered = options.baseline_worker.is_none();
                let workers = super::memory::WorkerPaths {
                    candidate: &candidate,
                    reference: &reference,
                    registered,
                };
                let files = case_files(&corpus, case)?;
                let (corpus, candidate, reference) =
                    (corpus.clone(), candidate.clone(), reference.clone());
                let job = super::memory::Job::new(&workers, files, move |slot| {
                    let memory = measure_pinned(
                        || input_failure(&corpus, case, &input, sha, prefix),
                        || {
                            super::memory::collect(
                                slot,
                                root,
                                super::memory::WorkerPaths {
                                    candidate: &candidate,
                                    reference: &reference,
                                    registered,
                                },
                                &input,
                                memory_pairs,
                                options.seed,
                                &operation,
                            )
                        },
                    )?;
                    store_measurement(&options.output, index, "memory", &memory)
                });
                memory_jobs.push((index, job));
            }
        }
        rows.push(object(row));
        save_checkpoint(&options.output.join("partial.json"), &partial(&base, &rows))?;
    }
    // Every case is timed before any memory runs; memory then fills the CPUs.
    let (indices, jobs): (Vec<usize>, Vec<_>) = memory_jobs.into_iter().unzip();
    for (index, memory) in indices.into_iter().zip(super::memory::collect_all(jobs)?) {
        validation::put(&mut rows[index], "memory", memory?)?;
    }
    if options.memory {
        save_checkpoint(&options.output.join("partial.json"), &partial(&base, &rows))?;
    }
    let mut unavailable = None;
    let load_case = |row: &Value| {
        if measures {
            validation::expand_measurements(&options.output, row)
        } else {
            Ok(row.clone())
        }
    };
    let mut verdict = acceptance_iter(
        root,
        &contract,
        rows.iter()
            .map(|row| load_case(row).inspect_err(|error| unavailable = Some(error.clone()))),
        &identity,
        &target,
        &coverage,
        &additional,
    );
    if let Some(error) = unavailable {
        return Err(error);
    }
    let control_error = validate_controls(&header).err();
    let controls_valid = controls.is_none() || control_error.is_none();
    if let Some(error) = &control_error
        && let Some((_, proof)) = base
            .iter_mut()
            .find(|(key, _)| *key == "functional_controls")
    {
        reject_controls(proof, error)?;
    }
    let identity_error = source_identity_error(root, &identity)
        .or_else(|| validation::current_workers(root, &identity).err())
        .or_else(|| {
            if candidate_name == "media-worker" {
                None
            } else {
                validation::candidate_worker(root, &identity, &candidate_name).err()
            }
        });
    let identity_valid = identity_error.is_none();
    for error in [identity_error, control_error].into_iter().flatten() {
        let Value::Object(fields) = &mut verdict else {
            return Err("acceptance verdict must be an object".into());
        };
        for (key, value) in fields {
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
        (
            "summary",
            Value::object([(
                "source_and_workers_unchanged",
                Value::Bool(identity_valid && controls_valid),
            )]),
        ),
        ("acceptance", verdict.clone()),
    ]);
    if measures {
        publish_measurements(&options.output, object(report), &rows, load_case)?;
    } else {
        report.push(("cases", Value::Array(rows.clone())));
        publish_json(
            &options.output.join("report.json"),
            &(json::to_python(&object(report), Some(2)) + "\n"),
        )?;
    }
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
    let parity_matches = ratchet(&label, &rows, &open);
    Ok(i32::from(
        !parity_matches || !controls_valid || !identity_valid,
    ))
}

fn publish_measurements(
    output: &Path,
    mut header: Value,
    rows: &[Value],
    load_case: impl FnMut(&Value) -> Result<Value, String>,
) -> Result<(), String> {
    validation::put(&mut header, "case_storage", Value::str("sha256-files"))?;
    validation::put(&mut header, "cases", Value::Array(rows.to_vec()))?;
    publish_json(
        &output.join("collection.json"),
        &(json::to_python(&header, Some(2)) + "\n"),
    )?;
    validation::put(&mut header, "case_storage", Value::str("inline"))?;
    write_observed_case_report(output, &header, rows.iter().map(load_case))
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
    use std::collections::BTreeMap;
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
    fn unreadable_input_before_memory_keeps_the_completed_timing_row() {
        let timing = measure_pinned(
            || Ok(None),
            || Ok(Value::object([("status", Value::str("measured"))])),
        )
        .unwrap();
        let memory = measure_pinned(
            || Err("input unreadable".into()),
            || panic!("memory must not run without a verified input"),
        )
        .unwrap();
        let row = Value::object([("timing", timing), ("memory", memory)]);
        assert_eq!(
            row.get("timing").and_then(|v| s(v, "status")),
            Some("measured")
        );
        let memory = row.get("memory").unwrap();
        assert_eq!(s(memory, "status"), Some("input-check-failed"));
        assert_eq!(s(memory, "input_error"), Some("input unreadable"));
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
    fn registered_prefix_features_use_supplied_corpus_directories() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut corpora = BTreeMap::from([
            ("hls-segments".to_owned(), root.join("harness/fixtures/hls")),
            (
                "matroska-startup-seek".to_owned(),
                root.join(".research/startup-seek-fixtures"),
            ),
            (
                "mp4-range".to_owned(),
                root.join(".research/mp4-range-fixtures"),
            ),
            (
                "progressive-ts188".to_owned(),
                root.join("harness/features/progressive-ts188/inputs"),
            ),
        ]);
        let features = PrefixFeature::registered(root, "playback-worker", &corpora).unwrap();
        for (id, directory) in &corpora {
            let feature = features.iter().find(|feature| feature.id == *id).unwrap();
            assert_eq!(&feature.corpus, directory);
            assert!(!feature.cases.is_empty());
        }
        corpora.insert("unused".to_owned(), root.to_path_buf());
        assert!(
            PrefixFeature::registered(root, "playback-worker", &corpora)
                .err()
                .unwrap()
                .contains("no registered prefix feature unused")
        );
    }

    #[test]
    fn dvd_palette_options_use_the_selected_corpus_and_keep_absence() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let manifest = read_json(&root.join("harness/features/subtitle-dvd/corpus.json")).unwrap();
        let cases = manifest.get("cases").and_then(Value::as_array).unwrap();
        let case = |id| cases.iter().find(|case| s(case, "id") == Some(id)).unwrap();
        let scratch = crate::walk::scratch("dvd-ifo");
        let corpus = scratch.join("relocated");
        std::fs::create_dir_all(corpus.join("dvd")).unwrap();
        let resolve = |id| {
            let case = case(id);
            super::resolved_case_arguments(&corpus, case, super::case_arguments(case))
        };
        let palette = corpus.join("dvd/VTS_01_0.IFO");
        std::fs::write(&palette, b"palette").unwrap();
        let arguments = resolve("vobsub-ifo-palette-vts").unwrap();
        assert_eq!(
            arguments[1],
            format!("opt:ifo_palette={}", palette.display()).as_str()
        );

        let missing = corpus.join("dvd/VTS_99_0.IFO");
        let arguments = resolve("vobsub-ifo-palette-missing").unwrap();
        assert_eq!(
            arguments[1],
            format!("opt:ifo_palette={}", missing.display()).as_str()
        );
        std::fs::write(&missing, b"unexpected palette").unwrap();
        assert_eq!(
            resolve("vobsub-ifo-palette-missing").unwrap_err(),
            "IFO palette input has no pin: dvd/VTS_99_0.IFO"
        );
        std::fs::remove_dir_all(scratch).unwrap();
    }

    #[test]
    fn the_font_features_are_registered_prefix_features() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let features = PrefixFeature::registered(root, "font-worker", &BTreeMap::new()).unwrap();
        let found: Vec<(&str, &str, usize)> = features
            .iter()
            .map(|f| (f.id.as_str(), f.prefix.as_str(), f.cases.len()))
            .collect();
        assert_eq!(
            found,
            [
                ("font-hinting-default", "font-sized-", 53),
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
            qualified("thumbv7neon-unknown-linux-gnueabihf"),
            "thumbv7neon-unknown-linux-gnueabihf"
        );
        for other in [
            "arm-unknown-linux-gnueabihf",
            "armv6-unknown-linux-gnueabihf",
            "armv7-unknown-linux-gnueabihf",
            "aarch64-unknown-linux-musl",
        ] {
            assert_eq!(qualified(other), format!("unqualified-{other}"));
        }
        let host = gate_target().unwrap();
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
    fn measured_checkpoint_keeps_samples_separate_and_checks_their_digest() {
        let output = Output::new();
        fs::create_dir(output.0.join("measurements")).unwrap();
        let timing = json::parse_strict(
            r#"{"ratio":{"upper_95":1.01234567890123456789},"samples":[18446744073709551615]}"#,
        )
        .unwrap();
        let memory = json::parse_observations(
            r#"{"status":"invalid-memory-work","samples":[{"reference_rss":9007199254740993,"candidate_heap":NaN}]}"#,
        )
        .unwrap();
        let row = Value::object([
            ("id", Value::str("sample")),
            (
                "timing",
                store_measurement(&output.0, 0, "timing", &timing).unwrap(),
            ),
            (
                "memory",
                store_measurement(&output.0, 0, "memory", &memory).unwrap(),
            ),
        ]);
        let checkpoint = Value::object([
            ("collection_status", Value::str("partial")),
            ("case_storage", Value::str("sha256-files")),
            ("cases", Value::Array(vec![row.clone()])),
        ]);
        let path = output.0.join("partial.json");
        save_checkpoint(&path, &checkpoint).unwrap();
        let text = fs::read_to_string(path).unwrap();
        assert!(!text.contains("samples"));
        assert!(!output.0.join("report.json").exists());
        let expanded = validation::expand_measurements(&output.0, &row).unwrap();
        assert_eq!(
            json::canonical(expanded.get("timing").unwrap()),
            json::canonical(&timing)
        );
        let memory = json::observations(&memory);
        assert_eq!(json::observations(expanded.get("memory").unwrap()), memory);
        let inline = output.0.join("inline");
        create_output(&inline).unwrap();
        stream_report(
            &inline,
            &Value::Object(Vec::new()),
            [Ok(expanded)].into_iter(),
            false,
        )
        .unwrap();
        assert!(
            fs::read_to_string(inline.join("report.json"))
                .unwrap()
                .contains(&memory)
        );
        assert!(store_measurement(&output.0, 0, "memory", &timing).is_err());
        fs::write(output.0.join("measurements/0000-memory.json"), b"{}\n").unwrap();
        assert!(
            validation::expand_measurements(&output.0, &row)
                .unwrap_err()
                .contains("raw measurement changed")
        );
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
            ("a", Value::str("complete")),
            (
                "cases",
                Value::Array(
                    raw.iter()
                        .map(|text| json::parse_strict(text).unwrap())
                        .collect(),
                ),
            ),
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
    fn parity_only_cannot_overwrite_a_measured_report() {
        let parent = Output::new();
        let output = parent.0.join("run");
        create_output(&output).unwrap();
        let path = output.join("partial.json");
        fs::write(&path, "measurement checkpoint").unwrap();
        let options = Options {
            output,
            ..Options::defaults(&parent.0)
        };
        assert!(
            run(&parent.0, &options)
                .unwrap_err()
                .contains("use a new output directory")
        );
        assert_eq!(fs::read_to_string(path).unwrap(), "measurement checkpoint");
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
    fn complete_json_keeps_existing_destination() {
        let output = Output::new();
        let destination = output.0.join("report.json");
        fs::write(&destination, b"existing grade\n").unwrap();
        assert!(publish_json(&destination, "{}\n").is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"existing grade\n");
        assert!(!output.0.join("report.json.tmp").exists());
    }

    #[test]
    fn case_output_keeps_existing_nested_traces() {
        let output = Output::new();
        let nested = output.0.join("nested/case");
        create_output(&nested).unwrap();
        let trace = nested.join("progressive-fold.json");
        fs::write(&trace, b"existing trace\n").unwrap();
        for id in ["nested/case", "../outside"] {
            let case = Value::object([("id", Value::str(id))]);
            assert!(reserve_case_outputs(&output.0, &[case]).is_err());
        }
        assert_eq!(fs::read(&trace).unwrap(), b"existing trace\n");
    }

    #[test]
    fn case_outputs_reserve_nested_cases_in_either_order() {
        for child in ["nested/child", "Nested/child"] {
            for ids in [[child, "nested"], ["nested", child]] {
                let output = Output::new();
                let cases = ids.map(|id| Value::object([("id", Value::str(id))]));
                reserve_case_outputs(&output.0, &cases).unwrap();
                assert!(output.0.join(child).is_dir());
            }
        }
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

    #[test]
    fn final_validation_failures_are_written_in_both_report_forms() {
        let output = Output::new();
        let mut proof = Value::object([
            ("passed", Value::Bool(true)),
            ("path", Value::str("controls/report.json")),
        ]);
        reject_controls(&mut proof, "functional control proof hash mismatch").unwrap();
        let header = Value::object([
            ("functional_controls", proof),
            (
                "summary",
                Value::object([("source_and_workers_unchanged", Value::Bool(false))]),
            ),
        ]);
        publish_measurements(&output.0, header, &[Value::int(1)], |case| Ok(case.clone())).unwrap();
        for name in ["collection.json", "report.json"] {
            let report = read_json(&output.0.join(name)).unwrap();
            assert_eq!(
                report
                    .get("summary")
                    .and_then(|summary| summary.get("source_and_workers_unchanged")),
                Some(&Value::Bool(false))
            );
            let proof = report.get("functional_controls").unwrap();
            assert_eq!(proof.get("passed"), Some(&Value::Bool(false)));
            assert_eq!(
                s(proof, "error"),
                Some("functional control proof hash mismatch")
            );
        }
    }
}
