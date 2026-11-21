use super::*;
use crate::harness::acceptance::{
    memory_outputs_match, output_matches as work_matches, timing_outputs_match,
};

fn final_report(output: &Path, base: &Value, rows: &[Value], summary: &Value) -> R<()> {
    let verdict = Value::object([
        ("accepted", Value::Bool(false)),
        ("slice_accepted", Value::Bool(false)),
        ("reasons", Value::strings(["missing observations"])),
    ]);
    super::final_report(output, base, rows, summary, &verdict)
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap()
}

#[test]
fn complete_membership_keeps_every_operation_and_refusal() {
    let suite = Suite::load(&root()).unwrap();
    assert_eq!(suite.cases.len(), 1020);
    assert_eq!(suite.selection(false).len(), 972);
    assert_eq!(suite.cases.iter().filter(|c| c.refusal).count(), 48);
    assert_eq!(
        suite
            .cases
            .iter()
            .filter(|c| !c.refusal && c.alias.is_none())
            .count(),
        956
    );
    let mut broken = suite;
    broken.cases.remove(100);
    assert!(broken.validate().is_err());
}

#[test]
fn subset52_is_exact_and_does_not_replace_parity() {
    let suite = Suite::load(&root()).unwrap();
    let expected: Vec<_> = (180..188)
        .chain(192..196)
        .chain(164..168)
        .chain(176..180)
        .chain(188..192)
        .chain(196..200)
        .chain(0..8)
        .chain(136..140)
        .chain(148..152)
        .chain(220..224)
        .chain(360..364)
        .collect();
    assert_eq!(suite.selection(true), expected);
    assert_eq!(suite.cases.len(), 1020);
    for i in expected {
        assert!(!suite.cases[i].refusal);
        assert!(suite.cases[i].alias.is_none());
    }
}

#[test]
fn parts_measure_every_operation_once_with_its_aliases() {
    let suite = Suite::load(&root()).unwrap();
    let all = suite.selection(false);
    let mut joined: Vec<usize> = (0..3).flat_map(|i| suite.part(&all, (i, 3))).collect();
    joined.sort_unstable();
    assert_eq!(joined, all);
    for i in 0..3 {
        let part = suite.part(&all, (i, 3));
        for &index in &part {
            if let Some(prior) = suite.cases[index].alias {
                assert!(part.contains(&prior));
            }
        }
    }
}

#[test]
fn aliases_keep_the_original_membership_and_work() {
    let mut suite = Suite::load(&root()).unwrap();
    let actual: Vec<_> = suite
        .cases
        .iter()
        .filter_map(|c| c.alias.map(|a| (c.index, a)))
        .collect();
    assert_eq!(
        actual,
        [
            (248, 56),
            (249, 57),
            (250, 58),
            (251, 59),
            (364, 0),
            (365, 1),
            (366, 2),
            (367, 3),
            (588, 432),
            (589, 433),
            (590, 434),
            (591, 435),
            (668, 0),
            (669, 1),
            (670, 2),
            (671, 3)
        ]
    );
    suite.cases[248].alias = Some(0);
    assert!(suite.validate().is_err());
}

#[test]
fn all_small_authored_inputs_match_their_source_hashes() {
    let root = root();
    let suite = Suite::load(&root).unwrap();
    let mut seen = BTreeSet::new();
    let mut bytes = 0;
    for case in &suite.cases {
        if seen.insert(&case.input_sha) {
            verify_input(&root, case).unwrap();
            bytes += case.input_bytes;
        }
    }
    assert_eq!(seen.len(), 251);
    assert_eq!(bytes, 254_641);
}

#[test]
fn recovery_options_precede_the_mode_for_each_role() {
    let suite = Suite::load(&root()).unwrap();
    for i in [0, 100, 176, 188, 196] {
        let case = &suite.cases[i];
        let operation = case.operation();
        for (mode, iterations) in [("trace", None), ("bench", Some(50)), ("memory", None)] {
            let reference = operation.worker_arguments(false, mode, Path::new("x.mov"), iterations);
            let candidate = operation.worker_arguments(true, mode, Path::new("x.mov"), iterations);
            assert_eq!(
                candidate[..case.candidate_options.len()],
                case.candidate_options
                    .iter()
                    .map(OsString::from)
                    .collect::<Vec<_>>()
            );
            assert_eq!(candidate[case.candidate_options.len()..], reference);
            assert_eq!(
                reference[0],
                OsString::from(format!("{}-{mode}", case.mode))
            );
        }
    }
    assert_eq!(
        suite.cases[176].candidate_options,
        ["--recover-regular-tables"]
    );
}

#[test]
fn expected_work_preserves_proofs_and_exact_integer_types() {
    let suite = Suite::load(&root()).unwrap();
    let case = &suite.cases[0];
    let candidate = case.work(true, false);
    let reference = case.work(false, false);
    assert!(candidate.get("input_sha256").is_none());
    assert_eq!(text(&reference, "input_sha256").unwrap(), case.input_sha);
    assert_eq!(text(&reference, "contract_sha256").unwrap(), WORK_CONTRACT);
    assert!(work_matches(&reference, &reference, false));
    assert!(!work_matches(&candidate, &reference, false));
    let mut altered = reference.clone();
    field(
        &mut altered,
        "digest",
        Value::float(number(&reference, "digest").unwrap() as f64),
    );
    assert!(!work_matches(&altered, &reference, false));
    field(
        &mut altered,
        "digest",
        reference.get("digest").unwrap().clone(),
    );
    field(&mut altered, "elapsed_ns", Value::int(1));
    assert!(!work_matches(&altered, &reference, false));
    assert!(work_matches(&altered, &reference, true));
}

fn value_at<'a>(value: &'a mut Value, path: &[&str]) -> &'a mut Value {
    let mut current = value;
    for key in path {
        current = match current {
            Value::Object(fields) => &mut fields.iter_mut().find(|(k, _)| k == key).unwrap().1,
            Value::Array(values) => &mut values[key.parse::<usize>().unwrap()],
            _ => panic!("expected a container at {key}"),
        };
    }
    current
}

#[test]
fn timing_role_proofs_are_required_in_warmups_and_every_pair() {
    let suite = Suite::load(&root()).unwrap();
    let case = &suite.cases[0];
    let result = |candidate| {
        let mut work = case.work(candidate, true);
        field(&mut work, "elapsed_ns", Value::int(100));
        Value::object([("status", Value::str("ok")), ("data", work)])
    };
    let raw = Value::object([
        (
            "warmups",
            Value::Array(vec![
                Value::object([
                    ("worker", Value::str("candidate")),
                    ("result", result(true)),
                ]),
                Value::object([
                    ("worker", Value::str("reference")),
                    ("result", result(false)),
                ]),
            ]),
        ),
        (
            "samples",
            Value::Array(vec![Value::object([
                ("candidate", result(true)),
                ("reference", result(false)),
            ])]),
        ),
    ]);
    assert!(timing_outputs_match(&raw, &case.time_outputs));
    for path in [
        vec!["warmups", "1", "result", "data", "input_sha256"],
        vec!["warmups", "1", "result", "data", "contract_sha256"],
        vec!["samples", "0", "reference", "data", "input_sha256"],
        vec!["samples", "0", "reference", "data", "contract_sha256"],
    ] {
        let mut changed = raw.clone();
        *value_at(&mut changed, &path) = Value::str("changed");
        assert!(
            !timing_outputs_match(&changed, &case.time_outputs),
            "{path:?}"
        );
    }
    for path in [
        vec!["warmups", "0", "result", "data", "elapsed_ns"],
        vec!["samples", "0", "candidate", "data", "elapsed_ns"],
    ] {
        let mut changed = raw.clone();
        *value_at(&mut changed, &path) = Value::int(0);
        assert!(
            !timing_outputs_match(&changed, &case.time_outputs),
            "{path:?}"
        );
    }
}

#[test]
fn memory_role_proofs_preserve_exact_outputs_and_all_four_reports() {
    let suite = Suite::load(&root()).unwrap();
    let case = &suite.cases[0];
    let expected = Value::object([
        ("candidate", case.work(true, false)),
        ("reference", case.work(false, false)),
    ]);
    let raw = Value::object([(
        "samples",
        Value::Array(vec![Value::object([
            (
                "candidate_heap",
                Value::object([("work", case.work(true, false))]),
            ),
            (
                "candidate_rss",
                Value::object([("work", case.work(true, false))]),
            ),
            (
                "reference_heap",
                Value::object([("work", case.work(false, false))]),
            ),
            (
                "reference_rss",
                Value::object([("work", case.work(false, false))]),
            ),
        ])]),
    )]);
    assert!(memory_outputs_match(&raw, &expected));
    for role in ["reference_heap", "reference_rss"] {
        let mut changed = raw.clone();
        *value_at(
            &mut changed,
            &["samples", "0", role, "work", "contract_sha256"],
        ) = Value::str("changed");
        assert!(!memory_outputs_match(&changed, &expected));
    }
    let mut changed = raw.clone();
    *value_at(&mut changed, &["samples", "0", "candidate_heap", "work"]) = case.work(false, false);
    assert!(!memory_outputs_match(&changed, &expected));
    let mut changed = raw.clone();
    let Value::Object(reports) = value_at(&mut changed, &["samples", "0"]) else {
        panic!("expected the four reports");
    };
    reports.pop();
    assert!(!memory_outputs_match(&changed, &expected));
    let evidence = case.evidence("memory", raw.clone());
    assert_eq!(evidence.get("memory"), Some(&raw));
    assert_eq!(
        evidence.get("parity").unwrap().get("expected"),
        expected.get("candidate")
    );
    assert_eq!(text(&evidence, "input_sha256").unwrap(), case.input_sha);
}

#[test]
fn recorded_sampling_must_match_the_selected_integer_counts_and_seed() {
    let mut raw = Value::object([("pairs", Value::int(30)), ("seed", Value::int(280))]);
    assert!(sampling_matches(&raw, 30, 280));
    assert!(!sampling_matches(&raw, 31, 280));
    assert!(!sampling_matches(&raw, 30, 281));
    field(&mut raw, "pairs", Value::float(30.0));
    assert!(!sampling_matches(&raw, 30, 280));
    field(&mut raw, "pairs", Value::int(30));
    field(&mut raw, "seed", Value::Bool(true));
    assert!(!sampling_matches(&raw, 30, 1));
}

#[test]
fn every_fifty_iteration_count_and_digest_composes_from_single_operation_work() {
    let suite = Suite::load(&root()).unwrap();
    for case in &suite.cases {
        let one = u64::try_from(number(&case.work(false, false), "digest").unwrap()).unwrap();
        let digest =
            (0..ITERATIONS).fold(0u64, |total, _| total.wrapping_mul(31).wrapping_add(one));
        assert_eq!(
            number(&case.work(false, true), "digest").unwrap(),
            i128::from(digest)
        );
        for key in ["packets", "bytes"] {
            assert_eq!(
                number(&case.work(false, true), key).unwrap(),
                number(&case.work(false, false), key).unwrap() * i128::from(ITERATIONS)
            );
        }
    }
}

#[test]
fn refusal_needs_both_explicit_failures_and_no_output() {
    let candidate = Finished {
        code: 1,
        stdout: vec![],
        stderr: b"isobmff-consumer: invalid data: box".to_vec(),
    };
    let mut reference = Finished {
        code: 1,
        stdout: vec![],
        stderr: b"mov-consumer: Invalid data (-1094995529)\n".to_vec(),
    };
    assert!(refusal_matches(&candidate, &reference));
    reference.code = 0;
    assert!(!refusal_matches(&candidate, &reference));
    reference.code = 1;
    reference.stdout.push(b' ');
    assert!(!refusal_matches(&candidate, &reference));
    reference.stdout.clear();
    reference.stderr.clear();
    assert!(!refusal_matches(&candidate, &reference));
}

#[test]
fn partial_measurement_sets_cannot_request_acceptance() {
    let suite = Suite::load(&root()).unwrap();
    let options = Options::parse(
        &root(),
        &["--subset52".into(), "--benchmark".into(), "--memory".into()],
    )
    .unwrap();
    let rows: Vec<_> = suite
        .cases
        .iter()
        .map(|case| {
            let mut row = case.description();
            field(&mut row, "match", Value::Bool(true));
            for mode in ["timing", "memory"] {
                field(&mut row, mode, Value::object([("pass", Value::Bool(true))]));
            }
            row
        })
        .collect();
    let result = summary(&rows, &suite.selection(true), &options, true);
    assert_eq!(result.get("parity_matches"), Some(&Value::int(1020)));
    assert_eq!(
        result.get("selected_physical_operations"),
        Some(&Value::int(52))
    );
    let part = suite.part(&suite.selection(false), (0, 3));
    let part_result = summary(&rows, &part, &options, true);
    assert_eq!(
        part_result.get("selected_physical_operations"),
        Some(&Value::int(319))
    );
    assert!(result.get("qualified").is_none());
    let mut failed = rows;
    field(&mut failed[100], "match", Value::Bool(false));
    assert_eq!(
        summary(&failed, &suite.selection(true), &options, true).get("all_requested_checks_pass"),
        Some(&Value::Bool(false))
    );
    assert!(
        Options::parse(
            &root(),
            &["--subset52".into(), "--require-acceptance".into()]
        )
        .is_err()
    );
}

#[test]
fn streamed_report_retains_raw_samples_aliases_and_proof_fields() {
    let out = crate::walk::scratch("isobmff-report");
    fs::create_dir_all(out.join("case-0000")).unwrap();
    let raw = Value::object([
        ("status", Value::str("measured")),
        (
            "samples",
            Value::Array(vec![Value::object([("digest", Value::int(u64::MAX))])]),
        ),
        ("input_sha256", Value::str("input-proof")),
    ]);
    let digest = write_json(&out.join("case-0000/timing.json"), &raw).unwrap();
    let grade = Value::object([
        ("raw", Value::str("case-0000/timing.json")),
        ("raw_sha256", Value::str(digest)),
        ("pass", Value::Bool(false)),
    ]);
    let memory = Value::object([
        ("status", Value::str("qualified")),
        (
            "certificate",
            Value::object([("kernel_rss_required", Value::Bool(false))]),
        ),
        (
            "samples",
            Value::Array(vec![Value::object([
                (
                    "candidate_heap",
                    Value::object([(
                        "work",
                        Value::object([
                            ("packets", Value::int(6)),
                            ("digest", Value::int(u64::MAX)),
                        ]),
                    )]),
                ),
                (
                    "reference_heap",
                    Value::object([(
                        "work",
                        Value::object([
                            ("packets", Value::int(6)),
                            ("digest", Value::int(u64::MAX)),
                            ("input_sha256", Value::str("input-proof")),
                            ("contract_sha256", Value::str(WORK_CONTRACT)),
                        ]),
                    )]),
                ),
            ])]),
        ),
    ]);
    let memory_digest = write_json(&out.join("case-0000/memory.json"), &memory).unwrap();
    let memory_grade = Value::object([
        ("raw", Value::str("case-0000/memory.json")),
        ("raw_sha256", Value::str(memory_digest)),
        ("pass", Value::Bool(false)),
    ]);
    let rows = vec![
        Value::object([
            ("timing", grade.clone()),
            ("memory", memory_grade.clone()),
            ("profile", Value::str("original")),
        ]),
        Value::object([
            ("timing", grade),
            ("memory", memory_grade),
            ("profile", Value::str("alias")),
            ("sampled_as", Value::int(0)),
        ]),
        Value::object([
            ("profile", Value::str("refusal")),
            ("outcome", Value::str("refusal")),
            ("memory", Value::Null),
        ]),
    ];
    let summary = Value::object([
        ("qualified", Value::Bool(false)),
        ("collection_complete", Value::Bool(false)),
    ]);
    let base = Value::object([
        ("schema", Value::int(1)),
        ("counts", counts()),
        (
            "identity",
            Value::object([("cases", Value::strings(["nested \"quote\"\\\n"]))]),
        ),
    ]);
    final_report(&out, &base, &rows, &summary).unwrap();
    let report = read_json(&out.join("report.json")).unwrap();
    assert_eq!(report.get("schema"), Some(&Value::int(1)));
    assert!(
        report
            .get("identity")
            .is_some_and(|v| equal(v, base.get("identity").unwrap()))
    );
    assert!(report.get("counts").is_some_and(|v| equal(v, &counts())));
    assert!(report.get("summary").is_some_and(|v| equal(v, &summary)));
    assert_eq!(
        report.get("acceptance").unwrap().get("accepted"),
        Some(&Value::Bool(false))
    );
    assert_eq!(
        fs::read_to_string(out.join("report.json")).unwrap(),
        format!("{}\n", json::canonical(&report))
    );
    let cases = report.get("cases").unwrap().as_array().unwrap();
    assert_eq!(cases.len(), 3);
    assert_eq!(
        cases
            .iter()
            .map(|c| text(c, "profile").unwrap())
            .collect::<Vec<_>>(),
        ["original", "alias", "refusal"]
    );
    assert!(
        cases[0]
            .get("timing")
            .is_some_and(|value| equal(value, &raw))
    );
    assert!(
        cases[1]
            .get("timing")
            .is_some_and(|value| equal(value, &raw))
    );
    assert_eq!(cases[1].get("sampled_as"), Some(&Value::int(0)));
    for case in &cases[..2] {
        assert!(case.get("memory").is_some_and(|v| equal(v, &memory)));
        assert!(case.get("memory_grade").is_none());
        assert!(case.get("timing_grade").is_none());
    }
    assert!(cases[2].get("timing").is_none());
    assert_eq!(cases[2].get("memory"), Some(&Value::Null));
    assert!(cases[2].get("memory_grade").is_none());
    assert_eq!(
        report.get("collection_status"),
        Some(&Value::str("partial"))
    );
    assert!(!out.join("report.json.tmp").exists());
    let published = fs::read(out.join("report.json")).unwrap();
    assert!(final_report(&out, &base, &rows, &summary).is_err());
    assert_eq!(fs::read(out.join("report.json")).unwrap(), published);
    fs::remove_file(out.join("report.json")).unwrap();
    fs::write(out.join("case-0000/timing.json"), "{}").unwrap();
    let error = final_report(&out, &base, &rows, &summary).unwrap_err();
    assert!(error.contains("raw measurement changed"));
    assert!(!out.join("report.json").exists());
    assert!(!out.join("report.json.tmp").exists());
    fs::remove_dir_all(out).unwrap();
}

#[test]
fn streamed_report_keeps_the_complete_ordered_membership() {
    let suite = Suite::load(&root()).unwrap();
    let rows: Vec<_> = suite.cases.iter().map(Case::description).collect();
    let out = crate::walk::scratch("isobmff-report-membership");
    let summary = Value::object([
        ("qualified", Value::Bool(false)),
        ("collection_complete", Value::Bool(false)),
    ]);
    final_report(
        &out,
        &Value::object([("schema", Value::int(1))]),
        &rows,
        &summary,
    )
    .unwrap();
    let report = read_json(&out.join("report.json")).unwrap();
    assert!(
        report
            .get("cases")
            .is_some_and(|v| equal(v, &Value::Array(rows)))
    );
    assert_eq!(report.get("cases").unwrap().as_array().unwrap().len(), 1020);
    fs::remove_dir_all(out).unwrap();
}

#[test]
#[ignore = "requires the built ISO BMFF example and public-API reference"]
fn installed_workers_match_all_1020_traces() {
    let root = root();
    let suite = Suite::load(&root).unwrap();
    let out = crate::walk::scratch("isobmff-parity");
    fs::create_dir_all(&out).unwrap();
    let mut mismatches = Vec::new();
    for case in &suite.cases {
        let directory = case_directory(&out, case.index);
        fs::create_dir(&directory).unwrap();
        let result = trace(
            &root,
            &root
                .join(gate_build::release_dir().unwrap())
                .join("isobmff-worker"),
            &root.join("target/harness/ffmpeg-isobmff-worker"),
            case,
            &directory,
        )
        .unwrap();
        if result.get("match") != Some(&Value::Bool(true)) {
            mismatches.push(case.index);
        }
    }
    assert!(
        mismatches.is_empty(),
        "mismatching traces {mismatches:?}; outputs: {}",
        out.display()
    );
    fs::remove_dir_all(out).unwrap();
}
