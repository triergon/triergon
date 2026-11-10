//! Fail-closed grading of a collected report against its feature contract.
//!
//! Every recorded number is recomputed from its raw samples before it counts:
//! timing intervals from the paired elapsed times, memory ratios from the raw
//! observer data. Missing, contaminated or unequal evidence fails the case.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::comparison::validate_case_evidence;
use super::diff::equal;
use super::gate_build;
use super::policy::Runtime;
use crate::json::Value;

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

/// Compare complete output objects without coercing integer and float fields.
pub(crate) fn output_matches(actual: &Value, expected: &Value, timed: bool) -> bool {
    let (Value::Object(actual), Value::Object(expected)) = (actual, expected) else {
        return false;
    };
    actual.len() == expected.len()
        && actual
            .iter()
            .map(|(key, _)| key)
            .collect::<BTreeSet<_>>()
            .len()
            == actual.len()
        && expected
            .iter()
            .map(|(key, _)| key)
            .collect::<BTreeSet<_>>()
            .len()
            == expected.len()
        && expected.iter().all(|(key, target)| {
            let Some((_, value)) = actual.iter().find(|(name, _)| name == key) else {
                return false;
            };
            if timed && key == "elapsed_ns" {
                return int(Some(target)).is_some_and(|n| n >= 0)
                    && int(Some(value)).is_some_and(|n| n > 0);
            }
            value == target
        })
}

fn output_roles(expected: &Value) -> bool {
    matches!(expected, Value::Object(fields) if fields.len() == 2)
        && ["candidate", "reference"]
            .iter()
            .all(|role| matches!(expected.get(role), Some(Value::Object(_))))
}

/// Check exact role-specific outputs in both warm-ups and every timing pair.
pub(crate) fn timing_outputs_match(raw: &Value, expected: &Value) -> bool {
    let check = || -> Option<bool> {
        if !output_roles(expected) {
            return Some(false);
        }
        let warmups = raw.get("warmups")?.as_array()?;
        let samples = raw.get("samples")?.as_array()?;
        if warmups.len() != 2 || samples.is_empty() {
            return Some(false);
        }
        for (index, role) in ["candidate", "reference"].into_iter().enumerate() {
            let output = expected.get(role)?;
            let warmup = &warmups[index];
            let result = warmup.get("result")?;
            if s(warmup, "worker") != Some(role)
                || !super::worker::ok(result)
                || !output_matches(super::worker::data(result), output, true)
            {
                return Some(false);
            }
            for sample in samples {
                let result = sample.get(role)?;
                if !super::worker::ok(result)
                    || !output_matches(super::worker::data(result), output, true)
                {
                    return Some(false);
                }
            }
        }
        Some(true)
    };
    check().unwrap_or(false)
}

/// Check exact key sets and field types for all four memory observer roles.
pub(crate) fn memory_outputs_match(raw: &Value, expected: &Value) -> bool {
    output_roles(expected)
        && raw
            .get("samples")
            .and_then(Value::as_array)
            .is_some_and(|samples| {
                !samples.is_empty()
                    && samples.iter().all(|sample| {
                        matches!(sample, Value::Object(entries) if entries.len() == 4)
                            && ["candidate", "reference"].iter().all(|role| {
                                ["heap", "rss"].iter().all(|mode| {
                                    sample
                                        .get(&format!("{role}_{mode}"))
                                        .and_then(|value| value.get("work"))
                                        .zip(expected.get(role))
                                        .is_some_and(|(actual, expected)| {
                                            output_matches(actual, expected, false)
                                        })
                                })
                            })
                    })
            })
}

/// Check a timing row against its contract: the declared iteration count, any
/// pinned outputs, and the record itself as [`super::timing::validate`] grades it.
pub fn timing_valid(row: &Value, contract: &Value, selected: &Value, require_linux: bool) -> bool {
    let timing = row.get("timing").unwrap_or(&Value::Null);
    let Ok(fields) = super::comparison::work_fields(contract) else {
        return false;
    };
    timing
        .get("iterations")
        .is_some_and(|count| super::validation::iterations_valid(contract, count))
        && row
            .get("expected_time_outputs")
            .is_none_or(|expected| timing_outputs_match(timing, expected))
        && super::timing::validate(
            timing,
            &super::timing::Expected {
                worker_hashes: selected,
                input_sha256: row.get("input_sha256").filter(|v| v.truthy()),
                minimum_pairs: int(contract.get("minimum_pairs")).unwrap_or(i128::MAX),
                work_fields: &fields,
                own_work: &[],
                require_linux,
                module_absence: contract.get("case_operations") == Some(&Value::Bool(true)),
            },
        )
        .is_ok()
}

/// Shared per-case results. Only this module constructs a verdict from evidence.
#[derive(Debug)]
pub(crate) struct CaseVerdict {
    pub(crate) timing_pass: bool,
    pub(crate) memory_pass: bool,
    pub(crate) reasons: Vec<String>,
    _evidence_checked: bool,
}

/// Grade one successful case using the current policy and complete raw evidence.
pub(crate) fn grade_case(
    root: &Path,
    contract: &Value,
    row: &Value,
    identity: &Value,
    target: &str,
) -> CaseVerdict {
    match Runtime::load(root) {
        Ok(runtime) => {
            grade_case_with_runtime(root, contract, row, identity, target, Some(runtime))
        }
        Err(error) => {
            let mut verdict = grade_case_with_runtime(root, contract, row, identity, target, None);
            verdict.reasons.insert(0, error);
            verdict
        }
    }
}

fn grade_case_with_runtime(
    root: &Path,
    contract: &Value,
    row: &Value,
    identity: &Value,
    target: &str,
    runtime: Option<Runtime>,
) -> CaseVerdict {
    let mut reasons = Vec::new();
    let contract_id = s(contract, "id");
    let require_linux = identity.get("host_platform").and_then(|h| s(h, "system")) == Some("Linux")
        || target.contains("linux");
    if !parity_passes(row) {
        reasons.push("selected-feature parity incomplete or failing".into());
    }
    if s(row, "outcome") == Some("refusal")
        || row.get("parity").and_then(|proof| s(proof, "comparison")) == Some("refusal")
    {
        reasons.push("refusal must be checked against the registered contract".into());
    }
    let id = s(row, "id").unwrap_or_default();
    let selected = match validate_case_evidence(row, identity, contract_id) {
        Ok(selected) => selected,
        Err(error) => {
            reasons.push(format!("{id}: {error}"));
            Value::Object(vec![])
        }
    };
    let proof = row.get("parity").unwrap_or(&Value::Null);
    match super::comparison::work_fields(contract) {
        Ok(fields) if fields.len() > super::comparison::BASE_WORK_FIELDS.len() => {
            if !super::comparison::parity_work_valid(proof, &fields) {
                reasons.push(format!("{id}: declared work parity incomplete or failing"));
            }
        }
        Err(error) => reasons.push(format!("{id}: {error}")),
        _ => (),
    }
    let three_passes = Value::Array(
        [1, 7, 4096]
            .map(|n| Value::object([("chunk", Value::int(n)), ("status", Value::str("pass"))]))
            .to_vec(),
    );
    let view = row.get("operation").and_then(|o| s(o, "view"));
    if contract_id == Some("matroska-zlib") && s(proof, "source_content_status") != Some("pass") {
        reasons.push(format!(
            "{id}: complete source content gate unproven or failed"
        ));
    }
    if contract_id == Some("matroska-colour") {
        if s(proof, "source_colour_status") != Some("pass")
            || s(proof, "colour_reference_status") != Some("pass")
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
        if s(proof, "source_catalogue_status") != Some("pass")
            || s(proof, "source_colour_status") != Some("pass")
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
    let timing = row.get("timing").unwrap_or(&Value::Null);
    let upper = float(timing.get("ratio").and_then(|r| r.get("upper_95"))).unwrap_or(f64::INFINITY);
    let prerequisites_pass = reasons.is_empty();
    let timing_ok = s(timing, "status") == Some("measured")
        && timing_valid(row, contract, &selected, require_linux)
        && !timing.get("contaminants").is_some_and(Value::truthy)
        && upper.is_finite()
        && upper > 0.0
        && runtime.is_some_and(|gate| upper <= gate.ceiling);
    if !timing_ok {
        reasons.push(format!("{id}: runtime gate unproven or failed"));
    }
    let memory = row.get("memory").unwrap_or(&Value::Null);
    // Graded as harness/policy.json states; RSS growth is reported only.
    let ratios = ["total_rss_ratio_upper_95", "heap_ratio_upper_95"]
        .map(|k| float(memory.get(k)).unwrap_or(f64::INFINITY));
    let memory_ok = super::memory::evidence_valid(
        root,
        row,
        memory,
        contract,
        identity,
        &selected,
        require_linux,
    ) && row
        .get("expected_memory_outputs")
        .is_none_or(|expected| memory_outputs_match(memory, expected))
        && s(memory, "status") == Some("qualified")
        && ratios.iter().all(|r| r.is_finite() && *r < 1.0)
        && ratios[0] > 0.0
        && ratios[1] >= 0.0;
    if !memory_ok {
        reasons.push(format!(
            "{id}: lower RSS/live allocation gate unproven or failed"
        ));
    }
    CaseVerdict {
        timing_pass: prerequisites_pass && timing_ok,
        memory_pass: prerequisites_pass && memory_ok,
        reasons,
        _evidence_checked: true,
    }
}

pub(super) fn parity_passes(row: &Value) -> bool {
    row.get("parity").is_some_and(|proof| {
        s(proof, "status") == Some("pass") && s(proof, "source_timing_status") == Some("pass")
    })
}

struct Refusal<'a> {
    profile: &'a str,
    sha256: &'a str,
}

fn registered_refusals(contract: &Value) -> Result<BTreeMap<&str, Refusal<'_>>, String> {
    let mut registered = BTreeMap::new();
    let Some(specs) = contract.get("required_refusals") else {
        return Ok(registered);
    };
    let specs = specs
        .as_array()
        .ok_or("required_refusals must be an array")?;
    let mut profiles = BTreeSet::new();
    for spec in specs {
        if !matches!(spec, Value::Object(fields) if fields.len() == 3) {
            return Err("required refusal must contain only id, profile and sha256".into());
        }
        let fields = || -> Option<(&str, &str, &str)> {
            Some((s(spec, "id")?, s(spec, "profile")?, s(spec, "sha256")?))
        };
        let (id, profile, sha256) = fields().ok_or("invalid required refusal identity")?;
        if id.is_empty()
            || profile.is_empty()
            || sha256.len() != 64
            || !sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || !profiles.insert(profile)
            || contract
                .get("required_profiles")
                .and_then(Value::as_array)
                .is_none_or(|required| !required.iter().any(|p| p.as_str() == Some(profile)))
            || registered.insert(id, Refusal { profile, sha256 }).is_some()
        {
            return Err("invalid or duplicate required refusal identity".into());
        }
    }
    Ok(registered)
}

fn refusal_valid(row: &Value, registered: &Refusal<'_>, contract: &Value) -> bool {
    let check = || -> Option<bool> {
        let spec = contract.get("refusal")?;
        let code = int(spec.get("reference_exit_code"))?;
        let error = int(spec.get("reference_error_code"))?;
        let prefixes = spec.get("candidate_stderr_prefixes")?.as_array()?;
        let candidate = row.get("candidate_trace")?;
        let reference = row.get("reference_trace")?;
        let empty_sha256 = crate::sha256::hex(b"");
        Some(
            code == 1
                && s(row, "profile") == Some(registered.profile)
                && s(row, "input_sha256") == Some(registered.sha256)
                && s(row, "outcome") == Some("refusal")
                && parity_passes(row)
                && row.get("parity").and_then(|proof| s(proof, "comparison")) == Some("refusal")
                && ["timing", "memory"]
                    .iter()
                    .all(|kind| row.get(kind).is_none_or(|v| *v == Value::Null))
                && [candidate, reference].iter().all(|trace| {
                    s(trace, "status") == Some("finished")
                        && int(trace.get("exit_code")) == Some(code)
                        && s(trace, "stdout_sha256") == Some(empty_sha256.as_str())
                        && s(trace, "stderr").is_some_and(|stderr| {
                            trace.get("stderr_sha256").is_none_or(|digest| {
                                digest.as_str()
                                    == Some(crate::sha256::hex(stderr.as_bytes()).as_str())
                            })
                        })
                })
                && s(reference, "stderr")?.contains(&format!("({error})"))
                && prefixes.iter().any(|prefix| {
                    prefix.as_str().is_some_and(|prefix| {
                        !prefix.is_empty()
                            && s(candidate, "stderr").is_some_and(|text| text.starts_with(prefix))
                    })
                }),
        )
    };
    check().unwrap_or(false)
}

/// The verdict: `accepted` needs the whole module gate, `slice_accepted` the
/// selected cases of this contract. Existing callers retain their slice API.
pub fn acceptance(
    root: &Path,
    contract: &Value,
    rows: &[Value],
    identity: &Value,
    target: &str,
    coverage: &[Value],
    additional: &[Value],
) -> Value {
    acceptance_iter(
        root,
        contract,
        rows.iter().cloned().map(Ok),
        identity,
        target,
        coverage,
        additional,
    )
}

/// The declared profiles absent from the supplied case set.
pub(super) fn missing_profiles(contract: &Value, present: &BTreeSet<String>) -> Vec<String> {
    let mut missing: Vec<String> = contract
        .get("required_profiles")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .filter(|profile| !present.contains(*profile))
        .map(str::to_owned)
        .collect();
    missing.sort();
    missing.dedup();
    missing
}

/// Grade owned rows one at a time, retaining only identities, profiles and findings.
/// An input error or a missing registered refusal withholds acceptance.
pub(crate) fn acceptance_iter(
    root: &Path,
    contract: &Value,
    rows: impl Iterator<Item = Result<Value, String>>,
    identity: &Value,
    target: &str,
    coverage: &[Value],
    additional: &[Value],
) -> Value {
    let mut reasons = Vec::new();
    let runtime = Runtime::load(root)
        .map_err(|error| reasons.push(error))
        .ok();
    let registered = registered_refusals(contract)
        .map_err(|error| reasons.push(error))
        .unwrap_or_default();
    let mut ids = BTreeSet::new();
    let mut passed = BTreeSet::new();
    let mut refusals = BTreeSet::new();
    let mut all_parity = true;
    for row in rows {
        let row = match row {
            Ok(row) => row,
            Err(error) => {
                reasons.push(format!("case evidence unavailable: {error}"));
                all_parity = false;
                break;
            }
        };
        let id = s(&row, "id").unwrap_or_default();
        if id.is_empty() {
            reasons.push("case identity missing".into());
        }
        if !ids.insert(id.to_owned()) {
            reasons.push("duplicate case identities".into());
        }
        all_parity &= parity_passes(&row);
        if let Some(spec) = registered.get(id) {
            if refusal_valid(&row, spec, contract) {
                refusals.insert(id.to_owned());
                passed.insert(spec.profile.to_owned());
            } else {
                reasons.push(format!("{id}: registered refusal proof missing or invalid"));
            }
            if let Err(error) = validate_case_evidence(&row, identity, s(contract, "id")) {
                reasons.push(format!("{id}: {error}"));
            }
        } else {
            if s(&row, "outcome") == Some("refusal")
                || row.get("parity").and_then(|proof| s(proof, "comparison")) == Some("refusal")
            {
                reasons.push(format!("{id}: unregistered refusal"));
            }
            if row.get("parity").and_then(|proof| s(proof, "status")) == Some("pass")
                && let Some(profile) = s(&row, "profile")
            {
                passed.insert(profile.to_owned());
            }
            if row.get("open_case") == Some(&Value::Bool(true)) {
                // A listed open case has no timing or memory to grade; its failing
                // parity already withholds acceptance.
                reasons.push(format!("{id}: listed open case"));
                if let Err(error) = validate_case_evidence(&row, identity, s(contract, "id")) {
                    reasons.push(format!("{id}: {error}"));
                }
            } else {
                let verdict =
                    grade_case_with_runtime(root, contract, &row, identity, target, runtime);
                reasons.extend(verdict.reasons);
            }
        }
    }
    if ids.is_empty() || !all_parity {
        reasons.push("selected-feature parity incomplete or failing".into());
    }
    for id in registered.keys().filter(|id| !refusals.contains(**id)) {
        reasons.push(format!("{id}: required refusal missing or invalid"));
    }
    let missing = missing_profiles(contract, &passed);
    if !missing.is_empty() {
        reasons.push(format!("missing required profiles: {}", missing.join(", ")));
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

#[cfg(test)]
mod refusal_tests {
    use super::*;

    fn fixture() -> (Value, Value) {
        let contract = Value::object([
            (
                "required_profiles",
                Value::strings(["output", "refusal-profile"]),
            ),
            (
                "required_refusals",
                Value::Array(vec![Value::object([
                    ("id", Value::str("refusal")),
                    ("profile", Value::str("refusal-profile")),
                    ("sha256", Value::str("a".repeat(64))),
                ])]),
            ),
            (
                "refusal",
                Value::object([
                    ("reference_exit_code", Value::int(1)),
                    ("reference_error_code", Value::int(-7)),
                    (
                        "candidate_stderr_prefixes",
                        Value::strings(["candidate: truncated"]),
                    ),
                ]),
            ),
        ]);
        let trace = |stderr| {
            Value::object([
                ("status", Value::str("finished")),
                ("exit_code", Value::int(1)),
                ("stdout_sha256", Value::str(crate::sha256::hex(b""))),
                ("stderr", Value::str(stderr)),
            ])
        };
        let row = Value::object([
            ("id", Value::str("refusal")),
            ("profile", Value::str("refusal-profile")),
            ("input_sha256", Value::str("a".repeat(64))),
            ("outcome", Value::str("refusal")),
            (
                "parity",
                Value::object([
                    ("status", Value::str("pass")),
                    ("source_timing_status", Value::str("pass")),
                    ("comparison", Value::str("refusal")),
                ]),
            ),
            ("candidate_trace", trace("candidate: truncated input")),
            ("reference_trace", trace("reference: invalid (-7)")),
        ]);
        (contract, row)
    }

    fn set(value: &mut Value, key: &str, data: Value) {
        let Value::Object(fields) = value else {
            panic!("expected object");
        };
        if let Some((_, old)) = fields.iter_mut().find(|(name, _)| name == key) {
            *old = data;
        } else {
            fields.push((key.into(), data));
        }
    }

    #[test]
    fn registered_refusal_requires_matching_source_and_actual_failure() {
        let (contract, row) = fixture();
        let registered = registered_refusals(&contract).unwrap();
        let spec = &registered["refusal"];
        assert!(refusal_valid(&row, spec, &contract));
        for (key, data) in [
            ("profile", Value::str("other")),
            ("input_sha256", Value::str("b".repeat(64))),
            ("timing", Value::object([] as [(&str, Value); 0])),
        ] {
            let mut changed = row.clone();
            set(&mut changed, key, data);
            assert!(!refusal_valid(&changed, spec, &contract), "{key}");
        }
        for role in ["candidate_trace", "reference_trace"] {
            for (key, data) in [
                ("exit_code", Value::int(0)),
                (
                    "stdout_sha256",
                    Value::str(crate::sha256::hex(b"unexpected output")),
                ),
                ("stderr", Value::str("unrelated diagnostic")),
                ("stderr_sha256", Value::str("wrong hash")),
            ] {
                let mut changed = row.clone();
                let mut trace = changed.get(role).unwrap().clone();
                set(&mut trace, key, data);
                set(&mut changed, role, trace);
                assert!(!refusal_valid(&changed, spec, &contract), "{role}/{key}");
            }
        }
    }

    #[test]
    fn refusal_registration_requires_declared_profiles_and_unique_identities() {
        let (mut contract, _) = fixture();
        set(
            &mut contract,
            "required_profiles",
            Value::strings(["output"]),
        );
        assert!(registered_refusals(&contract).is_err());
        let (mut contract, _) = fixture();
        let spec = contract
            .get("required_refusals")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .clone();
        set(
            &mut contract,
            "required_refusals",
            Value::Array(vec![spec.clone(), spec]),
        );
        assert!(registered_refusals(&contract).is_err());
    }

    #[test]
    fn streaming_failure_cannot_omit_required_refusals() {
        let (contract, _) = fixture();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let verdict = acceptance_iter(
            root,
            &contract,
            std::iter::once(Err("raw file unavailable".into())),
            &Value::Null,
            "host",
            &[],
            &[],
        );
        assert_eq!(verdict.get("slice_accepted"), Some(&Value::Bool(false)));
        let reasons = verdict.get("slice_reasons").unwrap().as_array().unwrap();
        assert!(reasons.contains(&Value::str(
            "case evidence unavailable: raw file unavailable"
        )));
        assert!(reasons.contains(&Value::str("refusal: required refusal missing or invalid")));
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    struct Fixture {
        directory: crate::process::TempDir,
        policy: Value,
        contract: Value,
        row: Value,
        identity: Value,
    }

    impl Fixture {
        fn new() -> Self {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
            let policy = crate::checks::read_json(&repository.join("harness/policy.json")).unwrap();
            let gate = Runtime::load(repository).unwrap();
            let pairs = int(policy.get("runtime").unwrap().get("minimum_pairs")).unwrap();
            let reference_ns = 1_000_000;
            let candidate_ns =
                ((gate.target + gate.ceiling) / 2.0 * f64::from(reference_ns)) as i128;
            let work = |elapsed_ns| {
                Value::object([
                    ("status", Value::str("ok")),
                    (
                        "data",
                        Value::object([
                            ("elapsed_ns", Value::int(elapsed_ns)),
                            ("iterations", Value::int(1)),
                            ("packets", Value::int(1)),
                            ("bytes", Value::int(10)),
                            ("digest", Value::int(9)),
                        ]),
                    ),
                ])
            };
            let mut stages = vec!["before-timing-session".to_owned()];
            let mut warmups = Vec::new();
            for (role, elapsed) in [
                ("candidate", candidate_ns),
                ("reference", i128::from(reference_ns)),
            ] {
                warmups.push(Value::object([
                    ("worker", Value::str(role)),
                    ("result", work(elapsed)),
                ]));
                stages.extend([
                    format!("before-warmup:{role}"),
                    format!("after-warmup:{role}"),
                ]);
            }
            let samples = (0..pairs)
                .map(|pair| {
                    for role in ["candidate", "reference"] {
                        stages.extend([
                            format!("before-pair:{pair}:{role}"),
                            format!("after-pair:{pair}:{role}"),
                        ]);
                    }
                    Value::object([
                        ("pair", Value::int(pair)),
                        ("order", Value::strings(["candidate", "reference"])),
                        ("candidate", work(candidate_ns)),
                        ("reference", work(i128::from(reference_ns))),
                        ("cpu_before", Value::Object(vec![])),
                        ("cpu_after", Value::Object(vec![])),
                    ])
                })
                .collect();
            let hashes = Value::object([
                ("candidate", Value::str("candidate-hash")),
                ("reference", Value::str("reference-hash")),
            ]);
            let interval = crate::stats::ratio_interval(
                &vec![
                    (candidate_ns as f64, f64::from(reference_ns));
                    usize::try_from(pairs).unwrap()
                ],
                0,
                4000,
            )
            .unwrap();
            let row = Value::object([
                ("id", Value::str("policy-fixture")),
                ("profile", Value::str("policy-fixture")),
                (
                    "parity",
                    Value::object([
                        ("status", Value::str("pass")),
                        ("source_timing_status", Value::str("pass")),
                    ]),
                ),
                (
                    "timing",
                    Value::object([
                        ("status", Value::str("measured")),
                        ("pairs", Value::int(pairs)),
                        ("iterations", Value::int(1)),
                        ("seed", Value::int(0)),
                        ("worker_hashes", hashes.clone()),
                        ("ratio", super::super::timing::interval_value(&interval)),
                        ("warmups", Value::Array(warmups)),
                        ("samples", Value::Array(samples)),
                        (
                            "timing_module_policy",
                            Value::str(super::super::timing::MODULE_POLICY),
                        ),
                        (
                            "timing_module_observations",
                            Value::Array(
                                stages
                                    .into_iter()
                                    .map(|stage| {
                                        Value::object([
                                            ("stage", Value::str(stage)),
                                            (
                                                "state",
                                                Value::object([(
                                                    "status",
                                                    Value::str("not-applicable"),
                                                )]),
                                            ),
                                        ])
                                    })
                                    .collect(),
                            ),
                        ),
                    ]),
                ),
            ]);
            let fixture = Self {
                directory: crate::process::TempDir::new(None).unwrap(),
                policy,
                contract: Value::object([
                    ("required_profiles", Value::strings(["policy-fixture"])),
                    ("minimum_pairs", Value::int(pairs)),
                    ("iterations", Value::int(1)),
                ]),
                row,
                identity: Value::object([("binaries", hashes)]),
            };
            std::fs::create_dir(fixture.directory.0.join("harness")).unwrap();
            fixture.write_policy();
            fixture
        }

        fn write_policy(&self) {
            std::fs::write(
                self.directory.0.join("harness/policy.json"),
                crate::json::canonical(&self.policy),
            )
            .unwrap();
        }

        fn set_runtime(&mut self, key: &str, value: f64) {
            let Value::Object(fields) = &mut self.policy else {
                unreachable!()
            };
            let Value::Object(runtime) = &mut fields
                .iter_mut()
                .find(|(name, _)| name == "runtime")
                .unwrap()
                .1
            else {
                unreachable!()
            };
            runtime.iter_mut().find(|(name, _)| name == key).unwrap().1 = Value::float(value);
            self.write_policy();
        }

        fn verdict(&self) -> CaseVerdict {
            grade_case(
                &self.directory.0,
                &self.contract,
                &self.row,
                &self.identity,
                "host",
            )
        }

        fn upper(&self) -> f64 {
            self.row
                .get("timing")
                .unwrap()
                .get("ratio")
                .unwrap()
                .get("upper_95")
                .unwrap()
                .as_f64()
                .unwrap()
        }
    }

    #[test]
    fn lowering_policy_ceiling_regrades_valid_raw_timing() {
        let mut fixture = Fixture::new();
        assert!(fixture.verdict().timing_pass);
        let gate = Runtime::load(&fixture.directory.0).unwrap();
        fixture.set_runtime("ratio_max", (gate.target + fixture.upper()) / 2.0);
        let verdict = fixture.verdict();
        assert!(!verdict.timing_pass);
        assert!(
            verdict
                .reasons
                .iter()
                .any(|reason| reason.ends_with("runtime gate unproven or failed"))
        );
    }

    #[test]
    fn changing_only_policy_target_does_not_refuse_or_regrade_runtime() {
        let mut fixture = Fixture::new();
        let before = fixture.verdict();
        assert!(before.timing_pass);
        let gate = Runtime::load(&fixture.directory.0).unwrap();
        fixture.set_runtime(
            "optimisation_target",
            (fixture.upper() + gate.ceiling) / 2.0,
        );
        let after = fixture.verdict();
        assert!(after.timing_pass);
        assert_eq!(before.reasons, after.reasons);
    }
}
