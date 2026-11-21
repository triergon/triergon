//! The recorded counts of what the gates require and of every exception to them.
//!
//! Data alone loosens a gate when a contract's `gate` leaves `required`, when a
//! feature leaves the gate plan, or when a list whose name starts with `open_` (a
//! contract's `open_cases` and `open_bounds`, a workload list's `open_workloads`, a
//! vector manifest's `open_cases`) gains an entry. `harness/gate-exceptions.json`
//! records how many of each the repository holds, and `cargo xtask check-design`
//! counts them again: a change that loosens anything changes that file too.

use std::collections::BTreeMap;

use crate::json::Value;

/// The record, relative to the repository root.
pub const FILE: &str = "harness/gate-exceptions.json";

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
    Value::object([
        ("features", Value::int(contracts.len() as i64)),
        ("required_by_check", with("gate", "required")),
        ("on_demand", with("gate", "on-demand")),
        ("diagnostic", with("gate", "diagnostic")),
        ("gated_by_report", with("runner", "cargo xtask report")),
        ("gated_by_another_command", Value::int(dedicated as i64)),
        ("not_gated", Value::int(excluded as i64)),
        ("open", Value::Object(open)),
    ])
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
}
