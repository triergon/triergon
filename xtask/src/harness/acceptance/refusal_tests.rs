use super::*;

fn fixture() -> (Value, Value) {
    let contract = Value::object([
        (
            "required_profiles",
            Value::strings(["output", "refusal-profile"]),
        ),
        (
            "required_refusals",
            Value::Array(vec![Value::object([
                ("id", Value::str("refusal")),
                ("profile", Value::str("refusal-profile")),
                ("sha256", Value::str("a".repeat(64))),
            ])]),
        ),
        (
            "refusal",
            Value::object([
                ("reference_exit_code", Value::int(1)),
                ("reference_error_code", Value::int(-7)),
                (
                    "candidate_stderr_prefixes",
                    Value::strings(["candidate: truncated"]),
                ),
            ]),
        ),
    ]);
    let trace = |stderr| {
        Value::object([
            ("status", Value::str("finished")),
            ("exit_code", Value::int(1)),
            ("stdout_sha256", Value::str(crate::sha256::hex(b""))),
            ("stderr", Value::str(stderr)),
        ])
    };
    let row = Value::object([
        ("id", Value::str("refusal")),
        ("profile", Value::str("refusal-profile")),
        ("input_sha256", Value::str("a".repeat(64))),
        ("outcome", Value::str("refusal")),
        (
            "parity",
            Value::object([
                ("status", Value::str("pass")),
                ("source_timing_status", Value::str("pass")),
                ("comparison", Value::str("refusal")),
            ]),
        ),
        ("candidate_trace", trace("candidate: truncated input")),
        ("reference_trace", trace("reference: invalid (-7)")),
    ]);
    (contract, row)
}

fn set(value: &mut Value, key: &str, data: Value) {
    let Value::Object(fields) = value else {
        panic!("expected object");
    };
    if let Some((_, old)) = fields.iter_mut().find(|(name, _)| name == key) {
        *old = data;
    } else {
        fields.push((key.into(), data));
    }
}

#[test]
fn registered_refusal_requires_matching_source_and_actual_failure() {
    let (contract, row) = fixture();
    let registered = registered_refusals(&contract).unwrap();
    let spec = &registered["refusal"];
    assert!(refusal_valid(&row, spec, &contract));
    for (key, data) in [
        ("profile", Value::str("other")),
        ("input_sha256", Value::str("b".repeat(64))),
        ("timing", Value::object([] as [(&str, Value); 0])),
    ] {
        let mut changed = row.clone();
        set(&mut changed, key, data);
        assert!(!refusal_valid(&changed, spec, &contract), "{key}");
    }
    for role in ["candidate_trace", "reference_trace"] {
        for (key, data) in [
            ("exit_code", Value::int(0)),
            (
                "stdout_sha256",
                Value::str(crate::sha256::hex(b"unexpected output")),
            ),
            ("stderr", Value::str("unrelated diagnostic")),
            ("stderr_sha256", Value::str("wrong hash")),
        ] {
            let mut changed = row.clone();
            let mut trace = changed.get(role).unwrap().clone();
            set(&mut trace, key, data);
            set(&mut changed, role, trace);
            assert!(!refusal_valid(&changed, spec, &contract), "{role}/{key}");
        }
    }
}

#[test]
fn refusal_registration_requires_declared_profiles_and_unique_identities() {
    let (mut contract, _) = fixture();
    set(
        &mut contract,
        "required_profiles",
        Value::strings(["output"]),
    );
    assert!(registered_refusals(&contract).is_err());
    let (mut contract, _) = fixture();
    let spec = contract
        .get("required_refusals")
        .unwrap()
        .as_array()
        .unwrap()[0]
        .clone();
    set(
        &mut contract,
        "required_refusals",
        Value::Array(vec![spec.clone(), spec]),
    );
    assert!(registered_refusals(&contract).is_err());
}

#[test]
fn streaming_failure_cannot_omit_required_refusals() {
    let (contract, _) = fixture();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let verdict = acceptance_iter(
        root,
        &contract,
        std::iter::once(Err("raw file unavailable".into())),
        &Value::Null,
        "host",
        &[],
        &[],
    );
    assert_eq!(verdict.get("slice_accepted"), Some(&Value::Bool(false)));
    let reasons = verdict.get("slice_reasons").unwrap().as_array().unwrap();
    assert!(reasons.contains(&Value::str(
        "case evidence unavailable: raw file unavailable"
    )));
    assert!(reasons.contains(&Value::str("refusal: required refusal missing or invalid")));
}

/// The verdict on the rows of a contract with one policy difference (`p`) and one
/// justified refusal (`r`).
fn listed_verdict(rows: &[Value]) -> Vec<String> {
    let contract = crate::json::parse(
        r#"{"open_policies": [{"policy": "q", "undecided": "u", "rule": "ours",
                "documented": "crates/x/README.md#rules", "reference_rule": "theirs",
                "compat": "none: a heuristic", "cases": ["p"]}],
            "open_refusals": [{"reason": "r", "clause": "c", "class": "unsupported",
                "detail": "d", "cases": ["r"]}]}"#,
    )
    .unwrap();
    let verdict = acceptance(
        Path::new("."),
        &contract,
        rows,
        &Value::Object(Vec::new()),
        "t",
        &[],
        &[],
    );
    verdict
        .get("slice_reasons")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

#[test]
fn only_a_justified_refusal_counts_among_the_listed_cases() {
    let row = |text: &str| crate::json::parse(text).unwrap();
    let refused = |named: &str, digest: u32| {
        row(&format!(
            r#"{{"id": "r", "parity": {{"status": "pass", "source_timing_status": "pass",
                "comparison": "registered-fold-refusal",
                "candidate_expected": {{"packets": 1, "bytes": 0, "digest": 8}},
                "refusal_proof": {{"named": "{named}", "candidate": {{"packets": 1, "bytes": 0,
                    "digest": {digest}, "eof": true, "unsupported": "d"}}}}}}}}"#
        ))
    };
    let refusal = refused("d", 8);
    let policy = row(
        r#"{"id": "p", "parity": {"status": "listed", "source_timing_status": "listed",
            "listed": ["open_policies"]}}"#,
    );
    let reasons = listed_verdict(&[refusal, policy]);
    // The policy difference withholds acceptance and is named by its list; the
    // refusal's proof holds and adds no reason of its own.
    assert!(
        reasons.contains(&"p: listed in open_policies".to_owned()),
        "{reasons:?}"
    );
    assert!(
        !reasons
            .iter()
            .any(|r| r.starts_with("r: registered") || r.starts_with("r: listed")),
        "{reasons:?}"
    );
    // A refusal whose recorded proof names another detail, or whose fold is not the
    // pin recorded with it, is no proof.
    for forged in [refused("e", 8), refused("d", 9)] {
        let reasons = listed_verdict(&[forged]);
        assert!(
            reasons.contains(&"r: registered fold refusal proof missing or invalid".to_owned()),
            "{reasons:?}"
        );
    }
    // A listed refusal the report did not see is missing.
    let reasons = listed_verdict(&[]);
    assert!(
        reasons.contains(&"r: required refusal missing or invalid".to_owned()),
        "{reasons:?}"
    );
}
