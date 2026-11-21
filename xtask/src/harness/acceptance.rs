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
use super::policy::Policy;
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
#[cfg(test)]
pub(crate) fn memory_outputs_match(raw: &Value, expected: &Value) -> bool {
    selected_memory_outputs_match(raw, expected, super::memory::Selection::Both)
}

pub(super) fn selected_memory_outputs_match(
    raw: &Value,
    expected: &Value,
    selection: super::memory::Selection,
) -> bool {
    let roles = selection.roles();
    output_roles(expected)
        && raw
            .get("samples")
            .and_then(Value::as_array)
            .is_some_and(|samples| {
                !samples.is_empty()
                    && samples.iter().all(|sample| {
                        matches!(sample, Value::Object(entries) if entries.len() == roles.len())
                            && roles.iter().all(|name| {
                                let expected = name
                                    .split_once('_')
                                    .and_then(|(role, _)| expected.get(role));
                                sample
                                    .get(name)
                                    .and_then(|value| value.get("work"))
                                    .zip(expected)
                                    .is_some_and(|(actual, expected)| {
                                        output_matches(actual, expected, false)
                                    })
                            })
                    })
            })
}

/// Check a timing row against its contract: the declared iteration count, any
/// pinned outputs, and the record itself as [`super::timing::validate`] grades it.
pub fn timing_valid(
    row: &Value,
    contract: &Value,
    selected: &Value,
    require_linux: bool,
    ceiling: Option<f64>,
) -> bool {
    let timing = row.get("timing").unwrap_or(&Value::Null);
    let Ok(fields) = super::comparison::work_fields(contract) else {
        return false;
    };
    let own = super::comparison::own_fields(row.get("expected_time_outputs"), &fields);
    let work = super::comparison::Work { fields, own };
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
                minimum_pairs: int(contract.get("minimum_pairs"))
                    .unwrap_or(i128::MAX)
                    .max(super::timing::GATE_PAIRS as i128),
                work: &work,
                require_linux,
                module_absence: contract.get("case_operations") == Some(&Value::Bool(true)),
                ceiling,
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
    match Policy::load(root) {
        Ok(policy) => grade_case_with_policy(root, contract, row, identity, target, Some(policy)),
        Err(error) => {
            let mut verdict = grade_case_with_policy(root, contract, row, identity, target, None);
            verdict.reasons.insert(0, error);
            verdict
        }
    }
}

fn grade_case_with_policy(
    root: &Path,
    contract: &Value,
    row: &Value,
    identity: &Value,
    target: &str,
    policy: Option<Policy>,
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
        && timing_valid(
            row,
            contract,
            &selected,
            require_linux,
            policy.map(|policy| policy.runtime.ceiling),
        )
        && !timing.get("contaminants").is_some_and(Value::truthy)
        && upper.is_finite()
        && upper > 0.0
        && policy.is_some_and(|policy| upper <= policy.runtime.ceiling);
    let not_measured = row.get("not_measured");
    if !timing_ok
        && not_measured
            .and_then(|bounds| bounds.get("runtime"))
            .is_none()
    {
        reasons.push(format!("{id}: runtime gate unproven or failed"));
    }
    let memory = row.get("memory").unwrap_or(&Value::Null);
    // Graded as harness/policy.json states; RSS growth is reported only.
    let ratios = ["total_rss_ratio_upper_95", "heap_ratio_upper_95"]
        .map(|k| float(memory.get(k)).unwrap_or(f64::INFINITY));
    let selection = super::memory::Selection::for_case(row);
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
        .is_none_or(|expected| selected_memory_outputs_match(memory, expected, selection))
        && s(memory, "status") == Some("qualified")
        && policy.is_some_and(|policy| {
            (!selection.rss() || (ratios[0] > 0.0 && ratios[0] < policy.memory.total_rss))
                && (!selection.heap() || (ratios[1] >= 0.0 && ratios[1] < policy.memory.heap))
        });
    if !memory_ok
        && !not_measured
            .is_some_and(|bounds| bounds.get("heap").is_some() && bounds.get("total_rss").is_some())
    {
        reasons.push(format!(
            "{id}: lower RSS/live allocation gate unproven or failed"
        ));
    }
    if let Some(Value::Object(bounds)) = not_measured {
        for (bound, cause) in bounds {
            if let Some(cause) = cause.as_str() {
                reasons.push(format!("{id}: {bound} not measured: {cause}"));
            }
        }
    }
    CaseVerdict {
        timing_pass: prerequisites_pass && timing_ok,
        memory_pass: prerequisites_pass && memory_ok,
        reasons,
        _evidence_checked: true,
    }
}

pub(crate) fn parity_passes(row: &Value) -> bool {
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
    let policy = Policy::load(root).map_err(|error| reasons.push(error)).ok();
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
                    grade_case_with_policy(root, contract, &row, identity, target, policy);
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
            gate_build::identity_refusals(root, identity, &pinned)
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
    // A run on one required target can accept its slice; the whole module gate
    // also needs every other required target's evidence, which one run lacks.
    let required_here = unique_targets.contains(&target);
    if !required_here {
        reasons.push("required cross-target evidence missing".into());
    }
    let slice_reasons = reasons.clone();
    if required_here && unique_targets.len() > 1 {
        reasons.push("required cross-target evidence missing".into());
    }
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
mod refusal_tests;

#[cfg(test)]
mod policy_tests;
