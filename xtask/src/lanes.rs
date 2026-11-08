//! The lanes whose features commit only their pins: fonts, shaping and
//! rasterising. Each generates its features' case rows from committed inputs and
//! seeds (`harness/fonts/corpus.json`, authored texts, seeded random cases), and
//! takes each case's expected work from `harness/features/<id>/pins.json`: the
//! reference's folds, written only by [`Lane::record`] and never edited by hand. The
//! harness reads a generated manifest at `harness/features/<id>/corpus.json`; its
//! identity is the SHA-256 of the canonical JSON the generator writes. Each
//! feature's contract names when it runs ([`Gate`]).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::harness::manifest::canonical;
use crate::json::Value;

type R<T> = Result<T, String>;

/// When a feature runs: its contract's `gate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// `"required"`: `cargo xtask check` requires it.
    Required,
    /// `"on-demand"`: run by `COMMAND compare --all`.
    OnDemand,
    /// `"diagnostic"`: records where another reference departs from the gated
    /// one; run by `COMMAND compare --diagnostic`.
    Diagnostic,
}

impl Gate {
    /// The gate a contract's `gate` names.
    fn named(name: &str) -> Option<Self> {
        match name {
            "required" => Some(Self::Required),
            "on-demand" => Some(Self::OnDemand),
            "diagnostic" => Some(Self::Diagnostic),
            _ => None,
        }
    }
}

/// A lane of generated-manifest features.
pub struct Lane {
    /// The `cargo xtask` command of the lane.
    pub command: &'static str,
    /// Its features' ids; each feature's gate is in its contract.
    pub features: &'static [&'static str],
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

pub const LANES: [&Lane; 3] = [
    &crate::fonts::LANE,
    &crate::shape::LANE,
    &crate::raster::LANE,
];

/// No generated input files, or nothing more to check.
pub fn no_inputs(_root: &Path) -> R<()> {
    Ok(())
}

pub fn no_checks(_root: &Path) -> Vec<String> {
    Vec::new()
}

/// The lane of feature `id`.
pub fn lane_of(id: &str) -> Option<&'static Lane> {
    LANES.into_iter().find(|lane| lane.features.contains(&id))
}

/// The lane of a `cargo xtask` command.
pub fn lane_named(command: &str) -> Option<&'static Lane> {
    LANES.into_iter().find(|lane| lane.command == command)
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

/// When feature `id` runs: its contract's `gate`, one of `"required"`,
/// `"on-demand"` and `"diagnostic"`.
pub fn gate(root: &Path, id: &str) -> R<Gate> {
    contract(root, id)?
        .get("gate")
        .and_then(Value::as_str)
        .and_then(Gate::named)
        .ok_or_else(|| {
            format!("{id}: its contract's gate is not required, on-demand or diagnostic")
        })
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
        for &feature in self.features {
            if !contract_path(root, feature).is_file() {
                continue;
            }
            if let Err(error) = gate(root, feature) {
                errors.push(error);
            }
            if let Err(error) = self.manifest(root, feature) {
                errors.push(error);
                continue;
            }
            let path = pins_path(root, feature);
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
        for &feature in self.features {
            let contract = contract(root, feature)?;
            // A diagnostic feature's pins are the gated reference's folds
            // (`pin_operation`), so its report shows where its own reference departs.
            let prefix = contract
                .get("pin_operation")
                .or_else(|| contract.get("worker_operation"))
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{feature}: no worker_operation"))?;
            let rows = (self.rows)(root, feature)?;
            let directory = root.join((self.directory)(feature));
            let folds = fold_all(&worker, &format!("{prefix}fold"), &directory, &rows)?;
            let pins = rows
                .iter()
                .zip(folds)
                .map(|(row, fold)| {
                    let id = row.get("id").and_then(Value::as_str).unwrap_or_default();
                    (id.to_owned(), fold)
                })
                .collect();
            crate::corpus::write_json(&pins_path(root, feature), &self.pin_file(pins))?;
            println!("{feature}: {} pins recorded", rows.len());
        }
        Ok(())
    }

    /// Writes the generated inputs and every manifest to
    /// `directory/<id>/corpus.json` for reading.
    pub fn write(&self, root: &Path, directory: &Path) -> R<()> {
        (self.write_inputs)(root)?;
        for &feature in self.features {
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
    /// list per feature: the required features, with `--all` also those run on
    /// demand, with `--diagnostic` the diagnostic ones only. Each feature's gate
    /// is read from its contract.
    pub fn reports(&self, root: &Path, args: &[String]) -> R<Vec<Vec<String>>> {
        let (mut output, mut gates) = (PathBuf::from(self.report_directory), vec![Gate::Required]);
        let mut options = Vec::new();
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            match (arg.as_str(), rest.clone().next()) {
                ("--all", _) => gates.push(Gate::OnDemand),
                ("--diagnostic", _) => gates = vec![Gate::Diagnostic],
                ("--output", Some(value)) => {
                    output = PathBuf::from(value);
                    rest.next();
                }
                _ => options.push(arg.clone()),
            }
        }
        let mut reports = Vec::new();
        for &feature in self.features {
            if !gates.contains(&gate(root, feature)?) {
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
                names.push(format!("harness/features/{feature}/{file}.json"));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The features `COMMAND compare ARGS` reports, in order.
    fn compared(lane: &Lane, args: &[&str]) -> Vec<String> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        lane.reports(root, &args)
            .unwrap()
            .iter()
            .map(|list| list[1].split('/').nth(2).unwrap().to_owned())
            .collect()
    }

    #[test]
    fn compare_selects_features_by_their_contracts_gate() {
        let [fonts, shape, raster] = LANES;
        assert_eq!(compared(fonts, &[]), ["font-outlines", "font-hinting-none"]);
        assert_eq!(compared(fonts, &["--all"]), fonts.features);
        assert_eq!(
            compared(shape, &[]),
            [
                "shape-text",
                "shape-libass",
                "shape-bidi",
                "shape-simple",
                "shape-unicode"
            ]
        );
        assert_eq!(compared(shape, &["--all"]), shape.features);
        assert!(compared(shape, &["--diagnostic"]).is_empty());
        let required = ["raster-glyphs", "raster-outlines", "raster-bitmaps"];
        assert_eq!(compared(raster, &[]), required);
        assert_eq!(compared(raster, &["--all"]), &raster.features[..4]);
        assert_eq!(compared(raster, &["--diagnostic"]), ["raster-simd"]);
    }

    #[test]
    fn a_contract_without_a_known_gate_fails_the_check() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let root = crate::walk::scratch("lane-gate");
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
        let lane = &crate::fonts::LANE;
        for (gate, errors) in [("\"required\"", 0), ("\"sometimes\"", 1), ("true", 1)] {
            let edited = contract.replace("\"required\"", gate);
            std::fs::write(feature.join("contract.json"), edited).unwrap();
            let found = lane.check(&root);
            assert_eq!(found.len(), errors, "{gate}: {found:?}");
            assert!(found.iter().all(|e| e.contains("gate")), "{found:?}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compare_passes_report_options_through() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let args = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
        let reports = crate::shape::LANE
            .reports(root, &args(&["--output", "out", "--memory"]))
            .unwrap();
        assert_eq!(
            reports[0],
            args(&[
                "--contract",
                "harness/features/shape-text/contract.json",
                "--manifest",
                "harness/features/shape-text/corpus.json",
                "--output",
                "out/shape-text",
                "--memory",
            ])
        );
    }
}
