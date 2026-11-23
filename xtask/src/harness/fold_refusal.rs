//! The proof of a listed case that compares nothing of the reference: a refusal the
//! specification justifies, or a feature neither side implements. It holds the
//! candidate's fold up to the refusal, as pinned, and the detail the candidate
//! names; what the reference makes of the input is no part of it.

use crate::json::Value;

use super::registry::{Case, Named};
use super::{comparison, diff, worker};

/// The comparison a justified refusal's row records.
pub(super) const COMPARISON: &str = "registered-fold-refusal";
/// The comparison of a case not comparable with the reference that is no refusal.
pub(super) const CANDIDATE_ONLY: &str = "candidate-only";
/// The argument that asks a worker to name its refusal on the result line.
pub(super) const REQUEST: &str = "--report-refusal";

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Whether the candidate named `named`: as the `unsupported` value of its result
/// line `data`, or in its `trace`.
pub(super) fn names(named: &Named, data: &Value, trace: Option<&str>) -> bool {
    if named.trace {
        trace.is_some_and(|trace| trace.contains(&named.detail))
    } else {
        text(data, "unsupported") == Some(named.detail.as_str())
    }
}

/// The proof of `case` from the candidate's run `left` and, under trace evidence,
/// its `trace`: the run finished, its fold equals `pin`, and it names the case's
/// detail. `None` when any of them fails or the case has no pin.
pub(super) fn proof(
    case: &Case,
    pin: Option<&Value>,
    left: &Value,
    trace: Option<&str>,
    fields: &[String],
) -> Option<Value> {
    if !worker::ok(left) {
        return None;
    }
    let candidate = worker::data(left);
    let named = case.named.as_ref();
    let holds =
        folded(candidate, pin?, fields) && named.is_none_or(|named| names(named, candidate, trace));
    holds.then(|| {
        Value::object([
            ("candidate", candidate.clone()),
            (
                "named",
                named.map_or(Value::Null, |named| Value::str(&named.detail)),
            ),
        ])
    })
}

/// Whether the result line `candidate` finished with exactly the fold `pin`.
fn folded(candidate: &Value, pin: &Value, fields: &[String]) -> bool {
    candidate.get("eof") == Some(&Value::Bool(true))
        && comparison::work_summary(candidate, fields).is_ok_and(|actual| diff::equal(&actual, pin))
}

/// Whether a report `row` still holds the proof of the refusal `case` against the
/// candidate's `pin`: the row was neither timed nor measured, its parity records
/// the refusal, and the recorded candidate result folds to the pin and names the
/// detail (which a trace's proof records as found).
pub(super) fn row_holds(row: &Value, case: &Case, pin: Option<&Value>, fields: &[String]) -> bool {
    let Some(pin) = pin else {
        return false;
    };
    ["timing", "memory"]
        .iter()
        .all(|key| row.get(key).is_none_or(|value| *value == Value::Null))
        && row.get("parity").is_some_and(|parity| {
            text(parity, "comparison") == Some(COMPARISON)
                && parity.get("refusal_proof").is_some_and(|proof| {
                    let candidate = proof.get("candidate").unwrap_or(&Value::Null);
                    folded(candidate, pin, fields)
                        && case.named.as_ref().is_none_or(|named| {
                            text(proof, "named") == Some(named.detail.as_str())
                                && (named.trace || names(named, candidate, None))
                        })
                })
        })
}

#[cfg(test)]
mod tests {
    use super::super::registry::Form;
    use super::*;
    use crate::json::parse;

    fn outcome(data: &str) -> Value {
        Value::object([("status", Value::str("ok")), ("data", parse(data).unwrap())])
    }

    fn refusal(trace: bool) -> Case {
        Case {
            forms: vec![Form::Refusal],
            named: Some(Named {
                detail: if trace {
                    "invalid data: short box".into()
                } else {
                    "slice groups".into()
                },
                trace,
            }),
            candidate_only: true,
            unpinned: false,
        }
    }

    #[test]
    fn a_refusal_is_proved_by_the_candidate_alone() {
        let pin = parse(r#"{"packets":2,"bytes":11,"digest":22}"#).unwrap();
        let left = outcome(
            r#"{"packets":2,"bytes":11,"digest":22,"unsupported":"slice groups","eof":true}"#,
        );
        let case = refusal(false);
        let proof = proof(&case, Some(&pin), &left, None, &[]).unwrap();
        assert_eq!(proof.get("named"), Some(&Value::str("slice groups")));
        let row = |proof: &Value| {
            Value::object([(
                "parity",
                Value::object([
                    ("comparison", Value::str(COMPARISON)),
                    ("refusal_proof", proof.clone()),
                ]),
            )])
        };
        assert!(row_holds(&row(&proof), &case, Some(&pin), &[]));
        // Another fold, another detail, an unfinished run, a failed run or no pin.
        for changed in [
            r#"{"packets":2,"bytes":11,"digest":23,"unsupported":"slice groups","eof":true}"#,
            r#"{"packets":2,"bytes":11,"digest":22,"unsupported":"other","eof":true}"#,
            r#"{"packets":2,"bytes":11,"digest":22,"eof":true}"#,
            r#"{"packets":2,"bytes":11,"digest":22,"unsupported":"slice groups","eof":false}"#,
        ] {
            assert!(
                super::proof(&case, Some(&pin), &outcome(changed), None, &[]).is_none(),
                "{changed}"
            );
            let forged = Value::object([
                ("candidate", parse(changed).unwrap()),
                ("named", Value::str("slice groups")),
            ]);
            assert!(
                !row_holds(&row(&forged), &case, Some(&pin), &[]),
                "{changed}"
            );
        }
        let failed = Value::object([("status", Value::str("worker-error"))]);
        assert!(super::proof(&case, Some(&pin), &failed, None, &[]).is_none());
        assert!(super::proof(&case, None, &left, None, &[]).is_none());
        assert!(!row_holds(&row(&proof), &case, None, &[]));
        // A measured row is no refusal.
        let mut timed = row(&proof);
        super::super::validation::put(&mut timed, "timing", Value::Object(Vec::new())).unwrap();
        assert!(!row_holds(&timed, &case, Some(&pin), &[]));
    }

    #[test]
    fn a_trace_names_the_refusal_where_the_result_line_does_not() {
        let pin = parse(r#"{"packets":1,"bytes":0,"digest":280}"#).unwrap();
        let left = outcome(r#"{"packets":1,"bytes":0,"digest":280,"eof":true}"#);
        let case = refusal(true);
        let trace = "9 1\n# candidate: invalid data: short box\n";
        assert!(proof(&case, Some(&pin), &left, Some(trace), &[]).is_some());
        assert!(proof(&case, Some(&pin), &left, Some("9 1\n"), &[]).is_none());
        assert!(proof(&case, Some(&pin), &left, None, &[]).is_none());
    }
}
