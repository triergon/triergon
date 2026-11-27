use super::*;

struct Temporary(PathBuf);

impl Temporary {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "triergon-control-proof-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn progressive_controls_keep_an_existing_grade() {
    let directory = Temporary::new();
    let mut selected = options(root(), &[]).unwrap();
    selected.output = directory.0.clone();
    let controls = directory.0.join("controls");
    fs::create_dir(&controls).unwrap();
    let grade = controls.join("report.json");
    fs::write(&grade, b"existing grade\n").unwrap();
    let error = check_controls(root(), &selected).unwrap_err();
    assert!(error.contains("use a new output directory"));
    assert_eq!(fs::read(&grade).unwrap(), b"existing grade\n");
}

fn set(value: &mut Value, key: &str, replacement: Value) -> Result<(), String> {
    let Value::Object(fields) = value else {
        return Err("expected an object".into());
    };
    fields.retain(|(name, _)| name != key);
    fields.push((key.to_owned(), replacement));
    Ok(())
}

fn change(value: &mut Value, path: &[&str], replacement: Value) {
    let (key, parents) = path.split_last().unwrap();
    let mut node = value;
    for key in parents {
        node = match node {
            Value::Object(fields) => {
                &mut fields.iter_mut().find(|(name, _)| name == key).unwrap().1
            }
            Value::Array(values) => &mut values[key.parse::<usize>().unwrap()],
            _ => panic!("invalid test path"),
        };
    }
    set(node, key, replacement).unwrap();
}

// Functional-proof metadata only: these fixtures have no timing, memory,
// observer or native gate evidence. Raw traces are checked separately.
fn proof_fixture(identity: Value) -> (Value, Value, Value) {
    let controls = json::read(&root().join(FEATURE).join("controls.json")).unwrap();
    let reference = comparison::reference_named(&identity, "ffmpeg-formats-native").unwrap();
    let binding = proof_binding(root(), &identity, &reference).unwrap();
    let cases = rows(&controls)
        .unwrap()
        .iter()
        .map(|case| {
            let outcomes = ["candidate", "reference"].map(|role| {
                let expected = case.get("outcomes").unwrap().get(role).unwrap();
                (
                    role,
                    Value::object([
                        ("expected", expected.clone()),
                        ("exit", expected.get("exit").unwrap().clone()),
                        (
                            "stdout_sha256",
                            expected.get("stdout_sha256").unwrap().clone(),
                        ),
                        ("stderr_sha256", Value::str(sha256::hex(b""))),
                        ("stdout", Value::str("trace.stdout")),
                        ("stderr", Value::str("trace.stderr")),
                        ("passed", Value::Bool(true)),
                        ("error", Value::Null),
                    ]),
                )
            });
            Value::object([
                ("id", case.get("id").unwrap().clone()),
                ("input_sha256", case.get("sha256").unwrap().clone()),
                ("outcomes", Value::object(outcomes)),
            ])
        })
        .collect();
    let proof = Value::object([
        ("schema", Value::int(1)),
        ("binding", binding.clone()),
        ("cases", Value::Array(cases)),
        ("complete", Value::Bool(true)),
        ("passed", Value::Bool(true)),
        ("context_error", Value::Null),
    ]);
    (controls, binding, proof)
}

fn proof_identity() -> Value {
    Value::object([
        ("source_revision", Value::str("test source binding")),
        ("instrumentation", Value::Object(Vec::new())),
        (
            "additional_candidates",
            Value::object([(
                "playback-worker",
                Value::object([
                    ("sha256", Value::str("a".repeat(64))),
                    ("selected_graph", Value::str("test source graph")),
                ]),
            )]),
        ),
        (
            "comparison_references",
            Value::object([(
                "ffmpeg-formats-native",
                Value::object([
                    ("path", Value::str("target/harness/ffmpeg-formats-worker")),
                    ("sha256", Value::str("b".repeat(64))),
                ]),
            )]),
        ),
    ])
}

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn all_ten_declared_outcomes_and_every_control_are_required() {
    let (controls, binding, proof) = proof_fixture(proof_identity());
    recorded_cases(&proof, &controls, &binding).unwrap();
    for index in 0..5 {
        for role in ["candidate", "reference"] {
            for (field, replacement) in [
                ("passed", Value::Bool(false)),
                ("passed", Value::int(1)),
                ("error", Value::str("worker failed")),
                ("exit", Value::int(9)),
                ("stdout_sha256", Value::str("c".repeat(64))),
                ("expected", Value::Null),
            ] {
                let mut changed = proof.clone();
                change(
                    &mut changed,
                    &["cases", &index.to_string(), "outcomes", role, field],
                    replacement,
                );
                assert!(
                    recorded_cases(&changed, &controls, &binding).is_err(),
                    "{index}/{role}/{field}"
                );
            }
        }
        for field in ["id", "input_sha256"] {
            let mut changed = proof.clone();
            change(
                &mut changed,
                &["cases", &index.to_string(), field],
                Value::str("changed"),
            );
            assert!(recorded_cases(&changed, &controls, &binding).is_err());
        }
        let mut changed = proof.clone();
        if let Value::Object(fields) = &mut changed {
            let Value::Array(cases) = &mut fields
                .iter_mut()
                .find(|(name, _)| name == "cases")
                .unwrap()
                .1
            else {
                panic!()
            };
            cases.remove(index);
        }
        assert!(recorded_cases(&changed, &controls, &binding).is_err());
    }
    for replacement in [
        Value::Object(Vec::new()),
        Value::object([("candidate", Value::Null)]),
        Value::object([
            ("candidate", Value::Null),
            ("reference", Value::Null),
            ("extra", Value::Null),
        ]),
    ] {
        let mut changed = proof.clone();
        change(&mut changed, &["cases", "4", "outcomes"], replacement);
        assert!(recorded_cases(&changed, &controls, &binding).is_err());
    }
    for (field, replacement) in [
        ("complete", Value::Bool(false)),
        ("passed", Value::Bool(false)),
        ("context_error", Value::str("source changed")),
    ] {
        let mut changed = proof.clone();
        set(&mut changed, field, replacement).unwrap();
        assert!(recorded_cases(&changed, &controls, &binding).is_err());
    }
}

#[test]
fn each_collection_uses_its_own_identity_and_worker_bindings() {
    let identity = proof_identity();
    let (controls, timing_binding, timed) = proof_fixture(identity.clone());
    let mut memory_identity = identity;
    change(
        &mut memory_identity,
        &["instrumentation", "rss_kernel_bound"],
        Value::str("test memory proof pin"),
    );
    let (_, memory_binding, memory) = proof_fixture(memory_identity);
    recorded_cases(&timed, &controls, &timing_binding).unwrap();
    recorded_cases(&memory, &controls, &memory_binding).unwrap();
    assert!(recorded_cases(&timed, &controls, &memory_binding).is_err());
    assert!(recorded_cases(&memory, &controls, &timing_binding).is_err());
    for path in [
        vec!["identity_sha256"],
        vec!["candidate_sha256"],
        vec!["controls_sha256"],
        vec!["contract_sha256"],
        vec!["corpus_manifest_sha256"],
        vec!["reference", "sha256"],
        vec!["identity", "source_revision"],
        vec![
            "identity",
            "additional_candidates",
            "playback-worker",
            "selected_graph",
        ],
    ] {
        let mut changed = timed.clone();
        let mut full = vec!["binding"];
        full.extend(path);
        change(&mut changed, &full, Value::str("changed"));
        assert!(recorded_cases(&changed, &controls, &timing_binding).is_err());
    }
}

#[test]
fn duplicate_or_extra_controls_are_not_a_complete_precheck() {
    let (controls, binding, proof) = proof_fixture(proof_identity());
    let mut duplicate = rows(&proof).unwrap().to_vec();
    duplicate[1] = duplicate[0].clone();
    let mut extra = rows(&proof).unwrap().to_vec();
    extra.push(extra[0].clone());
    for cases in [Vec::new(), duplicate, extra] {
        let mut changed = proof.clone();
        set(&mut changed, "cases", Value::Array(cases)).unwrap();
        assert!(recorded_cases(&changed, &controls, &binding).is_err());
    }
}

#[test]
fn raw_control_files_and_their_declared_hashes_are_rechecked() {
    let directory = Temporary::new();
    let (controls, binding, mut proof) = proof_fixture(proof_identity());
    recorded_cases(&proof, &controls, &binding).unwrap();
    let output = directory.0.join("trace.stdout");
    let errors = directory.0.join("trace.stderr");
    fs::write(&output, b"altered trace\n").unwrap();
    fs::write(&errors, b"").unwrap();
    let recorded = rows(&proof).unwrap();
    assert!(
        check_artifacts(
            &directory.0,
            &directory.0.join("report.json"),
            recorded,
            &controls
        )
        .unwrap_err()
        .contains("hash mismatch")
    );
    change(
        &mut proof,
        &["cases", "0", "outcomes", "candidate", "stdout_sha256"],
        Value::str(sha256::hex(b"altered trace\n")),
    );
    assert!(
        check_artifacts(
            &directory.0,
            &directory.0.join("report.json"),
            rows(&proof).unwrap(),
            &controls
        )
        .unwrap_err()
        .contains("literal stdout")
    );
    fs::remove_file(&output).unwrap();
    assert!(
        check_artifacts(
            &directory.0,
            &directory.0.join("report.json"),
            rows(&proof).unwrap(),
            &controls
        )
        .is_err()
    );
}

#[test]
fn missing_or_rehashed_proof_is_rejected() {
    let directory = Temporary::new();
    let selected = options(root(), &[]).unwrap();
    let path = directory.0.join("proof with spaces.json");
    fs::write(&path, b"{}\n").unwrap();
    let report = Value::object([(
        "functional_controls",
        Value::object([
            ("passed", Value::Bool(true)),
            ("path", Value::str(path.to_string_lossy())),
            ("sha256", Value::str("f".repeat(64))),
        ]),
    )]);
    let contract = json::read(&selected.contract).unwrap();
    let manifest = json::read(&selected.manifest).unwrap();
    assert!(
        validate_report_controls(
            root(),
            &directory.0.join("timing.json"),
            &report,
            &contract,
            &manifest
        )
        .unwrap_err()
        .contains("proof hash mismatch")
    );
    let mut failed = report;
    change(
        &mut failed,
        &["functional_controls", "passed"],
        Value::Bool(false),
    );
    assert!(validate_report_controls(root(), &path, &failed, &contract, &manifest).is_err());
}

#[test]
fn rerun_preserves_existing_reports_and_controls() {
    let directory = Temporary::new();
    let output = directory.0.join("output");
    let saved = [
        "report.json",
        "collection.json",
        "parity/report.json",
        "controls/report.json",
    ];
    for name in saved {
        let path = output.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, name).unwrap();
    }
    let args = args(&["--output", output.to_str().unwrap()]);
    assert!(
        main(root(), &args)
            .unwrap_err()
            .contains("use a new output directory")
    );
    for name in saved {
        assert_eq!(fs::read_to_string(output.join(name)).unwrap(), name);
    }
}

#[test]
fn original_three_timing_cases_and_five_functional_controls_keep_the_gates() {
    let options = options(root(), &[]).unwrap();
    validate(root(), &options).unwrap();
    let contract = json::read(&options.contract).unwrap();
    for field in [
        "runtime_ratio_max",
        "runtime_optimization_ratio_target",
        "memory_ratio_max_exclusive",
        "runtime_policy",
    ] {
        assert!(
            contract.get(field).is_none(),
            "duplicate policy field: {field}"
        );
    }
    assert_eq!(contract.get("iterations"), Some(&Value::int(50)));
    assert_eq!(contract.get("minimum_pairs"), Some(&Value::int(30)));
    assert_eq!(
        contract.get("required_profiles"),
        Some(&Value::strings(TIMED))
    );
    let controls = json::read(&root().join(FEATURE).join("controls.json")).unwrap();
    assert_eq!(rows(&controls).unwrap().len(), 5);
    let refusal = rows(&controls).unwrap()[4]
        .get("outcomes")
        .unwrap()
        .get("reference")
        .unwrap();
    assert_eq!(refusal.get("exit"), Some(&Value::int(1)));
}

#[test]
fn report_options_cannot_narrow_the_work_or_claim_baseline_acceptance() {
    let options = options(
        root(),
        &args(&["--benchmark", "--memory", "--pairs", "100"]),
    )
    .unwrap();
    assert!(options.benchmark && options.memory);
    assert_eq!(options.pairs, 100);
    for flags in [
        args(&["--case", "continuous-44100"]),
        args(&["--manifest", "subset.json"]),
        args(&["--iterations", "1"]),
        args(&["--memory", "--pairs", "29"]),
        args(&["--baseline-worker", "other", "--require-slice-acceptance"]),
    ] {
        assert!(super::options(root(), &flags).is_err(), "{flags:?}");
    }
}

#[test]
fn a_failed_correctness_report_prevents_timing_and_memory_collection() {
    let selected = options(root(), &args(&["--benchmark", "--memory"])).unwrap();
    let mut calls = Vec::new();
    let mut controls = 0;
    let result = collect(
        root(),
        &selected,
        |_, request, proof| {
            assert_eq!(proof.and_then(Value::as_str), Some("proof"));
            calls.push((request.benchmark, request.memory));
            assert_eq!(request.manifest, selected.manifest);
            assert_eq!(request.output, selected.output.join("parity"));
            assert!(!request.require_acceptance && !request.require_slice_acceptance);
            Ok(1)
        },
        |_, _| {
            controls += 1;
            Ok(Some(Value::str("proof")))
        },
    );
    assert_eq!(result.unwrap(), 1);
    assert_eq!(calls, [(false, false)]);
    assert_eq!(controls, 1);
}

#[test]
fn complete_correctness_admits_the_selected_standard_report_unchanged() {
    let selected = options(
        root(),
        &args(&["--benchmark", "--memory", "--pairs", "100"]),
    )
    .unwrap();
    let mut calls = Vec::new();
    let result = collect(
        root(),
        &selected,
        |_, request, proof| {
            assert_eq!(proof.and_then(Value::as_str), Some("proof"));
            calls.push((request.benchmark, request.memory));
            assert_eq!(request.manifest, selected.manifest);
            assert_eq!(request.corpus, selected.corpus);
            if request.benchmark {
                assert_eq!(request.pairs, 100);
                assert_eq!(request.output, selected.output);
            }
            Ok(0)
        },
        |_, _| Ok(Some(Value::str("proof"))),
    );
    assert_eq!(result.unwrap(), 0);
    assert_eq!(calls, [(false, false), (true, true)]);
}

#[test]
fn a_failed_functional_control_prevents_all_measurement() {
    let selected = options(root(), &args(&["--benchmark", "--memory"])).unwrap();
    let mut calls = Vec::new();
    let code = collect(
        root(),
        &selected,
        |_, request, proof| {
            assert!(proof.is_none());
            calls.push((request.benchmark, request.memory));
            Ok(0)
        },
        |_, _| Ok(None),
    )
    .unwrap();
    assert_eq!(code, 1);
    assert_eq!(calls, [(false, false)]);
}

#[test]
fn plain_correctness_also_requires_the_functional_precheck() {
    let selected = options(root(), &[]).unwrap();
    let mut calls = 0;
    let mut controls = 0;
    let code = collect(
        root(),
        &selected,
        |_, _, proof| {
            assert!(proof.is_none());
            calls += 1;
            Ok(0)
        },
        |_, _| {
            controls += 1;
            Ok(None)
        },
    )
    .unwrap();
    assert_eq!((code, calls, controls), (1, 1, 1));
}

#[test]
fn an_incomplete_control_precheck_cannot_start_measurement() {
    let selected = options(root(), &args(&["--benchmark", "--memory"])).unwrap();
    let mut calls = 0;
    let result = collect(
        root(),
        &selected,
        |_, request, proof| {
            assert!(proof.is_none());
            assert!(!request.benchmark && !request.memory);
            calls += 1;
            Ok(0)
        },
        |_, _| Err("control worker timed out or could not start".into()),
    );
    assert!(result.is_err());
    assert_eq!(calls, 1);
}

fn refusal() -> (Value, worker::Finished) {
    let manifest = json::read(&root().join(FEATURE).join("controls.json")).unwrap();
    let case = rows(&manifest).unwrap()[4].clone();
    // The real partial trace is checked through the public worker in the CLI.
    let finished = worker::Finished {
            code: 1,
            stdout: b"recorded partial trace\n".to_vec(),
            stderr: b"reference stage: read progressive packet\nreference: Invalid data found when processing input (-1094995529)\n".to_vec(),
        };
    (case, finished)
}

#[test]
fn refusal_requires_its_exit_partial_trace_and_specific_diagnostics() {
    let (case, mut finished) = refusal();
    let mut expected = case
        .get("outcomes")
        .unwrap()
        .get("reference")
        .unwrap()
        .clone();
    if let Value::Object(fields) = &mut expected {
        fields.retain(|(key, _)| key != "stdout_sha256");
        fields.push((
            "stdout_sha256".into(),
            Value::str(sha256::hex(&finished.stdout)),
        ));
    }
    check_trace(&case, &expected, &finished).unwrap();
    for exit in [0, 2, -11] {
        finished.code = exit;
        assert!(check_trace(&case, &expected, &finished).is_err());
    }
    finished.code = 1;
    finished.stderr = b"reference: another error\n".to_vec();
    assert!(check_trace(&case, &expected, &finished).is_err());
    let (_, original) = refusal();
    finished.stderr = original.stderr;
    finished.stdout.push(b'x');
    assert!(check_trace(&case, &expected, &finished).is_err());
}

#[test]
fn successful_literal_trace_also_requires_the_complete_normative_footer() {
    let manifest = json::read(&root().join(FEATURE).join("controls.json")).unwrap();
    let case = &rows(&manifest).unwrap()[0];
    let footer = Value::object([
        ("schema", Value::int(1)),
        ("operation", Value::str(OPERATION)),
        ("iterations", Value::int(1)),
        ("elapsed_ns", Value::int(0)),
        ("eof", Value::Bool(true)),
        ("packets", Value::int(294)),
        ("bytes", Value::int(22945)),
        ("digest", Value::int(1_094_976_622_303_096_385u64)),
    ]);
    let stdout = format!("literal trace\n{}\n", json::to_python(&footer, None)).into_bytes();
    let mut finished = worker::Finished {
        code: 0,
        stdout,
        stderr: Vec::new(),
    };
    let expected = Value::object([
        ("exit", Value::int(0)),
        ("stdout_sha256", Value::str(sha256::hex(&finished.stdout))),
    ]);
    check_trace(case, &expected, &finished).unwrap();
    finished.stdout = b"literal trace\n{\"packets\":294}\n".to_vec();
    let expected = Value::object([
        ("exit", Value::int(0)),
        ("stdout_sha256", Value::str(sha256::hex(&finished.stdout))),
    ]);
    assert!(check_trace(case, &expected, &finished).is_err());
}
