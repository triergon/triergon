use super::*;

#[test]
fn only_an_open_conversion_may_fail_capture_parity() {
    let verdict = json::parse(r#"{"conversion": false, "mux": true, "encoder": true}"#).unwrap();
    assert_eq!(
        failing_parity("picture", &verdict, false).collect::<Vec<_>>(),
        ["capture: picture: conversion parity fails"]
    );
    assert_eq!(failing_parity("picture", &verdict, true).count(), 0);

    let invalid = json::parse(r#"{"conversion": false, "mux": false, "encoder": false}"#).unwrap();
    assert_eq!(
        failing_parity("picture", &invalid, true).collect::<Vec<_>>(),
        [
            "capture: picture: mux parity fails",
            "capture: picture: encoder parity fails",
        ]
    );
}

#[test]
fn a_failing_capture_bound_fails_unless_its_case_is_open_on_the_target() {
    let line = |id: &str, open: bool, rss: bool| {
        json::parse(&format!(
            r#"{{"id": "{id}", "runtime_gate": true, "heap_gate": true, "rss_gate": {rss},
                    "open_on_target": {open}, "runtime_upper95": 1.1}}"#
        ))
        .unwrap()
    };
    assert!(failing_bounds(&[line("c01", false, true)]).is_empty());
    assert_eq!(
        failing_bounds(&[line("c01", false, false)]),
        vec!["capture: c01: total RSS bound fails"]
    );
    assert!(failing_bounds(&[line("c01", true, false)]).is_empty());
    let slow = json::parse(
        r#"{"id": "c02", "runtime_gate": false, "heap_gate": true, "rss_gate": true,
                "open_on_target": false, "runtime_upper95": 1.2345}"#,
    )
    .unwrap();
    assert_eq!(
        failing_bounds(&[slow]),
        vec!["capture: c02: runtime bound fails at 1.2345"]
    );
}

fn warmups(candidate_digest: u64) -> Value {
    let run = |digest: u64| {
        Value::object([
            ("status", Value::str("ok")),
            (
                "data",
                Value::object([("bytes", Value::int(376)), ("digest", Value::int(digest))]),
            ),
        ])
    };
    Value::object([
        // Two sessions of a 188-byte stream.
        ("iterations", Value::int(2)),
        (
            "warmups",
            Value::Array(vec![
                Value::object([
                    ("worker", Value::str("candidate")),
                    ("result", run(candidate_digest)),
                ]),
                Value::object([("worker", Value::str("reference")), ("result", run(0xff))]),
            ]),
        ),
    ])
}

#[test]
fn a_memory_run_reads_only_the_cases_its_timing_run_recorded() {
    let dir = crate::walk::scratch("capture-timing-records");
    let input = dir.join("noise-64x48-2.rgba");
    fs::create_dir_all(dir.join("timing/noise")).unwrap();
    fs::write(&input, b"pixels").unwrap();
    let case = json::parse(
        r#"{"id": "noise", "content": "noise", "width": 64, "height": 48, "frames": 2}"#,
    )
    .unwrap();
    let record = Value::object([
        ("case", case.clone()),
        ("input", Value::str(python::path(&input))),
        ("input_sha256", Value::str(file_sha(&input).unwrap())),
        (
            "correctness_summary",
            Value::object([("pipeline", Value::Bool(true))]),
        ),
    ]);
    fs::write(
        dir.join("timing/noise").join(TIMING_RECORD),
        json::to_python(&record, Some(1)),
    )
    .unwrap();
    let read = MeasuredCase::read(&dir.join("timing"), &case, &input).unwrap();
    assert_eq!(read.input, input);
    assert_eq!(
        read.memory_arguments(&input).last(),
        Some(&python::path(&input))
    );
    let copies = InputCopies(dir.join("copies"));
    let copy = read.copy_input(&copies.0).unwrap();
    assert_eq!(fs::read(&copy).unwrap(), b"pixels");
    assert_eq!(copy, dir.join("copies/noise/noise-64x48-2.rgba"));
    drop(copies);
    assert!(
        !dir.join("copies").exists(),
        "copies are removed with the run"
    );
    let other = json::parse(r#"{"id": "noise", "content": "bars"}"#).unwrap();
    assert!(MeasuredCase::read(&dir.join("timing"), &other, &input).is_err());
    fs::write(&input, b"changed").unwrap();
    let error = MeasuredCase::read(&dir.join("timing"), &case, &input)
        .err()
        .unwrap();
    assert!(error.contains("changed since timing"), "{error}");
    fs::remove_dir_all(dir).unwrap();
}

/// A gate run that reuses a case's timing restores the records that run wrote
/// from its kept set, and the memory stage measures those cases: at this
/// run's copy of each input, on another host of the class.
#[test]
fn a_memory_run_measures_the_cases_of_a_reused_timing_run() {
    use crate::harness::reuse;
    let root = crate::walk::scratch("capture-reused-timing");
    let case =
        json::parse(r#"{"id": "c01", "content": "noise", "width": 64, "height": 48, "frames": 2}"#)
            .unwrap();
    // The sources a run hashes include files its gate key does not bind, such as
    // the capture crate's README, which may change between the two runs.
    let identity = |node: &str, candidate: &str, readme: &str| {
        Value::object([
            ("source_digest", Value::str(readme)),
            (
                "source_manifest",
                Value::object([("crates/capture/README.md", Value::str(readme))]),
            ),
            ("candidate_sha256", Value::str(candidate)),
            ("cpus", Value::str("1")),
            (
                "host",
                Value::object([
                    ("node", Value::str(node)),
                    ("release", Value::str("7.0")),
                    ("version", Value::str(format!("#1 {node}"))),
                ]),
            ),
            ("stage", Value::str("timing")),
            ("utc", Value::float(1.0)),
        ])
    };
    // What the earlier run's timing stage wrote, at its own revision's paths.
    let earlier = root.join("earlier");
    let timed = earlier.join(".research/gate/old/capture-inputs/noise-64x48-2.rgba");
    fs::create_dir_all(timed.parent().unwrap()).unwrap();
    fs::write(&timed, b"pixels").unwrap();
    let step = earlier.join("run/capture-c01-timing");
    fs::create_dir_all(step.join("c01")).unwrap();
    let record = Value::object([
        ("case", case.clone()),
        ("input", Value::str(python::path(&timed))),
        ("input_sha256", Value::str(file_sha(&timed).unwrap())),
        (
            "correctness_summary",
            Value::object([("pipeline", Value::Bool(true))]),
        ),
        ("runtime", Value::object([("gate_pass", Value::Bool(true))])),
    ]);
    fs::write(
        step.join("c01").join(TIMING_RECORD),
        json::to_python(&record, Some(1)),
    )
    .unwrap();
    fs::write(
        step.join("identity.json"),
        json::to_python(&identity("ip-a", "c0", "readme-1"), Some(1)),
    )
    .unwrap();
    let key = reuse::Key {
        sha256: "t".repeat(64),
        components: Value::object([("command", Value::str("capture qualify"))]),
    };
    reuse::record(&step, &key, 0, None).unwrap();
    // That run's outputs, packed, kept and in this host's cache.
    let pack = root.join("pack.tar.gz");
    crate::evidence::pack_command(&earlier.join("run"), &pack).unwrap();
    let sha = file_sha(&pack).unwrap();
    let cache = root.join(".research/evidence-cache");
    fs::create_dir_all(&cache).unwrap();
    fs::rename(&pack, cache.join(format!("{sha}.tar.gz"))).unwrap();
    let set = Value::object([
        ("name", Value::str("earlier-run")),
        ("retention", Value::str("retained")),
        (
            "key",
            Value::str(format!("evidence/earlier-run/{sha}.tar.gz")),
        ),
        ("sha256", Value::str(&sha)),
        (
            "results",
            Value::Array(vec![Value::object([
                ("path", Value::str("capture-c01-timing/gate-result.json")),
                ("command", Value::str("capture qualify")),
                ("key", Value::str(&key.sha256)),
                ("status", Value::int(0)),
            ])]),
        ),
    ]);
    let index = root.join(crate::evidence::SETS).join("earlier-run.json");
    fs::create_dir_all(index.parent().unwrap()).unwrap();
    json::write(&index, &set).unwrap();

    // This run reuses the timing step: its records are restored, nothing is timed.
    let timing = root.join(".research/gate/new/capture-c01-timing");
    let status =
        reuse::restoring_step(&root, &timing, &key, false, || panic!("timed again")).unwrap();
    assert_eq!(status, 0);
    assert!(reuse::was_reused(&timing));
    assert!(timing.join("c01").join(TIMING_RECORD).is_file());
    assert!(timing.join("identity.json").is_file());

    // Its memory stage reads them, at this run's copy of the input, though the
    // README changed and the host is another of the class.
    let inputs = root.join(".research/gate/new/capture-inputs");
    fs::create_dir_all(&inputs).unwrap();
    fs::write(inputs.join("noise-64x48-2.rgba"), b"pixels").unwrap();
    let options = Options {
        candidate: String::new(),
        reference: String::new(),
        reference_small: None,
        oracle_bin: String::new(),
        observer: String::new(),
        cases: PathBuf::new(),
        only: vec!["c01".into()],
        inputs: inputs.clone(),
        out: root.join(".research/gate/new/capture-c01"),
        pairs: 30,
        iterations: 3,
        memory_pairs: 30,
        seed: 0,
        early_stop: true,
        stage: Stage::Memory {
            timing: timing.clone(),
        },
    };
    let mut now = identity("ip-b", "c0", "readme-2");
    if let Value::Object(fields) = &mut now {
        fields.retain(|(k, _)| k != "stage");
        fields.push(("stage".into(), Value::str("memory")));
    }
    let cases = [case.clone()];
    let measured = timed_cases(&root, &options, &timing, &now, &cases).unwrap();
    assert_eq!(measured.len(), 1);
    assert_eq!(measured[0].input, inputs.join("noise-64x48-2.rgba"));
    assert_eq!(
        measured[0].correctness().get("pipeline"),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        measured[0].memory_arguments(&measured[0].input).last(),
        Some(&python::path(&inputs.join("noise-64x48-2.rgba")))
    );
    // Other workers, another kernel, or other input bytes are refused.
    let mut kernel = now.clone();
    if let Value::Object(fields) = &mut kernel
        && let Some((_, Value::Object(host))) = fields.iter_mut().find(|(k, _)| k == "host")
    {
        host.retain(|(k, _)| k != "release");
        host.push(("release".into(), Value::str("7.1")));
    }
    assert!(timed_cases(&root, &options, &timing, &kernel, &cases).is_err());
    let other = identity("ip-b", "c1", "readme-2");
    assert!(timed_cases(&root, &options, &timing, &other, &cases).is_err());
    fs::write(inputs.join("noise-64x48-2.rgba"), b"other").unwrap();
    let error = timed_cases(&root, &options, &timing, &now, &cases)
        .err()
        .unwrap();
    assert!(error.contains("changed since timing"), "{error}");
    // Only a reused timing run may come from another host or other sources.
    let measured_here = root.join("measured-here");
    fs::create_dir_all(&measured_here).unwrap();
    for file in ["identity.json", "c01/timing.json"] {
        let to = measured_here.join(file);
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::copy(timing.join(file), to).unwrap();
    }
    fs::write(inputs.join("noise-64x48-2.rgba"), b"pixels").unwrap();
    reuse::record(&measured_here, &key, 0, None).unwrap();
    let here = |readme: &str| {
        let mut identity = identity("ip-a", "c0", readme);
        if let Value::Object(fields) = &mut identity {
            fields.retain(|(k, _)| k != "stage");
            fields.push(("stage".into(), Value::str("memory")));
        }
        identity
    };
    assert!(timed_cases(&root, &options, &measured_here, &here("readme-1"), &cases).is_ok());
    assert!(timed_cases(&root, &options, &measured_here, &here("readme-2"), &cases).is_err());
    assert!(timed_cases(&root, &options, &measured_here, &now, &cases).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn timed_outputs_must_be_each_workers_checked_stream() {
    let outputs = json::parse(
        r#"{"candidate": {"bytes": 188, "fnv1a64": "00000000000000ab"},
                "reference": {"bytes": 188, "fnv1a64": "00000000000000ff"}}"#,
    )
    .unwrap();
    assert!(outputs_match(&warmups(0xab), &outputs));
    assert!(!outputs_match(&warmups(0xff), &outputs));
    assert!(!outputs_match(&warmups(0xab), &Value::Object(vec![])));
}

#[test]
fn open_cases_are_listed_per_target_and_must_be_cases() {
    let contract = json::parse(
        r#"{"open_cases": [
                {"id": "c01", "targets": ["aarch64-unknown-linux-gnu"]},
                {"id": "c02", "targets": ["thumbv7neon-unknown-linux-gnueabihf"]}]}"#,
    )
    .unwrap();
    let ids = ["c01", "c02", "c03"];
    assert_eq!(
        listed_open(&contract, "aarch64-unknown-linux-gnu", &ids).unwrap(),
        ["c01"]
    );
    assert!(
        listed_open(&contract, "aarch64-unknown-linux-gnu", &ids[1..])
            .unwrap_err()
            .contains("c01")
    );
    assert!(
        listed_open(&Value::Object(vec![]), "x", &ids)
            .unwrap()
            .is_empty()
    );
}
