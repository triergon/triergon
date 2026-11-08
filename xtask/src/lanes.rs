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
    /// `cargo xtask COMMAND ARGS` for the lane's own subcommands: every one but
    /// `compare`, which runs [`Lane::reports`].
    pub main: fn(&Path, &[String]) -> R<()>,
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
    /// The features whose rows and pins make feature `id`'s manifest: `id`
    /// itself, or the features its contract's `cases_from` names, in order. Each
    /// named feature is another of the lane's, with its own rows and pins in the
    /// same case directory.
    fn sources(&self, root: &Path, id: &str) -> R<Vec<&'static str>> {
        let document = contract(root, id)?;
        let Some(named) = document.get("cases_from") else {
            let own = self.features.iter().copied().find(|&feature| feature == id);
            return Ok(Vec::from_iter(own));
        };
        let named = named
            .as_array()
            .filter(|named| !named.is_empty())
            .ok_or_else(|| format!("{id}: cases_from is not a list of features"))?;
        named
            .iter()
            .map(|name| {
                let name = name.as_str().unwrap_or_default();
                let source = self
                    .features
                    .iter()
                    .copied()
                    .find(|&feature| feature == name && feature != id)
                    .ok_or_else(|| {
                        format!(
                            "{id}: cases_from: {name} is not another {} feature",
                            self.command
                        )
                    })?;
                if contract(root, source)?.get("cases_from").is_some() {
                    return Err(format!(
                        "{id}: cases_from: {source} takes its cases from other features"
                    ));
                }
                if (self.directory)(source) != (self.directory)(id) {
                    return Err(format!(
                        "{id}: cases_from: {source} has another case directory"
                    ));
                }
                Ok(source)
            })
            .collect()
    }

    /// The files feature `id`'s manifest is generated from besides its contract:
    /// the pins it takes and the lane's row sources, relative to the root. A
    /// feature whose contract cannot be read is listed with its own pins.
    pub fn inputs(&self, root: &Path, id: &str) -> Vec<String> {
        let pinned = self.sources(root, id).unwrap_or_default();
        let mut files: Vec<String> = if pinned.is_empty() { vec![id] } else { pinned }
            .into_iter()
            .map(|feature| format!("harness/features/{feature}/pins.json"))
            .collect();
        files.extend(self.row_sources.iter().map(|&source| source.to_owned()));
        files
    }

    /// The generated manifest of feature `id`: the rows of its sources with their
    /// pinned work (see [`Lane::sources`]). The case ids and the pinned ids must
    /// match exactly.
    pub fn manifest(&self, root: &Path, id: &str) -> R<Value> {
        let (mut rows, mut pinned) = (Vec::new(), Vec::new());
        for source in self.sources(root, id)? {
            rows.extend((self.rows)(root, source)?);
            pinned.extend(pins(root, source)?);
        }
        if rows.len() != pinned.len() {
            return Err(format!(
                "{id}: {} cases but {} pins; re-record them",
                rows.len(),
                pinned.len()
            ));
        }
        let mut cases = Vec::new();
        for (row, (pin, expected)) in rows.into_iter().zip(pinned) {
            let case = row.get("id").and_then(Value::as_str).unwrap_or_default();
            if case != pin {
                return Err(format!(
                    "{id}: case {case} is pinned as {pin}; re-record them"
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

    /// Every registered feature has a gate and its generated manifest builds. A
    /// feature with its own cases has a pin file exactly as [`Lane::record`]
    /// writes it; one that takes another's (`cases_from`) is diagnostic and has
    /// none. Pins are always recorded with the `worker_operation` the harness
    /// runs, so no contract names another operation for them.
    pub fn check(&self, root: &Path) -> Vec<String> {
        let mut errors = (self.check_inputs)(root);
        for &feature in self.features {
            if !contract_path(root, feature).is_file() {
                continue;
            }
            let gate = gate(root, feature);
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
    /// reference, running each case's `worker_operation` fold.
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
            let sources = self.sources(root, feature)?;
            if sources != [feature] {
                println!("{feature}: the pins of {}", sources.join(", "));
                continue;
            }
            let prefix = contract(root, feature)?
                .get("worker_operation")
                .and_then(Value::as_str)
                .map(str::to_owned)
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
        for &feature in self.features {
            if !selected(gate(root, feature)?) {
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

/// Every generated feature's contract, the pins it takes and the files its rows
/// come from, once each: the inputs a report's provenance covers.
pub fn provenance_inputs() -> Vec<String> {
    let root = crate::root();
    let mut names: Vec<String> = Vec::new();
    for lane in LANES {
        for &feature in lane.features {
            let contract = format!("harness/features/{feature}/contract.json");
            for name in std::iter::once(contract).chain(lane.inputs(&root, feature)) {
                if !names.contains(&name) {
                    names.push(name);
                }
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
        for flags in [["--all", "--diagnostic"], ["--diagnostic", "--all"]] {
            assert_eq!(compared(raster, &flags), raster.features, "{flags:?}");
        }
    }

    #[test]
    fn a_feature_takes_its_cases_and_pins_from_cases_from() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let root = crate::walk::scratch("lane-cases-from");
        let features = root.join("harness/features");
        std::fs::create_dir_all(features.join("font-outlines")).unwrap();
        std::fs::create_dir_all(features.join("font-hinting-none")).unwrap();
        std::fs::create_dir_all(root.join("harness/fonts")).unwrap();
        for file in [
            "harness/fonts/corpus.json",
            "harness/features/font-outlines/contract.json",
            "harness/features/font-outlines/pins.json",
        ] {
            std::fs::copy(source.join(file), root.join(file)).unwrap();
        }
        let lane = &crate::fonts::LANE;
        let derived = features.join("font-hinting-none");
        let check = |contract: &str| {
            std::fs::write(derived.join("contract.json"), contract).unwrap();
            lane.check(&root)
        };
        let diagnostic = r#"{"gate": "diagnostic", "cases_from": ["font-outlines"]}"#;
        assert_eq!(check(diagnostic), Vec::<String>::new());
        let cases = |id| lane.manifest(&root, id).unwrap().get("cases").cloned();
        assert_eq!(cases("font-hinting-none"), cases("font-outlines"));
        assert_eq!(
            lane.inputs(&root, "font-hinting-none")[0],
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
