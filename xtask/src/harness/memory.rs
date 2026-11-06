//! Grading recorded memory evidence: peak live heap and resident-set bounds
//! from paired observer receipts, recomputed from the raw samples.
//!
//! Only Darwin receipts (high-water-mark bound) are validated here; evidence
//! whose certificate names the Linux platform is rejected.

use super::acceptance::active_cpu;
use super::diff::equal;
use crate::json::Value;
use crate::stats;

fn int(value: Option<&Value>) -> Option<i128> {
    match value? {
        Value::Number(n) => n.as_i128(),
        _ => None,
    }
}

fn s<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn interval(values: &[(i128, i128)], seed: u64) -> Result<Value, String> {
    let pairs: Vec<(f64, f64)> = values.iter().map(|&(a, b)| (a as f64, b as f64)).collect();
    let i = stats::ratio_interval(&pairs, seed, 4000)?;
    Ok(Value::object([
        ("median", Value::float(i.median)),
        ("lower_95", Value::float(i.lower_95)),
        ("upper_95", Value::float(i.upper_95)),
    ]))
}

/// Recompute the memory verdict (`status` and the ratio fields) of raw samples.
pub fn evaluate(samples: &[Value], seed: u64, certificate: &Value) -> Result<Value, String> {
    if s(certificate, "platform") == Some("Linux") {
        return Err("Linux memory evidence is not validated by this tool".into());
    }
    let failed = |status: &str| Ok(Value::object([("status", Value::str(status))]));
    let (mut rss, mut heaps, mut totals) = (Vec::new(), Vec::new(), Vec::new());
    for row in samples {
        let Value::Object(entries) = row else {
            return Err("memory sample is not an object".into());
        };
        if entries
            .iter()
            .any(|(_, v)| s(v, "status") != Some("measured"))
        {
            return failed("measurement-failed");
        }
        let get = |key: &str| row.get(key).ok_or_else(|| format!("'{key}'"));
        let expected = get("reference_heap")?.get("work").ok_or("'work'")?;
        if entries
            .iter()
            .any(|(_, v)| !v.get("work").is_some_and(|w| equal(w, expected)))
        {
            return failed("unequal-memory-work");
        }
        let observer = |key: &str| {
            get(key).and_then(|v| v.get("observer").ok_or_else(|| "'observer'".to_owned()))
        };
        let (a, b) = (observer("candidate_rss")?, observer("reference_rss")?);
        let field = |o: &Value, k: &str| int(o.get(k)).ok_or_else(|| format!("'{k}'"));
        let candidate_peak = field(a, "hwm_end")?.max(field(a, "rss_peak_observed")?);
        let upper = candidate_peak - field(a, "rss_start")? + 2 * field(a, "page_size")?;
        let lower =
            field(b, "rss_peak_observed")? - field(b, "rss_start")? - 2 * field(b, "page_size")?;
        if lower <= 0 || upper <= 0 {
            return failed("rss-attribution-unresolved");
        }
        rss.push((upper, lower));
        heaps.push((
            field(observer("candidate_heap")?, "heap_peak_bytes")?,
            field(observer("reference_heap")?, "heap_peak_bytes")?,
        ));
        totals.push((
            candidate_peak,
            field(b, "hwm_end")?.max(field(b, "rss_peak_observed")?),
        ));
    }
    let (Ok(rss), Ok(heap), Ok(total)) = (
        interval(&rss, seed),
        interval(&heaps, seed),
        interval(&totals, seed),
    ) else {
        return failed("invalid-memory-observation");
    };
    Ok(Value::object([
        ("status", Value::str("qualified")),
        ("rss_ratio_upper_95", rss.get("upper_95").cloned().unwrap()),
        (
            "heap_ratio_upper_95",
            heap.get("upper_95").cloned().unwrap(),
        ),
        ("rss_bound_ratio", rss),
        ("heap_ratio", heap),
        ("total_rss_ratio", total),
    ]))
}

/// Whether a row's recorded memory evidence is complete, bound to the selected
/// workers and instrumentation, and reproduces its recorded ratios.
pub fn evidence_valid(
    row: &Value,
    memory: &Value,
    contract: &Value,
    identity: &Value,
    selected: &Value,
    require_linux: bool,
) -> bool {
    let check = || -> Option<bool> {
        let certificate = memory.get("certificate")?;
        let raw = memory.get("samples")?.as_array()?;
        let probe = certificate.get("probe_heap")?;
        let mut covered = s(certificate, "status")? == "qualified-selected-workers"
            && !certificate.get("errors")?.truthy()
            && equal(certificate.get("worker_hashes")?, selected);
        if int(identity.get("schema")).unwrap_or(1) >= 2 {
            covered &= !memory.get("contaminants").is_some_and(Value::truthy)
                && active_cpu(
                    memory.get("cpu_before")?,
                    memory.get("cpu_after")?,
                    require_linux,
                )
                .is_empty();
        }
        if let Some(input) = row.get("input_sha256").filter(|v| v.truthy()) {
            covered &= memory.get("input_sha256") == Some(input);
        }
        if let Some(instrumentation) = identity.get("instrumentation").filter(|v| v.truthy()) {
            for key in ["probe_sha256", "observer_sha256"] {
                covered &= certificate.get(key) == instrumentation.get(key);
            }
        }
        let observer = probe.get("observer")?;
        covered &= s(probe, "status")? == "measured"
            && equal(
                observer.get("heap_peak_bytes")?,
                probe.get("work")?.get("expected_peak_bytes")?,
            )
            && equal(
                observer.get("heap_live_start")?,
                observer.get("heap_live_end")?,
            );
        let pairs = int(memory.get("pairs"))?;
        let minimum = int(contract.get("minimum_pairs"))?;
        if !(covered && raw.len() as i128 == pairs && raw.len() as i128 >= minimum) {
            return Some(false);
        }
        for sample in raw {
            let Value::Object(entries) = sample else {
                return None;
            };
            for (_, v) in entries {
                covered &= v.get("instrument_sha256")? == certificate.get("observer_sha256")?
                    && s(v.get("observer")?, "status")? == "measured";
            }
        }
        let seed = int(memory.get("seed")).unwrap_or(280) as u64;
        let recomputed = evaluate(raw, seed, certificate).ok()?;
        let same = [
            "status",
            "rss_ratio_upper_95",
            "heap_ratio_upper_95",
            "rss_bound_ratio",
            "heap_ratio",
            "total_rss_ratio",
        ]
        .iter()
        .all(|k| match (recomputed.get(k), memory.get(k)) {
            (Some(a), Some(b)) => equal(a, b),
            (None, None) => true,
            _ => false,
        });
        Some(covered && same)
    };
    check().unwrap_or(false)
}
