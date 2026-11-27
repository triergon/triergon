//! The recorded counts of what the gates require and of every exception to them.
//!
//! Data alone loosens a gate when a contract's `gate` leaves `required`, when a
//! feature leaves the gate plan, when a list whose name starts with `open_` (a
//! contract's `open_cases`, `open_deviations` and `open_bounds`, a workload list's
//! `open_workloads`, a vector manifest's `open_cases`, the status page's
//! `open_cells`) gains an entry, or
//! when a crate's list of public features no gated comparison compares directly
//! ([`NOT_COMPARED`]) grows. `harness/gate-exceptions.json` records how many of each
//! the repository holds, and `cargo xtask check-design` counts them again: a change
//! that loosens anything changes that file too.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::json::Value;

/// The record, relative to the repository root.
pub const FILE: &str = "harness/gate-exceptions.json";

/// The public features of each library crate that no gated comparison compares
/// directly with its reference, relative to the repository root: by crate name, each
/// feature's short stable `name`, the public items it is reached through
/// (`entry_points`), how near it comes to a direct comparison (`degree`, one of
/// [`DEGREES`]) and what a comparison would need (`needs`). A crate listed with none
/// was reviewed and has none.
pub const NOT_COMPARED: &str = "harness/not-compared.json";

/// The degrees of [`NOT_COMPARED`], nearest a direct comparison first: compared only
/// as a step inside a larger compared operation; checked against reference output
/// recorded once in fixtures that no worker re-runs; compared with the reference by
/// an opt-in test or a command the gate does not run; unit tests only; no tests; no
/// reference implements it.
pub const DEGREES: [&str; 6] = [
    "step",
    "recorded",
    "outside-gate",
    "unit-tests",
    "untested",
    "no-reference",
];

/// The counts of `data`, the parsed JSON files under `harness/` by their path from
/// the repository root. An open entry counts once for each case it names, under
/// every gate target it is open on (both, when it names none).
pub fn counted(data: &[(String, Value)]) -> Value {
    let contracts: Vec<&Value> = data
        .iter()
        .filter(|(path, _)| is_contract(path))
        .map(|(_, contract)| contract)
        .collect();
    let with = |key: &str, value: &str| {
        let count = contracts
            .iter()
            .filter(|contract| contract.get(key).and_then(Value::as_str) == Some(value))
            .count();
        Value::int(count as i64)
    };
    let (dedicated, excluded) = crate::gate::dedicated_coverage();
    let mut open = Vec::new();
    for (path, document) in data.iter().filter(|(path, _)| !is_evidence(path)) {
        let Value::Object(members) = document else {
            continue;
        };
        let lists: Vec<(String, Value)> = members
            .iter()
            .filter(|(key, _)| key.starts_with("open_"))
            .filter_map(|(key, list)| Some((key.clone(), per_target(list.as_array()?)?)))
            .collect();
        if !lists.is_empty() {
            open.push((path.clone(), Value::Object(lists)));
        }
    }
    if let Some(counts) = elementary_reader_deviations(data) {
        open.push((
            "harness/codecs/reader-differences.json".into(),
            Value::object([("elementary_deviations", counts)]),
        ));
    }
    Value::object([
        ("features", Value::int(contracts.len() as i64)),
        ("required_by_check", with("gate", "required")),
        ("on_demand", with("gate", "on-demand")),
        ("diagnostic", with("gate", "diagnostic")),
        ("gated_by_report", with("runner", "cargo xtask report")),
        ("gated_by_another_command", Value::int(dedicated as i64)),
        ("not_gated", Value::int(excluded as i64)),
        ("open", Value::Object(open)),
        ("not_compared", not_compared(data)),
    ])
}

/// Count the frozen elementary packet comparisons by their manifest identity.
/// The same differences file also holds Ogg packet and seek projections.
fn elementary_reader_deviations(data: &[(String, Value)]) -> Option<Value> {
    fn identity(row: &Value) -> Option<(&str, &str)> {
        Some((row.get("name")?.as_str()?, row.get("sha256")?.as_str()?))
    }
    let rows = |path: &str, key: &str| {
        data.iter()
            .find(|(file, _)| file == path)
            .and_then(|(_, doc)| doc.get(key)?.as_array())
    };
    let elementary: BTreeSet<_> = rows("harness/codecs/reader-manifest.json", "vectors")?
        .iter()
        .filter(|row| {
            matches!(
                row.get("lane").and_then(Value::as_str),
                Some("reader-flac" | "reader-mp3" | "reader-aac" | "reader-ac3" | "reader-eac3")
            )
        })
        .filter_map(identity)
        .collect();
    let count = rows("harness/codecs/reader-differences.json", "cases")?
        .iter()
        .filter(|row| identity(row).is_some_and(|key| elementary.contains(&key)))
        .count();
    (count != 0).then(|| {
        Value::object(
            crate::gate::TARGETS
                .iter()
                .map(|&target| (target, Value::int(count as i64))),
        )
    })
}

/// How many features each crate of [`NOT_COMPARED`] lists.
fn not_compared(data: &[(String, Value)]) -> Value {
    let crates = data
        .iter()
        .find(|(path, _)| path == NOT_COMPARED)
        .and_then(|(_, list)| list.get("crates"));
    match crates {
        Some(Value::Object(crates)) => Value::object(crates.iter().map(|(name, features)| {
            let count = features.as_array().map_or(0, <[Value]>::len);
            (name.clone(), Value::int(count as i64))
        })),
        _ => Value::Object(Vec::new()),
    }
}

/// Why [`NOT_COMPARED`] is not a well-formed list: each crate a library package under
/// `crates/` names, listed once, each of its features once, by a lowercase name of
/// letters, digits and `-`, with at least one entry point, one of the [`DEGREES`] and
/// a one-line `needs`.
pub fn not_compared_errors(root: &Path, list: Option<&Value>) -> Vec<String> {
    let Some(list) = list else {
        return vec![format!("{NOT_COMPARED}: missing")];
    };
    let Some(Value::Object(crates)) = list.get("crates") else {
        return vec![format!("{NOT_COMPARED}: no crates object")];
    };
    let libraries = library_crates(root);
    let mut errors = Vec::new();
    let mut seen = BTreeSet::new();
    for (name, features) in crates {
        let at = format!("{NOT_COMPARED}: {name}");
        if !seen.insert(name) {
            errors.push(format!("{at}: listed twice"));
        }
        if !libraries.contains(name) {
            errors.push(format!("{at}: not a library crate under crates/"));
        }
        let Some(features) = features.as_array() else {
            errors.push(format!("{at}: not a list of features"));
            continue;
        };
        let mut names = BTreeSet::new();
        for feature in features {
            let text = |key: &str| feature.get(key).and_then(Value::as_str);
            let Some(feature_name) = text("name").filter(|n| is_name(n)) else {
                errors.push(format!(
                    "{at}: a feature's name is lowercase letters, digits and `-`"
                ));
                continue;
            };
            let at = format!("{at}: {feature_name}");
            if !names.insert(feature_name) {
                errors.push(format!("{at}: listed twice"));
            }
            let entry_points = feature.get("entry_points").and_then(Value::as_array);
            if entry_points.is_none_or(|points| {
                points.is_empty()
                    || points
                        .iter()
                        .any(|point| point.as_str().is_none_or(|p| p.trim().is_empty()))
            }) {
                errors.push(format!("{at}: entry_points names no public item"));
            }
            if text("degree").is_none_or(|degree| !DEGREES.contains(&degree)) {
                errors.push(format!("{at}: degree is one of {}", DEGREES.join(", ")));
            }
            if text("needs").is_none_or(|needs| needs.trim().is_empty() || needs.contains('\n')) {
                errors.push(format!(
                    "{at}: needs is one line saying what a comparison would need"
                ));
            }
            if let Value::Object(fields) = feature
                && let Some((key, _)) = fields.iter().find(|(key, _)| {
                    !["name", "entry_points", "degree", "needs"].contains(&key.as_str())
                })
            {
                errors.push(format!("{at}: no field {key}"));
            }
        }
    }
    errors
}

fn is_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}

/// The package names of the library crates under `crates/`: each directory with a
/// manifest and a `src/lib.rs`.
fn library_crates(root: &Path) -> BTreeSet<String> {
    let Ok(entries) = fs::read_dir(root.join("crates")) else {
        return BTreeSet::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|dir| dir.join("src/lib.rs").is_file())
        .filter_map(|dir| {
            let manifest = fs::read_to_string(dir.join("Cargo.toml")).ok()?;
            let manifest = crate::toml::parse(&manifest).ok()?;
            Some(manifest.get("package")?.get("name")?.as_str()?.to_owned())
        })
        .collect()
}

fn is_contract(path: &str) -> bool {
    path.strip_prefix("harness/features/")
        .and_then(|rest| rest.strip_suffix("/contract.json"))
        .is_some_and(|id| !id.contains('/'))
}

fn is_evidence(path: &str) -> bool {
    path.starts_with("harness/evidence/")
}

/// The entries of one open list by gate target, or `None` for an empty list.
fn per_target(entries: &[Value]) -> Option<Value> {
    let mut counts: BTreeMap<&str, i64> = BTreeMap::new();
    for entry in entries {
        let cases = entry
            .get("cases")
            .and_then(Value::as_array)
            .map_or(1, <[Value]>::len) as i64;
        let named: Vec<&str> = entry
            .get("targets")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let targets = if named.is_empty() {
            crate::gate::TARGETS.to_vec()
        } else {
            named
        };
        for target in targets {
            *counts.entry(target).or_default() += cases;
        }
    }
    (!counts.is_empty()).then(|| {
        Value::object(
            counts
                .into_iter()
                .map(|(target, count)| (target, Value::int(count))),
        )
    })
}

/// Every count that the record and `data` state differently.
pub fn check(recorded: Option<&Value>, data: &[(String, Value)]) -> Vec<String> {
    let Some(recorded) = recorded.and_then(|record| record.get("counts")) else {
        return vec![format!("{FILE}: no recorded counts")];
    };
    let (recorded, counted) = (leaves(recorded), leaves(&counted(data)));
    let mut names: Vec<&String> = recorded.keys().chain(counted.keys()).collect();
    names.sort_unstable();
    names.dedup();
    let shown = |count: Option<&String>| count.map_or("nothing", String::as_str).to_owned();
    names
        .into_iter()
        .filter(|name| recorded.get(*name) != counted.get(*name))
        .map(|name| {
            format!(
                "{FILE}: {name}: records {}, the repository holds {}; a change to what the \
                 gates require or except must record it",
                shown(recorded.get(name)),
                shown(counted.get(name))
            )
        })
        .collect()
}

/// The numbers of a nested object by their path of keys.
fn leaves(value: &Value) -> BTreeMap<String, String> {
    fn walk(value: &Value, path: &str, out: &mut BTreeMap<String, String>) {
        match value {
            Value::Object(members) => {
                for (key, member) in members {
                    let path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path} / {key}")
                    };
                    walk(member, &path, out);
                }
            }
            other => {
                out.insert(path.to_owned(), crate::json::canonical(other));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(value, "", &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn data(files: &[(&str, &str)]) -> Vec<(String, Value)> {
        files
            .iter()
            .map(|(path, text)| ((*path).to_owned(), parse(text).unwrap()))
            .collect()
    }

    #[test]
    fn every_loosening_edit_changes_a_count() {
        let arm = "thumbv7neon-unknown-linux-gnueabihf";
        let contract = "harness/features/a/contract.json";
        let base = data(&[
            (
                contract,
                r#"{"gate": "required", "runner": "cargo xtask report"}"#,
            ),
            ("harness/codecs/workloads.json", r#"{"open_workloads": []}"#),
        ]);
        let record = Value::object([("counts", counted(&base))]);
        assert_eq!(check(Some(&record), &base), Vec::<String>::new());
        for (path, loosened, changed) in [
            (
                contract,
                r#"{"gate": "on-demand", "runner": "cargo xtask report"}"#,
                "on_demand",
            ),
            (contract, r#"{"gate": "required"}"#, "gated_by_report"),
            (
                contract,
                r#"{"gate": "required", "runner": "cargo xtask report",
                    "open_cases": [{"id": "x", "requirement": "r"}]}"#,
                "open_cases",
            ),
            (
                contract,
                r#"{"gate": "required", "runner": "cargo xtask report", "open_bounds":
                    [{"bound": "heap", "cases": ["x", "y"], "targets": ["thumbv7neon-unknown-linux-gnueabihf"]}]}"#,
                "open_bounds",
            ),
            (
                "harness/codecs/workloads.json",
                r#"{"open_workloads": [{"id": "w", "cause": "c"}]}"#,
                "open_workloads",
            ),
        ] {
            let mut edited = base.clone();
            edited.retain(|(file, _)| file != path);
            edited.extend(data(&[(path, loosened)]));
            let errors = check(Some(&record), &edited);
            assert!(
                errors.iter().any(|e| e.contains(changed)),
                "{changed}: {errors:?}"
            );
        }
        // Two cases of one bound on one target count twice there and nowhere else.
        let bounds = data(&[(
            contract,
            r#"{"open_bounds": [{"bound": "heap", "cases": ["x", "y"], "targets": ["thumbv7neon-unknown-linux-gnueabihf"]}]}"#,
        )]);
        let open = counted(&bounds);
        let entry = open.get("open").and_then(|o| o.get(contract)).unwrap();
        assert_eq!(
            entry.get("open_bounds"),
            Some(&Value::object([(arm, Value::int(2))]))
        );
        // The evidence index is not a list of exceptions, and a missing record fails.
        let evidence = data(&[("harness/evidence/sets/s.json", r#"{"open_cases": ["x"]}"#)]);
        assert_eq!(
            counted(&evidence).get("open"),
            Some(&Value::Object(Vec::new()))
        );
        assert_eq!(check(None, &base).len(), 1);
    }

    #[test]
    fn an_uncompared_feature_changes_its_crates_count() {
        let list = |features: &str| {
            data(&[(
                NOT_COMPARED,
                &format!(r#"{{"crates": {{"triergon-core": [{features}], "triergon-ogg": []}}}}"#),
            )])
        };
        let feature = r#"{"name": "a", "entry_points": ["A"], "degree": "step", "needs": "a reference of A"}"#;
        let base = list(feature);
        let record = Value::object([("counts", counted(&base))]);
        assert_eq!(
            counted(&base).get("not_compared"),
            Some(&Value::object([
                ("triergon-core", Value::int(1)),
                ("triergon-ogg", Value::int(0))
            ]))
        );
        let grown = list(&format!(
            r#"{feature}, {{"name": "b", "entry_points": ["B"], "degree": "untested", "needs": "x"}}"#
        ));
        let errors = check(Some(&record), &grown);
        assert!(
            errors.len() == 1 && errors[0].contains("not_compared / triergon-core: records 1"),
            "{errors:?}"
        );
    }

    #[test]
    fn an_uncompared_feature_is_named_once_in_a_library_crate_with_its_degree_and_needs() {
        let root = crate::root();
        let well_formed = parse(
            r#"{"crates": {"triergon-core": [
                {"name": "a-1", "entry_points": ["Time"], "degree": "unit-tests",
                 "needs": "a reference of A"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            not_compared_errors(&root, Some(&well_formed)),
            Vec::<String>::new()
        );
        assert_eq!(not_compared_errors(&root, None).len(), 1);
        let errors = not_compared_errors(
            &root,
            Some(
                &parse(
                    r#"{"crates": {"codec-worker-x": [], "triergon-core": [
                    {"name": "A", "entry_points": ["X"], "degree": "step", "needs": "x"},
                    {"name": "b", "entry_points": [], "degree": "step", "needs": "x"},
                    {"name": "c", "entry_points": ["C"], "degree": "step", "needs": "two\nlines"},
                    {"name": "d", "entry_points": ["D"], "degree": "step", "needs": "x", "note": "x"},
                    {"name": "d", "entry_points": ["D"], "degree": "step", "needs": "x"},
                    {"name": "e", "entry_points": ["E"], "needs": "x"},
                    {"name": "f", "entry_points": ["F"], "degree": "close", "needs": "x"}]}}"#,
                )
                .unwrap(),
            ),
        );
        assert_eq!(
            errors,
            [
                "harness/not-compared.json: codec-worker-x: not a library crate under crates/",
                "harness/not-compared.json: triergon-core: a feature's name is lowercase letters, digits and `-`",
                "harness/not-compared.json: triergon-core: b: entry_points names no public item",
                "harness/not-compared.json: triergon-core: c: needs is one line saying what a comparison would need",
                "harness/not-compared.json: triergon-core: d: no field note",
                "harness/not-compared.json: triergon-core: d: listed twice",
                "harness/not-compared.json: triergon-core: e: degree is one of step, recorded, outside-gate, unit-tests, untested, no-reference",
                "harness/not-compared.json: triergon-core: f: degree is one of step, recorded, outside-gate, unit-tests, untested, no-reference",
            ]
        );
    }
}
