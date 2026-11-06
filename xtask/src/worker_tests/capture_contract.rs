//! The capture collector's finite encoder rate search follows the approved rule.

use std::path::Path;

use super::{Context, Outcome};
use crate::json::Value;

/// The approved requested-rate factors of the encoder comparison rule.
pub const APPROVED_RATE_FACTORS: [f64; 6] = [1.25, 1.6, 2.0, 2.6, 3.4, 4.5];

/// The collector's rate-search loop, `harness/capture/compare.py`.
const COLLECTOR: &str = "harness/capture/compare.py";

/// The iterables of every `for factor in ...:` statement of a Python source,
/// up to the colon that ends the header.
fn factor_loops(source: &str) -> Vec<String> {
    let mut loops = Vec::new();
    for (at, _) in source.match_indices("for factor in") {
        let line_start = source[..at].rfind('\n').map_or(0, |i| i + 1);
        let after = &source[at + "for factor in".len()..];
        if !source[line_start..at].trim().is_empty() || !after.starts_with([' ', '\t', '(']) {
            // A comprehension, a comment or a longer name, not a loop statement.
            continue;
        }
        let mut depth = 0i32;
        let mut end = None;
        for (i, c) in after.char_indices() {
            match c {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                ':' if depth == 0 => {
                    end = Some(i);
                    break;
                }
                _ => {}
            }
        }
        loops.push(after[..end.unwrap_or(after.len())].trim().to_owned());
    }
    loops
}

/// The numbers of a tuple display, `(a, b, ...)` or `a, b, ...`; `None` for any
/// other iterable.
fn tuple_numbers(iterable: &str) -> Option<Result<Vec<f64>, String>> {
    let inner = match iterable
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
    {
        Some(inner) => inner,
        None => iterable,
    };
    let parenthesised = inner.len() != iterable.len();
    if inner.contains(['(', ')', '[', ']', '{', '}']) {
        return None;
    }
    let mut parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    let tuple = parts.len() > 1 || (parenthesised && inner.trim().is_empty());
    if !tuple {
        return None;
    }
    if parts.last() == Some(&"") {
        parts.pop();
    }
    Some(
        parts
            .iter()
            .map(|p| {
                p.replace('_', "")
                    .parse::<f64>()
                    .map_err(|_| format!("not a number: {p}"))
            })
            .collect(),
    )
}

/// The rate factors the collector searches.
fn collector_rate_factors(root: &Path) -> Result<(usize, Option<Vec<f64>>), String> {
    let path = root.join(COLLECTOR);
    let source = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let loops = factor_loops(&source);
    let factors = match loops.as_slice() {
        [only] => tuple_numbers(only).transpose()?,
        _ => None,
    };
    Ok((loops.len(), factors))
}

fn numbers(value: Option<&Value>) -> Option<Vec<f64>> {
    value?
        .as_array()?
        .iter()
        .map(|v| match v {
            Value::Number(n) => Some(n.as_f64()),
            _ => None,
        })
        .collect()
}

pub fn encoder_rate_loop_matches_the_approved_contract(cx: &mut Context<'_>) -> Outcome {
    let contract = crate::checks::read_json(&cx.root.join("harness/capture/contract.json"))?;
    let factors = numbers(
        contract
            .get("comparison_rules")
            .and_then(|r| r.get("encoder_rate_factors")),
    );
    let (count, collector) = collector_rate_factors(cx.root)?;
    cx.same(
        count,
        1,
        "expected one finite encoder rate-search loop in the collector",
    )?;
    cx.check(collector.is_some(), || {
        "the collector's rate-search loop does not iterate over a tuple".into()
    })?;
    cx.subtest("contract records the approved six points", |cx| {
        cx.same(
            factors.as_deref(),
            Some(&APPROVED_RATE_FACTORS[..]),
            "contract encoder_rate_factors",
        )
    });
    cx.subtest("collector matches contract", |cx| {
        cx.same(
            collector.as_deref(),
            factors.as_deref(),
            "collector factors",
        )
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_only_loop_statements_over_tuples() {
        let source = "x = [f for factor in y]\n    for factor in (1.25, 2):\n        pass\n# for factor in (9,):\nfor factors in (3, 4):\n";
        assert_eq!(factor_loops(source), ["(1.25, 2)"]);
        assert_eq!(tuple_numbers("(1.25, 2)").unwrap().unwrap(), [1.25, 2.0]);
        assert_eq!(tuple_numbers("1.5, 3").unwrap().unwrap(), [1.5, 3.0]);
        assert_eq!(tuple_numbers("(4.5,)").unwrap().unwrap(), [4.5]);
        assert!(tuple_numbers("(4.5)").is_none());
        assert!(tuple_numbers("range(3)").is_none());
        assert!(tuple_numbers("[1.0, 2.0]").is_none());
        assert!(tuple_numbers("(a, b)").unwrap().is_err());
    }
}
