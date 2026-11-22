//! One-sided decoder refusals with complete, input-bound worker folds.

use std::collections::BTreeMap;

use crate::json::Value;

use super::{comparison, diff, worker};

pub(super) const COMPARISON: &str = "registered-fold-refusal";
pub(super) const REQUEST: &str = "--report-refusal";

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

pub(super) struct Refusal<'a> {
    sha256: &'a str,
    detail: &'a str,
    candidate: &'a Value,
}

pub(super) fn read<'a>(
    contract: &'a Value,
    target: &str,
) -> Result<BTreeMap<&'a str, Refusal<'a>>, String> {
    let Some(groups) = contract.get("open_refusals") else {
        return Ok(BTreeMap::new());
    };
    let mut all = BTreeMap::new();
    if text(contract, "worker_operation").is_none() || contract.get("oracle").is_some() {
        return Err("registered fold refusals require a prefix worker comparison".into());
    }
    let groups = groups.as_array().ok_or("open_refusals must be an array")?;
    let targets = contract
        .get("required_targets")
        .and_then(Value::as_array)
        .unwrap_or_default();
    let fields = comparison::work_fields(contract)?;
    for group in groups {
        let line = |key| {
            text(group, key)
                .filter(|text| !text.trim().is_empty() && !text.contains('\n'))
                .ok_or_else(|| format!("registered fold refusal needs {key}"))
        };
        line("reason")?;
        line("clause")?;
        let detail = line("detail")?;
        let active = match group.get("targets") {
            None => true,
            Some(scope) => {
                let scope = scope.as_array().ok_or("invalid refusal targets")?;
                if scope.is_empty() || scope.iter().any(|entry| !targets.contains(entry)) {
                    return Err("refusal targets must be among the required targets".into());
                }
                scope.iter().any(|value| value.as_str() == Some(target))
            }
        };
        let cases = group
            .get("cases")
            .and_then(Value::as_array)
            .filter(|cases| !cases.is_empty())
            .ok_or("registered fold refusal needs cases")?;
        for case in cases {
            let id = text(case, "id").filter(|id| !id.is_empty()).ok_or("missing refusal id")?;
            let sha256 = super::validation::hash(case, "sha256")?;
            let candidate = case.get("candidate").ok_or("missing candidate refusal fold")?;
            if !diff::equal(candidate, &comparison::work_summary(candidate, &fields)?) {
                return Err(format!("{id}: candidate refusal fold has unexpected fields"));
            }
            let entry = Refusal { sha256, detail, candidate };
            if all.insert(id, active.then_some(entry)).is_some() {
                return Err(format!("{id}: duplicate registered fold refusal"));
            }
        }
    }
    Ok(all.into_iter().filter_map(|(id, entry)| Some((id, entry?))).collect())
}

impl Refusal<'_> {
    pub(super) fn binds_input(&self, sha256: Option<&str>) -> bool {
        sha256 == Some(self.sha256)
    }

    pub(super) fn proof(
        &self,
        left: &Value,
        right: &Value,
        expected: &Value,
        fields: &[String],
    ) -> Option<Value> {
        if !worker::ok(left) || !worker::ok(right) {
            return None;
        }
        let candidate = worker::data(left);
        let reference = worker::data(right);
        self.matches(candidate, reference, expected, fields).then(|| {
            Value::object([("candidate", candidate.clone()), ("reference", reference.clone())])
        })
    }

    fn matches(&self, candidate: &Value, reference: &Value, expected: &Value, fields: &[String]) -> bool {
        let fold = |observed: &Value, pin: &Value| {
            observed.get("eof") == Some(&Value::Bool(true))
                && comparison::work_summary(observed, fields)
                    .is_ok_and(|actual| diff::equal(&actual, pin))
        };
        text(candidate, "unsupported") == Some(self.detail)
            && !crate::generators::host_dependent(reference)
            && !crate::generators::host_dependent(expected)
            && !diff::equal(self.candidate, expected)
            && fold(candidate, self.candidate)
            && fold(reference, expected)
    }

    pub(super) fn matches_row(&self, row: &Value, expected: &Value, fields: &[String]) -> bool {
        self.binds_input(text(row, "input_sha256"))
            && ["timing", "memory"]
                .iter()
                .all(|key| row.get(key).is_none_or(|value| *value == Value::Null))
            && row.get("parity").is_some_and(|parity| {
                text(parity, "comparison") == Some(COMPARISON)
                    && parity.get("refusal_proof").is_some_and(|proof| {
                        self.matches(
                            proof.get("candidate").unwrap_or(&Value::Null),
                            proof.get("reference").unwrap_or(&Value::Null),
                            expected,
                            fields,
                        )
                    })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    #[test]
    fn refusal_requires_the_exact_typed_error_and_both_complete_folds() {
        let expected = parse(r#"{"packets":4,"bytes":19,"digest":42}"#).unwrap();
        let candidate = parse(r#"{"packets":2,"bytes":11,"digest":22}"#).unwrap();
        let refusal = Refusal {
            sha256: "input",
            detail: "slice groups",
            candidate: &candidate,
        };
        let outcome = |data| Value::object([("status", Value::str("ok")), ("data", data)]);
        let left = outcome(parse(
            r#"{"packets":2,"bytes":11,"digest":22,"unsupported":"slice groups","eof":true}"#,
        ).unwrap());
        let right = outcome(parse(r#"{"packets":4,"bytes":19,"digest":42,"eof":true}"#).unwrap());
        assert!(refusal.proof(&left, &right, &expected, &[]).is_some());
        for (side, key, value) in [
            (true, "unsupported", Value::str("other")),
            (true, "digest", Value::int(23)),
            (true, "eof", Value::Bool(false)),
            (false, "digest", Value::int(43)),
            (false, "eof", Value::Bool(false)),
        ] {
            let mut data = worker::data(if side { &left } else { &right }).clone();
            super::super::validation::put(&mut data, key, value).unwrap();
            let changed = outcome(data);
            let (a, b) = if side { (&changed, &right) } else { (&left, &changed) };
            assert!(refusal.proof(a, b, &expected, &[]).is_none(), "{key}");
        }
        assert!(refusal.proof(&left, &Value::Null, &expected, &[]).is_none());
        assert!(refusal.proof(&left, &left, &candidate, &[]).is_none());
        assert!(!refusal.binds_input(Some("different")));
    }
}
