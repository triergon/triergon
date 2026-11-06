//! Reviewed, input-bound reference choices (`harness/comparison-rules.json`).
//! A rule selects a different reference view for one exact input; it is never
//! a general fix and fails closed when anything it was reviewed against changes.

use crate::json::Value;

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
    let name = rule
        .and_then(|r| s(r, "reference"))
        .unwrap_or("ffmpeg-native");
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
    let selected = reference_record(identity, rule)?;
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
