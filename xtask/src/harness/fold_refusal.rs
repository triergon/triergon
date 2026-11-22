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
    compare_reference: bool,
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
            let id = text(case, "id")
                .filter(|id| !id.is_empty())
                .ok_or("missing refusal id")?;
            let sha256 = super::validation::hash(case, "sha256")?;
            let candidate = case
                .get("candidate")
                .ok_or("missing candidate refusal fold")?;
            if !diff::equal(candidate, &comparison::work_summary(candidate, &fields)?) {
                return Err(format!(
                    "{id}: candidate refusal fold has unexpected fields"
                ));
            }
            let compare_reference = match case.get("reference_pin_targets") {
                None => true,
                Some(pins) => {
                    let pins = pins
                        .as_array()
                        .filter(|pins| !pins.is_empty() && pins.iter().all(|p| targets.contains(p)))
                        .ok_or_else(|| {
                            format!("{id}: reference_pin_targets must name required targets")
                        })?;
                    pins.iter().any(|pin| pin.as_str() == Some(target))
                }
            };
            let entry = Refusal {
                sha256,
                detail,
                candidate,
                compare_reference,
            };
            if all.insert(id, active.then_some(entry)).is_some() {
                return Err(format!("{id}: duplicate registered fold refusal"));
            }
        }
    }
    Ok(all
        .into_iter()
        .filter_map(|(id, entry)| Some((id, entry?)))
        .collect())
}

impl Refusal<'_> {
    pub(super) fn binds_input(&self, sha256: Option<&str>) -> bool {
        sha256 == Some(self.sha256)
    }

    pub(super) fn compares_reference(&self) -> bool {
        self.compare_reference
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
        self.matches(candidate, reference, expected, fields)
            .then(|| {
                Value::object([
                    ("candidate", candidate.clone()),
                    ("reference", reference.clone()),
                ])
            })
    }

    fn matches(
        &self,
        candidate: &Value,
        reference: &Value,
        expected: &Value,
        fields: &[String],
    ) -> bool {
        let eof = Some(&Value::Bool(true));
        if text(candidate, "unsupported") != Some(self.detail)
            || candidate.get("eof") != eof
            || reference.get("eof") != eof
            || !comparison::work_summary(candidate, fields)
                .is_ok_and(|actual| diff::equal(&actual, self.candidate))
        {
            return false;
        }
        comparison::work_summary(reference, fields).is_ok_and(|actual| {
            !self.compare_reference
                || (!crate::generators::host_dependent(reference)
                    && !crate::generators::host_dependent(expected)
                    && !diff::equal(self.candidate, expected)
                    && diff::equal(&actual, expected))
        })
    }

    pub(super) fn matches_row(&self, row: &Value, expected: &Value, fields: &[String]) -> bool {
        self.compare_reference
            && self.binds_input(text(row, "input_sha256"))
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
    fn registered_refusal_requires_candidate_evidence_and_compares_only_pinned_references() {
        const PINNED: &str = "aarch64-unknown-linux-gnu";
        const UNPINNED: &str = "x86_64-unknown-linux-gnu";
        const SHA: &str = "09c66ef606be137cb001fd6f806deabdc493ce5a06469ed8ce6ae8811ce8fade";
        let contract = parse(&format!(
            r#"{{
                "worker_operation": "picture-properties",
                "required_targets": ["{PINNED}"],
                "open_refusals": [{{
                    "reason": "Slice groups", "clause": "H.264 §8.2.2", "detail": "slice groups",
                    "cases": [
                        {{"id":"strict","sha256":"{SHA}",
                          "candidate":{{"packets":2,"bytes":11,"digest":22}}}},
                        {{"id":"marked","sha256":"{SHA}","reference_pin_targets":["{PINNED}"],
                          "candidate":{{"packets":2,"bytes":11,"digest":22}}}}
                    ]
                }}]
            }}"#
        ))
        .unwrap();
        let expected = parse(r#"{"packets":4,"bytes":19,"digest":42}"#).unwrap();
        let outcome = |data| Value::object([("status", Value::str("ok")), ("data", data)]);
        let left = outcome(
            parse(
                r#"{"packets":2,"bytes":11,"digest":22,"unsupported":"slice groups","eof":true}"#,
            )
            .unwrap(),
        );
        let right = outcome(parse(r#"{"packets":4,"bytes":19,"digest":42,"eof":true}"#).unwrap());
        let changed = |worker, key, value| {
            let mut data = worker::data(worker).clone();
            super::super::validation::put(&mut data, key, value).unwrap();
            outcome(data)
        };
        for target in [PINNED, UNPINNED] {
            let refusals = read(&contract, target).unwrap();
            for (id, refusal) in refusals {
                let compares = id == "strict" || target == PINNED;
                assert_eq!(refusal.compares_reference(), compares);
                let proof = refusal.proof(&left, &right, &expected, &[]).unwrap();
                let row = |sha| {
                    Value::object([
                        ("input_sha256", Value::str(sha)),
                        (
                            "parity",
                            Value::object([
                                ("comparison", Value::str(COMPARISON)),
                                ("refusal_proof", proof.clone()),
                            ]),
                        ),
                    ])
                };
                assert!(refusal.binds_input(Some(SHA)));
                assert!(!refusal.binds_input(Some("different")));
                assert_eq!(refusal.matches_row(&row(SHA), &expected, &[]), compares);
                assert!(!refusal.matches_row(&row("different"), &expected, &[]));
                for (key, value) in [
                    ("unsupported", Value::str("other")),
                    ("digest", Value::int(23)),
                    ("eof", Value::Bool(false)),
                ] {
                    let bad = changed(&left, key, value);
                    assert!(
                        refusal.proof(&bad, &right, &expected, &[]).is_none(),
                        "{key}"
                    );
                }
                for (key, value, passes) in [
                    ("digest", Value::int(43), !compares),
                    ("host_dependent", Value::Bool(true), !compares),
                    ("eof", Value::Bool(false), false),
                    ("digest", Value::Null, false),
                ] {
                    let other = changed(&right, key, value);
                    assert_eq!(
                        refusal.proof(&left, &other, &expected, &[]).is_some(),
                        passes,
                        "{key}"
                    );
                }
                let mut flagged = expected.clone();
                super::super::validation::put(&mut flagged, "host_dependent", Value::Bool(true))
                    .unwrap();
                assert_eq!(
                    refusal.proof(&left, &right, &flagged, &[]).is_some(),
                    !compares
                );
                assert_eq!(
                    refusal
                        .proof(&left, &left, refusal.candidate, &[])
                        .is_some(),
                    !compares
                );
                assert!(refusal.proof(&left, &Value::Null, &expected, &[]).is_none());
                assert!(
                    refusal
                        .proof(&Value::Null, &right, &expected, &[])
                        .is_none()
                );
            }
        }
    }
}
