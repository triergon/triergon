use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::json::{self, Value};
use crate::sha256;

use super::check::{Source, Tree, kept_files};
use super::plan::{
    CAPTURE_MEMORY, CAPTURE_QUALIFY, ISOBMFF_GRADE_TARGET, ISOBMFF_PARTS, items, report_contracts,
    report_features, step,
};
use super::run::{Ran, checked_result, record_step, run_batch, unmeasured_cases, unmeasured_in};
use super::summary::{SUMMARY_OUTPUT, isobmff_step_problems, only_open_cases, step_seconds};
use super::workspace_tests::{INDEX_TESTS, sources_digest, tree_digest};
use super::*;

fn plan() -> Vec<Item> {
    let capture: Vec<String> = (1..=15).map(|i| format!("c{i:02}")).collect();
    items(
        &report_features(&crate::root()).unwrap(),
        "/o",
        false,
        "thumbv7neon-unknown-linux-gnueabihf",
        "target/thumbv7neon-unknown-linux-gnueabihf/release",
        &capture,
    )
}

#[test]
fn every_step_is_named_once_and_every_collection_is_graded_or_suite_run() {
    let plan = plan();
    let names: Vec<&str> = plan
        .iter()
        .flat_map(|i| &i.steps)
        .map(|s| s.name.as_str())
        .collect();
    let mut unique = names.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), names.len());
    for item in &plan {
        let timed = item.steps.iter().any(|s| s.placement == Placement::Timed);
        let memory = item.steps.iter().any(|s| s.placement == Placement::Memory);
        // A collection is timed in its item, but for a codec list's memory parts,
        // whose list is timed in an item of its own.
        let part = item.name.starts_with("codec-") && item.steps.len() == 1;
        assert!(!memory || timed || part, "{}", item.name);
        for step in &item.steps {
            assert_eq!(
                step.placement == Placement::Memory,
                step.stage == Stage::Memory && step.placement != Placement::Grade
            );
            if step.placement == Placement::Memory {
                assert!(
                    step.environment
                        .contains(&("TRIERGON_RSS_KERNEL_BOUND", "1")),
                    "{}",
                    step.name
                );
            }
        }
    }
    assert!(names.contains(&"grade-subtitle-cc"));
    assert!(names.contains(&"benchmark-isobmff-3"));
    assert!(names.contains(&"benchmark-capture-c04") && names.contains(&"memory-capture-c04"));
    assert!(names.contains(&"isobmff-reader-compare"));
    assert!(names.contains(&"memory-codec-file-workloads-3") && names.contains(&"test-release"));
    assert!(
        names.contains(&"memory-codec-workloads-5") && names.contains(&"benchmark-codec-workloads")
    );
}

#[test]
fn aarch64_plan_binds_each_isobmff_grade_to_its_shard_and_collections() {
    let realized = items(
        &report_features(&crate::root()).unwrap(),
        "/run",
        true,
        ISOBMFF_GRADE_TARGET,
        "target/release",
        &[],
    );
    let mut records = BTreeMap::new();
    for item in realized.iter().filter(|i| i.name.starts_with("isobmff-")) {
        let steps = item.steps.iter().map(|step| {
            let mut command = step.command.clone();
            let cpus = match step.placement {
                Placement::Timed => Some("1"),
                Placement::Memory => Some("1-15"),
                _ => None,
            };
            if let Some(cpus) = cpus {
                command.splice(0..0, owned(&["taskset", "-c", cpus]));
            }
            Value::object([
                ("step", Value::str(&step.name)),
                ("item", Value::str(&item.name)),
                ("stage", Value::str(step.stage.name())),
                ("command", Value::strings(command)),
                (
                    "environment",
                    Value::object(step.environment.iter().map(|(k, v)| (*k, Value::str(*v)))),
                ),
            ])
        });
        records.insert(item.name.as_str(), steps.collect::<Vec<_>>());
    }
    let plan = shards(
        items(
            &report_features(&crate::root()).unwrap(),
            SUMMARY_OUTPUT,
            false,
            ISOBMFF_GRADE_TARGET,
            "target/release",
            &[],
        ),
        3,
    );
    assert_eq!(records.len(), ISOBMFF_PARTS + 2);
    assert!(records.contains_key("isobmff-seek"));
    assert!(records.contains_key("isobmff-reader"));
    for shard in &plan {
        let assigned: Vec<_> = shard
            .iter()
            .filter_map(|item| records.get(item.name.as_str()))
            .flatten()
            .cloned()
            .collect();
        assert!(isobmff_step_problems(shard, Some("/run"), &assigned).is_empty());
        for (index, record) in assigned.iter().enumerate() {
            let name = record.get("step").and_then(Value::as_str).unwrap();
            if !name.starts_with("grade-") {
                continue;
            }
            let mut missing = assigned.clone();
            missing.remove(index);
            assert!(!isobmff_step_problems(shard, Some("/run"), &missing).is_empty());

            let change = |edit: fn(&mut Vec<Value>)| {
                let mut changed = assigned.clone();
                let Value::Object(fields) = &mut changed[index] else {
                    unreachable!()
                };
                let Some((_, Value::Array(args))) =
                    fields.iter_mut().find(|(key, _)| key == "command")
                else {
                    unreachable!()
                };
                edit(args);
                assert!(!isobmff_step_problems(shard, Some("/run"), &changed).is_empty());
            };
            change(|args| args[2] = Value::str("/run/benchmark/other/collection.json"));
            change(|args| args[3] = Value::str("/run/memory/other/collection.json"));
            change(|args| {
                assert_eq!(args.pop(), Some(Value::str("--full")));
            });
        }
    }
}

#[test]
fn capture_memory_reads_the_records_its_timing_wrote() {
    for item in plan().iter().filter(|i| i.name.starts_with("capture-")) {
        let [timing, memory] = &item.steps[..] else {
            panic!("{}: a timing and a memory step", item.name);
        };
        let value = |step: &Step, flag: &str| {
            let at = step.command.iter().position(|a| a == flag).unwrap();
            step.command[at + 1].clone()
        };
        assert_eq!(value(timing, "--stage"), "timing");
        assert_eq!(value(memory, "--stage"), "memory");
        assert_eq!(value(memory, "--timing"), value(timing, "--out"));
        assert_ne!(value(memory, "--out"), value(timing, "--out"));
        assert_eq!(value(memory, "--only"), value(timing, "--only"));
        assert_eq!(
            (timing.placement, memory.placement),
            (Placement::Timed, Placement::Memory)
        );
    }
}

/// A shard's capture memory steps share one batch, so they run as one
/// collection; nothing else is batched, and the batch can join their commands.
#[test]
fn capture_memory_steps_alone_share_a_batch() {
    let plan = plan();
    let steps: Vec<&Step> = plan.iter().flat_map(|item| &item.steps).collect();
    let batched: Vec<&&Step> = steps.iter().filter(|s| s.batch.is_some()).collect();
    assert_eq!(batched.len(), 15);
    for step in batched {
        assert!(step.name.starts_with("memory-capture-"), "{}", step.name);
        assert_eq!(step.batch, Some(CAPTURE_MEMORY));
        assert_eq!(step.placement, Placement::Memory);
        assert!(step.command.starts_with(&owned(&CAPTURE_QUALIFY)));
        assert!(!step.command.iter().any(|arg| arg == crate::capture::AND));
        assert_eq!(
            step.environment,
            [
                ("TRIERGON_RSS_KERNEL_BOUND", "1"),
                ("TRIERGON_PYTHON", "python3")
            ]
        );
    }
}

/// A batch runs as one command and records each step with the gate result in
/// its `--out`: a step that wrote none, or a failing one, fails alone.
#[cfg(unix)]
#[test]
fn a_batch_runs_once_and_records_each_step_by_its_result() {
    let dir = crate::walk::scratch("gate-batch");
    // Writes a gate result in each --out but the one named `silent`, failing in
    // the one named `fails`, and prints its arguments and a verdict line per case.
    let program = dir.join("qualify.sh");
    crate::walk::write_script(
        &program,
        r#"#!/bin/sh
echo "$@"
while [ $# -gt 0 ]; do
  if [ "$1" = --only ]; then echo "$2: 1 of 1 cases pass"; fi
  if [ "$1" = --out ]; then
    case $2 in
      *silent) ;;
      *fails) mkdir -p "$2"; echo '{"status": 3}' > "$2/gate-result.json" ;;
      *) mkdir -p "$2"; echo '{"status": 0}' > "$2/gate-result.json" ;;
    esac
  fi
  shift
done
"#,
    );
    let output = dir.join("run");
    // An earlier run's result is not this run's.
    fs::create_dir_all(output.join("silent")).unwrap();
    fs::write(output.join("silent/gate-result.json"), r#"{"status": 0}"#).unwrap();
    let items: Vec<Item> = ["passes", "fails", "silent"]
        .iter()
        .map(|case| {
            let mut command = owned(&CAPTURE_QUALIFY);
            command.extend(owned(&["--only", case, "--out"]));
            command.push(output.join(case).display().to_string());
            let mut step = step(
                format!("memory-{case}"),
                Stage::Memory,
                Placement::Memory,
                command,
            );
            step.environment.push(("TRIERGON_PYTHON", "python3"));
            step.batch = Some("batch");
            Item {
                name: (*case).into(),
                weight: 1,
                steps: vec![step],
                preflight: Vec::new(),
            }
        })
        .collect();
    let steps: Vec<(&Item, &Step)> = items.iter().map(|i| (i, &i.steps[0])).collect();
    let log = output.join("run.log");
    let failed = run_batch(&dir, &program, &output, &log, &steps, None).unwrap();
    assert_eq!(failed, 2);
    let joined = fs::read_to_string(output.join("logs/batch.log")).unwrap();
    assert_eq!(joined.matches("capture qualify").count(), 1, "{joined}");
    assert_eq!(joined.matches(" --and ").count(), 2, "{joined}");
    let record =
        |case: &str| json::read(&output.join(format!("steps/memory-{case}.json"))).unwrap();
    for (case, status) in [("passes", 0.0), ("fails", 3.0), ("silent", 1.0)] {
        let record = record(case);
        assert_eq!(record.get("status").and_then(Value::as_f64), Some(status));
        assert_eq!(record.get("batch").and_then(Value::as_str), Some("batch"));
        assert_eq!(record.get("item").and_then(Value::as_str), Some(case));
        assert_eq!(record.get("seconds"), None);
        // Of the shared log, only its own case's verdict.
        assert_eq!(
            record.get("outcome"),
            Some(&Value::strings([format!("{case}: 1 of 1 cases pass")]))
        );
        let command = record.get("command").and_then(Value::as_array).unwrap();
        assert!(command.contains(&Value::str(case)));
        assert!(!command.contains(&Value::str(crate::capture::AND)));
    }
    let ran = fs::read_to_string(&log).unwrap();
    assert!(ran.contains("memory-fails EXIT=3"), "{ran}");
    assert!(ran.contains("memory-silent EXIT=1"), "{ran}");
    // The shard's step time counts the batch once.
    let records: Vec<Value> = ["passes", "fails", "silent"].map(record).into();
    let once = records[0]
        .get("batch_seconds")
        .and_then(Value::as_f64)
        .unwrap();
    assert!((step_seconds(&records) - once).abs() < 1e-9);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_batch_counts_its_time_once() {
    let record = |seconds: f64, batch: Option<&str>| {
        Value::object(match batch {
            Some(batch) => vec![
                ("batch", Value::str(batch)),
                ("batch_seconds", Value::float(seconds)),
            ],
            None => vec![("seconds", Value::float(seconds))],
        })
    };
    let records = [
        record(2.0, None),
        record(60.0, Some("memory-capture")),
        record(60.0, Some("memory-capture")),
        record(3.0, None),
    ];
    assert!((step_seconds(&records) - 65.0).abs() < 1e-9);
}

#[test]
fn every_capture_case_is_an_item_of_its_own() {
    let plan = plan();
    let ids: Vec<String> = plan
        .iter()
        .filter(|i| i.name.starts_with("capture-"))
        .flat_map(|i| {
            let command = &i.steps[0].command;
            let at = command.iter().position(|a| a == "--only").unwrap();
            command[at + 1..].to_vec()
        })
        .collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        (1..=15).map(|i| format!("c{i:02}")).collect::<Vec<_>>()
    );
}

#[test]
fn shards_are_balanced_deterministically_and_hold_every_item() {
    let total: u32 = plan().iter().map(|i| i.weight).sum();
    for count in 1..=6 {
        let split = shards(plan(), count);
        assert_eq!(split.len(), count);
        let weights: Vec<u32> = split
            .iter()
            .map(|s| s.iter().map(|i| i.weight).sum())
            .collect();
        assert_eq!(weights.iter().sum::<u32>(), total);
        let heaviest = plan().iter().map(|i| i.weight).max().unwrap();
        assert!(weights.iter().max().unwrap() - weights.iter().min().unwrap() <= heaviest);
        let again: Vec<Vec<String>> = shards(plan(), count)
            .iter()
            .map(|s| s.iter().map(|i| i.name.clone()).collect())
            .collect();
        let names: Vec<Vec<String>> = split
            .iter()
            .map(|s| s.iter().map(|i| i.name.clone()).collect())
            .collect();
        assert_eq!(again, names);
    }
}

#[test]
fn a_test_result_is_keyed_by_every_source_the_working_tree_holds() {
    let root = crate::walk::scratch("gate-test-sources");
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    fs::write(root.join("lib.rs"), b"a").unwrap();
    git(&["add", "lib.rs"]);
    git(&["commit", "-qm", "a"]);
    let committed = sources_digest(&root).unwrap();
    fs::write(root.join("lib.rs"), b"b").unwrap();
    let changed = sources_digest(&root).unwrap();
    assert_ne!(committed, changed);
    fs::write(root.join("new.rs"), b"c").unwrap();
    assert_ne!(sources_digest(&root).unwrap(), changed);
    fs::remove_file(root.join("new.rs")).unwrap();
    fs::write(root.join("lib.rs"), b"a").unwrap();
    assert_eq!(sources_digest(&root).unwrap(), committed);
    fs::create_dir_all(root.join("harness/evidence")).unwrap();
    fs::create_dir_all(root.join("harness/evidence/sets")).unwrap();
    fs::write(root.join("harness/evidence/sets/run.json"), b"{}").unwrap();
    assert_eq!(sources_digest(&root).unwrap(), committed);
    assert_eq!(tree_digest(&root.join("absent")).unwrap(), "absent");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_grade_failing_only_on_listed_open_cases_is_told_apart() {
    let record =
        |lines: &[&str]| Value::object([("outcome", Value::strings(lines.iter().copied()))]);
    let open = "subtitle-cc: 4 open failures, as listed: hevc-eos-ts, hevc-eos-ts-realtime, \
                    hevc-eos-ts-first, hevc-eos-ts-second";
    assert!(only_open_cases(&record(&[
        "subtitle-cc: 201 of 205 cases pass",
        open
    ])));
    assert!(!only_open_cases(&record(&[
        "subtitle-cc: 200 of 205 cases pass",
        open
    ])));
    assert!(!only_open_cases(&record(&[
        "subtitle-cc: 201 of 205 cases pass"
    ])));
    assert!(!only_open_cases(&record(&[])));
    // A codec collection's verdict lines read the same way.
    assert!(only_open_cases(&record(&[
        "workloads timing: 19 of 20 cases pass",
        "workloads timing: 1 open failures, as listed: hevc-main10-2160p",
    ])));
    assert!(!only_open_cases(&record(&[
        "workloads timing: 19 of 20 cases pass",
        "workloads timing: failing: hevc-main10-2160p",
    ])));
}

#[test]
fn missing_registered_report_fails_planning() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut contracts = crate::harness::features::discover(root).unwrap();
    contracts.retain(|contract| contract.id != "subtitle-text");
    assert_eq!(
        report_contracts(root, contracts).err().as_deref(),
        Some("subtitle-text: planned, but not registered")
    );
}

#[test]
fn every_registered_feature_is_planned_or_excluded() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut contracts = crate::harness::features::discover(root).unwrap();
    let id = "new-report-feature";
    contracts.push(crate::harness::features::Contract {
        id: id.into(),
        path: root.join("harness/features").join(id).join("contract.json"),
        document: Value::object([("runner", Value::str("cargo xtask report"))]),
        compare_order: 0,
    });
    let reports = report_contracts(root, contracts).unwrap();
    let new = reports.iter().find(|report| report.id == id).unwrap();
    assert_eq!(new.weight, 1);
    assert!(
        items(&reports, "OUT", false, TARGETS[0], "target/release", &[])
            .iter()
            .any(|item| item.name == id)
    );
}

#[test]
fn features_with_committed_inputs_are_preflighted_against_them() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for item in plan() {
        let args = &item.preflight;
        for (index, arg) in args.iter().enumerate() {
            if arg != "--feature" {
                continue;
            }
            let id = &args[index + 1];
            let directory = root.join(format!("harness/features/{id}/inputs"));
            if !directory.is_dir() {
                continue;
            }
            let prefix = format!("{id}=");
            let mut feature_args = owned(&["--feature", id]);
            for pair in args.windows(2) {
                if matches!(pair[0].as_str(), "--corpus" | "--manifest")
                    && pair[1].starts_with(&prefix)
                {
                    feature_args.extend_from_slice(pair);
                }
            }
            // A manifest can locate its inputs without a corpus override.
            // Check the paths and pins the generated preflight actually uses.
            let inputs =
                crate::harness::preflight::inputs(root, &feature_args, &json::read).unwrap();
            let mut pinned = inputs
                .iter()
                .filter_map(|input| Some((&input.path, input.sha256.as_deref()?)))
                .peekable();
            assert!(
                pinned.peek().is_some(),
                "{}: preflight of {id} has no pinned inputs",
                item.name
            );
            for (file, pin) in pinned {
                assert!(
                    file.starts_with(&directory),
                    "{}: preflight of {id} resolves {} outside {}",
                    item.name,
                    file.display(),
                    directory.display()
                );
                assert_eq!(sha256::file_hex(file).unwrap(), pin, "{}", file.display());
            }
        }
    }
}

/// An item's preflight in the AArch64 plan, which leaves no item out.
fn preflight_of(name: &str) -> Vec<String> {
    let capture: Vec<String> = (1..=15).map(|i| format!("c{i:02}")).collect();
    items(
        &report_features(&crate::root()).unwrap(),
        "/o",
        false,
        "aarch64-unknown-linux-gnu",
        "target/release",
        &capture,
    )
    .into_iter()
    .find(|item| item.name == name)
    .unwrap()
    .preflight
}

#[test]
fn a_feature_whose_inputs_are_neither_committed_nor_kept_is_refused() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut tree = Tree::committed(root).unwrap();
    tree.kept = kept_files(root).unwrap();
    // The progressive item as an earlier plan had it: the feature alone, so its
    // cases resolve to .research/fixtures, where no host has them.
    let (_, problems) = tree.problems(&owned(&["--feature", "progressive-ts188"]));
    assert_eq!(problems.len(), 3, "{problems:?}");
    assert!(problems.iter().all(
            |p| p.starts_with(".research/fixtures/") && p.ends_with("in no kept evidence set")
        ));
    let (sources, problems) = tree.problems(&preflight_of("progressive"));
    assert_eq!(problems, Vec::<String>::new());
    assert!(sources.iter().all(|&source| source == Source::Committed));
}

#[test]
fn inputs_in_a_kept_set_count_by_its_listing_and_pins() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let args = preflight_of("codec-file-workloads");
    let mut tree = Tree::committed(root).unwrap();
    let (_, problems) = tree.problems(&args);
    assert_eq!(problems.len(), 10, "{problems:?}");
    tree.kept = kept_files(root).unwrap();
    let (sources, problems) = tree.problems(&args);
    assert_eq!(problems, Vec::<String>::new());
    assert_eq!(sources.iter().filter(|&&s| s == Source::Kept).count(), 10);
    let listed = ".research/codec-inputs/flac-16-44k-stereo.flac";
    *tree.kept.get_mut(listed).unwrap() = "0".repeat(64);
    let (_, problems) = tree.problems(&args);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].starts_with(&format!("{listed}: pinned as")));
}

fn timed_step(args: &[&str]) -> Step {
    step(
        "benchmark-test",
        Stage::Timing,
        Placement::Timed,
        xtask(args),
    )
}

fn parsed(raw: &str) -> Value {
    json::parse_observations(raw).unwrap()
}

fn external_receipt() -> Value {
    parsed(
        r#"{"case":{"id":"mkv/hdr10tags-both.mkv","parity":{"status":"pass"},
            "timing":{"status":"measured"},"memory":{"status":"qualified"}}}"#,
    )
}

#[test]
fn collection_checks_single_external_vectors_and_rejects_missing_measurements() {
    let step = timed_step(&["external-vector"]);
    let good = external_receipt();
    for (key, status) in [("timing", "measured"), ("memory", "qualified")] {
        let check = |report: &Value| unmeasured_in(&step, report, key, status);
        assert!(check(&good).unwrap().is_empty());
        for bad in [
            None,
            Some("null"),
            Some("false"),
            Some("[]"),
            Some("{}"),
            Some(r#"{"status":"rejected"}"#),
        ] {
            let mut row = parsed(r#"{"id":"external","parity":{"status":"pass"}}"#);
            if let Some(raw) = bad {
                crate::isobmff_bench::field(&mut row, key, parsed(raw));
            }
            assert_eq!(
                check(&Value::object([("case", row)])).unwrap(),
                ["external"]
            );
        }
        for bad in [
            "{}",
            r#"{"cases":[]}"#,
            r#"{"case":[]}"#,
            r#"{"case":{},"cases":[]}"#,
            r#"{"case":{"id":"","timing":{"status":"measured"}}}"#,
            r#"{"cases":[{}],"collection_status":"complete"}"#,
            r#"{"case":{},"collection_status":"partial"}"#,
        ] {
            assert!(check(&parsed(bad)).is_err(), "{bad}");
        }
    }
}

#[test]
fn a_measurement_with_an_unmeasured_case_names_it() {
    let report = parsed(
        r#"{"collection_status":"complete","cases":[
            {"id":"a","parity":{"status":"pass"},"timing":{"status":"measured"},"memory":{"status":"qualified"}},
            {"id":"b","parity":{"status":"pass"},"timing":{"status":"warmup-failed"},"memory":{"status":"invalid-memory-work"}},
            {"id":"c","parity":{"status":"pass"},"timing":null},
            {"id":"open","parity":{"status":"mismatch"}},
            {"id":"unpinned","parity":{"status":"unpinned"},"timing":null},
            {"id":"bad","parity":{"status":"unpinned"},"timing":false,"memory":{}}
        ]}"#,
    );
    for command in ["report", "framing", "progressive"] {
        let step = timed_step(&[command]);
        for (key, status) in [("timing", "measured"), ("memory", "qualified")] {
            assert_eq!(
                unmeasured_in(&step, &report, key, status).unwrap(),
                ["b", "c", "bad"]
            );
        }
        // A corpus cannot claim ISO BMFF selection exemptions via its header.
        let mut wrong = report.clone();
        crate::isobmff_bench::field(&mut wrong, "contract_id", Value::str("isobmff-seek"));
        crate::isobmff_bench::field(&mut wrong, "selection", parsed("[0]"));
        assert_eq!(
            unmeasured_in(&step, &wrong, "timing", "measured").unwrap(),
            ["b", "c", "bad"]
        );
        for bad in [
            r#"{"cases":[],"collection_status":"complete"}"#,
            r#"{"cases":[{}]}"#,
            r#"{"cases":[{}],"collection_status":"partial"}"#,
        ] {
            assert!(unmeasured_in(&step, &parsed(bad), "timing", "measured").is_err());
        }
        assert!(unmeasured_in(&step, &external_receipt(), "timing", "measured").is_err());
    }
    assert_eq!(
        unmeasured_cases(&timed_step(&["report"])),
        ["the step names no --output"]
    );
    assert!(
        unmeasured_cases(&timed_step(&[
            "report",
            "--output",
            "/nonexistent-gate-output"
        ]))[0]
            .starts_with("no report: ")
    );
    for cmd in ["codecs", "capture"] {
        assert!(
            unmeasured_cases(&timed_step(&[cmd, "--out", "/nonexistent-gate-output"])).is_empty()
        );
    }
}

#[test]
fn isobmff_step_checks_the_registered_partial_collection() {
    use crate::isobmff_bench::{field, full_part_selection};
    let root = crate::root();
    let output = crate::walk::scratch("gate-isobmff-completion");
    let shard = full_part_selection(&root, (1, 3)).unwrap();
    let seek = crate::isobmff_seek::measurement_selection(&root, None).unwrap();
    for (cmd, feature, part, (contract, selected), filename) in [
        (
            "isobmff-bench",
            "isobmff-open-index",
            Some("2/3"),
            shard,
            "collection.json",
        ),
        ("isobmff-seek", "isobmff-seek", None, seek, "report.json"),
    ] {
        let mut step = timed_step(&[cmd, "--output", output.to_str().unwrap()]);
        if let Some(part) = part {
            step.command.extend(owned(&["--part", part]));
        }
        let profiles = contract
            .get("required_profiles")
            .unwrap()
            .as_array()
            .unwrap();
        assert!(selected.len() < profiles.len());
        let mut rows: Vec<_> = profiles
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let mut row = Value::object([
                    ("id", id.clone()),
                    ("parity", parsed(r#"{"status":"pass"}"#)),
                ]);
                if selected.binary_search(&i).is_ok() {
                    field(&mut row, "timing", parsed(r#"{"status":"measured"}"#));
                }
                row
            })
            .collect();
        let indices =
            |set: &[usize]| Value::Array(set.iter().map(|&i| Value::int(i as i128)).collect());
        let mut report = Value::object([
            ("cases", Value::Array(rows.clone())),
            ("collection_status", Value::str("complete")),
            ("contract_id", Value::str(feature)),
            ("part", part.map_or(Value::Null, Value::str)),
            ("selection", indices(&selected)),
        ]);
        json::write(&output.join(filename), &report).unwrap();
        assert!(unmeasured_cases(&step).is_empty(), "{cmd}");
        let check = |r: &Value| unmeasured_in(&step, r, "timing", "measured");
        for (k, v) in [
            ("part", Value::str("1/3")),
            ("selection", indices(&selected[1..])),
            ("selection", parsed("[0,0]")),
            ("contract_id", Value::str("other")),
        ] {
            let mut wrong = report.clone();
            field(&mut wrong, k, v);
            assert!(check(&wrong).is_err(), "{cmd} {k}");
        }
        let i = selected[0];
        let j = selected[1];
        let mut duplicate = rows.clone();
        field(&mut duplicate[j], "id", profiles[i].clone());
        let mut reordered = rows.clone();
        reordered.swap(i, j);
        let mut wrong_rows = vec![("duplicate ID", duplicate), ("reordered IDs", reordered)];
        if cmd == "isobmff-seek" {
            let measured_end = selected.last().unwrap() + 1;
            assert_eq!(rows.len() - measured_end, 4);
            wrong_rows.push(("missing trailing refusals", rows[..measured_end].to_vec()));
        }
        for (reason, cases) in wrong_rows {
            field(&mut report, "cases", Value::Array(cases));
            assert!(check(&report).is_err(), "{cmd}: {reason}");
        }
        field(&mut rows[i], "timing", Value::Null);
        let other = (0..rows.len())
            .find(|i| selected.binary_search(i).is_err())
            .unwrap();
        field(
            &mut rows[other],
            "timing",
            parsed(r#"{"status":"rejected"}"#),
        );
        field(&mut report, "cases", Value::Array(rows));
        let failed = check(&report).unwrap();
        assert_eq!(failed.len(), 2);
        for index in [i, other] {
            assert!(
                failed
                    .iter()
                    .any(|id| Some(id.as_str()) == profiles[index].as_str())
            );
        }
    }
    fs::remove_dir_all(output).unwrap();
}

#[test]
fn a_reused_measurement_passes_only_with_the_verdict_a_gate_run_recorded() {
    use crate::harness::reuse;
    let root = crate::walk::scratch("gate-reused-check");
    let output = root.join("benchmark/f");
    let timed = timed_step(&["external-vector", "--output", output.to_str().unwrap()]);
    let key = reuse::Key {
        sha256: "k".repeat(64),
        components: Value::object([("command", Value::str("external-vector --benchmark"))]),
    };
    let mut row = external_receipt().get("case").unwrap().clone();
    let id = row.get("id").unwrap().as_str().unwrap().to_owned();
    crate::isobmff_bench::field(&mut row, "timing", parsed(r#"{"status":"warmup-failed"}"#));
    json::write(&output.join("report.json"), &Value::object([("case", row)])).unwrap();
    reuse::record(&output, &key, 0, None).unwrap();
    assert_eq!(checked_result(&timed), Some(output.clone()));
    assert_eq!(unmeasured_cases(&timed), [id]);
    let item = Item {
        name: "external-vector".into(),
        steps: vec![],
        weight: 1,
        preflight: vec![],
    };
    let ran = Ran {
        name: timed.name.clone(),
        started: "test".into(),
        seconds: 0.0,
        code: 0,
    };
    assert_eq!(
        record_step(
            &root,
            &root.join("run.log"),
            &item,
            &timed,
            Some("1"),
            &ran,
            0
        )
        .unwrap(),
        1
    );
    let result = reuse::read(&output).unwrap();
    assert_eq!(result.get("status"), Some(&Value::int(1)));
    assert_eq!(result.get("cases_checked"), Some(&Value::Bool(true)));
    assert_eq!(result.get("key"), Some(&Value::str(&key.sha256)));
    // Reused results: one a gate run checked passes, one it did not fails.
    let cited = |checked: Value| {
        Value::object([
            ("set", Value::str("run")),
            ("status", Value::int(0)),
            ("cases_checked", checked),
        ])
    };
    fs::remove_file(output.join("report.json")).unwrap();
    reuse::record(&output, &key, 0, Some(cited(Value::Bool(true)))).unwrap();
    assert_eq!(checked_result(&timed), None);
    assert!(unmeasured_cases(&timed).is_empty());
    reuse::record(&output, &key, 0, Some(cited(Value::Null))).unwrap();
    assert!(unmeasured_cases(&timed)[0].starts_with("a reused result whose cases"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_selection_narrows_the_plan_and_refuses_names_it_lacks() {
    let names = |selection: &[&str]| {
        let selection: Vec<String> = selection.iter().map(|&s| s.to_owned()).collect();
        select(plan(), Some(&selection))
            .map(|items| items.into_iter().map(|i| i.name).collect::<Vec<_>>())
    };
    assert_eq!(
        names(&["codec-workloads-2", "font-shape-raster-ass"]),
        Ok(vec![
            "codec-workloads-2".to_owned(),
            "font-shape-raster-ass".to_owned()
        ])
    );
    assert_eq!(
        names(&["no-such-item"]),
        Err("--items no-such-item: not an item of this target's plan".to_owned())
    );
    assert_eq!(select(plan(), None).unwrap().len(), plan().len());
    // Both targets plan every item, the progressive suite among them.
    let capture: Vec<String> = (1..=15).map(|i| format!("c{i:02}")).collect();
    for target in TARGETS {
        let release = format!("target/{target}/release");
        let selected = select(
            items(
                &report_features(&crate::root()).unwrap(),
                "/o",
                false,
                target,
                &release,
                &capture,
            ),
            Some(&["progressive".to_owned()]),
        );
        assert_eq!(selected.map(|items| items.len()), Ok(1), "{target}");
    }
}

#[test]
fn the_index_tests_exist_by_their_names() {
    // `--exact` and `--skip` match these names; a renamed test must rename them.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sources: String = ["src/evidence/tests.rs", "src/gate/tests.rs"]
        .iter()
        .map(|file| fs::read_to_string(root.join(file)).unwrap())
        .collect();
    for name in INDEX_TESTS {
        let function = name.rsplit("::").next().unwrap();
        assert!(sources.contains(&format!("fn {function}()")), "{name}");
    }
}

#[test]
fn shard_arguments_count_from_one() {
    assert_eq!(parse_part("1/4").unwrap(), (0, 4));
    assert_eq!(parse_part("4/4").unwrap(), (3, 4));
    assert!(parse_part("0/4").is_err());
    assert!(parse_part("5/4").is_err());
}
