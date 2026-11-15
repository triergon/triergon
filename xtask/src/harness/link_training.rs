//! The training of the workers' link order: a corpus of its own, kept apart from
//! everything the gate reads, the operations the workers run on it, and the commands
//! that check the separation, record the trained orders and check the committed
//! ones.
//!
//! [`MANIFEST`] lists the training files by SHA-256 and the operations: a worker, its
//! command line, where `@NAME` stands for the training file `NAME`, and optionally
//! the gate targets it runs on. A worker with operations on a target links there in
//! its trained order; every other worker links as its toolchain does. `train` runs
//! each operation of each ordered worker under QEMU's user-mode emulator, which logs
//! every block of code when it first runs it, and writes the worker's trained order
//! ([`super::link_order::first_use`]) to `harness/link-order/TARGET/WORKER.txt`, with
//! the SHA-256 of the operations it ran ([`Training::digest`]) in its header.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::build::{Candidate, cargo_rustc, workers};
use super::gate_build;
use super::link_order;
use crate::json::Value;
use crate::sha256;

type R<T> = Result<T, String>;

/// The training manifest.
pub const MANIFEST: &str = "harness/link-order/training.json";

/// Where the trained orders are committed, one directory per gate target.
const ORDERS: &str = "harness/link-order";

/// One training operation: a worker's command line on training files.
struct Operation {
    worker: String,
    arguments: Vec<String>,
    /// The targets it runs on; every target when empty.
    targets: Vec<String>,
}

impl Operation {
    /// The training files the command line names.
    fn files(&self) -> impl Iterator<Item = &str> {
        self.arguments
            .iter()
            .filter_map(|argument| argument.strip_prefix('@'))
    }

    /// The command line, run in the training directory.
    fn command_line(&self) -> Vec<&str> {
        self.arguments
            .iter()
            .map(|argument| argument.strip_prefix('@').unwrap_or(argument))
            .collect()
    }

    fn runs_on(&self, target: &str) -> bool {
        self.targets.is_empty() || self.targets.iter().any(|own| own == target)
    }
}

/// The training manifest: the directory the training files are in, relative to the
/// repository root, the files with their SHA-256 and the operations, in order.
struct Training {
    directory: PathBuf,
    files: BTreeMap<String, String>,
    operations: Vec<Operation>,
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

impl Training {
    /// The operations of `worker` that run on `target`.
    fn operations_of<'a>(
        &'a self,
        worker: &'a str,
        target: &'a str,
    ) -> impl Iterator<Item = &'a Operation> {
        self.operations
            .iter()
            .filter(move |operation| operation.worker == worker && operation.runs_on(target))
    }

    /// Whether the manifest has operations for `worker` on `target`, where it then
    /// links in a trained order.
    fn orders(&self, worker: &str, target: &str) -> bool {
        self.operations_of(worker, target).next().is_some()
    }

    /// The SHA-256 of what a trained order of `worker` for `target` is recorded
    /// from: the command line of each of its operations there, in order, with each
    /// training file as its name and SHA-256.
    fn digest(&self, worker: &str, target: &str) -> String {
        let mut text = String::new();
        for operation in self.operations_of(worker, target) {
            for argument in &operation.arguments {
                match argument.strip_prefix('@') {
                    Some(file) => text.push_str(&format!("@{file}:{}", self.files[file])),
                    None => text.push_str(argument),
                }
                text.push('\t');
            }
            text.push('\n');
        }
        sha256::hex(text.as_bytes())
    }

    fn load(root: &Path) -> R<Self> {
        let path = root.join(MANIFEST);
        let document = crate::checks::read_json(&path)?;
        let fail = |problem: String| format!("{}: {problem}", path.display());
        let directory = document
            .get("directory")
            .and_then(Value::as_str)
            .ok_or_else(|| fail("no directory".into()))?;
        let mut files = BTreeMap::new();
        for file in document
            .get("files")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            let (Some(name), Some(hash)) = (
                file.get("file").and_then(Value::as_str),
                file.get("sha256").and_then(Value::as_str),
            ) else {
                return Err(fail("a file without its name or SHA-256".into()));
            };
            if name.is_empty() || name.contains('/') || name.starts_with('.') {
                return Err(fail(format!("{name}: not a plain file name")));
            }
            if files.insert(name.to_owned(), hash.to_owned()).is_some() {
                return Err(fail(format!("{name}: listed twice")));
            }
        }
        let known: BTreeSet<&str> = workers().map(|worker| worker.name).collect();
        let mut operations = Vec::new();
        for operation in document
            .get("operations")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            let operation = Operation {
                worker: operation
                    .get("worker")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                arguments: strings(operation.get("arguments")),
                targets: strings(operation.get("targets")),
            };
            if !known.contains(operation.worker.as_str()) {
                return Err(fail(format!("{}: not a worker", operation.worker)));
            }
            if operation.arguments.is_empty() {
                return Err(fail(format!(
                    "{}: an operation without arguments",
                    operation.worker
                )));
            }
            if let Some(file) = operation.files().find(|file| !files.contains_key(*file)) {
                return Err(fail(format!("{file}: not a training file")));
            }
            if let Some(target) = operation
                .targets
                .iter()
                .find(|target| !crate::gate::TARGETS.contains(&target.as_str()))
            {
                return Err(fail(format!("{target}: not a gate target")));
            }
            operations.push(operation);
        }
        if let Some(unused) = files.keys().find(|file| {
            !operations
                .iter()
                .any(|operation| operation.files().any(|own| own == file.as_str()))
        }) {
            return Err(fail(format!("{unused}: no operation reads it")));
        }
        Ok(Self {
            directory: root.join(directory),
            files,
            operations,
        })
    }
}

/// A committed trained order: its function names and the SHA-256 of its file.
pub struct Trained {
    pub names: Vec<String>,
    pub sha256: String,
}

/// The trained order `worker` links in on `target`: none when the manifest has no
/// operations for it there. Fails when the manifest and the committed orders
/// disagree: an order the manifest does not train, one missing, or one recorded from
/// other operations or training files than the manifest's.
pub fn trained_order(root: &Path, target: &str, worker: &str) -> R<Option<Trained>> {
    committed(root, &Training::load(root)?, target, worker)
}

/// The header line that names the operations a trained order was recorded from.
const DIGEST_LINE: &str = "# Recorded from operations ";

fn committed(root: &Path, training: &Training, target: &str, worker: &str) -> R<Option<Trained>> {
    let path = link_order::trained_path(root, target, worker);
    let text = match fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let retrain = "run cargo xtask link-order train on a gate host of the target";
    let text = match (text, training.orders(worker, target)) {
        (None, false) => return Ok(None),
        (Some(_), false) => {
            return Err(format!(
                "{}: {MANIFEST} has no operations of {worker} for {target}",
                path.display()
            ));
        }
        (None, true) => {
            return Err(format!(
                "{}: missing, though {MANIFEST} trains {worker} for {target}; {retrain}",
                path.display()
            ));
        }
        (Some(text), true) => text,
    };
    let digest = training.digest(worker, target);
    let recorded = text
        .lines()
        .find_map(|line| line.strip_prefix(DIGEST_LINE))
        .and_then(|rest| rest.split_whitespace().next());
    if recorded != Some(digest.as_str()) {
        return Err(format!(
            "{}: recorded from operations {}, but {MANIFEST} gives {worker}'s on {target} as \
             {digest}; {retrain}",
            path.display(),
            recorded.unwrap_or("not named")
        ));
    }
    let names =
        link_order::parse_trained(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(Some(Trained {
        names,
        sha256: sha256::hex(text.as_bytes()),
    }))
}

/// A committed trained order with its worker and target.
type Committed = (&'static str, &'static str, Trained);

/// The trained orders the manifest gives, by worker and target, after checking that
/// the committed ones agree with it ([`committed`]) and that `harness/link-order`
/// holds no order of an unknown worker or target.
fn all_committed(root: &Path, training: &Training) -> R<Vec<Committed>> {
    let known: BTreeSet<String> = workers()
        .map(|worker| format!("{}.txt", worker.name))
        .collect();
    let directory = root.join(ORDERS);
    for entry in fs::read_dir(&directory).map_err(|e| format!("{}: {e}", directory.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.path() == root.join(MANIFEST) {
            continue;
        }
        if !crate::gate::TARGETS.contains(&name.as_str()) {
            return Err(format!("{}: not a gate target", entry.path().display()));
        }
        for file in fs::read_dir(entry.path()).map_err(|e| format!("{name}: {e}"))? {
            let file = file.map_err(|e| e.to_string())?.file_name();
            if !known.contains(&*file.to_string_lossy()) {
                return Err(format!(
                    "{}: not the order of a worker",
                    entry.path().join(file).display()
                ));
            }
        }
    }
    let mut orders = Vec::new();
    for worker in workers() {
        for target in crate::gate::TARGETS {
            if let Some(trained) = committed(root, training, target, worker.name)? {
                orders.push((worker.name, target, trained));
            }
        }
    }
    Ok(orders)
}

/// Why a build identity's workers are not linked as the manifest and the committed
/// trained orders give for the worker target: a worker the manifest orders recorded
/// without its order, one it does not order recorded with one, an order other than
/// the committed one, or one the binary held too little of. Empty when they agree;
/// where the policy leaves linking to the toolchain, every worker must record none.
pub fn link_order_refusals(root: &Path, identity: &Value) -> Vec<String> {
    let target = match gate_build::worker_target() {
        Ok(target) => target,
        Err(error) => return vec![error],
    };
    let training = match Training::load(root) {
        Ok(training) => training,
        Err(error) => return vec![error],
    };
    let ordered = gate_build::orders_links(target);
    let mut records = vec![(
        "media-worker".to_owned(),
        identity.get("candidate_link_order"),
    )];
    if let Some(Value::Object(candidates)) = identity.get("additional_candidates") {
        records.extend(
            candidates
                .iter()
                .map(|(name, record)| (name.clone(), record.get("link_order"))),
        );
    }
    let mut reasons = Vec::new();
    for (worker, record) in records {
        let expected = if ordered {
            match committed(root, &training, target, &worker) {
                Ok(trained) => trained,
                Err(error) => {
                    reasons.push(error);
                    continue;
                }
            }
        } else {
            None
        };
        match (record, expected) {
            (None, _) => reasons.push(format!("{worker} records no link order")),
            (Some(Value::Null), None) => {}
            (Some(Value::Null), Some(_)) => {
                reasons.push(format!("{worker} links without its trained order"));
            }
            (Some(_), None) => reasons.push(format!(
                "{worker} links in a trained order {MANIFEST} does not give for {target}"
            )),
            (Some(record), Some(trained)) => {
                if record.get("trained_sha256").and_then(Value::as_str)
                    != Some(trained.sha256.as_str())
                {
                    reasons.push(format!("{worker} links in another trained order"));
                }
                let count = |key: &str| {
                    record
                        .get(key)
                        .and_then(crate::python::int)
                        .and_then(|count| usize::try_from(count).ok())
                };
                match (count("held"), count("trained")) {
                    (Some(held), Some(names))
                        if names == trained.names.len()
                            && link_order::holds_enough(held, names) => {}
                    (Some(held), Some(names)) => reasons.push(format!(
                        "{worker} holds {held} of the {names} functions of its trained order"
                    )),
                    _ => reasons.push(format!("{worker} records no trained order coverage")),
                }
            }
        }
    }
    reasons
}

/// Checks that the training corpus in `directory` (the manifest's when none) is
/// apart from the gate by exact bytes: each training file has its listed SHA-256 and
/// none has the SHA-256 of a file a gate item reads on either target or of a
/// committed file ([`crate::gate::input_hashes`]). Needs a prepared host, where every
/// gate input is present.
pub fn separate(root: &Path, directory: Option<&Path>) -> R<()> {
    separate_corpus(root, &Training::load(root)?, directory)
}

fn separate_corpus(root: &Path, training: &Training, directory: Option<&Path>) -> R<()> {
    let directory = directory.unwrap_or(&training.directory);
    for (name, pin) in &training.files {
        let path = directory.join(name);
        let hash = sha256::file_hex(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if hash != *pin {
            return Err(format!(
                "{}: SHA-256 {hash}, listed as {pin}",
                path.display()
            ));
        }
    }
    let gate = crate::gate::input_hashes(root)?;
    apart(training, &gate)?;
    println!(
        "{} training files and {} operations, none the same bytes as any of {} gate \
         inputs and committed files",
        training.files.len(),
        training.operations.len(),
        gate.len()
    );
    Ok(())
}

/// Fails when a training file's pin is the SHA-256 of one of the gate's files.
fn apart(training: &Training, gate: &BTreeMap<String, String>) -> R<()> {
    let problems: Vec<String> = training
        .files
        .iter()
        .filter_map(|(name, hash)| {
            gate.get(hash)
                .map(|path| format!("{name}: the same file as the gate's {path}"))
        })
        .collect();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}

/// The user-mode emulator for a worker target and the CPU it emulates: the gate
/// host's, so code that depends on CPU features takes the same path.
fn emulator(target: &str) -> R<(&'static str, &'static str)> {
    if target == gate_build::ARM32_TARGET {
        Ok(("qemu-arm", "cortex-a15"))
    } else if target.starts_with("aarch64-") {
        Ok(("qemu-aarch64", "neoverse-n1"))
    } else {
        Err(format!("{target}: no trained link order"))
    }
}

/// The first lines of a trained order.
fn header(worker: &str, target: &str, digest: &str) -> String {
    format!(
        "# The functions the link-order training runs in {worker} on {target}, in first-use order.\n\
         {DIGEST_LINE}{digest} of {MANIFEST}.\n\
         # Written by `cargo xtask link-order train`; see docs/quality.md.\n"
    )
}

/// Runs the training operations of each built ordered worker (only `only` when
/// given) under the emulator and writes its trained order for the worker target.
/// The corpus is checked with [`separate`] first.
pub fn train(root: &Path, directory: Option<&Path>, only: Option<&str>) -> R<()> {
    let training = Training::load(root)?;
    separate_corpus(root, &training, directory)?;
    let directory = directory.unwrap_or(&training.directory).to_path_buf();
    let target = gate_build::worker_target()?;
    let (emulator, cpu) = emulator(target)?;
    let release = root.join(gate_build::release_dir()?);
    let logs = root.join("target/harness/link-order/training");
    for worker in workers() {
        if only.is_some_and(|only| only != worker.name) || !training.orders(worker.name, target) {
            continue;
        }
        let binary = release.join(worker.name);
        let directory_logs = logs.join(worker.name);
        if directory_logs.exists() {
            fs::remove_dir_all(&directory_logs)
                .map_err(|e| format!("{}: {e}", directory_logs.display()))?;
        }
        fs::create_dir_all(&directory_logs)
            .map_err(|e| format!("{}: {e}", directory_logs.display()))?;
        let mut runs = Vec::new();
        for operation in training.operations_of(worker.name, target) {
            let log = directory_logs.join(format!("{}.log", runs.len()));
            let status = Command::new(emulator)
                .args(["-cpu", cpu, "-d", "page,in_asm", "-D"])
                .arg(&log)
                .arg(&binary)
                .args(operation.command_line())
                .current_dir(&directory)
                .stdout(Stdio::null())
                .status()
                .map_err(|e| format!("{emulator}: {e}"))?;
            if !status.success() {
                return Err(format!(
                    "{} {}: {status}",
                    worker.name,
                    operation.arguments.join(" ")
                ));
            }
            runs.push(log);
        }
        let names = link_order::first_use(&binary, &runs)?;
        let path = link_order::trained_path(root, target, worker.name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let mut text = header(worker.name, target, &training.digest(worker.name, target));
        for name in &names {
            text.push_str(name);
            text.push('\n');
        }
        fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        println!(
            "{}: {} functions from {} operations",
            path.strip_prefix(root).unwrap_or(&path).display(),
            names.len(),
            runs.len()
        );
    }
    Ok(())
}

/// Checks the training manifest and the committed trained orders: the orders agree
/// with the manifest ([`all_committed`]), no training file's pin is the SHA-256 of a
/// file the gate reads, as far as it is known without a prepared host, and, where the
/// policy orders the link, each order lays out the worker built for this host: the
/// worker holds at least [`link_order::MIN_HELD_PERCENT`] of its names, and linking
/// it with its ordering file and again gives the same file.
pub fn check(root: &Path) -> R<()> {
    let training = Training::load(root)?;
    let trained = all_committed(root, &training)?;
    let (gate, unknown) = crate::gate::input_pins(root)?;
    apart(&training, &gate)?;
    println!(
        "link-order check: {} training files apart from {} gate inputs and committed files \
         ({unknown} more are only known on a prepared host)",
        training.files.len(),
        gate.len()
    );
    if gate_build::ordered_link_arguments(root, None)?.is_empty() {
        println!(
            "link-order check: {} trained orders agree with {MANIFEST}; this host links \
             without them",
            trained.len()
        );
        return Ok(());
    }
    let directory = root.join("target/link-order-check");
    fs::create_dir_all(&directory).map_err(|e| format!("{}: {e}", directory.display()))?;
    for worker in workers() {
        if !trained.iter().any(|(name, ..)| *name == worker.name) {
            continue;
        }
        let binary = link(root, worker, None)?;
        for (_, target, order) in trained.iter().filter(|(name, ..)| *name == worker.name) {
            let ordering = link_order::symbol_order(&binary, &order.names)?;
            println!(
                "link-order check: {} holds {} of the {} functions of its {target} order",
                worker.name,
                ordering.held,
                order.names.len()
            );
            if !link_order::holds_enough(ordering.held, order.names.len()) {
                return Err(format!(
                    "{} holds {} of the {} functions of its {target} order; retrain it",
                    worker.name,
                    ordering.held,
                    order.names.len()
                ));
            }
            let hash = sha256::hex(ordering.text.as_bytes());
            let path = directory.join(format!("{}-{}.txt", worker.name, &hash[..16]));
            fs::write(&path, &ordering.text).map_err(|e| format!("{}: {e}", path.display()))?;
            link(root, worker, Some(&path))?;
            if link_order::symbol_order(&binary, &order.names)?.text != ordering.text {
                return Err(format!(
                    "{} with the {target} order: the ordered link holds other functions",
                    worker.name
                ));
            }
        }
    }
    println!(
        "link-order check: {} trained orders agree with {MANIFEST} and link",
        trained.len()
    );
    Ok(())
}

/// Builds `worker` for this host in the dev profile, linked by the toolchain's `lld`
/// in `order`, and returns the binary.
fn link(root: &Path, worker: &Candidate, order: Option<&Path>) -> R<PathBuf> {
    let target_dir = root.join("target/link-order-check");
    let command = cargo_rustc(worker, &target_dir, false)?;
    let status = Command::new(&command[0])
        .args(&command[1..])
        .args(gate_build::worker_link_arguments()?)
        .args(gate_build::ordered_link_arguments(root, order)?)
        .env("CARGO_INCREMENTAL", "0")
        .current_dir(root)
        .status()
        .map_err(|e| format!("cargo: {e}"))?;
    if !status.success() {
        return Err(format!("{}: link failed: {status}", worker.name));
    }
    let debug = target_dir.join("debug");
    Ok(match worker.example {
        Some(example) => debug.join("examples").join(example),
        None => debug.join(worker.name),
    })
}

#[cfg(test)]
mod tests {
    use super::{Training, all_committed};
    use std::path::Path;

    #[test]
    fn the_committed_orders_agree_with_the_training_manifest() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let training = Training::load(root).unwrap();
        assert!(!training.operations.is_empty());
        assert!(!all_committed(root, &training).unwrap().is_empty());
        let mp4_range = training
            .operations
            .iter()
            .find(|operation| operation.arguments[0] == "mp4-range-memory")
            .unwrap();
        assert_eq!(mp4_range.command_line()[1], "avc-aac.mp4");
    }

    #[test]
    fn the_digest_follows_the_operations_and_their_files() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut training = Training::load(root).unwrap();
        let target = "aarch64-unknown-linux-gnu";
        let before = training.digest("media-worker", target);
        let file = training
            .operations_of("media-worker", target)
            .find_map(|operation| operation.files().next())
            .unwrap()
            .to_owned();
        training.files.insert(file, "0".repeat(64));
        assert_ne!(training.digest("media-worker", target), before);
        let pinned = training.digest("media-worker", target);
        training
            .operations
            .iter_mut()
            .find(|operation| operation.worker == "media-worker")
            .unwrap()
            .arguments
            .push("--extra".into());
        assert_ne!(training.digest("media-worker", target), pinned);
    }
}
