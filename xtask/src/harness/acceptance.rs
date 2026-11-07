//! Fail-closed grading of a collected report against its feature contract.
//!
//! Every recorded number is recomputed from its raw samples before it counts:
//! timing intervals from the paired elapsed times, memory ratios from the raw
//! observer receipts. Missing, contaminated or unequal evidence fails the case.

use super::comparison::validate_case_evidence;
use super::diff::equal;
use super::gate_build;
use crate::json::Value;
use crate::stats;

pub const LINUX_CPU_KEY: &str = "__linux_cpu__";

fn s<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn int(value: Option<&Value>) -> Option<i128> {
    match value? {
        Value::Number(n) => n.as_i128(),
        _ => None,
    }
}

fn float(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64)
}

fn members(value: &Value) -> &[(String, Value)] {
    match value {
        Value::Object(m) => m,
        _ => &[],
    }
}

fn linux_cpu_findings(before: &Value, after: &Value, required: bool) -> Vec<String> {
    let (first, last) = (before.get(LINUX_CPU_KEY), after.get(LINUX_CPU_KEY));
    let (Some(first), Some(last)) = (first, last) else {
        return if required || first.is_some() || last.is_some() {
            vec!["linux-cpu-evidence-missing".into()]
        } else {
            vec![]
        };
    };
    if !matches!(first, Value::Object(_))
        || !matches!(last, Value::Object(_))
        || [first, last]
            .iter()
            .any(|r| s(r, "status") != Some("recorded"))
    {
        return vec!["linux-cpu-evidence-unavailable".into()];
    }
    let counters = |row: &Value| -> Option<Vec<i128>> {
        row.get("counters")?
            .as_array()?
            .iter()
            .map(|v| int(Some(v)).filter(|n| *n >= 0))
            .collect()
    };
    let valid = |row: &Value| {
        row.get("boot_id").is_some_and(Value::truthy)
            && row.get("cpus").is_some_and(Value::truthy)
            && int(row.get("ticks_per_second")).is_some_and(|t| t > 0)
            && counters(row).is_some_and(|c| c.len() >= 8)
    };
    if !valid(first) || !valid(last) {
        return vec!["linux-cpu-evidence-invalid".into()];
    }
    let (a, b) = (counters(first).unwrap(), counters(last).unwrap());
    let changed = ["boot_id", "cpus", "ticks_per_second"]
        .iter()
        .any(|k| !equal(first.get(k).unwrap(), last.get(k).unwrap()));
    if changed || a.len() != b.len() || a.iter().zip(&b).any(|(x, y)| x > y) {
        return vec!["linux-cpu-evidence-invalid".into()];
    }
    if a[7] != b[7] {
        vec!["linux-cpu-steal".into()]
    } else {
        vec![]
    }
}

/// Builds or other tracked processes that used CPU between two snapshots,
/// plus any Linux accounting finding (steal time, missing evidence).
pub fn active_cpu(before: &Value, after: &Value, require_linux: bool) -> Vec<String> {
    let mut names = linux_cpu_findings(before, after, require_linux);
    let processes = |snapshot: &Value| -> Vec<(String, Value)> {
        members(snapshot)
            .iter()
            .filter(|(k, _)| k != LINUX_CPU_KEY)
            .cloned()
            .collect()
    };
    let (first, last) = (processes(before), processes(after));
    let find = |list: &[(String, Value)], pid: &str| {
        list.iter().find(|(k, _)| k == pid).map(|(_, v)| v.clone())
    };
    for (pid, item) in &first {
        if find(&last, pid).is_none() {
            names.push(s(item, "name").unwrap_or_default().to_owned());
        }
    }
    for (pid, item) in &last {
        let old = find(&first, pid);
        let running = |v: &Value| s(v, "state").is_some_and(|state| state.contains('R'));
        let busy = match &old {
            None => true,
            Some(old) => {
                !item
                    .get("cpu_ticks")
                    .zip(old.get("cpu_ticks"))
                    .is_some_and(|(a, b)| equal(a, b))
                    || item.get("start_ticks") != old.get("start_ticks")
                    || running(item)
                    || running(old)
            }
        };
        if busy {
            names.push(s(item, "name").unwrap_or_default().to_owned());
        }
    }
    names.sort();
    names.dedup();
    names
}

fn sample_work_ok(sample: &Value, iterations: i128) -> Result<(f64, f64), ()> {
    let a = sample
        .get("candidate")
        .and_then(|c| c.get("data"))
        .ok_or(())?;
    let b = sample
        .get("reference")
        .and_then(|c| c.get("data"))
        .ok_or(())?;
    let keys = ["iterations", "packets", "bytes", "digest", "elapsed_ns"];
    for work in [a, b] {
        for key in keys {
            if int(work.get(key)).is_none_or(|v| v < 0)
                || matches!(work.get(key), Some(Value::Bool(_)))
            {
                return Err(());
            }
        }
    }
    if int(a.get("iterations")) != Some(iterations) || int(b.get("iterations")) != Some(iterations)
    {
        return Err(());
    }
    let statuses_ok = ["candidate", "reference"]
        .iter()
        .all(|k| sample.get(k).and_then(|v| s(v, "status")) == Some("ok"));
    if !statuses_ok
        || ["iterations", "packets", "bytes", "digest"]
            .iter()
            .any(|k| int(a.get(k)) != int(b.get(k)))
    {
        return Err(());
    }
    Ok((
        int(a.get("elapsed_ns")).unwrap() as f64,
        int(b.get("elapsed_ns")).unwrap() as f64,
    ))
}

fn interval_value(interval: &stats::Interval) -> Value {
    Value::object([
        ("median", Value::float(interval.median)),
        ("lower_95", Value::float(interval.lower_95)),
        ("upper_95", Value::float(interval.upper_95)),
    ])
}

/// Check a timing row's samples, worker bindings, isolation and recorded interval.
pub fn timing_valid(row: &Value, contract: &Value, selected: &Value, require_linux: bool) -> bool {
    let timing = row.get("timing").cloned().unwrap_or(Value::Object(vec![]));
    let raw = timing
        .get("samples")
        .and_then(Value::as_array)
        .unwrap_or_default();
    let (Some(pairs), Some(iterations)) = (int(timing.get("pairs")), int(timing.get("iterations")))
    else {
        return false;
    };
    let minimum = int(contract.get("minimum_pairs")).unwrap_or(i128::MAX);
    let mut valid = raw.len() as i128 == pairs
        && raw.len() as i128 >= minimum
        && timing
            .get("iterations")
            .is_some_and(|count| super::validation::iterations_valid(contract, count));
    valid &= timing
        .get("worker_hashes")
        .is_some_and(|h| equal(h, selected));
    if let Some(input) = row.get("input_sha256").filter(|v| v.truthy()) {
        valid &= timing.get("input_sha256") == Some(input);
    }
    if !valid {
        return false;
    }
    let mut paired = Vec::new();
    for (index, sample) in raw.iter().enumerate() {
        if int(sample.get("pair")) != Some(index as i128)
            || matches!(sample.get("pair"), Some(Value::Bool(_)))
        {
            return false;
        }
        let (Some(before), Some(after)) = (sample.get("cpu_before"), sample.get("cpu_after"))
        else {
            return false;
        };
        if !active_cpu(before, after, require_linux).is_empty() {
            return false;
        }
        match sample_work_ok(sample, iterations) {
            Ok(pair) => paired.push(pair),
            Err(()) => return false,
        }
    }
    let seed = int(timing.get("seed")).unwrap_or(0);
    match stats::ratio_interval(&paired, seed as u64, 4000) {
        Ok(interval) => timing
            .get("ratio")
            .is_some_and(|r| equal(r, &interval_value(&interval))),
        Err(_) => false,
    }
}

/// The verdict: `accepted` needs the whole module gate, `slice_accepted` the
/// selected cases of this contract.
pub fn acceptance(
    root: &std::path::Path,
    contract: &Value,
    rows: &[Value],
    identity: &Value,
    target: &str,
    coverage: &[Value],
    additional: &[Value],
) -> Value {
    let mut reasons: Vec<String> = Vec::new();
    let ids: Vec<&str> = rows.iter().filter_map(|r| s(r, "id")).collect();
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    if unique.len() != rows.len() {
        reasons.push("duplicate case identities".into());
    }
    let parity_field =
        |row: &Value, key: &str| row.get("parity").and_then(|p| s(p, key)).map(str::to_owned);
    if rows.is_empty()
        || rows.iter().any(|r| {
            parity_field(r, "status").as_deref() != Some("pass")
                || parity_field(r, "source_timing_status").as_deref() != Some("pass")
        })
    {
        reasons.push("selected-feature parity incomplete or failing".into());
    }
    let passed: Vec<String> = rows
        .iter()
        .filter(|r| parity_field(r, "status").as_deref() == Some("pass"))
        .filter_map(|r| s(r, "profile").map(str::to_owned))
        .collect();
    let mut missing: Vec<String> = contract
        .get("required_profiles")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .filter(|p| !passed.iter().any(|q| q == p))
        .map(str::to_owned)
        .collect();
    missing.sort();
    missing.dedup();
    if !missing.is_empty() {
        reasons.push(format!("missing required profiles: {}", missing.join(", ")));
    }
    let contract_id = s(contract, "id");
    let require_linux = identity.get("host_platform").and_then(|h| s(h, "system")) == Some("Linux")
        || target.contains("linux");
    for row in rows {
        let id = s(row, "id").unwrap_or_default();
        let selected = match validate_case_evidence(row, identity, contract_id) {
            Ok(selected) => selected,
            Err(error) => {
                reasons.push(format!("{id}: {error}"));
                Value::Object(vec![])
            }
        };
        let proof = row.get("parity").cloned().unwrap_or(Value::Object(vec![]));
        let three_passes = Value::Array(
            [1, 7, 4096]
                .map(|n| Value::object([("chunk", Value::int(n)), ("status", Value::str("pass"))]))
                .to_vec(),
        );
        let view = row.get("operation").and_then(|o| s(o, "view"));
        if contract_id == Some("matroska-zlib")
            && s(&proof, "source_content_status") != Some("pass")
        {
            reasons.push(format!(
                "{id}: complete source content gate unproven or failed"
            ));
        }
        if contract_id == Some("matroska-colour") {
            if s(&proof, "source_colour_status") != Some("pass")
                || s(&proof, "colour_reference_status") != Some("pass")
                || !proof
                    .get("colour_short_reads")
                    .is_some_and(|v| equal(v, &three_passes))
            {
                reasons.push(format!(
                    "{id}: complete source/native colour gate unproven or failed"
                ));
            }
            if view != Some("colour-source-and-ffmpeg9-v1") {
                reasons.push(format!("{id}: colour benchmark operation view unproven"));
            }
        }
        if contract_id == Some("matroska-segment-metadata") {
            if s(&proof, "source_catalogue_status") != Some("pass")
                || s(&proof, "source_colour_status") != Some("pass")
                || !proof
                    .get("short_reads")
                    .is_some_and(|v| equal(v, &three_passes))
            {
                reasons.push(format!(
                    "{id}: complete source/native catalogue gate unproven or failed"
                ));
            }
            if view != Some("catalogue-source-and-native-v1") {
                reasons.push(format!("{id}: catalogue timing view unproven"));
            }
        }
        let timing = row.get("timing").cloned().unwrap_or(Value::Object(vec![]));
        let upper =
            float(timing.get("ratio").and_then(|r| r.get("upper_95"))).unwrap_or(f64::INFINITY);
        let ceiling = float(contract.get("runtime_ratio_max")).unwrap_or(0.0);
        let timing_ok = s(&timing, "status") == Some("measured")
            && timing_valid(row, contract, &selected, require_linux)
            && !timing.get("contaminants").is_some_and(Value::truthy)
            && upper.is_finite()
            && upper > 0.0
            && upper <= ceiling;
        if !timing_ok {
            reasons.push(format!("{id}: runtime gate unproven or failed"));
        }
        let memory = row.get("memory").cloned().unwrap_or(Value::Object(vec![]));
        // Graded as harness/policy.json states; RSS growth is reported only.
        let ratios = ["total_rss_ratio_upper_95", "heap_ratio_upper_95"]
            .map(|k| float(memory.get(k)).unwrap_or(f64::INFINITY));
        let memory_ok = super::memory::evidence_valid(
            root,
            row,
            &memory,
            contract,
            identity,
            &selected,
            require_linux,
        ) && s(&memory, "status") == Some("qualified")
            && ratios.iter().all(|r| r.is_finite() && *r > 0.0 && *r < 1.0);
        if !memory_ok {
            reasons.push(format!(
                "{id}: lower RSS/live allocation gate unproven or failed"
            ));
        }
    }
    // Required intact external vectors pass the same gates as authored cases.
    for spec in contract
        .get("required_additional_vectors")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        let file = s(spec, "file").unwrap_or_default();
        let matches: Vec<&Value> = additional
            .iter()
            .filter(|c| s(c, "id") == Some(file) && s(c, "profile") == Some(file))
            .collect();
        if matches.len() != 1 || matches[0].get("input_sha256") != spec.get("sha256") {
            reasons.push(format!(
                "required additional vector missing or changed: {file}"
            ));
            continue;
        }
        let Ok(derived) = super::validation::external_contract(contract, file) else {
            reasons.push(format!("invalid additional vector contract: {file}"));
            continue;
        };
        let extra = acceptance(
            root,
            &derived,
            &[matches[0].clone()],
            identity,
            target,
            coverage,
            &[],
        );
        for reason in extra
            .get("slice_reasons")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            reasons.push(format!(
                "additional {file}: {}",
                reason.as_str().unwrap_or_default()
            ));
        }
    }
    if identity
        .get("source_revision")
        .is_none_or(|v| *v == Value::Null)
    {
        reasons.push("FFmpeg source build/configuration is not qualified".into());
    }
    match gate_build::pinned_release(root) {
        Ok(pinned) => reasons.extend(
            gate_build::refusals(identity, &pinned)
                .into_iter()
                .map(|reason| format!("not the gate build: {reason}")),
        ),
        Err(error) => reasons.push(format!("gate build unknown: {error}")),
    }
    let targets: Vec<&str> = contract
        .get("required_targets")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let mut unique_targets = targets.clone();
    unique_targets.sort_unstable();
    unique_targets.dedup();
    if unique_targets != [target] {
        reasons.push("required cross-target evidence missing".into());
    }
    let slice_reasons = reasons.clone();
    if coverage.is_empty() || coverage.iter().any(|c| s(c, "status") != Some("pass")) {
        reasons.push("FATE mapping/asset/expected-output coverage pending".into());
    }
    if contract
        .get("pending_module_requirements")
        .is_some_and(Value::truthy)
    {
        reasons.push("broader module requirements remain pending".into());
    }
    Value::object([
        ("accepted", Value::Bool(reasons.is_empty())),
        ("slice_accepted", Value::Bool(slice_reasons.is_empty())),
        ("slice_reasons", Value::strings(slice_reasons)),
        ("reasons", Value::strings(reasons)),
        ("missing_profiles", Value::strings(missing)),
    ])
}
