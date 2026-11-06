//! Capture diagnostics as a Markdown table.

use std::path::{Path, PathBuf};

use crate::json::Value;
use crate::python;

const HEADER: &str = "| Case | Conversion | Mux | Encoder (mode, bits vs FFmpeg, PSNR-Y ours/FFmpeg) | Runtime ratio median / upper95 | vs --enable-small build (median) | Peak heap upper95 or diagnostic MB | RSS growth upper95 (diagnostic) | Total resident peak upper95 | Memory qualification |";

fn get<'a>(value: &'a Value, key: &str) -> Result<&'a Value, String> {
    value
        .get(key)
        .ok_or_else(|| format!("result without {key}"))
}

fn float(value: &Value) -> Result<f64, String> {
    match value {
        Value::Bool(b) => Ok(f64::from(u8::from(*b))),
        other => other
            .as_f64()
            .ok_or_else(|| format!("not a number: {}", python::repr(other))),
    }
}

/// `format(value, ".Nf")`.
fn fixed(value: f64, digits: usize) -> String {
    if value.is_nan() {
        "nan".into()
    } else {
        format!("{value:.digits$}")
    }
}

fn optional<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.get(key).unwrap_or(&Value::Null)
}

fn numbers(value: &Value) -> Result<Vec<f64>, String> {
    value
        .as_array()
        .ok_or("expected a list")?
        .iter()
        .map(float)
        .collect()
}

fn row(r: &Value) -> Result<String, String> {
    let c = get(r, "correctness_summary")?;
    let rt = get(r, "runtime")?;
    let m = get(r, "memory")?;
    let heap = m.get("heap_peak_bytes").filter(|h| h.truthy());
    let heap_cell = match (m.get("heap_ratio_upper_95"), heap) {
        (Some(ratio), _) => format!("{}×", fixed(float(ratio)?, 3)),
        (None, Some(heap)) => {
            let candidate = numbers(get(heap, "candidate")?)?;
            let reference = numbers(get(heap, "reference")?)?;
            format!(
                "{} / {}",
                fixed(python::max(&candidate) / 1e6, 2),
                fixed(
                    reference.iter().copied().fold(f64::INFINITY, f64::min) / 1e6,
                    2
                )
            )
        }
        (None, None) => "n/a".into(),
    };
    let growth = m
        .get("rss_growth_ratio")
        .or_else(|| m.get("rss_bound_ratio"));
    let growth = growth.and_then(|ratio| ratio.get("upper_95"));
    let total = m.get("total_rss_ratio_upper_95");
    let upper = |value: Option<&Value>| {
        value.map_or_else(|| Ok("n/a".into()), |v| float(v).map(|v| fixed(v, 3)))
    };
    let small = r
        .get("runtime_vs_small_config")
        .and_then(|s| s.get("ratio"))
        .filter(|x| x.truthy());
    let truthy = |value: &Value, key: &str| value.get(key).is_some_and(Value::truthy);
    let pass = |value: &Value, key: &str| if truthy(value, key) { "pass" } else { "FAIL" };
    let exact = match c.get("conversion_exact") {
        Some(x) => float(x)?,
        None => 0.0,
    };
    let interpolated = c.get("interpolated_psnr_y").filter(|x| x.truthy());
    let psnr = get(c, "psnr_y")?.as_array().ok_or("psnr_y is not a list")?;
    let pc = match interpolated {
        Some(x) => float(x)?,
        None => float(psnr.first().ok_or("psnr_y is empty")?)?,
    };
    let pr = float(psnr.get(1).ok_or("psnr_y has one value")?)?;
    let ratio = get(rt, "ratio")?;
    let verdict = get(r, "verdict")?;
    let memory = if truthy(verdict, "heap_gate") && truthy(verdict, "rss_gate") {
        "pass"
    } else {
        match optional(verdict, "memory_status").as_str() {
            Some("qualified") => "FAIL",
            Some(status) => status,
            None => "unresolved",
        }
    };
    Ok(format!(
        "| {} | {} (±{}, {}% exact) | {} | {} ({}, {}×, {}/{} dB) | {} / **{}**{} | {} | {} | {} | {} | {} |",
        python::str(get(get(r, "case")?, "id")?),
        pass(c, "conversion"),
        python::str(optional(c, "conversion_max_abs")),
        fixed(100.0 * exact, 2),
        pass(c, "mux"),
        pass(c, "encoder"),
        python::str(optional(c, "encoder_mode")),
        python::str(optional(c, "bits_ratio")),
        fixed(pc, 2),
        fixed(pr, 2),
        fixed(float(get(ratio, "median")?)?, 3),
        fixed(float(get(ratio, "upper_95")?)?, 3),
        if truthy(rt, "target_pass") {
            " ✓0.80"
        } else {
            ""
        },
        match small {
            Some(s) => fixed(float(get(s, "median")?)?, 3),
            None => "–".into(),
        },
        heap_cell,
        upper(growth)?,
        upper(total)?,
        memory,
    ))
}

/// `capture summarize RUN`: print the Markdown table of every `*/result.json`.
pub fn run(run: &Path) -> Result<(), String> {
    let mut results: Vec<PathBuf> = std::fs::read_dir(run)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path().join("result.json"))
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default();
    results.sort();
    println!("{HEADER}");
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for path in results {
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let result = python::loads(&text).ok_or_else(|| format!("{}: not JSON", path.display()))?;
        println!("{}", row(&result)?);
    }
    Ok(())
}
