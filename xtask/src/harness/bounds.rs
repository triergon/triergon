//! The per-case bound verdict of a step that grades runtime or memory: a case whose
//! runtime, heap or total-RSS bound fails fails the step, unless the contract's
//! `open_bounds` lists that bound for the case on the measured target.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

use super::policy::Policy;
use crate::json::{self, Value};

/// A bound the gate grades each case on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bound {
    /// The runtime ratio's upper 95% bound, at most the ceiling.
    Runtime,
    /// The peak heap ratio's upper 95% bound, below its ceiling.
    Heap,
    /// The total RSS ratio's upper 95% bound, below its ceiling.
    TotalRss,
}

impl Bound {
    const ALL: [Bound; 3] = [Bound::Runtime, Bound::Heap, Bound::TotalRss];

    fn parse(name: &str) -> Option<Bound> {
        match name {
            "runtime" => Some(Bound::Runtime),
            "heap" => Some(Bound::Heap),
            "total_rss" => Some(Bound::TotalRss),
            _ => None,
        }
    }
}

impl fmt::Display for Bound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Bound::Runtime => "runtime",
            Bound::Heap => "heap",
            Bound::TotalRss => "total RSS",
        })
    }
}

/// One case's graded bounds: each upper 95% bound the row records, infinite where its
/// measurement is there but not valid (not `measured`, not `qualified`), and `None`
/// where the row has no such measurement, as a collection of the other kind has none.
#[derive(Debug, PartialEq)]
pub struct CaseBounds {
    pub id: String,
    values: [Option<f64>; 3],
}

/// The bounds a case row records, or `None` for a row the bounds do not grade: an
/// open case, a refusal, or an ISO BMFF alias that repeats the case it was sampled as.
pub fn case_bounds(row: &Value) -> Option<CaseBounds> {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    if row.get("open_case").is_some_and(Value::truthy)
        || text(row, "outcome").as_deref() == Some("refusal")
        || row.get("sampled_as").is_some()
    {
        return None;
    }
    let id = text(row, "id")?;
    let present = |key: &str| row.get(key).filter(|value| **value != Value::Null);
    let number = |value: Option<&Value>| value.and_then(Value::as_f64).filter(|v| v.is_finite());
    let runtime = present("timing").map(|timing| {
        let upper = number(timing.get("ratio").and_then(|ratio| ratio.get("upper_95")));
        match (text(timing, "status").as_deref(), upper) {
            (Some("measured"), Some(upper)) => upper,
            _ => f64::INFINITY,
        }
    });
    let memory = present("memory").map(|memory| {
        let qualified = text(memory, "status").as_deref() == Some("qualified");
        let ratio = |key: &str| match number(memory.get(key)) {
            Some(value) if qualified => value,
            _ => f64::INFINITY,
        };
        (
            ratio("heap_ratio_upper_95"),
            ratio("total_rss_ratio_upper_95"),
        )
    });
    Some(CaseBounds {
        id,
        values: [runtime, memory.map(|m| m.0), memory.map(|m| m.1)],
    })
}

/// The contract's `open_bounds` on `target`: each entry names a `bound` (`runtime`,
/// `heap` or `total_rss`), the `cases` whose bound fails there today and the measured
/// `cause`; an entry with `targets` is open on those of the gate's targets only. Every
/// case must be a case of the manifest, and a bound of a case is listed once for each
/// target.
pub fn open_bounds(
    contract: &Value,
    manifest: &Value,
    target: &str,
) -> Result<BTreeSet<(Bound, String)>, String> {
    let array = |value: &Value, key: &str| -> Vec<Value> {
        value
            .get(key)
            .and_then(Value::as_array)
            .unwrap_or_default()
            .to_vec()
    };
    let cases: BTreeSet<String> = array(manifest, "cases")
        .iter()
        .filter_map(|case| case.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let mut listed = BTreeSet::new();
    let mut open = BTreeSet::new();
    for entry in array(contract, "open_bounds") {
        let text = |key: &str| entry.get(key).and_then(Value::as_str);
        let Some(bound) = text("bound").and_then(Bound::parse) else {
            return Err("an open bound needs a bound: runtime, heap or total_rss".into());
        };
        if text("cause").is_none_or(|cause| cause.trim().is_empty()) {
            return Err(format!("open {bound} bound: its cause is empty"));
        }
        let ids = array(&entry, "cases");
        if ids.is_empty() {
            return Err(format!("open {bound} bound: it lists no cases"));
        }
        let targets: Vec<&str> = match entry.get("targets") {
            None => crate::gate::TARGETS.to_vec(),
            Some(scope) => {
                let names: Vec<Option<&str>> = scope
                    .as_array()
                    .unwrap_or_default()
                    .iter()
                    .map(Value::as_str)
                    .collect();
                let known = |t: Option<&str>| t.is_some_and(|t| crate::gate::TARGETS.contains(&t));
                if names.is_empty() || !names.iter().copied().all(known) {
                    return Err(format!(
                        "open {bound} bound: its targets must be some of the gate's targets"
                    ));
                }
                names.into_iter().flatten().collect()
            }
        };
        for id in &ids {
            let Some(id) = id.as_str() else {
                return Err(format!("open {bound} bound: a case is not a name"));
            };
            if !cases.contains(id) {
                return Err(format!(
                    "open {bound} bound of {id}: not a case of the manifest"
                ));
            }
            for on in &targets {
                if !listed.insert((bound, id.to_owned(), (*on).to_owned())) {
                    return Err(format!("open {bound} bound of {id}: listed twice on {on}"));
                }
            }
            if targets.contains(&target) {
                open.insert((bound, id.to_owned()));
            }
        }
    }
    Ok(open)
}

/// One line per failing bound of `cases`, telling those the contract lists as open from
/// the others, and one per listed open bound that now holds, to be removed from the
/// list. Returns the lines and whether the step fails: on a failing bound that is not
/// listed as open.
pub fn verdict(
    label: &str,
    cases: &[CaseBounds],
    open: &BTreeSet<(Bound, String)>,
    policy: Policy,
) -> (Vec<String>, bool) {
    let mut lines = Vec::new();
    let mut failed = false;
    for case in cases {
        for (bound, value) in Bound::ALL.into_iter().zip(case.values) {
            let Some(value) = value else { continue };
            let holds = match bound {
                Bound::Runtime => value > 0.0 && value <= policy.runtime.ceiling,
                Bound::Heap => value < policy.memory.heap,
                Bound::TotalRss => value > 0.0 && value < policy.memory.total_rss,
            };
            let listed = open.contains(&(bound, case.id.clone()));
            let shown = if value.is_finite() {
                format!("{value:.4}")
            } else {
                "an invalid measurement".to_owned()
            };
            match (holds, listed) {
                (true, false) => {}
                (true, true) => lines.push(format!(
                    "{label}: {}: {bound} bound holds at {shown}; remove it from open_bounds",
                    case.id
                )),
                (false, true) => lines.push(format!(
                    "{label}: {}: {bound} bound fails at {shown}, open as listed",
                    case.id
                )),
                (false, false) => {
                    failed = true;
                    lines.push(format!(
                        "{label}: {}: {bound} bound fails at {shown}",
                        case.id
                    ));
                }
            }
        }
    }
    (lines, failed)
}

/// The bound verdict of the collection a report command wrote to `output`, for a
/// command whose collections have no grade to join them: reads its `report.json`,
/// prints the lines and returns whether the step fails.
pub fn collection_verdict(root: &Path, options: &super::report::Options) -> Result<bool, String> {
    let read = json::read;
    let report = read(&options.output.join("report.json"))?;
    let contract = read(&options.contract)?;
    let (manifest, _) = super::manifest::read(root, &options.manifest)?;
    let target = report
        .get("target")
        .and_then(Value::as_str)
        .ok_or("the report names no target")?;
    let open = open_bounds(&contract, &manifest, target)?;
    let cases: Vec<CaseBounds> = report
        .get("cases")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(case_bounds)
        .collect();
    let label = contract
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (lines, failed) = verdict(label, &cases, &open, Policy::load(root)?);
    for line in &lines {
        println!("{line}");
    }
    Ok(failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn row(id: &str, upper: f64, heap: f64, rss: f64) -> Value {
        parse(&format!(
            r#"{{"id": "{id}", "timing": {{"status": "measured", "ratio": {{"upper_95": {upper}}}}},
                "memory": {{"status": "qualified", "heap_ratio_upper_95": {heap},
                            "total_rss_ratio_upper_95": {rss}}}}}"#
        ))
        .unwrap()
    }

    /// The repository's memory ceilings under a runtime ceiling of 1.2.
    fn policy() -> Policy {
        let mut policy = Policy::load(&crate::root()).unwrap();
        policy.runtime.ceiling = 1.2;
        policy
    }

    fn manifest() -> Value {
        parse(r#"{"cases": [{"id": "a"}, {"id": "b"}, {"id": "c"}]}"#).unwrap()
    }

    #[test]
    fn a_failing_bound_fails_the_step_unless_it_is_listed_open() {
        let cases: Vec<CaseBounds> = [row("a", 0.9, 0.5, 0.8), row("b", 1.0, 0.9, 1.43)]
            .iter()
            .filter_map(case_bounds)
            .collect();
        let none = BTreeSet::new();
        let (lines, failed) = verdict("pgs", &cases, &none, policy());
        assert!(failed);
        assert_eq!(lines, vec!["pgs: b: total RSS bound fails at 1.4300"]);
        let open = BTreeSet::from([(Bound::TotalRss, "b".to_owned())]);
        let (lines, failed) = verdict("pgs", &cases, &open, policy());
        assert!(!failed);
        assert_eq!(
            lines,
            vec!["pgs: b: total RSS bound fails at 1.4300, open as listed"]
        );
        // An open total RSS bound does not cover the case's other bounds.
        let slow: Vec<CaseBounds> = [row("b", 1.25, 0.9, 1.43)]
            .iter()
            .filter_map(case_bounds)
            .collect();
        let (lines, failed) = verdict("pgs", &slow, &open, policy());
        assert!(failed);
        assert_eq!(lines[0], "pgs: b: runtime bound fails at 1.2500");
        // A listed bound that holds is reported and does not fail the step.
        let (lines, failed) = verdict(
            "pgs",
            &cases[..1],
            &BTreeSet::from([(Bound::TotalRss, "a".to_owned())]),
            policy(),
        );
        assert!(!failed);
        assert_eq!(
            lines,
            vec!["pgs: a: total RSS bound holds at 0.8000; remove it from open_bounds"]
        );
    }

    #[test]
    fn a_collection_grades_the_bounds_it_measured_and_an_invalid_one_fails() {
        let timed =
            parse(r#"{"id": "a", "timing": {"status": "measured", "ratio": {"upper_95": 1.3}}}"#)
                .unwrap();
        let unqualified = parse(r#"{"id": "a", "memory": {"status": "contaminated"}}"#).unwrap();
        let cases: Vec<CaseBounds> = [timed, unqualified]
            .iter()
            .filter_map(case_bounds)
            .collect();
        let (lines, failed) = verdict("f", &cases, &BTreeSet::new(), policy());
        assert!(failed);
        assert_eq!(
            lines,
            vec![
                "f: a: runtime bound fails at 1.3000",
                "f: a: heap bound fails at an invalid measurement",
                "f: a: total RSS bound fails at an invalid measurement",
            ]
        );
        for skipped in [
            r#"{"id": "a", "open_case": true}"#,
            r#"{"id": "a", "outcome": "refusal"}"#,
            r#"{"id": "a", "sampled_as": 3, "timing": {"status": "measured", "ratio": {"upper_95": 9}}}"#,
        ] {
            assert_eq!(case_bounds(&parse(skipped).unwrap()), None, "{skipped}");
        }
    }

    #[test]
    fn open_bounds_are_scoped_to_targets_and_checked() {
        let arm = "thumbv7neon-unknown-linux-gnueabihf";
        let contract =
            |entries: &str| parse(&format!(r#"{{"open_bounds": [{entries}]}}"#)).unwrap();
        let scoped = contract(&format!(
            r#"{{"bound": "total_rss", "cases": ["a", "b"], "targets": ["{arm}"], "cause": "measured"}}"#
        ));
        assert_eq!(
            open_bounds(&scoped, &manifest(), arm),
            Ok(BTreeSet::from([
                (Bound::TotalRss, "a".to_owned()),
                (Bound::TotalRss, "b".to_owned())
            ]))
        );
        assert_eq!(
            open_bounds(&scoped, &manifest(), "aarch64-unknown-linux-gnu"),
            Ok(BTreeSet::new())
        );
        // A case may be open on each target through an entry of its own, with its cause.
        let per_target = contract(&format!(
            r#"{{"bound": "total_rss", "cases": ["a"], "targets": ["aarch64-unknown-linux-gnu"], "cause": "x"}},
               {{"bound": "total_rss", "cases": ["a", "b"], "targets": ["{arm}"], "cause": "y"}}"#
        ));
        assert_eq!(
            open_bounds(&per_target, &manifest(), "aarch64-unknown-linux-gnu"),
            Ok(BTreeSet::from([(Bound::TotalRss, "a".to_owned())]))
        );
        assert_eq!(
            open_bounds(&per_target, &manifest(), arm).map(|o| o.len()),
            Ok(2)
        );
        for (entries, error) in [
            (
                r#"{"bound": "rss", "cases": ["a"], "cause": "x"}"#,
                "an open bound needs a bound: runtime, heap or total_rss",
            ),
            (
                r#"{"bound": "heap", "cases": ["a"], "cause": " "}"#,
                "open heap bound: its cause is empty",
            ),
            (
                r#"{"bound": "heap", "cases": [], "cause": "x"}"#,
                "open heap bound: it lists no cases",
            ),
            (
                r#"{"bound": "heap", "cases": ["z"], "cause": "x"}"#,
                "open heap bound of z: not a case of the manifest",
            ),
            (
                r#"{"bound": "heap", "cases": ["a"], "cause": "x"}, {"bound": "heap", "cases": ["a"], "cause": "y"}"#,
                "open heap bound of a: listed twice on thumbv7neon-unknown-linux-gnueabihf",
            ),
            (
                r#"{"bound": "runtime", "cases": ["a"], "cause": "x", "targets": ["x86_64-unknown-linux-gnu"]}"#,
                "open runtime bound: its targets must be some of the gate's targets",
            ),
        ] {
            assert_eq!(
                open_bounds(&contract(entries), &manifest(), arm),
                Err(error.to_owned()),
                "{entries}"
            );
        }
    }
}
