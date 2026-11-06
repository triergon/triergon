//! The capture comparison's finite encoder rate search follows the approved rule.

use super::{Context, Outcome};
use crate::capture::compare::ENCODER_RATE_FACTORS;
use crate::json::Value;

/// The approved requested-rate factors of the encoder comparison rule.
pub const APPROVED_RATE_FACTORS: [f64; 6] = [1.25, 1.6, 2.0, 2.6, 3.4, 4.5];

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
    cx.subtest("contract records the approved six points", |cx| {
        cx.same(
            factors.as_deref(),
            Some(&APPROVED_RATE_FACTORS[..]),
            "contract encoder_rate_factors",
        )
    });
    cx.subtest("comparison matches contract", |cx| {
        cx.same(
            Some(&ENCODER_RATE_FACTORS[..]),
            factors.as_deref(),
            "cargo xtask capture compare rate factors",
        )
    });
    Ok(())
}
