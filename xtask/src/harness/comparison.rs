//! Reviewed, input-bound reference choices (`harness/comparison-rules.json`).
//! A rule selects a different reference view for one exact input; it is never
//! a general fix and fails closed when anything it was reviewed against changes.

use crate::json::Value;

pub(crate) const BASE_WORK_FIELDS: [&str; 3] = ["packets", "bytes", "digest"];

fn required_fields(declared: &[String]) -> Vec<String> {
    let mut fields: Vec<_> = BASE_WORK_FIELDS.map(str::to_owned).into();
    for field in declared {
        if !fields.contains(field) {
            fields.push(field.clone());
        }
    }
    fields
}

/// Every declared numeric work field, with the three ordinary fields mandatory.
pub(crate) fn work_fields(contract: &Value) -> Result<Vec<String>, String> {
    let mut declared = Vec::new();
    if let Some(value) = contract.get("work_fields") {
        let values = value.as_array().ok_or("work_fields must be an array")?;
        for value in values {
            let field = value
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("work_fields must contain nonempty strings")?
                .to_owned();
            if declared.contains(&field) {
                return Err(format!("duplicate work field {field}"));
            }
            declared.push(field);
        }
    }
    Ok(required_fields(&declared))
}

/// A complete numeric work projection. Missing fields on both sides still fail.
pub(crate) fn work_summary(data: &Value, fields: &[String]) -> Result<Value, String> {
    let mut work = Vec::new();
    for key in required_fields(fields) {
        if super::linux::py_int(data.get(&key)).is_none_or(|n| n < 0) {
            return Err(format!("missing or invalid work field {key}"));
        }
        work.push((
            key.clone(),
            data.get(&key).expect("checked work field").clone(),
        ));
    }
    Ok(Value::Object(work))
}

/// Equal work by both timed roles, including the declared iteration count.
pub(crate) fn paired_timing_work(
    candidate: &Value,
    reference: &Value,
    iterations: i128,
    fields: &[String],
) -> Result<Value, String> {
    for outcome in [candidate, reference] {
        let data = super::worker::data(outcome);
        if !super::worker::ok(outcome)
            || super::linux::py_int(data.get("iterations")) != Some(iterations)
            || super::linux::py_int(data.get("elapsed_ns")).is_none_or(|n| n < 0)
        {
            return Err("invalid timed worker result".into());
        }
    }
    let a = work_summary(super::worker::data(candidate), fields)?;
    let b = work_summary(super::worker::data(reference), fields)?;
    if !super::diff::equal(&a, &b) {
        return Err("unequal timed work".into());
    }
    Ok(a)
}

/// Both warmups publish the same complete work before any timed pair runs.
pub(crate) fn warmup_work(
    warmups: &[Value],
    iterations: i128,
    fields: &[String],
) -> Result<Value, String> {
    if warmups.len() != 2
        || warmups[0].get("worker").and_then(Value::as_str) != Some("candidate")
        || warmups[1].get("worker").and_then(Value::as_str) != Some("reference")
    {
        return Err("missing or duplicate timed warmups".into());
    }
    paired_timing_work(
        warmups[0].get("result").ok_or("missing candidate warmup")?,
        warmups[1].get("result").ok_or("missing reference warmup")?,
        iterations,
        fields,
    )
}

/// Recheck the complete selected work in a detailed parity record.
pub(crate) fn parity_work_valid(proof: &Value, fields: &[String]) -> bool {
    let Some(expected) = proof
        .get("expected")
        .and_then(|v| work_summary(v, fields).ok())
    else {
        return false;
    };
    ["candidate", "reference"].iter().all(|role| {
        proof
            .get(role)
            .and_then(|v| work_summary(v, fields).ok())
            .is_some_and(|work| super::diff::equal(&work, &expected))
    })
}

fn s<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// The rule that applies to `case`, if any.
pub fn rule_for<'a>(
    case: &Value,
    identity: &'a Value,
    contract_id: Option<&str>,
) -> Result<Option<&'a Value>, String> {
    let registry = identity.get("comparison_rules");
    let rules = registry
        .and_then(|r| r.get("rules"))
        .and_then(Value::as_array)
        .unwrap_or_default();
    let matches: Vec<&Value> = rules
        .iter()
        .filter(|r| s(r, "case") == s(case, "id") || s(r, "profile") == s(case, "profile"))
        .collect();
    if matches.is_empty() {
        return Ok(None);
    }
    let approved = registry
        .and_then(|r| r.get("approval"))
        .and_then(|a| s(a, "status"))
        == Some("approved");
    if matches.len() != 1 || !approved {
        return Err("comparison rule is missing a unique approval".into());
    }
    let rule = matches[0];
    if contract_id != Some("matroska-zlib") {
        return Err("comparison rule used outside its selected contract".into());
    }
    if identity.get("source_revision") != registry.and_then(|r| r.get("reference_source_revision"))
    {
        return Err("comparison reference source pin changed".into());
    }
    if s(case, "id") != s(rule, "case") || s(case, "profile") != s(rule, "profile") {
        return Err("comparison case/profile changed".into());
    }
    let input = case.get("input_sha256").or_else(|| case.get("sha256"));
    if input.and_then(Value::as_str) != s(rule, "input_sha256") {
        return Err("comparison input identity changed".into());
    }
    if case
        .get("seek_target_ns")
        .is_some_and(|v| *v != Value::Null)
    {
        return Err("comparison rule does not cover a seek operation".into());
    }
    Ok(Some(rule))
}

/// The built reference worker a case uses: `{id, path, sha256, kind, ...}`.
pub fn reference_record(identity: &Value, rule: Option<&Value>) -> Result<Value, String> {
    reference_named(
        identity,
        rule.and_then(|r| s(r, "reference"))
            .unwrap_or("ffmpeg-native"),
    )
}

/// The built comparison reference recorded under `name` in the identity.
pub fn reference_named(identity: &Value, name: &str) -> Result<Value, String> {
    let record = identity
        .get("comparison_references")
        .and_then(|r| r.get(name))
        .cloned();
    let record = match record {
        Some(record) => record,
        None if name == "ffmpeg-native" => Value::object([
            ("path", Value::str("target/harness/ffmpeg-worker")),
            (
                "sha256",
                identity
                    .get("binaries")
                    .and_then(|b| b.get("reference"))
                    .cloned()
                    .unwrap_or(Value::Null),
            ),
            ("kind", Value::str("pinned native FFmpeg")),
        ]),
        None => return Err("selected native reference was not built".into()),
    };
    let mut members = vec![("id".to_owned(), Value::str(name))];
    if let Value::Object(fields) = record {
        members.extend(fields.into_iter().filter(|(k, _)| k != "id"));
    }
    Ok(Value::Object(members))
}

pub fn reference_summary(record: &Value) -> Value {
    Value::object(
        ["id", "sha256", "kind"].map(|k| (k, record.get(k).cloned().unwrap_or(Value::Null))),
    )
}

/// The exact selected worker hashes of a recorded case, or why its evidence
/// does not match the reviewed disposition.
pub fn validate_case_evidence(
    case: &Value,
    identity: &Value,
    contract_id: Option<&str>,
) -> Result<Value, String> {
    let rule = rule_for(case, identity, contract_id)?;
    // A contract without comparison rules may name its own reference.
    let selected = match (rule, s(case, "reference_worker")) {
        (None, Some(name)) => reference_named(identity, name)?,
        _ => reference_record(identity, rule)?,
    };
    if (rule.is_some() || case.get("reference").is_some())
        && !case
            .get("reference")
            .is_some_and(|r| super::diff::equal(r, &reference_summary(&selected)))
    {
        return Err("case reference does not match the built selected worker".into());
    }
    if let Some(rule) = rule {
        let parity = case.get("parity").cloned().unwrap_or(Value::Object(vec![]));
        if parity.get("comparison_rule") != rule.get("id") {
            return Err("approved comparison rule evidence missing".into());
        }
        if s(&parity, "source_content_status") != Some("pass") {
            return Err("complete source content proof missing".into());
        }
        let reviewed = Value::object([
            ("status", Value::str("reviewed")),
            ("rule", rule.get("id").cloned().unwrap_or(Value::Null)),
        ]);
        if !parity
            .get("raw_native_disposition")
            .is_some_and(|d| super::diff::equal(d, &reviewed))
        {
            return Err("raw native disagreement was not retained and checked".into());
        }
        let expected = s(rule, "raw_native_output_sha256").unwrap_or_default();
        if expected.is_empty() || s(&parity, "raw_native_output_sha256") != Some(expected) {
            return Err("exact raw native output proof missing".into());
        }
        if case.get("operation").and_then(|o| o.get("view")) != rule.get("view") {
            return Err("timed comparison view differs from the approved view".into());
        }
    }
    // A row measured with a contract's own candidate names it; the hash comes
    // from the build identity, never from the row.
    let candidate = match s(case, "candidate_worker") {
        None => identity.get("binaries").and_then(|b| b.get("candidate")),
        Some(name) => identity
            .get("additional_candidates")
            .and_then(|c| c.get(name))
            .and_then(|record| record.get("sha256")),
    };
    Ok(Value::object([
        ("candidate", candidate.cloned().unwrap_or(Value::Null)),
        (
            "reference",
            selected.get("sha256").cloned().unwrap_or(Value::Null),
        ),
    ]))
}

#[cfg(test)]
mod work_tests {
    use super::*;
    use crate::json::parse;

    fn v(text: &str) -> Value {
        parse(text).unwrap()
    }

    fn result(second: Option<u64>) -> Value {
        let mut data = vec![
            ("iterations", Value::int(50)),
            ("elapsed_ns", Value::int(100)),
            ("packets", Value::int(1)),
            ("bytes", Value::int(2)),
            ("digest", Value::int(3)),
        ];
        if let Some(second) = second {
            data.push(("digest_b", Value::int(second)));
        }
        Value::object([("status", Value::str("ok")), ("data", Value::object(data))])
    }

    #[test]
    fn base_fields_stay_mandatory_and_default_projection_stays_three_fields() {
        let fields = work_fields(&v(r#"{"work_fields":["digest_b"]}"#)).unwrap();
        assert_eq!(fields, ["packets", "bytes", "digest", "digest_b"]);
        let data = super::super::worker::data(&result(Some(4))).clone();
        assert_eq!(
            work_summary(&data, &[]).unwrap(),
            v(r#"{"packets":1,"bytes":2,"digest":3}"#)
        );
        for missing in BASE_WORK_FIELDS {
            let Value::Object(mut changed) = data.clone() else {
                unreachable!()
            };
            changed.retain(|(k, _)| k != missing);
            assert!(work_summary(&Value::Object(changed), &fields).is_err());
        }
        for contract in [
            r#"{"work_fields":null}"#,
            r#"{"work_fields":[""]}"#,
            r#"{"work_fields":[1]}"#,
            r#"{"work_fields":["digest_b","digest_b"]}"#,
        ] {
            assert!(work_fields(&v(contract)).is_err());
        }
    }

    #[test]
    fn pairs_and_warmups_require_every_declared_field_on_both_roles() {
        let fields = work_fields(&v(r#"{"work_fields":["digest_b"]}"#)).unwrap();
        let complete = result(Some(4));
        let summary = paired_timing_work(&complete, &complete, 50, &fields).unwrap();
        assert_eq!(summary.get("digest_b"), Some(&Value::int(4)));
        for (a, b) in [
            (result(None), complete.clone()),
            (complete.clone(), result(None)),
            (result(None), result(None)),
            (result(Some(5)), complete.clone()),
        ] {
            assert!(paired_timing_work(&a, &b, 50, &fields).is_err());
            let warmups = [
                Value::object([("worker", Value::str("candidate")), ("result", a)]),
                Value::object([("worker", Value::str("reference")), ("result", b)]),
            ];
            assert!(warmup_work(&warmups, 50, &fields).is_err());
        }
        let warmups = [
            Value::object([
                ("worker", Value::str("candidate")),
                ("result", complete.clone()),
            ]),
            Value::object([("worker", Value::str("reference")), ("result", complete)]),
        ];
        assert_eq!(warmup_work(&warmups, 50, &fields).unwrap(), summary);
    }
}
