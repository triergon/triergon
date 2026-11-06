//! Header stripping keeps the source timing.

use super::{Context, Outcome, Scratch, expected, items};
use crate::corpus;
use crate::harness::oracle::Kind;
use crate::json::Value;

pub fn header_encoding_does_not_rewrite_source_timing(cx: &mut Context<'_>) -> Outcome {
    let data = corpus::header_removal_case(&corpus::Source::load(cx.root)?, 1, true, false)?;
    let scratch = Scratch::new()?;
    let path = scratch.file("header.mkv", &data)?;
    let trace = expected(cx.root, Kind::Timing, &path)?;
    let numerators = items(&trace, "packets")?
        .iter()
        .map(|row| {
            row.get("pts")
                .and_then(|pts| pts.get("numerator"))
                .cloned()
                .unwrap_or(Value::Null)
        })
        .collect();
    cx.equal(
        &Value::Array(numerators),
        &Value::Array(vec![Value::int(0), Value::int(1_000_000)]),
        "source presentation times",
    )
}
