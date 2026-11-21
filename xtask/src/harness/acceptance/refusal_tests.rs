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
