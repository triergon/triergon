use super::super::policy::Runtime;
use super::*;

struct Fixture {
    directory: crate::process::TempDir,
    policy: Value,
    contract: Value,
    row: Value,
    identity: Value,
}

impl Fixture {
    fn new() -> Self {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let policy = crate::checks::read_json(&repository.join("harness/policy.json")).unwrap();
        let gate = Runtime::load(repository).unwrap();
        let pairs = int(policy.get("runtime").unwrap().get("minimum_pairs")).unwrap();
        let reference_ns = 1_000_000;
        let candidate_ns = ((gate.target + gate.ceiling) / 2.0 * f64::from(reference_ns)) as i128;
        let work = |elapsed_ns| {
            Value::object([
                ("status", Value::str("ok")),
                (
                    "data",
                    Value::object([
                        ("elapsed_ns", Value::int(elapsed_ns)),
                        ("iterations", Value::int(1)),
                        ("packets", Value::int(1)),
                        ("bytes", Value::int(10)),
                        ("digest", Value::int(9)),
                    ]),
                ),
            ])
        };
        let mut stages = vec!["before-timing-session".to_owned()];
        let mut warmups = Vec::new();
        for (role, elapsed) in [
            ("candidate", candidate_ns),
            ("reference", i128::from(reference_ns)),
        ] {
            warmups.push(Value::object([
                ("worker", Value::str(role)),
                ("result", work(elapsed)),
            ]));
            stages.extend([
                format!("before-warmup:{role}"),
                format!("after-warmup:{role}"),
            ]);
        }
        let samples = (0..pairs)
            .map(|pair| {
                for role in ["candidate", "reference"] {
                    stages.extend([
                        format!("before-pair:{pair}:{role}"),
                        format!("after-pair:{pair}:{role}"),
                    ]);
                }
                Value::object([
                    ("pair", Value::int(pair)),
                    ("order", Value::strings(["candidate", "reference"])),
                    ("candidate", work(candidate_ns)),
                    ("reference", work(i128::from(reference_ns))),
                    ("cpu_before", Value::Object(vec![])),
                    ("cpu_after", Value::Object(vec![])),
                ])
            })
            .collect();
        let hashes = Value::object([
            ("candidate", Value::str("candidate-hash")),
            ("reference", Value::str("reference-hash")),
        ]);
        let interval = crate::stats::ratio_interval(
            &vec![(candidate_ns as f64, f64::from(reference_ns)); usize::try_from(pairs).unwrap()],
            0,
            4000,
        )
        .unwrap();
        let row = Value::object([
            ("id", Value::str("policy-fixture")),
            ("profile", Value::str("policy-fixture")),
            (
                "parity",
                Value::object([
                    ("status", Value::str("pass")),
                    ("source_timing_status", Value::str("pass")),
                ]),
            ),
            (
                "timing",
                Value::object([
                    ("status", Value::str("measured")),
                    ("pairs", Value::int(pairs)),
                    ("iterations", Value::int(1)),
                    ("seed", Value::int(0)),
                    ("worker_hashes", hashes.clone()),
                    ("ratio", super::super::timing::interval_value(&interval)),
                    ("warmups", Value::Array(warmups)),
                    ("samples", Value::Array(samples)),
                    (
                        "timing_module_policy",
                        Value::str(super::super::timing::MODULE_POLICY),
                    ),
                    (
                        "timing_module_observations",
                        Value::Array(
                            stages
                                .into_iter()
                                .map(|stage| {
                                    Value::object([
                                        ("stage", Value::str(stage)),
                                        (
                                            "state",
                                            Value::object([(
                                                "status",
                                                Value::str("not-applicable"),
                                            )]),
                                        ),
                                    ])
                                })
                                .collect(),
                        ),
                    ),
                ]),
            ),
        ]);
        let fixture = Self {
            directory: crate::process::TempDir::new(None).unwrap(),
            policy,
            contract: Value::object([
                ("required_profiles", Value::strings(["policy-fixture"])),
                ("minimum_pairs", Value::int(pairs)),
                ("iterations", Value::int(1)),
            ]),
            row,
            identity: Value::object([("binaries", hashes)]),
        };
        std::fs::create_dir(fixture.directory.0.join("harness")).unwrap();
        fixture.write_policy();
        fixture
    }

    fn write_policy(&self) {
        std::fs::write(
            self.directory.0.join("harness/policy.json"),
            crate::json::canonical(&self.policy),
        )
        .unwrap();
    }

    fn set_runtime(&mut self, key: &str, value: f64) {
        let Value::Object(fields) = &mut self.policy else {
            unreachable!()
        };
        let Value::Object(runtime) = &mut fields
            .iter_mut()
            .find(|(name, _)| name == "runtime")
            .unwrap()
            .1
        else {
            unreachable!()
        };
        runtime.iter_mut().find(|(name, _)| name == key).unwrap().1 = Value::float(value);
        self.write_policy();
    }

    fn verdict(&self) -> CaseVerdict {
        grade_case(
            &self.directory.0,
            &self.contract,
            &self.row,
            &self.identity,
            "host",
        )
    }

    fn upper(&self) -> f64 {
        self.row
            .get("timing")
            .unwrap()
            .get("ratio")
            .unwrap()
            .get("upper_95")
            .unwrap()
            .as_f64()
            .unwrap()
    }
}

#[test]
fn lowering_policy_ceiling_regrades_valid_raw_timing() {
    let mut fixture = Fixture::new();
    assert!(fixture.verdict().timing_pass);
    let gate = Runtime::load(&fixture.directory.0).unwrap();
    fixture.set_runtime("ratio_max", (gate.target + fixture.upper()) / 2.0);
    let verdict = fixture.verdict();
    assert!(!verdict.timing_pass);
    assert!(
        verdict
            .reasons
            .iter()
            .any(|reason| reason.ends_with("runtime gate unproven or failed"))
    );
}

#[test]
fn changing_only_policy_target_does_not_refuse_or_regrade_runtime() {
    let mut fixture = Fixture::new();
    let before = fixture.verdict();
    assert!(before.timing_pass);
    let gate = Runtime::load(&fixture.directory.0).unwrap();
    fixture.set_runtime(
        "optimisation_target",
        (fixture.upper() + gate.ceiling) / 2.0,
    );
    let after = fixture.verdict();
    assert!(after.timing_pass);
    assert_eq!(before.reasons, after.reasons);
}
