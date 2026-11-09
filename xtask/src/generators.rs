//! The generators whose features commit only their pins: fonts, shaping, rasterising
//! and ASS parsing. Each generates its features' case rows from committed inputs
//! and seeds (`harness/fonts/corpus.json`, authored texts and fixtures, seeded
//! random cases), and
//! takes each case's expected work from `harness/features/<id>/pins.json`: the
//! reference's folds, written only by [`Generator::record`] and never edited by hand. The
//! harness reads a generated manifest at `harness/features/<id>/corpus.json`; its
//! identity is the SHA-256 of the canonical JSON the generator writes. Each
//! feature's contract names when it runs ([`Gate`]).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::harness::features::{Gate, gate};
use crate::harness::manifest::canonical;
use crate::json::Value;

type R<T> = Result<T, String>;

/// A generator of generated-manifest features.
pub struct Generator {
    /// The `cargo xtask` command of the generator.
    pub command: &'static str,
    /// `cargo xtask COMMAND ARGS` for the generator's own subcommands: every one but
    /// `compare`, which runs [`Generator::reports`].
    pub main: fn(&Path, &[String]) -> R<()>,
    /// Candidate worker named by this generator's feature contracts.
    pub candidate: &'static str,
    /// The reference worker the pins are recorded from, by its identity record.
    pub reference: &'static str,
    /// The pin files' note.
    pub pin_note: &'static str,
    /// Where `COMMAND compare` reports and `COMMAND manifest` writes by default.
    pub report_directory: &'static str,
    pub manifest_directory: &'static str,
    /// The files case rows are generated from, besides the pins.
    pub row_sources: &'static [&'static str],
    /// The case rows of a feature, without expected work, in case order.
    pub rows: fn(&Path, &str) -> R<Vec<Value>>,
    /// The manifest's note of a feature.
    pub note: fn(&str) -> &'static str,
    /// The directory, relative to the root, that a feature's case files are in.
    pub directory: fn(&str) -> &'static str,
    /// Writes generated input files that differ from what is on disk.
    pub write_inputs: fn(&Path) -> R<()>,
    /// Checks of the generator's generator inputs beyond the pins.
    pub check_inputs: fn(&Path) -> Vec<String>,
    /// The cases `cargo xtask check` mutates for one fuzz round, by feature.
    pub smoke: &'static [(&'static str, &'static [&'static str])],
}

pub const GENERATORS: [&Generator; 4] = [
    &crate::fonts::GENERATOR,
    &crate::shape::GENERATOR,
    &crate::raster::GENERATOR,
    &crate::ass::GENERATOR,
];

/// No generated input files, or nothing more to check.
pub fn no_inputs(_root: &Path) -> R<()> {
    Ok(())
}

pub fn no_checks(_root: &Path) -> Vec<String> {
    Vec::new()
}

/// The generator selected by a registered feature's candidate worker.
pub fn for_contract(contract: &Value) -> Option<&'static Generator> {
    let candidate = contract.get("candidate").and_then(Value::as_str)?;
    GENERATORS
        .into_iter()
        .find(|generator| generator.candidate == candidate)
}

/// The generator for a `cargo xtask` command.
pub fn named(command: &str) -> Option<&'static Generator> {
    GENERATORS
        .into_iter()
        .find(|generator| generator.command == command)
}

fn pins_path(root: &Path, id: &str) -> PathBuf {
    root.join("harness/features").join(id).join("pins.json")
}

fn contract_path(root: &Path, id: &str) -> PathBuf {
    root.join("harness/features").join(id).join("contract.json")
}

/// The contract of feature `id`.
fn contract(root: &Path, id: &str) -> R<Value> {
    let path = contract_path(root, id);
    crate::checks::read_json(&path).map_err(|e| format!("{}: {e}", path.display()))
}

fn required_gate(root: &Path, id: &str) -> R<Gate> {
    gate(id, &contract(root, id)?)?.ok_or_else(|| format!("{id}: missing gate"))
}

/// The pins of `id`: case id to expected work.
fn pins(root: &Path, id: &str) -> R<Vec<(String, Value)>> {
    let path = pins_path(root, id);
    let document =
        crate::checks::read_json(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    match document.get("pins") {
        Some(Value::Object(pins)) => Ok(pins.clone()),
        _ => Err(format!("{}: no pins", path.display())),
    }
}

impl Generator {
    pub fn features(&self, root: &Path) -> R<Vec<String>> {
        Ok(crate::harness::features::discover(root)?
            .into_iter()
            .filter(|contract| contract.generated() && contract.candidate() == Some(self.candidate))
            .map(|contract| contract.id)
            .collect())
    }

    /// The feature's case and pin sources, in the order its contract gives.
    fn sources(&self, root: &Path, id: &str) -> R<Vec<String>> {
        let document = contract(root, id)?;
        if for_contract(&document).map(|generator| generator.candidate) != Some(self.candidate) {
            return Err(format!("{id}: not a {} feature", self.command));
        }
        let Some(named) = document.get("cases_from") else {
            return Ok(vec![id.to_owned()]);
        };
        let named = named
            .as_array()
            .filter(|named| !named.is_empty())
            .ok_or_else(|| format!("{id}: cases_from is not a list of features"))?;
        named
            .iter()
            .map(|name| {
                let source = name
                    .as_str()
                    .filter(|name| {
                        !name.is_empty()
                            && name
                                .bytes()
                                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    })
                    .ok_or_else(|| format!("{id}: invalid cases_from feature"))?;
                let document = contract(root, source)?;
                if source == id
                    || for_contract(&document).map(|generator| generator.candidate)
                        != Some(self.candidate)
                {
                    return Err(format!(
                        "{id}: cases_from: {source} is not another {} feature",
                        self.command
                    ));
                }
                if document.get("cases_from").is_some() {
                    return Err(format!(
                        "{id}: cases_from: {source} takes its cases from other features"
                    ));
                }
                if (self.directory)(source) != (self.directory)(id) {
                    return Err(format!(
                        "{id}: cases_from: {source} has another case directory"
                    ));
                }
                Ok(source.to_owned())
            })
            .collect()
    }

    /// The files feature `id`'s manifest is generated from besides its contract:
    /// the pins it takes and the generator's row sources, relative to the root. A
    /// feature whose contract cannot be read is listed with its own pins.
    pub fn inputs(&self, root: &Path, id: &str) -> Vec<String> {
        let pinned = self
            .sources(root, id)
            .unwrap_or_else(|_| vec![id.to_owned()]);
        let mut files: Vec<String> = pinned
            .into_iter()
            .map(|feature| format!("harness/features/{feature}/pins.json"))
            .collect();
        files.extend(self.row_sources.iter().map(|&source| source.to_owned()));
        files
    }

    /// The generated manifest of feature `id`: the rows of its sources with their
    /// pinned work (see [`Generator::sources`]). The case ids and the pinned ids must
    /// match exactly.
    pub fn manifest(&self, root: &Path, id: &str) -> R<Value> {
        let expected = if pinned_per_target(&contract(root, id)?) {
            EXPECTED_BY_TARGET
        } else {
            "expected"
        };
        let (mut rows, mut pinned) = (Vec::new(), Vec::new());
        for source in self.sources(root, id)? {
            rows.extend((self.rows)(root, &source)?);
            pinned.extend(pins(root, &source)?);
        }
        if rows.len() != pinned.len() {
            return Err(format!(
                "{id}: {} cases but {} pins; re-record them",
                rows.len(),
                pinned.len()
            ));
        }
        let mut cases = Vec::new();
        for (row, (pin, work)) in rows.into_iter().zip(pinned) {
            let case = row.get("id").and_then(Value::as_str).unwrap_or_default();
            if case != pin {
                return Err(format!(
                    "{id}: case {case} is pinned as {pin}; re-record them"
                ));
            }
            let Value::Object(mut fields) = row else {
                return Err(format!("{id}: a case row is not an object"));
            };
            fields.push((expected.into(), work));
            cases.push(Value::Object(fields));
        }
        Ok(Value::object([
            ("schema", Value::int(1)),
            ("note", Value::str((self.note)(id))),
            ("directory", Value::str((self.directory)(id))),
            ("cases", Value::Array(cases)),
        ]))
    }

    /// The pin file holding `pins` in case order.
    fn pin_file(&self, pins: Vec<(String, Value)>) -> Value {
        Value::object([
            ("schema", Value::int(1)),
            ("reference", Value::str(self.reference)),
            ("note", Value::str(self.pin_note)),
            ("pins", Value::Object(pins)),
        ])
    }

    /// Every registered feature has a gate and its generated manifest builds. A
    /// feature with its own cases has a pin file exactly as [`Generator::record`]
    /// writes it; one that takes another's (`cases_from`) is diagnostic and has
    /// none. Pins are always recorded with the `worker_operation` the harness
    /// runs, so no contract names another operation for them.
    pub fn check(&self, root: &Path) -> Vec<String> {
        let features = match self.features(root) {
            Ok(features) if features.is_empty() => return Vec::new(),
            Ok(features) => features,
            Err(error) => return vec![error],
        };
        let mut errors = (self.check_inputs)(root);
        for id in features {
            let feature = id.as_str();
            let gate = required_gate(root, feature);
            if let Err(error) = &gate {
                errors.push(error.clone());
            }
            if contract(root, feature).is_ok_and(|c| c.get("pin_operation").is_some()) {
                errors.push(format!(
                    "{feature}: pins are recorded with worker_operation; \
                     name the features whose pins apply in cases_from"
                ));
            }
            if let Err(error) = self.manifest(root, feature) {
                errors.push(error);
                continue;
            }
            errors.extend(target_pins(root, feature));
            let path = pins_path(root, feature);
            if self.sources(root, feature).is_ok_and(|s| s != [feature]) {
                if gate.is_ok_and(|gate| gate != Gate::Diagnostic) {
                    errors.push(format!(
                        "{feature}: it takes its cases and pins from cases_from, \
                         so its gate is diagnostic"
                    ));
                }
                if path.exists() {
                    errors.push(format!(
                        "{}: {feature} takes its pins from cases_from",
                        path.display()
                    ));
                }
                continue;
            }
            let committed = std::fs::read_to_string(&path).unwrap_or_default();
            match pins(root, feature).map(|pins| canonical(&self.pin_file(pins))) {
                Ok(text) if text == committed => {}
                _ => errors.push(format!(
                    "{}: not in the form `cargo xtask {} manifest --record` writes",
                    path.display(),
                    self.command
                )),
            }
        }
        errors
    }

    /// Re-records the pins of every feature with its own cases from the built
    /// reference, running each case's `worker_operation` fold. A feature pinned
    /// per target gets this host's entries, if the host's target is one of its
    /// `required_targets`, and keeps the other targets'.
    pub fn record(&self, root: &Path) -> R<()> {
        let identity = crate::checks::read_json(&root.join("target/harness/identity.json"))?;
        let reference = crate::harness::comparison::reference_named(&identity, self.reference)?;
        let worker = root.join(
            reference
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        let built = crate::sha256::hex(
            &std::fs::read(&worker).map_err(|e| format!("{}: {e}", worker.display()))?,
        );
        if reference.get("sha256").and_then(Value::as_str) != Some(built.as_str()) {
            return Err(format!(
                "the {} reference changed since build-workers; rebuild",
                self.reference
            ));
        }
        self.record_with(root, &worker, &crate::harness::report::host_target(), false)
    }

    /// Records with `worker` as the reference on `target`; with `per_target_only`
    /// only the features pinned per target, for a target where the workers'
    /// build identity is not recorded.
    fn record_with(
        &self,
        root: &Path,
        worker: &Path,
        target: &str,
        per_target_only: bool,
    ) -> R<()> {
        (self.write_inputs)(root)?;
        for id in self.features(root)? {
            let feature = id.as_str();
            let sources = self.sources(root, feature)?;
            if sources != [feature] {
                println!("{feature}: the pins of {}", sources.join(", "));
                continue;
            }
            let contract = contract(root, feature)?;
            let per_target = pinned_per_target(&contract);
            if per_target_only && !per_target {
                println!("{feature}: pinned for every target; recorded with the build identity");
                continue;
            }
            if per_target && !targets(&contract, "required_targets").contains(&target) {
                println!("{feature}: pinned per target, and {target} is not one of its targets");
                continue;
            }
            let prefix = contract
                .get("worker_operation")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("{feature}: no worker_operation"))?;
            let rows = (self.rows)(root, feature)?;
            let directory = root.join((self.directory)(feature));
            let folds = fold_all(worker, &format!("{prefix}fold"), &directory, &rows)?;
            // A feature pinned per target keeps the other targets' pins, so a
            // pin file it cannot read is an error, not an empty one.
            let earlier = if per_target && pins_path(root, feature).exists() {
                pins(root, feature)?
            } else {
                Vec::new()
            };
            let mut pinned = Vec::new();
            for (row, fold) in rows.iter().zip(folds) {
                let id = row.get("id").and_then(Value::as_str).unwrap_or_default();
                if !per_target {
                    pinned.push((id.to_owned(), fold));
                    continue;
                }
                let mut by_target: Vec<(String, Value)> =
                    match earlier.iter().find(|(case, _)| case == id) {
                        None => Vec::new(),
                        Some((_, Value::Object(pins)))
                            if pins
                                .iter()
                                .all(|(_, fold)| matches!(fold, Value::Object(_))) =>
                        {
                            pins.clone()
                        }
                        Some(_) => {
                            return Err(format!("{feature}: {id} is not pinned per target"));
                        }
                    };
                by_target.retain(|(t, _)| t != target);
                if !unpinned(&contract, id).iter().any(|(t, _)| *t == target) {
                    by_target.push((target.to_owned(), fold));
                }
                by_target.sort_by(|a, b| a.0.cmp(&b.0));
                pinned.push((id.to_owned(), Value::Object(by_target)));
            }
            crate::corpus::write_json(&pins_path(root, feature), &self.pin_file(pinned))?;
            let scope = if per_target {
                format!(" for {target}")
            } else {
                String::new()
            };
            println!("{feature}: {} pins recorded{scope}", rows.len());
        }
        Ok(())
    }

    /// Writes the generated inputs and every manifest to
    /// `directory/<id>/corpus.json` for reading.
    pub fn write(&self, root: &Path, directory: &Path) -> R<()> {
        (self.write_inputs)(root)?;
        for id in self.features(root)? {
            let feature = id.as_str();
            let path = directory.join(feature).join("corpus.json");
            let manifest = self.manifest(root, feature)?;
            let cases = manifest
                .get("cases")
                .and_then(Value::as_array)
                .map_or(0, <[_]>::len);
            crate::corpus::write_json(&path, &manifest)?;
            println!("{}: {cases} cases", path.display());
        }
        Ok(())
    }

    /// `COMMAND manifest [--record [--target TARGET --reference-worker PATH]]
    /// [--output DIR]`. With a target and a worker, only the features pinned per
    /// target are recorded, on a target where `build-workers` cannot run.
    pub fn manifest_command(&self, root: &Path, args: &[String], usage: &str) -> R<()> {
        let (mut record, mut output) = (false, root.join(self.manifest_directory));
        let (mut target, mut worker) = (None, None);
        let mut rest = args.iter();
        while let Some(flag) = rest.next() {
            match flag.as_str() {
                "--record" => record = true,
                "--output" => output = PathBuf::from(rest.next().ok_or(usage)?),
                "--target" => target = Some(rest.next().ok_or(usage)?.clone()),
                "--reference-worker" => worker = Some(PathBuf::from(rest.next().ok_or(usage)?)),
                _ => return Err(usage.into()),
            }
        }
        match (record, target, worker) {
            (true, None, None) => self.record(root)?,
            (true, Some(target), Some(worker)) => self.record_with(root, &worker, &target, true)?,
            (false, None, None) => {}
            _ => return Err(usage.into()),
        }
        self.write(root, &output)
    }

    /// The `cargo xtask report` arguments `COMMAND compare ARGS` stands for, one
    /// list per feature selected by its contract's gate: the required features,
    /// with `--all` also those run on demand, with `--diagnostic` the diagnostic
    /// ones only, and with both every feature, in either order.
    pub fn reports(&self, root: &Path, args: &[String]) -> R<Vec<Vec<String>>> {
        let (mut all, mut diagnostic) = (false, false);
        let mut output = PathBuf::from(self.report_directory);
        let mut options = Vec::new();
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            match (arg.as_str(), rest.clone().next()) {
                ("--all", _) => all = true,
                ("--diagnostic", _) => diagnostic = true,
                ("--output", Some(value)) => {
                    output = PathBuf::from(value);
                    rest.next();
                }
                _ => options.push(arg.clone()),
            }
        }
        let selected = |gate| match gate {
            Gate::Required => all || !diagnostic,
            Gate::OnDemand => all,
            Gate::Diagnostic => diagnostic,
        };
        let mut reports = Vec::new();
        for id in self.features(root)? {
            let feature = id.as_str();
            if !selected(required_gate(root, feature)?) {
                continue;
            }
            let path = |file: &str| format!("harness/features/{feature}/{file}.json");
            let mut list = vec![
                "--contract".to_owned(),
                path("contract"),
                "--manifest".to_owned(),
                path("corpus"),
                "--output".to_owned(),
                output.join(feature).display().to_string(),
            ];
            list.extend(options.iter().cloned());
            reports.push(list);
        }
        if reports.is_empty() && !diagnostic {
            return Err(format!("{}: no required feature contracts", self.command));
        }
        Ok(reports)
    }
}

/// The reference's `{packets, bytes, digest}` for every row, run in parallel.
fn fold_all(worker: &Path, mode: &str, directory: &Path, rows: &[Value]) -> R<Vec<Value>> {
    let results: Mutex<Vec<Option<R<Value>>>> = Mutex::new(vec![None; rows.len()]);
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    let Some(row) = rows.get(at) else { break };
                    let result = fold(worker, mode, directory, row);
                    results.lock().expect("results")[at] = Some(result);
                }
            });
        }
    });
    results
        .into_inner()
        .expect("results")
        .into_iter()
        .map(|r| r.unwrap_or_else(|| Err("not run".into())))
        .collect()
}

fn fold(worker: &Path, mode: &str, directory: &Path, row: &Value) -> R<Value> {
    let file = row.get("file").and_then(Value::as_str).unwrap_or_default();
    let arguments = row
        .get("arguments")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str);
    let output = Command::new(worker)
        .arg(mode)
        .arg(directory.join(file))
        .args(arguments)
        .output()
        .map_err(|e| format!("{}: {e}", worker.display()))?;
    let id = row.get("id").and_then(Value::as_str).unwrap_or_default();
    if !output.status.success() {
        return Err(format!("{id}: the reference failed: {}", output.status));
    }
    let data = crate::json::parse(&String::from_utf8_lossy(&output.stdout))
        .map_err(|e| format!("{id}: {e}"))?;
    let mut pin: Vec<(&str, Value)> = ["packets", "bytes", "digest"]
        .map(|key| (key, data.get(key).cloned().unwrap_or(Value::Null)))
        .into();
    if host_dependent(&data) {
        pin.push((HOST_DEPENDENT, Value::Bool(true)));
    }
    Ok(Value::object(pin))
}

/// The flag a reference adds to a fold, and its pin keeps, when the case's
/// result went through a step C leaves to the host; a contract declares it
/// when its feature holds such cases.
pub const HOST_DEPENDENT: &str = "host_dependent";

/// Whether a fold, pin or contract carries [`HOST_DEPENDENT`].
pub fn host_dependent(value: &Value) -> bool {
    value.get(HOST_DEPENDENT) == Some(&Value::Bool(true))
}

/// The manifest field of a case pinned per target: its pins keyed by target.
pub const EXPECTED_BY_TARGET: &str = "expected_by_target";

/// Whether a contract pins its cases per target (`pinned_per_target`).
pub fn pinned_per_target(contract: &Value) -> bool {
    contract.get("pinned_per_target") == Some(&Value::Bool(true))
}

/// The target triples a contract lists under `key`.
fn targets<'a>(contract: &'a Value, key: &str) -> Vec<&'a str> {
    contract
        .get(key)
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .collect()
}

/// The targets case `id` is not pinned for, each with its contract's reason
/// (`unpinned_targets`).
pub fn unpinned<'a>(contract: &'a Value, id: &str) -> Vec<(&'a str, &'a str)> {
    let entries = contract
        .get("unpinned_targets")
        .and_then(Value::as_array)
        .unwrap_or_default();
    entries
        .iter()
        .filter(|entry| entry.get("id").and_then(Value::as_str) == Some(id))
        .flat_map(|entry| {
            let reason = entry
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default();
            targets(entry, "targets")
                .into_iter()
                .map(move |t| (t, reason))
        })
        .collect()
}

/// A feature pinned per target has, for each case, a pin on each of its
/// `required_targets` but those `unpinned_targets` names, and holds only cases
/// whose result differs by target: unpinned somewhere, flagged by the reference
/// ([`HOST_DEPENDENT`]), or pinned differently. Every other feature has one pin
/// per case, which no reference flags.
fn target_pins(root: &Path, feature: &str) -> Vec<String> {
    let Ok(document) = contract(root, feature) else {
        return Vec::new();
    };
    let per_target = pinned_per_target(&document);
    let required = targets(&document, "required_targets");
    let pinned = pins(root, feature).unwrap_or_default();
    let mut errors = Vec::new();
    if !per_target && document.get("unpinned_targets").is_some() {
        errors.push(format!(
            "{feature}: unpinned_targets needs pinned_per_target"
        ));
    }
    for entry in document
        .get("unpinned_targets")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        let id = entry.get("id").and_then(Value::as_str).unwrap_or_default();
        let listed = targets(entry, "targets");
        if !pinned.iter().any(|(case, _)| case == id)
            || listed.is_empty()
            || listed.iter().any(|t| !required.contains(t))
            || entry
                .get("reason")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            errors.push(format!(
                "{feature}: unpinned_targets: {id} needs a case, required targets and a reason"
            ));
        }
    }
    // An open case scoped to targets is compared there: it names some of the
    // required targets, and in a feature pinned per target, ones it has pins for.
    for entry in document
        .get("open_cases")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        let Some(listed) = entry.get("targets") else {
            continue;
        };
        let id = entry.get("id").and_then(Value::as_str).unwrap_or_default();
        let scope = targets(entry, "targets");
        let skipped = unpinned(&document, id);
        let compared = |t: &&str| required.contains(t) && !skipped.iter().any(|(u, _)| u == t);
        let complete = listed
            .as_array()
            .is_some_and(|all| all.len() == scope.len());
        if scope.is_empty() || !complete || !scope.iter().all(compared) {
            errors.push(format!(
                "{feature}: open case {id}: its targets must be required targets it is pinned for"
            ));
        }
    }
    for (case, pin) in &pinned {
        if !per_target {
            if pin.get("packets").is_none() {
                errors.push(format!(
                    "{feature}: {case} is pinned per target; its contract is not"
                ));
            } else if host_dependent(pin) {
                errors.push(format!(
                    "{feature}: the reference flags {case}; pin it per target in a feature \
                     whose contract says pinned_per_target"
                ));
            }
            continue;
        }
        let Value::Object(by_target) = pin else {
            errors.push(format!("{feature}: {case} is not pinned per target"));
            continue;
        };
        let skipped: Vec<&str> = unpinned(&document, case).iter().map(|(t, _)| *t).collect();
        let mut wanted: Vec<&str> = required
            .iter()
            .copied()
            .filter(|t| !skipped.contains(t))
            .collect();
        wanted.sort_unstable();
        let have: Vec<&str> = by_target.iter().map(|(t, _)| t.as_str()).collect();
        if have != wanted {
            errors.push(format!(
                "{feature}: {case} is pinned for {} but needs {}; record it on each target",
                have.join(", "),
                wanted.join(", ")
            ));
        }
        if by_target
            .iter()
            .any(|(_, fold)| fold.get("packets").is_none())
        {
            errors.push(format!(
                "{feature}: {case} has a target pin that is not a fold"
            ));
        }
        let folds: Vec<&Value> = by_target.iter().map(|(_, fold)| fold).collect();
        let alike = folds
            .windows(2)
            .all(|pair| crate::harness::diff::equal(pair[0], pair[1]));
        if skipped.is_empty() && alike && !folds.iter().any(|fold| host_dependent(fold)) {
            errors.push(format!(
                "{feature}: {case} is the same on every target; \
                 it belongs in a feature pinned for every target"
            ));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The features `COMMAND compare ARGS` reports, in order.
    fn compared(generator: &Generator, args: &[&str]) -> Vec<String> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        generator
            .reports(root, &args)
            .unwrap()
            .iter()
            .map(|list| list[1].split('/').nth(2).unwrap().to_owned())
            .collect()
    }

    #[test]
    fn compare_selects_features_by_their_contracts_gate() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let [fonts, shape, raster, ass] = GENERATORS;
        assert_eq!(compared(fonts, &[]), ["font-hinting-none", "font-outlines"]);
        assert_eq!(compared(fonts, &["--all"]), fonts.features(root).unwrap());
        assert_eq!(
            compared(shape, &[]),
            [
                "shape-bidi",
                "shape-libass",
                "shape-simple",
                "shape-text",
                "shape-unicode"
            ]
        );
        assert_eq!(compared(shape, &["--all"]), shape.features(root).unwrap());
        assert!(compared(shape, &["--diagnostic"]).is_empty());
        let required = ["raster-bitmaps", "raster-glyphs", "raster-outlines"];
        assert_eq!(compared(raster, &[]), required);
        assert_eq!(
            compared(raster, &["--all"]),
            [
                "raster-bitmaps",
                "raster-glyphs",
                "raster-outlines",
                "raster-sweep"
            ]
        );
        assert_eq!(compared(raster, &["--diagnostic"]), ["raster-simd"]);
        for flags in [["--all", "--diagnostic"], ["--diagnostic", "--all"]] {
            assert_eq!(
                compared(raster, &flags),
                raster.features(root).unwrap(),
                "{flags:?}"
            );
        }
        assert_eq!(compared(ass, &[]), ass.features(root).unwrap());
        assert_eq!(compared(ass, &["--all"]), ass.features(root).unwrap());
    }

    #[test]
    fn a_feature_takes_its_cases_and_pins_from_cases_from() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let root = crate::walk::scratch("generator-cases-from");
        let features = root.join("harness/features");
        std::fs::create_dir_all(features.join("font-outlines")).unwrap();
        std::fs::create_dir_all(features.join("font-hinting-none")).unwrap();
        std::fs::create_dir_all(features.join("shape-text")).unwrap();
        std::fs::create_dir_all(root.join("harness/fonts")).unwrap();
        for file in [
            "harness/fonts/corpus.json",
            "harness/features/font-outlines/contract.json",
            "harness/features/font-outlines/pins.json",
            "harness/features/shape-text/contract.json",
        ] {
            std::fs::copy(source.join(file), root.join(file)).unwrap();
        }
        let generator = &crate::fonts::GENERATOR;
        let derived = features.join("font-hinting-none");
        let check = |contract: &str| {
            let contract = contract.replacen(
                '{',
                r#"{"id":"font-hinting-none","candidate":"font-worker","#,
                1,
            );
            std::fs::write(derived.join("contract.json"), contract).unwrap();
            generator.check(&root)
        };
        let diagnostic = r#"{"gate": "diagnostic", "cases_from": ["font-outlines"]}"#;
        assert_eq!(check(diagnostic), Vec::<String>::new());
        let cases = |id| generator.manifest(&root, id).unwrap().get("cases").cloned();
        assert_eq!(cases("font-hinting-none"), cases("font-outlines"));
        assert_eq!(
            generator.inputs(&root, "font-hinting-none")[0],
            "harness/features/font-outlines/pins.json"
        );
        for (contract, error) in [
            (
                r#"{"gate": "required", "cases_from": ["font-outlines"]}"#,
                "diagnostic",
            ),
            (r#"{"gate": "diagnostic", "cases_from": []}"#, "not a list"),
            (
                r#"{"gate": "diagnostic", "cases_from": ["font-hinting-none"]}"#,
                "not another",
            ),
            (
                r#"{"gate": "diagnostic", "cases_from": ["shape-text"]}"#,
                "not another",
            ),
            (
                r#"{"gate": "diagnostic", "cases_from": ["font-outlines"], "pin_operation": "font-"}"#,
                "worker_operation",
            ),
        ] {
            let found = check(contract);
            assert!(
                found.iter().any(|e| e.contains(error)),
                "{contract}: {found:?}"
            );
        }
        std::fs::copy(
            features.join("font-outlines/pins.json"),
            derived.join("pins.json"),
        )
        .unwrap();
        assert!(check(diagnostic)[0].contains("takes its pins from cases_from"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_contract_without_a_known_gate_fails_the_check() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let root = crate::walk::scratch("generator-gate");
        let feature = root.join("harness/features/font-outlines");
        std::fs::create_dir_all(&feature).unwrap();
        std::fs::create_dir_all(root.join("harness/fonts")).unwrap();
        for file in [
            "harness/fonts/corpus.json",
            "harness/features/font-outlines/pins.json",
        ] {
            std::fs::copy(source.join(file), root.join(file)).unwrap();
        }
        let contract =
            std::fs::read_to_string(source.join("harness/features/font-outlines/contract.json"))
                .unwrap();
        let generator = &crate::fonts::GENERATOR;
        for (gate, errors) in [("\"required\"", 0), ("\"sometimes\"", 1), ("true", 1)] {
            let edited = contract.replace("\"required\"", gate);
            std::fs::write(feature.join("contract.json"), edited).unwrap();
            let found = generator.check(&root);
            assert_eq!(found.len(), errors, "{gate}: {found:?}");
            assert!(found.iter().all(|e| e.contains("gate")), "{found:?}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compare_passes_report_options_through() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let args = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
        let reports = crate::shape::GENERATOR
            .reports(root, &args(&["--output", "out", "--memory"]))
            .unwrap();
        assert_eq!(
            reports[0],
            args(&[
                "--contract",
                "harness/features/shape-bidi/contract.json",
                "--manifest",
                "harness/features/shape-bidi/corpus.json",
                "--output",
                "out/shape-bidi",
                "--memory",
            ])
        );
    }
}
