use super::*;
use crate::harness::comparison::reference_record;

#[test]
fn an_unmeasured_isobmff_output_must_match_as_an_output() {
    let row = |comparison| {
        Value::object([
            ("outcome", Value::str("output")),
            (
                "parity",
                Value::object([
                    ("status", Value::str("pass")),
                    ("source_timing_status", Value::str("pass")),
                    ("comparison", Value::str(comparison)),
                ]),
            ),
        ])
    };
    let output = row("output");
    let refusal = row("refusal");
    assert!(unmeasured_output(&output, &output).is_ok());
    assert!(unmeasured_output(&refusal, &refusal).is_err());
    assert!(unmeasured_output(&output, &refusal).is_err());
    let mut unexpected_measurement = output.clone();
    put(&mut unexpected_measurement, "timing", Value::Bool(true)).unwrap();
    assert!(unmeasured_output(&output, &unexpected_measurement).is_err());
}

#[test]
fn isobmff_parts_bind_the_registered_selection_and_cover_all_outputs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut membership = BTreeSet::new();
    for index in 0..3 {
        let requested = (index, 3);
        let (contract, selection) =
            crate::isobmff_bench::full_part_selection(root, requested).unwrap();
        assert!(selection.iter().all(|&case| membership.insert(case)));
        let report = object(&[
            ("part", Value::str(format!("{}/3", index + 1))),
            (
                "selection",
                Value::Array(selection.iter().map(|&i| Value::int(i as i128)).collect()),
            ),
        ]);
        assert_eq!(
            part_selection(root, &contract, &report, &report, Some(requested)).unwrap(),
            Some(selection.into_iter().collect())
        );
        assert!(part_selection(root, &contract, &report, &report, None).is_err());
        let mut incomplete = report.clone();
        put(&mut incomplete, "selection", Value::Array(Vec::new())).unwrap();
        assert!(part_selection(root, &contract, &report, &incomplete, Some(requested)).is_err());
        let mut mislabeled = report.clone();
        put(&mut mislabeled, "part", Value::str("1/1")).unwrap();
        assert!(part_selection(root, &contract, &report, &mislabeled, Some(requested)).is_err());
    }
    assert_eq!(membership.len(), 972);
}

#[test]
fn an_early_stopped_collection_counts_its_planned_pairs() {
    let record = |pairs: i64, planned: Option<i64>| {
        let mut fields = vec![
            ("pairs", Value::int(pairs)),
            (
                "samples",
                Value::Array((0..pairs).map(|_| Value::Object(vec![])).collect()),
            ),
        ];
        if let Some(planned) = planned {
            fields.push((
                "early_stop",
                Value::object([
                    ("rule", Value::str(timing::EARLY_STOP_RULE)),
                    ("planned_pairs", Value::int(planned)),
                ]),
            ));
        }
        Value::object(fields)
    };
    assert_eq!(samples(&record(20, Some(30)), 30, false).unwrap().len(), 20);
    assert!(samples(&record(20, Some(20)), 30, false).is_err());
    assert!(samples(&record(20, None), 30, false).is_err());
    assert!(samples(&record(30, None), 30, false).is_ok());
}

#[test]
fn grading_cannot_reuse_a_collection_directory() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let args = [
        "unused-timing.json",
        "unused-memory.json",
        "--contract",
        "unused-contract.json",
        "--manifest",
        "unused-manifest.json",
        "--output",
        "harness/features/progressive-ts188",
    ]
    .map(str::to_owned);
    let error = main(root, &args).unwrap_err();
    assert!(error.contains("use a new output directory"));
}

#[test]
fn ordinary_contracts_do_not_require_functional_control_reports() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut ordinary = 0;
    for feature in fs::read_dir(root.join("harness/features")).unwrap() {
        let contract = feature.unwrap().path().join("contract.json");
        if !contract.is_file() {
            continue;
        }
        let contract = json::read(&contract).unwrap();
        if contract.get("functional_controls").is_some() {
            continue;
        }
        control_proofs(
            Path::new("unused"),
            &contract,
            &Value::Null,
            (Path::new("timing"), &Value::Null),
            (Path::new("memory"), &Value::Null),
        )
        .unwrap();
        ordinary += 1;
    }
    assert!(ordinary >= 8);
}

#[test]
fn progressive_grading_requires_functional_proof_before_numeric_gates() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let feature = root.join("harness/features/progressive-ts188");
    let contract = json::read(&feature.join("contract.json")).unwrap();
    let manifest = json::read(&feature.join("corpus.json")).unwrap();
    let error = control_proofs(
        root,
        &contract,
        &manifest,
        (Path::new("timing"), &Value::Null),
        (Path::new("memory"), &Value::Null),
    )
    .unwrap_err();
    assert!(error.contains("timing functional controls: missing functional controls"));
}

fn digest(byte: char) -> String {
    std::iter::repeat_n(byte, 64).collect()
}

fn object(fields: &[(&str, Value)]) -> Value {
    Value::object(fields.iter().cloned())
}

fn change(value: &mut Value, path: &[&str], key: &str, new: Value) {
    let mut node = value;
    for step in path {
        let Value::Object(fields) = node else {
            panic!("object at {step}");
        };
        node = &mut fields.iter_mut().find(|(key, _)| key == step).unwrap().1;
    }
    put(node, key, new).unwrap();
}

fn identity() -> Value {
    object(&[
        ("schema", Value::int(2)),
        ("rustc", Value::str("compiler record")),
        ("compiler", Value::str("reference compiler record")),
        ("compiler_command", Value::strings(["cc", "reference.c"])),
        ("candidate_command", Value::strings(["cargo", "build"])),
        ("build_environment", object(&[])),
        ("cargo_configuration", Value::Array(vec![])),
        (
            "selected_graph",
            object(&[("sha256", Value::str(digest('d')))]),
        ),
        ("source_revision", Value::str("reference revision")),
        (
            "reference_build",
            object(&[("configuration", Value::str("reference settings"))]),
        ),
        (
            "binaries",
            object(&[
                ("candidate", Value::str(digest('a'))),
                ("reference", Value::str(digest('b'))),
            ]),
        ),
        (
            "instrumentation",
            object(&[
                ("observer_sha256", Value::str(digest('c'))),
                ("probe_sha256", Value::str(digest('d'))),
            ]),
        ),
    ])
}

// These rows exercise joining only. The memory status is unqualified and
// there is no observer/kernel proof or runtime interval that could pass a gate.
struct Fixture {
    timed: Value,
    measured: Value,
    expected: Value,
    contract: Value,
    identity: Value,
}

impl Fixture {
    fn new() -> Self {
        let identity = identity();
        let contract = object(&[
            ("id", Value::str("matroska-demux")),
            ("minimum_pairs", Value::int(30)),
        ]);
        let expected = object(&[
            ("id", Value::str("case")),
            ("profile", Value::str("case")),
            ("sha256", Value::str(digest('e'))),
        ]);
        let output = object(&[
            ("packets", Value::int(2)),
            ("bytes", Value::int(40)),
            ("digest", Value::int(7)),
        ]);
        let mut observations = Vec::new();
        let mut observe = |stage: String| {
            observations.push(object(&[
                ("stage", Value::str(stage)),
                ("state", object(&[("status", Value::str("not-applicable"))])),
            ]));
        };
        observe("before-timing-session".into());
        for role in ["candidate", "reference"] {
            observe(format!("before-warmup:{role}"));
            observe(format!("after-warmup:{role}"));
        }
        // An idle host's CPU record, valid whether or not Linux evidence is required.
        let cpu = object(&[(
            super::super::acceptance::LINUX_CPU_KEY,
            object(&[
                ("status", Value::str("recorded")),
                ("boot_id", Value::str("boot")),
                ("cpus", Value::strings(["cpu0"])),
                ("ticks_per_second", Value::int(100)),
                ("counters", Value::Array(vec![Value::int(0); 8])),
            ]),
        )]);
        let mut timed_raw = Vec::new();
        for pair in 0..30 {
            let mut data = output.clone();
            put(&mut data, "packets", Value::int(400)).unwrap();
            put(&mut data, "bytes", Value::int(8000)).unwrap();
            put(&mut data, "digest", Value::int(19)).unwrap();
            put(&mut data, "iterations", Value::int(200)).unwrap();
            put(&mut data, "elapsed_ns", Value::int(100)).unwrap();
            let worker = object(&[("status", Value::str("ok")), ("data", data)]);
            timed_raw.push(object(&[
                ("pair", Value::int(pair)),
                ("order", Value::strings(["candidate", "reference"])),
                ("candidate", worker.clone()),
                ("reference", worker),
                ("cpu_before", cpu.clone()),
                ("cpu_after", cpu.clone()),
            ]));
            for role in ["candidate", "reference"] {
                observe(format!("before-pair:{pair}:{role}"));
                observe(format!("after-pair:{pair}:{role}"));
            }
        }
        let row = object(&[
            ("id", Value::str("case")),
            ("profile", Value::str("case")),
            ("input_sha256", Value::str(digest('e'))),
            (
                "reference",
                reference_summary(&reference_record(&identity, None).unwrap()),
            ),
            (
                "operation",
                object(&[
                    ("name", Value::str("container-demux-v1")),
                    ("seek_target_ns", Value::Null),
                    ("view", Value::str("source-frames-ffmpeg9-clock-v1")),
                ]),
            ),
            (
                "parity",
                object(&[
                    ("status", Value::str("pass")),
                    ("source_timing_status", Value::str("pass")),
                ]),
            ),
            (
                "retained_record",
                object(&[("source", Value::str("complete"))]),
            ),
        ]);
        let mut timed = row.clone();
        put(
            &mut timed,
            "timing",
            object(&[
                ("status", Value::str("measured")),
                ("pairs", Value::int(30)),
                ("iterations", Value::int(200)),
                ("seed", Value::int(0)),
                (
                    "ratio",
                    timing::interval_value(
                        &crate::stats::ratio_interval(&[(100.0, 100.0); 30], 0, 4000).unwrap(),
                    ),
                ),
                ("input_sha256", Value::str(digest('e'))),
                (
                    "worker_hashes",
                    field(&identity, "binaries").unwrap().clone(),
                ),
                (
                    "warmups",
                    Value::Array(
                        ["candidate", "reference"]
                            .map(|role| {
                                object(&[
                                    ("worker", Value::str(role)),
                                    ("result", field(&timed_raw[0], role).unwrap().clone()),
                                ])
                            })
                            .into(),
                    ),
                ),
                ("samples", Value::Array(timed_raw)),
                ("timing_module_policy", Value::str(timing::MODULE_POLICY)),
                ("timing_module_observations", Value::Array(observations)),
                (
                    "rejected_contaminated_attempts",
                    Value::Array(vec![object(&[
                        ("status", Value::str("contaminated")),
                        (
                            "samples",
                            Value::Array(vec![Value::str("raw failed sample")]),
                        ),
                    ])]),
                ),
            ]),
        )
        .unwrap();
        let memory_sample = Value::object(
            [
                "candidate_heap",
                "candidate_rss",
                "reference_heap",
                "reference_rss",
            ]
            .map(|key| (key, object(&[("work", output.clone())]))),
        );
        let mut measured = row;
        put(
            &mut measured,
            "memory",
            object(&[
                ("status", Value::str("observer-unqualified")),
                ("pairs", Value::int(30)),
                ("input_sha256", Value::str(digest('e'))),
                ("samples", Value::Array(vec![memory_sample; 30])),
                (
                    "certificate",
                    object(&[
                        ("status", Value::str("unqualified")),
                        (
                            "worker_hashes",
                            field(&identity, "binaries").unwrap().clone(),
                        ),
                        ("observer_sha256", Value::str(digest('c'))),
                        ("probe_sha256", Value::str(digest('d'))),
                    ]),
                ),
            ]),
        )
        .unwrap();
        Self {
            timed,
            measured,
            expected,
            contract,
            identity,
        }
    }

    fn join(&self) -> R<Value> {
        join_case(
            &self.timed,
            &self.measured,
            &self.expected,
            &self.contract,
            &self.identity,
            "unqualified-test",
            1.2,
            &super::super::bounds::Unmeasured::default(),
        )
    }

    fn standard_linux() -> Self {
        let mut fixture = Self::new();
        let mut memory = field(&fixture.measured, "memory").unwrap().clone();
        change(
            &mut memory,
            &["certificate"],
            "platform",
            Value::str("Linux"),
        );
        change(
            &mut memory,
            &["certificate"],
            "kernel_rss_required",
            Value::Bool(false),
        );
        let mut samples = array(&memory, "samples").unwrap().to_vec();
        for sample in &mut samples {
            for role in ["candidate_rss", "reference_rss"] {
                change(
                    sample,
                    &[role],
                    "observer",
                    object(&[("kernel_rss_requested", Value::int(0))]),
                );
            }
        }
        put(&mut memory, "samples", Value::Array(samples)).unwrap();
        put(&mut fixture.measured, "memory", memory).unwrap();
        let mut clock = field(&fixture.timed, "timing").unwrap().clone();
        let mut observations = array(&clock, "timing_module_observations")
            .unwrap()
            .to_vec();
        for observation in &mut observations {
            change(
                observation,
                &["state"],
                "status",
                Value::str("module-absent"),
            );
        }
        put(
            &mut clock,
            "timing_module_observations",
            Value::Array(observations),
        )
        .unwrap();
        put(&mut fixture.timed, "timing", clock).unwrap();
        put(
            &mut fixture.identity,
            "host_platform",
            object(&[("system", Value::str("Linux"))]),
        )
        .unwrap();
        fixture
    }
}

#[test]
fn complete_membership_rejects_missing_duplicate_extra_and_changed_inputs() {
    let fixture = Fixture::new();
    let required = [fixture.expected.clone()];
    assert!(membership(&required, std::slice::from_ref(&fixture.timed)).is_ok());
    assert!(
        membership(&required, &[])
            .unwrap_err()
            .contains("missing case")
    );
    assert!(
        membership(&required, &[fixture.timed.clone(), fixture.timed.clone()])
            .unwrap_err()
            .contains("duplicate")
    );
    let mut extra = fixture.timed.clone();
    put(&mut extra, "id", Value::str("extra")).unwrap();
    assert!(
        membership(&required, &[extra])
            .unwrap_err()
            .contains("unexpected")
    );
    let mut changed = fixture.timed.clone();
    put(&mut changed, "input_sha256", Value::str(digest('f'))).unwrap();
    assert!(
        membership(&required, &[changed])
            .unwrap_err()
            .contains("input hash")
    );
    assert!(
        membership(
            &[fixture.expected.clone(), fixture.expected],
            &[fixture.timed]
        )
        .unwrap_err()
        .contains("duplicate manifest")
    );
}

#[test]
fn every_feature_manifest_keeps_all_cases_including_nonrequired_profiles() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut matroska = 0;
    for (feature, count) in [
        ("matroska-demux", 12),
        ("matroska-header-removal", 4),
        ("matroska-zlib", 10),
        ("matroska-cues-preroll", 21),
        ("matroska-colour", 33),
        ("matroska-segment-metadata", 48),
        ("matroska-startup-seek", 45),
        ("hls-segments", 71),
        ("packet-framing", 135),
        ("isobmff-open-index", 1020),
        ("isobmff-seek", 476),
        ("font-outlines", 53),
        ("font-hinting-none", 159),
        ("font-hinting-default", 159),
        ("font-hinting-modes", 954),
        ("matroska-audio-clocks", 10),
        ("matroska-legacy-aac", 4),
        ("mp4-forward", 51),
        ("ts-program", 35),
        ("raw-annex-b", 7),
        ("mpegts-pes", 97),
        ("subtitle-pgs", 319),
        ("subtitle-dvb", 241),
        ("subtitle-dvd", 228),
        ("subtitle-dvd-host", 5),
        ("subtitle-cc", 461),
        ("subtitle-teletext", 72),
        ("subtitle-pgs-capsules", 229),
        ("subtitle-text", 367),
        ("subtitle-text-host", 23),
    ] {
        let (manifest, manifest_sha) = super::super::manifest::read(
            root,
            &root.join(format!("harness/features/{feature}/corpus.json")),
        )
        .unwrap();
        let (contract, contract_sha) =
            document(&root.join(format!("harness/features/{feature}/contract.json"))).unwrap();
        let binding = Binding {
            contract: &contract,
            manifest: &manifest,
            contract_sha: &contract_sha,
            manifest_sha: &manifest_sha,
        };
        binding.registered(root).unwrap();
        let expected = array(&manifest, "cases").unwrap();
        let actual: Vec<_> = expected
            .iter()
            .rev()
            .map(|row| {
                let mut report = object(&[
                    ("id", row.get("id").unwrap().clone()),
                    ("profile", row.get("profile").unwrap().clone()),
                    ("input_sha256", row.get("sha256").unwrap().clone()),
                ]);
                if let Some(entities) = row.get("entities") {
                    put(&mut report, "input_entities", entities.clone()).unwrap();
                }
                report
            })
            .collect();
        assert_eq!(
            membership(expected, &actual).unwrap().len(),
            count,
            "{feature}"
        );
        assert!(membership(expected, &actual[1..]).is_err(), "{feature}");
        if let Some(index) = actual
            .iter()
            .position(|row| row.get("input_entities").is_some())
        {
            let mut changed = actual.clone();
            put(&mut changed[index], "input_entities", Value::Array(vec![])).unwrap();
            assert!(membership(expected, &changed).is_err(), "{feature}");
        }
        let mut pilot = manifest.clone();
        put(&mut pilot, "cases", Value::Array(expected[1..].to_vec())).unwrap();
        let pilot_sha = sha256::hex(json::to_python(&pilot, Some(2)).as_bytes());
        let pilot_binding = Binding {
            manifest: &pilot,
            manifest_sha: &pilot_sha,
            ..binding
        };
        assert!(pilot_binding.registered(root).is_err(), "{feature}");
        let mut changed_contract = contract.clone();
        put(&mut changed_contract, "minimum_pairs", Value::int(31)).unwrap();
        let changed_sha = sha256::hex(json::to_python(&changed_contract, Some(2)).as_bytes());
        let changed_binding = Binding {
            contract: &changed_contract,
            contract_sha: &changed_sha,
            manifest: &manifest,
            manifest_sha: &manifest_sha,
        };
        assert!(changed_binding.registered(root).is_err(), "{feature}");
        if feature.starts_with("matroska-")
            && !matches!(
                feature,
                "matroska-startup-seek" | "matroska-audio-clocks" | "matroska-legacy-aac"
            )
        {
            matroska += count;
        }
    }
    let contract =
        json::read(&root.join("harness/features/matroska-segment-metadata/contract.json")).unwrap();
    assert_eq!(
        matroska
            + array(&contract, "required_additional_vectors")
                .unwrap()
                .len(),
        129
    );
}

#[test]
fn aliases_require_registered_identical_operations_and_separate_traces() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let manifest =
        json::read(&root.join("harness/features/isobmff-open-index/corpus.json")).unwrap();
    let cases = array(&manifest, "cases").unwrap();
    let expected = [cases[56].clone(), cases[248].clone()];
    let mut actual: Vec<_> = expected
        .iter()
        .map(|spec| {
            let mut row = spec.clone();
            put(
                &mut row,
                "input_sha256",
                field(spec, "sha256").unwrap().clone(),
            )
            .unwrap();
            for key in ["candidate_trace", "reference_trace"] {
                put(
                    &mut row,
                    key,
                    object(&[("stdout_sha256", Value::str(digest('1')))]),
                )
                .unwrap();
            }
            row
        })
        .collect();
    put(&mut actual[1], "sampled_as", Value::int(56)).unwrap();
    put(
        &mut actual[1],
        "sampled_profile",
        field(&expected[0], "profile").unwrap().clone(),
    )
    .unwrap();
    assert!(aliases(&expected, &membership(&expected, &actual).unwrap(), false).is_ok());

    let mut altered = actual.clone();
    put(&mut altered[1], "sampled_as", Value::int(55)).unwrap();
    assert!(aliases(&expected, &membership(&expected, &altered).unwrap(), false).is_err());
    let mut altered = actual.clone();
    change(
        &mut altered[1],
        &["candidate_trace"],
        "stdout_sha256",
        Value::str(digest('2')),
    );
    assert!(aliases(&expected, &membership(&expected, &altered).unwrap(), false).is_err());
    let mut altered = expected.clone();
    change(
        &mut altered[1],
        &["operation"],
        "arguments",
        Value::strings(["1", "0", "memory", "3"]),
    );
    assert!(aliases(&altered, &membership(&altered, &actual).unwrap(), false).is_err());
    put(&mut actual[0], "sampled_as", Value::int(0)).unwrap();
    assert!(aliases(&expected, &membership(&expected, &actual).unwrap(), false).is_err());
}

/// The open-case ratchet: a listed failing case is graded on parity alone, a
/// listed case that passes is refused, a listed id outside the corpus is
/// refused, and an unlisted failing case still needs its measurements.
#[test]
fn open_cases_are_graded_on_parity_and_only_shrink() {
    let mut fixture = Fixture::new();
    let failing = object(&[
        ("status", Value::str("mismatch")),
        ("source_timing_status", Value::str("pass")),
    ]);
    for row in [&mut fixture.timed, &mut fixture.measured] {
        *row = without(row, &["timing", "memory"]).unwrap();
        put(row, "parity", failing.clone()).unwrap();
    }
    let graded = open_case(&fixture.timed, &fixture.measured).unwrap();
    assert_eq!(graded.get("open_case"), Some(&Value::Bool(true)));
    assert_eq!(graded.get("parity"), Some(&failing));
    assert!(graded.get("timing").is_none() && graded.get("memory").is_none());
    assert!(fixture.join().unwrap_err().contains("missing timing"));

    let passing = Fixture::new();
    let error = open_case(&passing.timed, &passing.measured).unwrap_err();
    assert!(error.contains("remove it from open_cases"), "{error}");

    let mut other = fixture.measured.clone();
    put(&mut other, "parity", Value::Null).unwrap();
    assert!(open_case(&fixture.timed, &other).is_err());

    let contract = |id: &str| {
        json::parse_strict(&format!(
                r#"{{"pending_module_requirements": ["reader"], "open_cases": [{{"id": "{id}", "requirement": "reader"}}]}}"#
            ))
            .unwrap()
    };
    let manifest = json::parse_strict(r#"{"cases": [{"id": "a"}]}"#).unwrap();
    assert_eq!(
        super::super::report::open_cases(&contract("a"), &manifest, "t").unwrap(),
        ["a"]
    );
    assert!(super::super::report::open_cases(&contract("gone"), &manifest, "t").is_err());
}

#[test]
fn explicit_refusals_join_without_successful_samples() {
    let mut fixture = Fixture::new();
    put(&mut fixture.expected, "outcome", Value::str("refusal")).unwrap();
    for row in [&mut fixture.timed, &mut fixture.measured] {
        *row = without(row, &["timing", "memory"]).unwrap();
        put(row, "outcome", Value::str("refusal")).unwrap();
    }
    let joined = fixture.join().unwrap();
    assert!(joined.get("timing").is_none());
    assert!(joined.get("memory").is_none());
    put(&mut fixture.measured, "memory", object(&[])).unwrap();
    assert!(fixture.join().unwrap_err().contains("refusal"));
}

#[test]
fn matching_refusals_may_log_different_diagnostic_addresses() {
    let mut fixture = Fixture::new();
    put(&mut fixture.expected, "outcome", Value::str("refusal")).unwrap();
    for (row, diagnostics) in [
        (&mut fixture.timed, "[mov,mp4 @ 0x1234] invalid box\n"),
        (&mut fixture.measured, "[mov,mp4 @ 0x4321] invalid box\n"),
    ] {
        *row = without(row, &["timing", "memory"]).unwrap();
        put(row, "outcome", Value::str("refusal")).unwrap();
        put(
            row,
            "reference_trace",
            object(&[
                ("exit_code", Value::int(1)),
                ("status", Value::str("finished")),
                ("stdout_sha256", Value::str(digest('0'))),
                ("stderr", Value::str(diagnostics)),
                (
                    "stderr_sha256",
                    Value::str(sha256::hex(diagnostics.as_bytes())),
                ),
            ]),
        )
        .unwrap();
    }
    let joined = fixture.join().unwrap();
    assert_eq!(
        joined.get("reference_trace"),
        fixture.timed.get("reference_trace")
    );
    assert_ne!(
        reference_diagnostics("[mov,mp4 @ 0x1234] invalid box\n"),
        reference_diagnostics("[mov,mp4 @ 0x4321] unsupported box\n")
    );
    assert_eq!(
        reference_diagnostics("packet at 0x1234\n"),
        "packet at 0x1234\n"
    );
    let message = "[mov,mp4] invalid offset @ 0xaaaa]\n";
    assert_eq!(reference_diagnostics(message), message);
    assert_ne!(
        reference_diagnostics(message),
        reference_diagnostics("[mov,mp4] invalid offset @ 0xbbbb]\n")
    );
    change(
        &mut fixture.measured,
        &["reference_trace"],
        "exit_code",
        Value::int(2),
    );
    assert!(fixture.join().unwrap_err().contains("case fields/parity"));
}

#[test]
fn only_an_added_kernel_proof_can_differ_between_builds() {
    let timed = identity();
    let mut memory = timed.clone();
    let pin = object(&[
        ("proof_path", Value::str("original-proof.json")),
        ("proof_sha256", Value::str(digest('f'))),
    ]);
    change(
        &mut memory,
        &["instrumentation"],
        "rss_kernel_bound",
        pin.clone(),
    );
    identities(&timed, &memory).unwrap();
    let mut instrument = memory.clone();
    change(
        &mut instrument,
        &["instrumentation"],
        "observer_sha256",
        Value::str(digest('f')),
    );
    assert!(identities(&timed, &instrument).is_err());
    let mut previous = timed.clone();
    change(&mut previous, &["instrumentation"], "rss_kernel_bound", pin);
    assert!(identities(&previous, &timed).is_err());
    change(
        &mut previous,
        &["instrumentation", "rss_kernel_bound"],
        "proof_sha256",
        Value::str(digest('a')),
    );
    assert!(identities(&previous, &memory).is_err());
}

#[test]
fn source_worker_compiler_environment_and_reference_changes_are_refused() {
    let original = identity();
    for (path, key, replacement) in [
        (vec!["selected_graph"], "sha256", Value::str(digest('f'))),
        (vec!["binaries"], "candidate", Value::str(digest('f'))),
        (vec!["binaries"], "reference", Value::str(digest('f'))),
        (vec![], "rustc", Value::str("different compiler")),
        (
            vec!["build_environment"],
            "RUSTFLAGS",
            Value::str("override"),
        ),
        (vec![], "cargo_configuration", Value::strings(["override"])),
        (
            vec!["reference_build"],
            "configuration",
            Value::str("changed"),
        ),
    ] {
        let mut changed = original.clone();
        change(&mut changed, &path, key, replacement);
        assert!(identities(&original, &changed).is_err(), "{path:?}/{key}");
    }
    let missing = without(&original, &["rustc"]).unwrap();
    assert!(identities(&missing, &missing).is_err());
}

#[test]
fn joining_preserves_all_raw_samples_parity_and_failed_attempts() {
    let fixture = Fixture::new();
    let joined = fixture.join().unwrap();
    assert_eq!(joined.get("timing"), fixture.timed.get("timing"));
    assert_eq!(joined.get("memory"), fixture.measured.get("memory"));
    assert_eq!(joined.get("parity"), fixture.timed.get("parity"));
    assert_eq!(
        joined.get("retained_record"),
        fixture.timed.get("retained_record")
    );
    assert_eq!(
        text(field(&joined, "memory").unwrap(), "status").unwrap(),
        "observer-unqualified"
    );
}

#[test]
fn failed_memory_without_pair_count_is_written_and_cannot_pass() {
    let mut fixture = Fixture::new();
    let mut memory = without(field(&fixture.measured, "memory").unwrap(), &["pairs"]).unwrap();
    put(
        &mut memory,
        "status",
        Value::str("invalid-memory-observation"),
    )
    .unwrap();
    put(&mut fixture.measured, "memory", memory.clone()).unwrap();
    let joined = fixture.join().unwrap();
    assert_eq!(joined.get("memory"), Some(&memory));
    assert!(field(&joined, "memory").unwrap().get("pairs").is_none());
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let verdict = super::super::acceptance::grade_case(
        root,
        &fixture.contract,
        &joined,
        &fixture.identity,
        "unqualified-test",
    );
    assert!(!verdict.memory_pass);
    let acceptance = acceptance_iter(
        root,
        &fixture.contract,
        [Ok(joined.clone())].into_iter(),
        &fixture.identity,
        "unqualified-test",
        &[],
        &[],
    );
    assert_eq!(acceptance.get("accepted"), Some(&Value::Bool(false)));
    assert_eq!(acceptance.get("slice_accepted"), Some(&Value::Bool(false)));
    let output = std::env::temp_dir().join(format!("grade-failed-memory-{}", std::process::id()));
    fs::create_dir_all(&output).unwrap();
    super::super::report::write_case_report(
        &output,
        &object(&[("acceptance", acceptance)]),
        [Ok(joined.clone())].into_iter(),
    )
    .unwrap();
    let written = json::read(&output.join("report.json")).unwrap();
    require_same(
        field(&written, "cases").unwrap(),
        &Value::Array(vec![joined]),
        "retained failed cases",
    )
    .unwrap();
    fs::remove_dir_all(output).unwrap();

    for (path, key) in [
        (vec!["memory"], "input_sha256"),
        (vec!["memory", "certificate"], "observer_sha256"),
        (vec!["memory", "certificate", "worker_hashes"], "candidate"),
    ] {
        let mut changed = fixture.measured.clone();
        change(&mut changed, &path, key, Value::str(digest('f')));
        let mut invalid = Fixture::new();
        invalid.measured = changed;
        assert!(invalid.join().is_err(), "{path:?}/{key}");
    }
}

#[test]
fn missing_pair_counts_never_relax_success_or_minimum_sample_guards() {
    let fixture = Fixture::new();
    let mut memory = without(field(&fixture.measured, "memory").unwrap(), &["pairs"]).unwrap();
    assert_eq!(samples(&memory, 30, true).unwrap().len(), 30);
    assert!(samples(&memory, 31, true).is_err());
    assert!(samples(&memory, 30, false).is_err());
    let raw = array(&memory, "samples").unwrap()[..29].to_vec();
    put(&mut memory, "samples", Value::Array(raw)).unwrap();
    assert!(samples(&memory, 30, true).is_err());

    let mut fixture = Fixture::new();
    let mut clock = without(field(&fixture.timed, "timing").unwrap(), &["pairs"]).unwrap();
    assert!(samples(&clock, 30, false).is_err());
    put(&mut clock, "status", Value::str("measured")).unwrap();
    put(&mut fixture.timed, "timing", clock).unwrap();
    assert!(fixture.join().unwrap_err().contains("pairs"));

    let mut fixture = Fixture::new();
    let mut memory = without(field(&fixture.measured, "memory").unwrap(), &["pairs"]).unwrap();
    put(&mut memory, "status", Value::str("qualified")).unwrap();
    put(&mut fixture.measured, "memory", memory).unwrap();
    assert!(fixture.join().unwrap_err().contains("pairs"));
}

#[test]
fn absent_phase_values_allow_separate_collections_to_join() {
    let mut fixture = Fixture::new();
    put(&mut fixture.timed, "memory", Value::Null).unwrap();
    put(&mut fixture.measured, "timing", Value::Null).unwrap();
    assert!(fixture.join().is_ok());
}

#[test]
fn registered_role_proofs_reject_integer_to_float_coercion() {
    let mut fixture = Fixture::new();
    let proof = object(&[
        ("candidate", object(&[("input_bytes", Value::int(747))])),
        ("reference", object(&[("input_bytes", Value::int(747))])),
    ]);
    put(
        &mut fixture.expected,
        "expected_time_outputs",
        proof.clone(),
    )
    .unwrap();
    put(&mut fixture.timed, "expected_time_outputs", proof).unwrap();
    assert!(
        selected(
            &fixture.timed,
            &fixture.expected,
            &fixture.contract,
            &fixture.identity
        )
        .is_ok()
    );
    change(
        &mut fixture.timed,
        &["expected_time_outputs", "reference"],
        "input_bytes",
        Value::float(747.0),
    );
    assert!(
        selected(
            &fixture.timed,
            &fixture.expected,
            &fixture.contract,
            &fixture.identity
        )
        .is_err()
    );
}

#[test]
fn joining_checks_role_specific_outputs_beyond_the_work_fold() {
    for (key, section, sample_role, output_key) in [
        ("expected_time_outputs", "timing", "candidate", "data"),
        (
            "expected_memory_outputs",
            "memory",
            "candidate_heap",
            "work",
        ),
    ] {
        let mut fixture = Fixture::new();
        let row = if section == "timing" {
            &fixture.timed
        } else {
            &fixture.measured
        };
        let sample = &array(field(row, section).unwrap(), "samples").unwrap()[0];
        let output = field(field(sample, sample_role).unwrap(), output_key).unwrap();
        let roles = object(&[("candidate", output.clone()), ("reference", output.clone())]);
        for row in [
            &mut fixture.expected,
            &mut fixture.timed,
            &mut fixture.measured,
        ] {
            put(row, key, roles.clone()).unwrap();
        }
        assert!(fixture.join().is_ok());
        let mut altered = roles;
        change(&mut altered, &["reference"], "input_bytes", Value::int(747));
        for row in [
            &mut fixture.expected,
            &mut fixture.timed,
            &mut fixture.measured,
        ] {
            put(row, key, altered.clone()).unwrap();
        }
        assert!(fixture.join().unwrap_err().contains(key));
    }
}

#[test]
fn joining_checks_declared_work_fields_without_dropping_the_second_digest() {
    let mut fixture = Fixture::new();
    put(
        &mut fixture.contract,
        "work_fields",
        Value::strings(["digest_b"]),
    )
    .unwrap();
    for (row, section, key, roles, digest) in [
        (
            &mut fixture.timed,
            "timing",
            "data",
            vec!["candidate", "reference"],
            17,
        ),
        (
            &mut fixture.measured,
            "memory",
            "work",
            vec![
                "candidate_heap",
                "candidate_rss",
                "reference_heap",
                "reference_rss",
            ],
            4,
        ),
    ] {
        let mut samples = array(field(row, section).unwrap(), "samples")
            .unwrap()
            .to_vec();
        for sample in &mut samples {
            for role in &roles {
                change(sample, &[role, key], "digest_b", Value::int(digest));
            }
        }
        change(row, &[section], "samples", Value::Array(samples));
    }
    let mut warmups = array(field(&fixture.timed, "timing").unwrap(), "warmups")
        .unwrap()
        .to_vec();
    for warmup in &mut warmups {
        change(warmup, &["result", "data"], "digest_b", Value::int(17));
    }
    change(
        &mut fixture.timed,
        &["timing"],
        "warmups",
        Value::Array(warmups),
    );
    put(
        &mut fixture.expected,
        "expected",
        object(&[
            ("packets", Value::int(2)),
            ("bytes", Value::int(40)),
            ("digest", Value::int(7)),
            ("digest_b", Value::int(4)),
        ]),
    )
    .unwrap();
    let joined = fixture.join().unwrap();
    assert_eq!(joined.get("timing"), fixture.timed.get("timing"));
    assert_eq!(joined.get("memory"), fixture.measured.get("memory"));
    let mut samples = array(field(&fixture.measured, "memory").unwrap(), "samples")
        .unwrap()
        .to_vec();
    for sample in &mut samples {
        let Value::Object(entries) = sample else {
            unreachable!()
        };
        for (_, observed) in entries {
            let mut work = field(observed, "work").unwrap().clone();
            let Value::Object(fields) = &mut work else {
                unreachable!()
            };
            fields.retain(|(k, _)| k != "digest_b");
            put(observed, "work", work).unwrap();
        }
    }
    change(
        &mut fixture.measured,
        &["memory"],
        "samples",
        Value::Array(samples),
    );
    assert!(fixture.join().unwrap_err().contains("digest_b"));
}

#[test]
fn independent_framing_probes_may_take_different_time() {
    let row = |status: &str, elapsed, digest| {
        object(&[(
            "parity",
            object(&[
                ("reference", object(&[("digest", Value::int(digest))])),
                (
                    "reference_run",
                    object(&[("elapsed_ns", elapsed), ("status", Value::str(status))]),
                ),
            ]),
        )])
    };
    let timed = joined_fields(&row("ok", Value::int(100), 1)).unwrap();
    let other = joined_fields(&row("ok", Value::int(200), 1)).unwrap();
    assert!(require_same(&timed, &other, "case fields/parity").is_ok());
    for different in [
        row("timeout", Value::int(200), 1),
        row("ok", Value::int(200), 2),
        row("ok", Value::Null, 1),
    ] {
        let different = joined_fields(&different).unwrap();
        assert!(require_same(&timed, &different, "case fields/parity").is_err());
    }
    assert!(joined_fields(&row("ok", Value::int(-1), 1)).is_err());
}

#[test]
fn collections_join_across_reference_run_times_but_no_other_parity_difference() {
    let run = |status: &str, elapsed: i64| {
        object(&[
            ("status", Value::str(status)),
            ("elapsed_ns", Value::int(elapsed)),
        ])
    };
    let fixture = || {
        let mut fixture = Fixture::new();
        change(
            &mut fixture.timed,
            &["parity"],
            "reference_run",
            run("ok", 2_301_137),
        );
        change(
            &mut fixture.measured,
            &["parity"],
            "reference_run",
            run("ok", 2_332_973),
        );
        fixture
    };
    // Each collection's parity run records its own elapsed time.
    fixture().join().unwrap();
    let mut status = fixture();
    change(
        &mut status.measured,
        &["parity"],
        "reference_run",
        run("timeout", 2_332_973),
    );
    let mut run_field = fixture();
    change(
        &mut run_field.measured,
        &["parity", "reference_run"],
        "signal",
        Value::int(9),
    );
    let mut output = fixture();
    change(
        &mut output.measured,
        &["parity"],
        "reference",
        object(&[("digest", Value::int(8))]),
    );
    let mut verdict = fixture();
    change(
        &mut verdict.measured,
        &["parity"],
        "status",
        Value::str("fail"),
    );
    for differing in [status, run_field, output, verdict] {
        assert!(differing.join().unwrap_err().contains("case fields/parity"));
        let open = open_case(&differing.timed, &differing.measured);
        assert!(open.unwrap_err().contains("case fields/parity"));
    }
}

#[test]
fn parity_operation_and_worker_selection_must_match_before_joining() {
    for (path, key, value) in [
        (vec!["operation"], "name", Value::str("other operation")),
        (vec!["parity"], "source_timing_status", Value::str("fail")),
        (vec![], "candidate_worker", Value::str("other-worker")),
        (vec![], "reference_worker", Value::str("other-reference")),
        (vec![], "profile", Value::str("other-profile")),
    ] {
        let mut fixture = Fixture::new();
        change(&mut fixture.measured, &path, key, value);
        assert!(fixture.join().unwrap_err().contains("case fields/parity"));
    }
    let mut fixture = Fixture::new();
    for row in [&mut fixture.timed, &mut fixture.measured] {
        change(row, &["operation"], "name", Value::str("other operation"));
    }
    assert!(fixture.join().unwrap_err().contains("contract operation"));
}

#[test]
fn nested_measurement_hashes_and_worker_bindings_cannot_change() {
    for (timed, path, key) in [
        (true, vec!["timing"], "input_sha256"),
        (false, vec!["memory"], "input_sha256"),
        (true, vec!["timing", "worker_hashes"], "candidate"),
        (
            false,
            vec!["memory", "certificate", "worker_hashes"],
            "reference",
        ),
        (false, vec!["memory", "certificate"], "observer_sha256"),
    ] {
        let mut fixture = Fixture::new();
        let row = if timed {
            &mut fixture.timed
        } else {
            &mut fixture.measured
        };
        change(row, &path, key, Value::str(digest('f')));
        assert!(fixture.join().is_err(), "{path:?}/{key}");
    }
}

#[test]
fn matching_but_wrong_workers_are_not_selected_by_rows() {
    let mut fixture = Fixture::new();
    for row in [&mut fixture.timed, &mut fixture.measured] {
        put(row, "candidate_worker", Value::str("other-worker")).unwrap();
    }
    assert!(fixture.join().unwrap_err().contains("candidate worker"));
}

#[test]
fn different_memory_output_and_contract_expected_output_are_refused() {
    let mut fixture = Fixture::new();
    let mut raw = array(field(&fixture.measured, "memory").unwrap(), "samples")
        .unwrap()
        .to_vec();
    change(
        &mut raw[29],
        &["candidate_rss", "work"],
        "digest",
        Value::int(8),
    );
    change(
        &mut fixture.measured,
        &["memory"],
        "samples",
        Value::Array(raw),
    );
    assert!(fixture.join().unwrap_err().contains("memory output"));
    let mut fixture = Fixture::new();
    put(
        &mut fixture.expected,
        "expected",
        object(&[
            ("packets", Value::int(2)),
            ("bytes", Value::int(40)),
            ("digest", Value::int(8)),
        ]),
    )
    .unwrap();
    assert!(fixture.join().unwrap_err().contains("expected output"));
}

#[test]
fn incomplete_raw_samples_and_changed_contract_iterations_are_refused() {
    let mut fixture = Fixture::new();
    change(&mut fixture.measured, &["memory"], "pairs", Value::int(29));
    assert!(fixture.join().unwrap_err().contains("pair count"));
    let mut fixture = Fixture::new();
    put(&mut fixture.contract, "iterations", Value::int(50)).unwrap();
    assert!(fixture.join().unwrap_err().contains("iterations"));
}

#[test]
fn module_observations_cover_warmup_and_every_pair_member() {
    let fixture = Fixture::new();
    let mut clock = field(&fixture.timed, "timing").unwrap().clone();
    let raw = array(&clock, "samples").unwrap().to_vec();
    let mut observations = array(&clock, "timing_module_observations")
        .unwrap()
        .to_vec();
    for observation in &mut observations {
        change(
            observation,
            &["state"],
            "status",
            Value::str("module-absent"),
        );
    }
    put(
        &mut clock,
        "timing_module_observations",
        Value::Array(observations.clone()),
    )
    .unwrap();
    timing::validate_module_absence(&clock, &raw, true).unwrap();
    change(
        &mut observations[64],
        &["state"],
        "status",
        Value::str("module-loaded"),
    );
    put(
        &mut clock,
        "timing_module_observations",
        Value::Array(observations.clone()),
    )
    .unwrap();
    assert!(timing::validate_module_absence(&clock, &raw, true).is_err());
    observations.pop();
    put(
        &mut clock,
        "timing_module_observations",
        Value::Array(observations),
    )
    .unwrap();
    assert!(
        timing::validate_module_absence(&clock, &raw, true)
            .unwrap_err()
            .contains("incomplete")
    );
}

#[test]
fn standard_linux_total_rss_binding_does_not_require_a_kernel_proof() {
    let fixture = Fixture::standard_linux();
    let memory = field(&fixture.measured, "memory").unwrap();
    let workers = field(&fixture.identity, "binaries").unwrap();
    memory_binding(memory, &fixture.identity, workers, true).unwrap();
    let mut without_opt_in = memory.clone();
    put(
        &mut without_opt_in,
        "certificate",
        without(
            field(memory, "certificate").unwrap(),
            &["kernel_rss_required"],
        )
        .unwrap(),
    )
    .unwrap();
    memory_binding(&without_opt_in, &fixture.identity, workers, true).unwrap();
    let row = fixture.join().unwrap();
    assert_eq!(row.get("memory"), Some(memory));
    // A successful binding/join does not qualify this deliberately incomplete
    // observer fixture. The shared acceptance evaluator still grades it.
    assert_eq!(text(memory, "status").unwrap(), "observer-unqualified");
    let mut identity = fixture.identity.clone();
    change(
        &mut identity,
        &["instrumentation"],
        "rss_kernel_bound",
        object(&[("proof_path", Value::str("unused-proof.json"))]),
    );
    memory_binding(memory, &identity, workers, true).unwrap();
}

#[test]
fn standard_linux_total_rss_still_requires_module_absence_during_timing() {
    let mut fixture = Fixture::standard_linux();
    let mut clock = field(&fixture.timed, "timing").unwrap().clone();
    let mut observations = array(&clock, "timing_module_observations")
        .unwrap()
        .to_vec();
    change(
        &mut observations[64],
        &["state"],
        "status",
        Value::str("module-loaded"),
    );
    put(
        &mut clock,
        "timing_module_observations",
        Value::Array(observations),
    )
    .unwrap();
    put(&mut fixture.timed, "timing", clock).unwrap();
    assert!(
        fixture
            .join()
            .unwrap_err()
            .contains("absence during timing")
    );
}

#[test]
fn kernel_rss_observations_cannot_bypass_the_certificate_opt_in() {
    let fixture = Fixture::standard_linux();
    let memory = field(&fixture.measured, "memory").unwrap();
    let workers = field(&fixture.identity, "binaries").unwrap();
    for role in ["candidate_rss", "reference_rss"] {
        let mut changed = memory.clone();
        let mut samples = array(&changed, "samples").unwrap().to_vec();
        change(
            &mut samples[29],
            &[role, "observer"],
            "kernel_rss_requested",
            Value::int(1),
        );
        put(&mut changed, "samples", Value::Array(samples)).unwrap();
        assert!(
            memory_binding(&changed, &fixture.identity, workers, true)
                .unwrap_err()
                .contains("kernel RSS proof")
        );
    }
    let mut changed = memory.clone();
    change(
        &mut changed,
        &["certificate"],
        "kernel_rss_required",
        Value::int(1),
    );
    assert!(
        memory_binding(&changed, &fixture.identity, workers, true)
            .unwrap_err()
            .contains("kernel RSS proof")
    );
}

#[test]
fn memory_kernel_pin_must_bind_the_original_proof() {
    let fixture = Fixture::standard_linux();
    let memory = field(&fixture.measured, "memory").unwrap();
    let workers = field(&fixture.identity, "binaries").unwrap();
    let mut memory = memory.clone();
    change(
        &mut memory,
        &["certificate"],
        "kernel_rss_required",
        Value::Bool(true),
    );
    change(
        &mut memory,
        &["certificate"],
        "kernel_rss_certificate",
        object(&[(
            "profile",
            object(&[
                ("proof_path", Value::str("different-proof.json")),
                ("proof_sha256", Value::str(digest('f'))),
            ]),
        )]),
    );
    let mut identity = fixture.identity.clone();
    change(
        &mut identity,
        &["instrumentation"],
        "rss_kernel_bound",
        object(&[
            ("proof_path", Value::str("original-proof.json")),
            ("proof_sha256", Value::str(digest('f'))),
        ]),
    );
    assert!(
        memory_binding(&memory, &identity, workers, true)
            .unwrap_err()
            .contains("proof_path")
    );
    change(
        &mut memory,
        &["certificate", "kernel_rss_certificate", "profile"],
        "proof_path",
        Value::str("original-proof.json"),
    );
    change(
        &mut memory,
        &["certificate", "kernel_rss_certificate", "profile"],
        "proof_sha256",
        Value::str(digest('a')),
    );
    assert!(
        memory_binding(&memory, &identity, workers, true)
            .unwrap_err()
            .contains("proof_sha256")
    );
    assert!(memory_binding(&memory, &fixture.identity, workers, true).is_err());
}

#[test]
fn matching_kernel_pin_does_not_replace_the_live_proof() {
    let fixture = Fixture::standard_linux();
    let mut memory = field(&fixture.measured, "memory").unwrap().clone();
    let mut identity = fixture.identity.clone();
    let pin = object(&[
        ("proof_path", Value::str("unavailable-proof.json")),
        ("proof_sha256", Value::str(digest('f'))),
    ]);
    change(
        &mut identity,
        &["instrumentation"],
        "rss_kernel_bound",
        pin.clone(),
    );
    change(
        &mut memory,
        &["certificate"],
        "kernel_rss_required",
        Value::Bool(true),
    );
    change(
        &mut memory,
        &["certificate"],
        "kernel_rss_certificate",
        object(&[("profile", pin)]),
    );
    let mut samples = array(&memory, "samples").unwrap().to_vec();
    for sample in &mut samples {
        for role in ["candidate_rss", "reference_rss"] {
            change(
                sample,
                &[role, "observer"],
                "kernel_rss_requested",
                Value::int(1),
            );
        }
    }
    put(&mut memory, "samples", Value::Array(samples)).unwrap();
    memory_binding(
        &memory,
        &identity,
        field(&identity, "binaries").unwrap(),
        true,
    )
    .unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let result = super::super::memory::evaluate(
        root,
        array(&memory, "samples").unwrap(),
        timing::Pairs::fixed(30),
        280,
        field(&memory, "certificate").unwrap(),
        None,
    );
    assert_eq!(text(&result, "status").unwrap(), "observer-unqualified");
}

#[test]
fn headers_and_external_reports_bind_contract_manifest_target_and_new_identity() {
    let fixture = Fixture::new();
    let mut contract = fixture.contract.clone();
    put(
        &mut contract,
        "required_additional_vectors",
        Value::Array(vec![object(&[
            ("file", Value::str("external.mkv")),
            ("sha256", Value::str(digest('e'))),
        ])]),
    )
    .unwrap();
    let manifest = object(&[("cases", Value::Array(vec![fixture.expected.clone()]))]);
    let binding = Binding {
        contract: &contract,
        manifest: &manifest,
        contract_sha: &digest('a'),
        manifest_sha: &digest('b'),
    };
    let header = object(&[
        ("schema", Value::int(1)),
        ("identity", fixture.identity.clone()),
        ("target", Value::str("unqualified-test")),
        ("contract_sha256", Value::str(digest('a'))),
        ("contract_id", Value::str("matroska-demux")),
        ("corpus_manifest_sha256", Value::str(digest('b'))),
        ("collection_status", Value::str("complete")),
        ("fate_coverage", Value::Array(vec![])),
        (
            "environment",
            object(&[(
                "cpus",
                object(&[("allowed", Value::str("1")), ("online", Value::str("0-1"))]),
            )]),
        ),
        ("cases", Value::Array(vec![fixture.timed])),
    ]);
    assert!(binding.headers(&header, &header).is_ok());
    let coverage = |observation| Value::Array(vec![object(&[("observation", observation)])]);
    let mut timed = header.clone();
    let mut memory = header.clone();
    put(
        &mut timed,
        "fate_coverage",
        coverage(json::parse_observations("1.0e-9223372036854775808").unwrap()),
    )
    .unwrap();
    put(&mut memory, "fate_coverage", coverage(Value::float(1.0))).unwrap();
    assert!(matches!(
        binding.headers(&timed, &memory),
        Err(error) if error == "fate_coverage mismatch"
    ));
    for (key, value) in [
        ("collection_status", Value::str("partial")),
        ("contract_sha256", Value::str(digest('c'))),
        ("corpus_manifest_sha256", Value::str(digest('c'))),
        ("contract_id", Value::str("other")),
        ("target", Value::str("other")),
        ("fate_coverage", Value::Array(vec![Value::str("changed")])),
    ] {
        let mut changed = header.clone();
        put(&mut changed, key, value).unwrap();
        assert!(binding.headers(&header, &changed).is_err(), "{key}");
    }
    for unchanged in [Value::Bool(false), Value::Null, Value::int(1)] {
        let mut failed = header.clone();
        put(
            &mut failed,
            "summary",
            object(&[("source_and_workers_unchanged", unchanged)]),
        )
        .unwrap();
        assert!(binding.headers(&header, &failed).is_err());
        assert!(binding.headers(&failed, &header).is_err());
    }
    let external = object(&[
        ("id", Value::str("external.mkv")),
        ("profile", Value::str("external.mkv")),
        ("input_sha256", Value::str(digest('e'))),
    ]);
    let mut report = without(
        &header,
        &[
            "cases",
            "collection_status",
            "contract_id",
            "corpus_manifest_sha256",
            "fate_coverage",
        ],
    )
    .unwrap();
    put(&mut report, "case", external.clone()).unwrap();
    put(
        &mut report,
        "cpus",
        field(field(&header, "environment").unwrap(), "cpus")
            .unwrap()
            .clone(),
    )
    .unwrap();
    assert!(binding.headers(&report, &report).is_ok());
    for base in [&header, &report] {
        let parent = if shape(base).unwrap() == Shape::Corpus {
            Some("environment")
        } else {
            None
        };
        for replacement in [
            None,
            Some(Value::Null),
            Some(object(&[("allowed", Value::str("0"))])),
        ] {
            let missing = replacement
                .as_ref()
                .is_none_or(|value| *value == Value::Null);
            let mut changed = base.clone();
            let mut record = parent.map_or_else(
                || changed.clone(),
                |key| field(&changed, key).unwrap().clone(),
            );
            record = without(&record, &["cpus"]).unwrap();
            if let Some(value) = replacement {
                put(&mut record, "cpus", value).unwrap();
            }
            match parent {
                Some(key) => put(&mut changed, key, record).unwrap(),
                None => changed = record,
            }
            if missing {
                assert!(binding.headers(base, &changed).is_err());
                assert!(binding.headers(&changed, &changed).is_err());
            } else {
                assert!(binding.headers(base, &changed).is_ok());
            }
        }
    }
    let expected = binding.expected(&report, Shape::External).unwrap();
    assert_eq!(
        membership(&expected, report_rows(&report).unwrap())
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        additional_case(&report, &header, &binding).unwrap(),
        &external
    );
    change(
        &mut report,
        &["identity", "selected_graph"],
        "sha256",
        Value::str(digest('f')),
    );
    assert!(additional_case(&report, &header, &binding).is_err());
}

#[test]
fn external_grading_preserves_pending_module_requirements_and_other_fields() {
    let contract = object(&[
        ("required_profiles", Value::strings(["authored"])),
        (
            "required_additional_vectors",
            Value::Array(vec![Value::str("required")]),
        ),
        (
            "pending_module_requirements",
            Value::strings(["FATE pending", "broader scope"]),
        ),
        ("minimum_pairs", Value::int(30)),
    ]);
    let derived = external_contract(&contract, "external.mkv").unwrap();
    assert_eq!(
        without(
            &contract,
            &["required_profiles", "required_additional_vectors"]
        )
        .unwrap(),
        without(
            &derived,
            &["required_profiles", "required_additional_vectors"]
        )
        .unwrap()
    );
    let verdict = object(&[
        ("accepted", Value::Bool(false)),
        ("slice_accepted", Value::Bool(false)),
    ]);
    assert_eq!(exit_code(&verdict, &[], Some("accepted")), 2);
    assert_eq!(exit_code(&verdict, &[], Some("slice_accepted")), 2);
}

#[test]
fn arbitrary_report_paths_and_invalid_cli_are_handled_without_panics() {
    let root = Path::new("/repository");
    let args = [
        "odd timing.name",
        "/elsewhere/memory+data",
        "--contract",
        "contract",
        "--manifest",
        "manifest",
        "--output",
        "out",
    ];
    let args: Vec<_> = args.map(str::to_owned).into();
    let parsed = Options::parse(root, &args).unwrap();
    assert_eq!(parsed.timing, root.join("odd timing.name"));
    assert_eq!(parsed.memory, Path::new("/elsewhere/memory+data"));
    for extra in [
        vec!["--contract"],
        vec!["--output", "--require-acceptance"],
        vec!["--unknown"],
        vec!["--require-acceptance", "--require-slice-acceptance"],
    ] {
        let mut invalid = args.clone();
        invalid.extend(extra.into_iter().map(str::to_owned));
        assert!(Options::parse(root, &invalid).is_err());
    }
    assert!(Options::parse(root, &[]).is_err());
    assert!(document(&root.join("missing report")).is_err());
}
