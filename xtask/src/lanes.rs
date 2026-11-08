//! The lanes whose features commit only their pins: fonts and shaping. Each
//! generates its features' case rows from committed inputs and
//! seeds (`harness/fonts/corpus.json`, authored texts, seeded random cases), and
//! takes each case's expected work from `harness/features/<id>/pins.json`: the
//! reference's folds, written only by [`Lane::record`] and never edited by hand. The
//! harness reads a generated manifest at `harness/features/<id>/corpus.json`; its
//! identity is the SHA-256 of the canonical JSON the generator writes.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::harness::manifest::canonical;
use crate::json::Value;

type R<T> = Result<T, String>;

/// One feature of a lane, and whether `cargo xtask check` requires it.
#[derive(Clone, Copy, Debug)]
pub struct Feature {
    pub id: &'static str,
    pub required: bool,
}

/// A lane of generated-manifest features.
pub struct Lane {
    /// The `cargo xtask` command of the lane.
    pub command: &'static str,
    pub features: &'static [Feature],
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
    /// Checks of the lane's generator inputs beyond the pins.
    pub check_inputs: fn(&Path) -> Vec<String>,
    /// The cases `cargo xtask check` mutates for one fuzz round, by feature.
    pub smoke: &'static [(&'static str, &'static [&'static str])],
}

pub const LANES: [&Lane; 2] = [&crate::fonts::LANE, &crate::shape::LANE];

/// No generated input files, or nothing more to check.
pub fn no_inputs(_root: &Path) -> R<()> {
    Ok(())
}

pub fn no_checks(_root: &Path) -> Vec<String> {
    Vec::new()
}

/// The lane of feature `id`.
pub fn lane_of(id: &str) -> Option<&'static Lane> {
    LANES
        .into_iter()
        .find(|lane| lane.features.iter().any(|f| f.id == id))
}

/// The lane of a `cargo xtask` command.
pub fn lane_named(command: &str) -> Option<&'static Lane> {
    LANES.into_iter().find(|lane| lane.command == command)
}

fn pins_path(root: &Path, id: &str) -> PathBuf {
    root.join("harness/features").join(id).join("pins.json")
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

impl Lane {
    /// The generated manifest of feature `id`: its rows with their pinned work.
    /// The case ids and the pinned ids must match exactly.
    pub fn manifest(&self, root: &Path, id: &str) -> R<Value> {
        let pins = pins(root, id)?;
        let rows = (self.rows)(root, id)?;
        if rows.len() != pins.len() {
            return Err(format!(
                "{id}: {} cases but {} pins; re-record them",
                rows.len(),
                pins.len()
            ));
        }
        let mut cases = Vec::new();
        for (row, (pinned, expected)) in rows.into_iter().zip(pins) {
            let case = row.get("id").and_then(Value::as_str).unwrap_or_default();
            if case != pinned {
                return Err(format!(
                    "{id}: case {case} is pinned as {pinned}; re-record them"
                ));
            }
            let Value::Object(mut fields) = row else {
                return Err(format!("{id}: a case row is not an object"));
            };
            fields.push(("expected".into(), expected));
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

    /// Every registered feature's generated manifest builds and its pin file is
    /// exactly what [`Lane::record`] writes for its cases.
    pub fn check(&self, root: &Path) -> Vec<String> {
        let mut errors = (self.check_inputs)(root);
        for feature in self.features {
            let registered = root.join("harness/features").join(feature.id);
            if !registered.join("contract.json").is_file() {
                continue;
            }
            if let Err(error) = self.manifest(root, feature.id) {
                errors.push(error);
                continue;
            }
            let path = pins_path(root, feature.id);
            let committed = std::fs::read_to_string(&path).unwrap_or_default();
            match pins(root, feature.id).map(|pins| canonical(&self.pin_file(pins))) {
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

    /// Re-records the pins of every feature from the built reference.
    pub fn record(&self, root: &Path) -> R<()> {
        (self.write_inputs)(root)?;
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
        for feature in self.features {
            let contract = root
                .join("harness/features")
                .join(feature.id)
                .join("contract.json");
            let contract = crate::checks::read_json(&contract)?;
            let prefix = contract
                .get("worker_operation")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{}: no worker_operation", feature.id))?;
            let rows = (self.rows)(root, feature.id)?;
            let directory = root.join((self.directory)(feature.id));
            let folds = fold_all(&worker, &format!("{prefix}fold"), &directory, &rows)?;
            let pins = rows
                .iter()
                .zip(folds)
                .map(|(row, fold)| {
                    let id = row.get("id").and_then(Value::as_str).unwrap_or_default();
                    (id.to_owned(), fold)
                })
                .collect();
            crate::corpus::write_json(&pins_path(root, feature.id), &self.pin_file(pins))?;
            println!("{}: {} pins recorded", feature.id, rows.len());
        }
        Ok(())
    }

    /// Writes the generated inputs and every manifest to
    /// `directory/<id>/corpus.json` for reading.
    pub fn write(&self, root: &Path, directory: &Path) -> R<()> {
        (self.write_inputs)(root)?;
        for feature in self.features {
            let path = directory.join(feature.id).join("corpus.json");
            let manifest = self.manifest(root, feature.id)?;
            let cases = manifest
                .get("cases")
                .and_then(Value::as_array)
                .map_or(0, <[_]>::len);
            crate::corpus::write_json(&path, &manifest)?;
            println!("{}: {cases} cases", path.display());
        }
        Ok(())
    }

    /// `COMMAND manifest [--record] [--output DIR]`.
    pub fn manifest_command(&self, root: &Path, args: &[String], usage: &str) -> R<()> {
        let (mut record, mut output) = (false, root.join(self.manifest_directory));
        let mut rest = args.iter();
        while let Some(flag) = rest.next() {
            match flag.as_str() {
                "--record" => record = true,
                "--output" => output = PathBuf::from(rest.next().ok_or(usage)?),
                _ => return Err(usage.into()),
            }
        }
        if record {
            self.record(root)?;
        }
        self.write(root, &output)
    }

    /// The `cargo xtask report` arguments `COMMAND compare ARGS` stands for, one
    /// list per feature: the required features, or with `--all` every feature;
    /// `None` for the other commands.
    pub fn reports(&self, args: &[String]) -> Option<Vec<Vec<String>>> {
        let (command, rest) = args.split_first()?;
        if command != "compare" {
            return None;
        }
        let (mut output, mut all) = (PathBuf::from(self.report_directory), false);
        let mut options = Vec::new();
        let mut rest = rest.iter();
        while let Some(arg) = rest.next() {
            match (arg.as_str(), rest.clone().next()) {
                ("--all", _) => all = true,
                ("--output", Some(value)) => {
                    output = PathBuf::from(value);
                    rest.next();
                }
                _ => options.push(arg.clone()),
            }
        }
        Some(
            self.features
                .iter()
                .filter(|feature| all || feature.required)
                .map(|feature| {
                    let feature = feature.id;
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
                    list
                })
                .collect(),
        )
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
    Ok(Value::object(["packets", "bytes", "digest"].map(|key| {
        (key, data.get(key).cloned().unwrap_or(Value::Null))
    })))
}

/// Every generated feature's contract, pins and the files its rows come from,
/// once each: the inputs a report's provenance covers.
pub fn provenance_inputs() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for lane in LANES {
        for feature in lane.features {
            for file in ["contract", "pins"] {
                names.push(format!("harness/features/{}/{file}.json", feature.id));
            }
        }
        for source in lane.row_sources {
            if !names.iter().any(|name| name == source) {
                names.push((*source).into());
            }
        }
    }
    names
}
