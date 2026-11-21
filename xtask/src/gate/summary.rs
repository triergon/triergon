//! `gate summary`: the shards' outputs joined into one result.

use std::fs;
use std::path::{Path, PathBuf};

use crate::json::{self, Value};
use crate::sha256;

use super::plan::{ISOBMFF_GRADE_TARGET, ISOBMFF_PARTS, capture_cases, items, report_features};
use super::{Item, Placement, R, SHARD, Step, select, shards};

/// Placeholder for the host output directory in summary command templates.
pub(super) const SUMMARY_OUTPUT: &str = "OUT";
/// Whether a grade step's only failures are the contract's listed open cases: its
/// log reports N of M cases passing and M - N open failures, as listed.
pub(super) fn only_open_cases(record: &Value) -> bool {
    let lines: Vec<&str> = record
        .get("outcome")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let passing = lines.iter().find_map(|line| {
        let words: Vec<&str> = line[..line.find(" cases pass")?]
            .split_whitespace()
            .collect();
        match words[..] {
            [.., passed, "of", total] => {
                Some((passed.parse::<usize>().ok()?, total.parse::<usize>().ok()?))
            }
            _ => None,
        }
    });
    let open = lines.iter().find_map(|line| {
        line[..line.find(" open failures, as listed")?]
            .split_whitespace()
            .last()?
            .parse::<usize>()
            .ok()
    });
    matches!((passing, open), (Some((passed, total)), Some(open)) if passed + open == total)
}

/// Joins the shards' outputs into one result at `file`; 0 when every planned
/// step ran once and passed on one source and one set of workers.
pub(super) fn summary(root: &Path, file: &Path, directories: &[PathBuf]) -> R<i32> {
    let mut problems = Vec::new();
    let mut shards = Vec::new();
    let mut records_by_shard = Vec::new();
    let mut identities: Vec<(String, String)> = Vec::new();
    for directory in directories {
        let shard = json::read(&directory.join(SHARD))
            .map_err(|e| format!("{}: {e}", directory.join(SHARD).display()))?;
        let mut records = Vec::new();
        if let Ok(entries) = fs::read_dir(directory.join("steps")) {
            // Step records only: no other file, and no file a copying tool adds.
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with('.') && name.ends_with(".json") {
                    records.push(json::read(&entry.path())?);
                }
            }
        }
        records.sort_by(|a, b| {
            let started = |v: &Value| {
                v.get("started")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            started(a).cmp(&started(b))
        });
        let seconds = step_seconds(&records);
        for stage in ["timing", "memory"] {
            let path = directory.join(format!("identity-{stage}.json"));
            match sha256::file_hex(&path) {
                Ok(sha) => identities.push((stage.to_owned(), sha)),
                Err(e) => problems.push(format!("{}: {e}", path.display())),
            }
        }
        shards.push(Value::object([
            ("directory", Value::str(directory.display().to_string())),
            ("shard", shard.clone()),
            ("step_seconds", Value::float(seconds.round())),
        ]));
        records_by_shard.push(records);
    }
    let field = |shard: &Value, key: &str| shard.get("shard").and_then(|s| s.get(key)).cloned();
    let count = shards
        .first()
        .and_then(|s| field(s, "count"))
        .and_then(|c| c.as_f64());
    let mut indices: Vec<i64> = shards
        .iter()
        .filter_map(|s| field(s, "index").and_then(|i| i.as_f64()).map(|i| i as i64))
        .collect();
    indices.sort_unstable();
    let expected: Vec<i64> = (1..=count.unwrap_or(0.0) as i64).collect();
    if indices != expected {
        problems.push(format!("shards {indices:?} of {count:?}: each shard once"));
    }
    for key in ["count", "revision", "changes", "target", "selection"] {
        let values: Vec<Option<Value>> = shards.iter().map(|s| field(s, key)).collect();
        if values.windows(2).any(|w| w[0] != w[1]) {
            problems.push(format!("the shards differ in {key}"));
        }
    }
    // The timing builds must be one build on every shard; the memory builds add
    // each host's proof pin.
    let timing: Vec<&String> = identities
        .iter()
        .filter(|(stage, _)| stage == "timing")
        .map(|(_, sha)| sha)
        .collect();
    if timing.windows(2).any(|w| w[0] != w[1]) {
        problems.push("the shards' timing builds differ".into());
    }
    // Every planned step, once.
    let ran_items: Vec<String> = shards
        .iter()
        .flat_map(|s| {
            field(s, "items")
                .and_then(|i| i.as_array().map(<[Value]>::to_vec))
                .unwrap_or_default()
        })
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let capture = capture_cases(root)?;
    // Verify the same selected plan and each shard's assigned commands.
    let target = shards
        .first()
        .and_then(|shard| field(shard, "target"))
        .and_then(|target| target.as_str().map(str::to_owned))
        .ok_or("gate summary needs at least one shard with a target")?;
    let selection: Option<Vec<String>> = shards
        .first()
        .and_then(|shard| field(shard, "selection"))
        .and_then(|names| names.as_array().map(<[Value]>::to_vec))
        .map(|names| {
            names
                .iter()
                .filter_map(|n| n.as_str().map(str::to_owned))
                .collect()
        });
    let reports = report_features(root)?;
    let plan_of = || {
        select(
            items(
                &reports,
                SUMMARY_OUTPUT,
                false,
                &target,
                "target/release",
                &capture,
            ),
            selection.as_deref(),
        )
    };
    let planned = plan_of()?;
    let count = count.unwrap_or(0.0) as usize;
    if count > 0 {
        let plan = self::shards(plan_of()?, count);
        for (shard, records) in shards.iter().zip(&records_by_shard) {
            let index = field(shard, "index")
                .and_then(|i| i.as_f64())
                .unwrap_or(0.0) as usize;
            let ran: Vec<String> = field(shard, "items")
                .and_then(|i| i.as_array().map(<[Value]>::to_vec))
                .unwrap_or_default()
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            let Some(assigned) = index.checked_sub(1).and_then(|i| plan.get(i)) else {
                problems.push(format!("invalid shard {index} of {count}"));
                continue;
            };
            if !ran.iter().eq(assigned.iter().map(|item| &item.name)) {
                problems.push(format!(
                    "shard {index} of {count} did not run its planned items"
                ));
            }
            if target == ISOBMFF_GRADE_TARGET {
                let output = field(shard, "output");
                let output = output.as_ref().and_then(Value::as_str);
                problems.extend(isobmff_step_problems(assigned, output, records));
            }
        }
    }
    let steps: Vec<_> = records_by_shard.into_iter().flatten().collect();
    for item in &planned {
        let times = ran_items.iter().filter(|name| **name == item.name).count();
        if times != 1 {
            problems.push(format!("{} planned once, run {times} times", item.name));
        }
        for step in &item.steps {
            let ran = steps
                .iter()
                .filter(|r| r.get("step").and_then(Value::as_str) == Some(&step.name))
                .count();
            if ran != 1 {
                problems.push(format!("{}: {ran} records", step.name));
            }
        }
    }
    let failed: Vec<String> = steps
        .iter()
        .filter(|r| r.get("status").and_then(Value::as_f64) != Some(0.0))
        .filter_map(|r| r.get("step").and_then(Value::as_str).map(str::to_owned))
        .collect();
    for record in &steps {
        println!(
            "{:<44} {:>8.0} s  exit {}",
            record
                .get("step")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            record_seconds(record).unwrap_or_default(),
            record.get("status").and_then(Value::as_f64).unwrap_or(-1.0)
        );
        for line in record
            .get("outcome")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            println!("    {}", line.as_str().unwrap_or_default());
        }
    }
    for problem in &problems {
        println!("FAIL {problem}");
    }
    let open: Vec<&String> = failed
        .iter()
        .filter(|name| {
            steps
                .iter()
                .find(|r| r.get("step").and_then(Value::as_str) == Some(name.as_str()))
                .is_some_and(only_open_cases)
        })
        .collect();
    // A run of part of the plan says so first, so nothing takes it for the full gate.
    if let Some(names) = &selection {
        println!(
            "partial run, not the full gate: {} items: {}",
            names.len(),
            names.join(" ")
        );
    }
    println!(
        "{} steps on {} shards, {} failed, {} of them only on the contract's listed open cases",
        steps.len(),
        shards.len(),
        failed.len(),
        open.len()
    );
    json::write(
        file,
        &Value::object([
            ("schema", Value::str("gate-summary-v1")),
            ("partial", Value::Bool(selection.is_some())),
            (
                "selection",
                selection
                    .as_ref()
                    .map_or(Value::Null, |names| Value::strings(names.iter().cloned())),
            ),
            ("shards", Value::Array(shards)),
            ("steps", Value::Array(steps)),
            ("failed", Value::strings(failed.clone())),
            (
                "failed_on_open_cases_only",
                Value::strings(open.into_iter().cloned()),
            ),
            ("problems", Value::strings(problems.clone())),
        ]),
    )?;
    Ok(i32::from(!failed.is_empty() || !problems.is_empty()))
}

/// A step record's time: its own, or its batch's.
fn record_seconds(record: &Value) -> Option<f64> {
    ["seconds", "batch_seconds"]
        .iter()
        .find_map(|key| record.get(key).and_then(Value::as_f64))
}

/// The seconds a shard's steps took: the steps of a batch ran as one command, so
/// its time counts once.
pub(super) fn step_seconds(records: &[Value]) -> f64 {
    let mut batches: Vec<&str> = Vec::new();
    records
        .iter()
        .filter(|record| match record.get("batch").and_then(Value::as_str) {
            Some(batch) if batches.contains(&batch) => false,
            Some(batch) => {
                batches.push(batch);
                true
            }
            None => true,
        })
        .filter_map(record_seconds)
        .sum()
}

// A part's collection and grade commands must all be on its assigned shard,
// with the exact stage, inputs and arguments written by the plan.
pub(super) fn isobmff_step_problems(
    plan: &[Item],
    output: Option<&str>,
    records: &[Value],
) -> Vec<String> {
    let mut problems = Vec::new();
    for item in plan.iter().filter(|item| {
        item.name
            .strip_prefix("isobmff-")
            .and_then(|number| number.parse::<usize>().ok())
            .is_some_and(|part| (1..=ISOBMFF_PARTS).contains(&part))
    }) {
        for step in &item.steps {
            let mut matching = records
                .iter()
                .filter(|record| record.get("step").and_then(Value::as_str) == Some(&step.name));
            let matches = matching.next().zip(output).is_some_and(|(record, output)| {
                Path::new(output).is_absolute()
                    && record.get("item").and_then(Value::as_str) == Some(&item.name)
                    && record.get("stage").and_then(Value::as_str) == Some(step.stage.name())
                    && isobmff_command_matches(step, record, output)
            });
            if !matches || matching.next().is_some() {
                problems.push(format!(
                    "{}: expected one planned command on its assigned shard",
                    step.name
                ));
            }
        }
    }
    problems
}

fn isobmff_command_matches(step: &Step, record: &Value, output: &str) -> bool {
    let Some(args) = record.get("command").and_then(Value::as_array) else {
        return false;
    };
    let expected_environment =
        Value::object(step.environment.iter().map(|(k, v)| (*k, Value::str(*v))));
    if record.get("environment") != Some(&expected_environment) {
        return false;
    }
    let mut args = args.iter().map(Value::as_str);
    if matches!(step.placement, Placement::Timed | Placement::Memory) {
        if args.next() != Some(Some("taskset")) || args.next() != Some(Some("-c")) {
            return false;
        }
        let cpu = args.next().flatten();
        match step.placement {
            Placement::Timed if cpu != Some("1") => return false,
            Placement::Memory if !cpu.is_some_and(|cpu| cpu.starts_with("1-")) => return false,
            _ => {}
        }
    }
    args.len() == step.command.len()
        && step.command.iter().zip(args).all(|(expected, actual)| {
            if let Some(relative) = expected.strip_prefix(SUMMARY_OUTPUT) {
                actual.and_then(|arg| arg.strip_prefix(output)) == Some(relative)
            } else {
                actual == Some(expected)
            }
        })
}
