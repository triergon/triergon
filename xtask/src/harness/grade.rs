//! Join separate timing and memory collections without changing their evidence.
//! The memory build identity remains current while the existing acceptance
//! evaluator checks the raw samples and the original kernel proof.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use super::acceptance::acceptance_iter;
use super::comparison::reference_summary;
use super::gate_build;
use super::reuse;
use super::timing::{self, GATE_PAIRS};
use super::validation::{
    self, array, external_contract, field, hash, members, put, require_same, selected_operation,
    text,
};
use crate::json::{self, Value};
use crate::sha256;

pub const USAGE: &str =
    "usage: cargo xtask grade TIMING MEMORY --contract PATH --manifest PATH --output DIR
    [--additional-report EXTERNAL-GRADED-REPORT] [--require-slice-acceptance|--require-acceptance]
    [--isobmff-part I/N] [--full]";

type R<T> = Result<T, String>;

struct Options {
    timing: PathBuf,
    memory: PathBuf,
    contract: PathBuf,
    manifest: PathBuf,
    output: PathBuf,
    additional: Vec<PathBuf>,
    requirement: Option<&'static str>,
    isobmff_part: Option<(usize, usize)>,
    /// Grade the reports even when a cited set holds this grade.
    full: bool,
}

impl Options {
    fn parse(root: &Path, args: &[String]) -> R<Self> {
        let [timing, memory, rest @ ..] = args else {
            return Err("both timing and memory reports are required".into());
        };
        let (mut contract, mut manifest, mut output) = (None, None, None);
        let mut additional = Vec::new();
        let mut requirement = None;
        let mut isobmff_part = None;
        let mut full = false;
        let mut rest = rest.iter();
        while let Some(flag) = rest.next() {
            match flag.as_str() {
                "--full" => full = true,
                "--isobmff-part" => {
                    let part = rest.next().ok_or("--isobmff-part needs I/N")?;
                    if isobmff_part
                        .replace(crate::gate::parse_part(part)?)
                        .is_some()
                    {
                        return Err("duplicate --isobmff-part".into());
                    }
                }
                "--require-slice-acceptance" | "--require-acceptance" => {
                    let key = if flag == "--require-acceptance" {
                        "accepted"
                    } else {
                        "slice_accepted"
                    };
                    if requirement.replace(key).is_some() {
                        return Err("specify one acceptance requirement".into());
                    }
                }
                "--contract" | "--manifest" | "--output" | "--additional-report" => {
                    let path = rest
                        .next()
                        .filter(|v| !v.is_empty() && !v.starts_with("--"))
                        .ok_or_else(|| format!("{flag} requires a path"))?;
                    let path = root.join(path);
                    let slot = match flag.as_str() {
                        "--contract" => &mut contract,
                        "--manifest" => &mut manifest,
                        "--output" => &mut output,
                        _ => {
                            additional.push(path);
                            continue;
                        }
                    };
                    if slot.replace(path).is_some() {
                        return Err(format!("duplicate option {flag}"));
                    }
                }
                _ => return Err(format!("unexpected argument {flag}")),
            }
        }
        if isobmff_part.is_some() && (requirement.is_some() || !additional.is_empty()) {
            return Err(
                "an ISO BMFF part cannot request full acceptance or additional reports".into(),
            );
        }
        Ok(Self {
            timing: root.join(timing),
            memory: root.join(memory),
            contract: contract.ok_or("--contract is required")?,
            manifest: manifest.ok_or("--manifest is required")?,
            output: output.ok_or("--output is required")?,
            additional,
            requirement,
            isobmff_part,
            full,
        })
    }
}

fn integer(value: &Value, key: &str) -> R<i128> {
    match field(value, key)? {
        Value::Number(n) => n
            .as_i128()
            .ok_or_else(|| format!("{key} must be an integer")),
        _ => Err(format!("{key} must be an integer")),
    }
}

fn same_field(a: &Value, b: &Value, key: &str) -> R<()> {
    match (a.get(key), b.get(key)) {
        (Some(a), Some(b)) => require_same(a, b, key),
        (None, None) => Ok(()),
        _ => Err(format!("{key} mismatch")),
    }
}

fn without(value: &Value, keys: &[&str]) -> R<Value> {
    Ok(Value::Object(
        members(value)?
            .iter()
            .filter(|(k, _)| !keys.contains(&k.as_str()))
            .cloned()
            .collect(),
    ))
}

/// The fields of a case row that its timing and memory collections must agree on:
/// Compare parity outputs and status, plus the presence of a valid duration.
/// Its numeric value may differ because each collection runs its own probe.
fn joined_fields(row: &Value) -> R<Value> {
    let mut fields = without(row, &["timing", "memory"])?;
    if let Some(parity) = row.get("parity")
        && let Some(run) = parity.get("reference_run")
    {
        let elapsed = run.get("elapsed_ns").ok_or("missing elapsed_ns")?;
        let has_duration = *elapsed != Value::Null;
        if has_duration {
            u64::try_from(integer(run, "elapsed_ns")?)
                .map_err(|_| "reference process duration must fit u64")?;
        }
        let mut parity = parity.clone();
        let mut run = run.clone();
        put(&mut run, "elapsed_ns", Value::Bool(has_duration))?;
        put(&mut parity, "reference_run", run)?;
        put(&mut fields, "parity", parity)?;
    }
    // The reference can log process addresses. Compare the diagnostic text
    // outside those addresses and keep both raw traces in their collections.
    if let Some(trace) = row.get("reference_trace")
        && let Some(stderr) = trace.get("stderr")
    {
        let stderr = stderr.as_str().ok_or("reference stderr must be text")?;
        if hash(trace, "stderr_sha256")? != sha256::hex(stderr.as_bytes()) {
            return Err("reference diagnostic hash mismatch".into());
        }
        let mut stable = without(trace, &["stderr", "stderr_sha256"])?;
        put(
            &mut stable,
            "stderr",
            Value::str(reference_diagnostics(stderr)),
        )?;
        put(&mut fields, "reference_trace", stable)?;
    }
    Ok(fields)
}

fn reference_diagnostics(stderr: &str) -> String {
    let mut normalized = String::with_capacity(stderr.len());
    for line in stderr.split_inclusive('\n') {
        if let Some((prefix, message)) =
            line.strip_prefix('[').and_then(|line| line.split_once(']'))
            && let Some((header, address)) = prefix.rsplit_once(" @ 0x")
            && !header.is_empty()
            && !address.is_empty()
            && address.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            normalized.push('[');
            normalized.push_str(header);
            normalized.push(']');
            normalized.push_str(message);
        } else {
            normalized.push_str(line);
        }
    }
    normalized
}

fn document(path: &Path) -> R<(Value, String)> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value = json::parse_observations(
        std::str::from_utf8(&bytes).map_err(|e| format!("{}: {e}", path.display()))?,
    )
    .map_err(|e| format!("{}: {e}", path.display()))?;
    members(&value).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((value, sha256::hex(&bytes)))
}

fn expanded_case(report: &Value, path: &Path, row: &Value) -> R<Value> {
    match report.get("case_storage").and_then(Value::as_str) {
        None | Some("inline") => Ok(row.clone()),
        Some("sha256-files") => validation::expand_measurements(
            path.parent().ok_or("report has no parent directory")?,
            row,
        ),
        Some(_) => Err("unsupported report case storage".into()),
    }
}

fn control_proofs(
    root: &Path,
    contract: &Value,
    manifest: &Value,
    timed: (&Path, &Value),
    memory: (&Path, &Value),
) -> R<()> {
    for (role, (path, report)) in [("timing", timed), ("memory", memory)] {
        crate::progressive_bench::validate_report_controls(root, path, report, contract, manifest)
            .map_err(|error| format!("{role} functional controls: {error}"))?;
    }
    Ok(())
}

/// Only the kernel proof pin may be added after timing. All other build fields,
/// including source graphs, compiler settings and every worker hash, stay equal.
fn identities(timed: &Value, memory: &Value) -> R<()> {
    for identity in [timed, memory] {
        for key in [
            "schema",
            "rustc",
            "compiler",
            "compiler_command",
            "candidate_command",
            "build_environment",
            "cargo_configuration",
            "binaries",
            "source_revision",
            "reference_build",
            "instrumentation",
        ] {
            field(identity, key)?;
        }
        if integer(identity, "schema")? == 2 {
            field(identity, "selected_graph")?;
        } else {
            field(identity, "source_hashes")?;
            field(identity, "build_input_hashes")?;
        }
        for role in ["candidate", "reference"] {
            hash(field(identity, "binaries")?, role)?;
        }
    }
    let (a, b) = (
        field(timed, "instrumentation")?,
        field(memory, "instrumentation")?,
    );
    if let Some(proof) = a.get("rss_kernel_bound") {
        require_same(proof, field(b, "rss_kernel_bound")?, "kernel proof pin")?;
    }
    require_same(
        &without(a, &["rss_kernel_bound"])?,
        &without(b, &["rss_kernel_bound"])?,
        "measurement instrumentation",
    )?;
    require_same(
        &without(timed, &["instrumentation"])?,
        &without(memory, &["instrumentation"])?,
        "source/build identity",
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Corpus,
    External,
}

/// The two collection formats. Only an external-vector report can omit the
/// completion marker. Both formats must contain a case.
fn shaped_rows(report: &Value) -> R<(Shape, &[Value])> {
    let (shape, rows) = match (report.get("cases"), report.get("case")) {
        (Some(Value::Array(rows)), None) if !rows.is_empty() => (Shape::Corpus, rows.as_slice()),
        (None, Some(row @ Value::Object(_))) => (Shape::External, std::slice::from_ref(row)),
        _ => return Err("report needs a nonempty cases array or one external case".into()),
    };
    if (shape == Shape::Corpus || report.get("collection_status").is_some())
        && text(report, "collection_status")? != "complete"
    {
        return Err("the report's collection is incomplete".into());
    }
    Ok((shape, rows))
}

fn shape(report: &Value) -> R<Shape> {
    Ok(shaped_rows(report)?.0)
}

pub(crate) fn report_rows(report: &Value) -> R<&[Value]> {
    Ok(shaped_rows(report)?.1)
}

struct Binding<'a> {
    contract: &'a Value,
    manifest: &'a Value,
    contract_sha: &'a str,
    manifest_sha: &'a str,
}

impl Binding<'_> {
    fn registered(&self, root: &Path) -> R<()> {
        let id = text(self.contract, "id")?;
        if Path::new(id).components().count() != 1
            || !matches!(
                Path::new(id).components().next(),
                Some(Component::Normal(_))
            )
        {
            return Err("contract must name a registered feature".into());
        }
        // The complete registered feature fixes case membership and operations.
        for (name, expected) in [
            ("contract.json", self.contract_sha),
            ("corpus.json", self.manifest_sha),
        ] {
            let path = root.join("harness/features").join(id).join(name);
            if super::manifest::read(root, &path)?.1 != expected {
                return Err(format!(
                    "{id}/{name} differs from the complete registered feature"
                ));
            }
        }
        Ok(())
    }

    fn headers(&self, timed: &Value, memory: &Value) -> R<Shape> {
        let kind = shape(timed)?;
        if kind != shape(memory)? {
            return Err("report case shapes differ".into());
        }
        for report in [timed, memory] {
            let unchanged = report
                .get("summary")
                .and_then(|summary| summary.get("source_and_workers_unchanged"));
            if unchanged.is_some_and(|value| *value != Value::Bool(true))
                || (self.contract.get("case_operations") == Some(&Value::Bool(true))
                    && unchanged.is_none())
            {
                return Err("source or worker bookend missing or changed during collection".into());
            }
            if integer(report, "schema")? != 1 {
                return Err("unsupported report schema".into());
            }
            if hash(report, "contract_sha256")? != self.contract_sha {
                return Err("contract hash mismatch".into());
            }
            for (key, expected) in [
                ("contract_id", text(self.contract, "id")?),
                ("corpus_manifest_sha256", self.manifest_sha),
            ] {
                if (kind == Shape::Corpus || report.get(key).is_some())
                    && text(report, key)? != expected
                {
                    return Err(format!("{key} mismatch"));
                }
            }
            text(report, "target")?;
            match kind {
                Shape::Corpus => field(field(report, "environment")?, "cpus")?,
                Shape::External => field(report, "cpus")?,
            };
        }
        for key in [
            "target",
            "fate_coverage",
            "additional_cases",
            "input_path",
            "origin",
            "redistribution_qualified",
        ] {
            same_field(timed, memory, key)?;
        }
        // Each report keeps its own CPU record: timing runs alone on one CPU,
        // memory side by side on several, which changes no observation.
        identities(field(timed, "identity")?, field(memory, "identity")?)?;
        Ok(kind)
    }

    fn expected(&self, report: &Value, kind: Shape) -> R<Vec<Value>> {
        if kind == Shape::Corpus {
            return Ok(array(self.manifest, "cases")?.to_vec());
        }
        let id = text(field(report, "case")?, "id")?;
        let matching: Vec<_> = self
            .external_specs()?
            .iter()
            .filter(|spec| spec.get("file").and_then(Value::as_str) == Some(id))
            .collect();
        if matching.len() != 1 {
            return Err("external case is not a unique required contract vector".into());
        }
        Ok(vec![Value::object([
            ("id", Value::str(id)),
            ("profile", Value::str(id)),
            ("sha256", Value::str(hash(matching[0], "sha256")?)),
        ])])
    }

    fn external_specs(&self) -> R<&[Value]> {
        match self.contract.get("required_additional_vectors") {
            Some(Value::Array(specs)) => Ok(specs),
            None => Ok(&[]),
            _ => Err("required_additional_vectors must be an array".into()),
        }
    }
}

/// Match the complete manifest, not just its measured or required-profile subset.
fn membership<'a>(expected: &[Value], actual: &'a [Value]) -> R<BTreeMap<&'a str, &'a Value>> {
    let mut required = BTreeSet::new();
    for row in expected {
        let id = text(row, "id")?;
        hash(row, "sha256")?;
        text(row, "profile")?;
        if !required.insert(id) {
            return Err(format!("duplicate manifest case {id}"));
        }
    }
    if required.is_empty() {
        return Err("manifest has no cases".into());
    }
    let mut found = BTreeMap::new();
    for row in actual {
        let id = text(row, "id")?;
        if !required.contains(id) {
            return Err(format!("unexpected case {id}"));
        }
        if found.insert(id, row).is_some() {
            return Err(format!("duplicate report case {id}"));
        }
    }
    for row in expected {
        let id = text(row, "id")?;
        let actual = found.get(id).ok_or_else(|| format!("missing case {id}"))?;
        if hash(actual, "input_sha256")? != hash(row, "sha256")? {
            return Err(format!("{id}: input hash mismatch"));
        }
        require_same(field(actual, "profile")?, field(row, "profile")?, "profile")?;
        match (row.get("entities"), actual.get("input_entities")) {
            (Some(expected), Some(reported)) => require_same(reported, expected, "input entities")?,
            (None, None) => {}
            _ => return Err(format!("{id}: input entities mismatch")),
        }
    }
    Ok(found)
}

/// An alias keeps its own parity proof but reuses only an identical operation.
fn aliases(expected: &[Value], actual: &BTreeMap<&str, &Value>, partial: bool) -> R<()> {
    for spec in expected {
        let id = text(spec, "id")?;
        let row = actual[id];
        let Some(alias) = spec.get("alias_of").filter(|value| **value != Value::Null) else {
            if row.get("sampled_as").is_some() || row.get("sampled_profile").is_some() {
                return Err(format!("{id}: undeclared measurement alias"));
            }
            continue;
        };
        let index = integer(spec, "index")?;
        let Some(prior) = expected
            .iter()
            .find(|prior| prior.get("index") == Some(alias))
        else {
            return Err(format!("{id}: alias operation is missing"));
        };
        if integer(prior, "index")? >= index
            || prior.get("outcome").and_then(Value::as_str) != Some("output")
        {
            return Err(format!("{id}: invalid measurement alias"));
        }
        let previous = actual[text(prior, "id")?];
        for key in [
            "sha256",
            "operation",
            "expected_time_outputs",
            "expected_memory_outputs",
        ] {
            require_same(field(spec, key)?, field(prior, key)?, "alias operation")?;
        }
        for key in ["timing", "memory"] {
            same_field(row, previous, key)?;
        }
        let sample_required = !partial
            || has_measurement(row)
            || row.get("sampled_as").is_some()
            || row.get("sampled_profile").is_some();
        if sample_required
            && (row.get("sampled_as") != Some(alias)
                || row.get("sampled_profile") != prior.get("profile"))
        {
            return Err(format!("{id}: invalid measurement alias"));
        }
        for key in ["candidate_trace", "reference_trace"] {
            require_same(
                field(field(row, key)?, "stdout_sha256")?,
                field(field(previous, key)?, "stdout_sha256")?,
                "alias trace",
            )?;
        }
    }
    Ok(())
}

fn has_measurement(row: &Value) -> bool {
    ["timing", "memory"]
        .iter()
        .any(|key| row.get(key).is_some_and(|value| *value != Value::Null))
}

fn unmeasured_output(timed: &Value, memory: &Value) -> R<()> {
    require_same(
        &joined_fields(timed)?,
        &joined_fields(memory)?,
        "case fields/parity",
    )?;
    if has_measurement(timed)
        || has_measurement(memory)
        || !super::acceptance::parity_passes(timed)
        || text(field(timed, "parity")?, "comparison")? != "output"
    {
        return Err("part requires matching output parity without measurements".into());
    }
    Ok(())
}

fn part_selection(
    root: &Path,
    contract: &Value,
    timed: &Value,
    memory: &Value,
    requested: Option<(usize, usize)>,
) -> R<Option<BTreeSet<usize>>> {
    let Some((index, count)) = requested else {
        if [timed, memory].iter().any(|report| {
            report
                .get("part")
                .is_some_and(|value| *value != Value::Null)
        }) {
            return Err("grading an ISO BMFF part requires its explicit --isobmff-part".into());
        }
        return Ok(None);
    };
    if text(contract, "id")? != "isobmff-open-index" {
        return Err(
            "--isobmff-part requires the registered ISO BMFF open and index contract".into(),
        );
    }
    let part = Value::str(format!("{}/{count}", index + 1));
    let (_, selection) = crate::isobmff_bench::full_part_selection(root, (index, count))?;
    let encoded = Value::Array(
        selection
            .iter()
            .map(|&index| Value::int(index as i128))
            .collect(),
    );
    for report in [timed, memory] {
        require_same(field(report, "part")?, &part, "ISO BMFF part")?;
        require_same(
            field(report, "selection")?,
            &encoded,
            "ISO BMFF part selection",
        )?;
    }
    Ok(Some(selection.into_iter().collect()))
}

fn selected(row: &Value, expected: &Value, contract: &Value, identity: &Value) -> R<Value> {
    let id = text(contract, "id")?;
    let resolved = selected_operation(expected, contract, identity)?;
    if row.get("reference_worker").and_then(Value::as_str) != resolved.reference_worker {
        return Err("reference worker differs from the contract".into());
    }
    require_same(
        field(row, "reference")?,
        &reference_summary(&resolved.reference),
        "reference",
    )?;
    if row.get("measurement_worker").and_then(Value::as_str) != resolved.measurement_worker {
        return Err("measurement reference differs from the contract".into());
    }
    if row.get("candidate_worker").and_then(Value::as_str) != resolved.candidate_worker {
        return Err("candidate worker differs from the contract".into());
    }
    require_same(
        field(row, "operation")?,
        &resolved.operation,
        "contract operation",
    )?;
    for key in [
        "expected_time_outputs",
        "expected_memory_outputs",
        "outcome",
        "alias_of",
    ] {
        if let Some(value) = expected.get(key) {
            let observed = row.get(key).ok_or_else(|| format!("missing {key}"))?;
            if key.ends_with("_outputs") {
                let expected_roles = members(value)?;
                if members(observed)?.len() != expected_roles.len()
                    || expected_roles.iter().any(|(role, output)| {
                        !observed.get(role).is_some_and(|actual| {
                            super::acceptance::output_matches(actual, output, false)
                        })
                    })
                {
                    return Err(format!("{key} differs from the exact registered outputs"));
                }
            } else {
                require_same(observed, value, key)?;
            }
        }
    }
    super::comparison::validate_case_evidence(row, identity, Some(id))
}

fn samples(measurement: &Value, minimum: i128, failed_memory: bool) -> R<&[Value]> {
    let raw = array(measurement, "samples")?;
    // Failed memory summaries can retain complete raw pairs without reporting a
    // count. Validate their actual membership without inventing summary fields.
    // A qualified memory row or any timing row must still declare its count.
    let pairs = if failed_memory && measurement.get("pairs").is_none() {
        raw.len() as i128
    } else {
        integer(measurement, "pairs")?
    };
    // A collection that stopped early by rule set out to run its planned pairs;
    // the evaluator replays the rule.
    let planned = timing::planned_pairs(measurement).unwrap_or(pairs);
    if planned < minimum.max(GATE_PAIRS as i128) || pairs != raw.len() as i128 {
        return Err("missing paired samples or pair count mismatch".into());
    }
    Ok(raw)
}

fn work(value: &Value, fields: &[String]) -> R<Value> {
    super::comparison::work_summary(value, fields)
}

fn memory_binding(memory: &Value, identity: &Value, workers: &Value, linux: bool) -> R<()> {
    let certificate = field(memory, "certificate")?;
    require_same(
        field(certificate, "worker_hashes")?,
        workers,
        "memory workers",
    )?;
    let instrumentation = field(identity, "instrumentation")?;
    for key in ["observer_sha256", "probe_sha256"] {
        require_same(field(certificate, key)?, field(instrumentation, key)?, key)?;
    }
    if linux {
        // Match memory::evaluate: the standard total resident peak does not
        // need a kernel proof. A certificate opt-in or any kernel-backed RSS
        // observation must retain the original proof binding; acceptance
        // recomputes its live kernel/boot validity and every sample below.
        if !super::memory::kernel_used(certificate, array(memory, "samples")?) {
            return Ok(());
        }
        if certificate.get("kernel_rss_required") != Some(&Value::Bool(true)) {
            return Err("memory kernel RSS proof is required".into());
        }
        let profile = field(field(certificate, "kernel_rss_certificate")?, "profile")?;
        let pin = field(instrumentation, "rss_kernel_bound")?;
        for key in ["proof_path", "proof_sha256"] {
            require_same(field(profile, key)?, field(pin, key)?, key)?;
        }
    }
    Ok(())
}

/// A case the contract lists in `open_cases`: its parity is graded, its timing and
/// memory are not. A listed case that passes must leave the list, which only
/// shrinks.
fn open_case(timed: &Value, measured: &Value) -> R<Value> {
    require_same(
        &joined_fields(timed)?,
        &joined_fields(measured)?,
        "case fields/parity",
    )?;
    if super::acceptance::parity_passes(timed) {
        return Err("listed open case now passes; remove it from open_cases".into());
    }
    let mut row = without(timed, &["timing", "memory"])?;
    put(&mut row, "open_case", Value::Bool(true))?;
    Ok(row)
}

/// `ceiling` is the policy's runtime ceiling, which a timing record that may
/// stop early must name.
#[allow(clippy::too_many_arguments)] // Grading binds the case, collections, workers, target and bound policy.
fn join_case(
    timed: &Value,
    measured: &Value,
    expected: &Value,
    contract: &Value,
    identity: &Value,
    target: &str,
    ceiling: f64,
    unmeasured: &super::bounds::Unmeasured<'_>,
) -> R<Value> {
    require_same(
        &joined_fields(timed)?,
        &joined_fields(measured)?,
        "case fields/parity",
    )?;
    let workers = selected(timed, expected, contract, identity)?;
    let id = text(expected, "id")?;
    let annotation = unmeasured.annotation(id);
    if unmeasured.causes(id).next().is_some() {
        require_same(
            field(timed, "not_measured")?,
            &annotation,
            "not measured bounds",
        )?;
    } else if timed.get("not_measured").is_some() {
        return Err("undeclared not measured bounds".into());
    }
    if expected.get("outcome").and_then(Value::as_str) == Some("refusal") {
        if has_measurement(timed) || has_measurement(measured) {
            return Err("a registered refusal cannot contain successful measurements".into());
        }
        return Ok(timed.clone());
    }
    let measurement = |first: &Value, other: &Value, key: &str| -> R<Option<Value>> {
        let present = |row: &Value| row.get(key).filter(|value| **value != Value::Null).cloned();
        let value = present(first);
        if unmeasured.skips(id, key) {
            if value.is_some() || present(other).is_some() {
                return Err(format!("unmeasured case contains {key} samples"));
            }
            return Ok(None);
        }
        let value = value.ok_or_else(|| format!("missing {key} measurement"))?;
        if let Some(previous) = present(other) {
            require_same(&previous, &value, &format!("retained {key} measurement"))?;
        }
        require_same(
            field(&value, "input_sha256")?,
            field(timed, "input_sha256")?,
            "measurement input hash",
        )?;
        Ok(Some(value))
    };
    let clock = measurement(timed, measured, "timing")?;
    let memory = measurement(measured, timed, "memory")?;
    let fields = super::comparison::work_fields(contract)?;
    let minimum = integer(contract, "minimum_pairs")?;
    let linux = target.contains("linux")
        || identity
            .get("host_platform")
            .and_then(|h| h.get("system"))
            .and_then(Value::as_str)
            == Some("Linux");
    let parity = field(timed, "parity")?;
    let check_work = |output: &Value, iterations: i128| -> R<()> {
        for expected in [
            expected.get("expected"),
            parity.get("expected"),
            parity.get("candidate"),
            parity.get("reference"),
        ]
        .into_iter()
        .flatten()
        {
            let expected = work(expected, &fields)?;
            if iterations == 1 {
                require_same(&expected, output, "expected output")?;
            } else {
                for key in ["packets", "bytes"] {
                    if integer(&expected, key)?.checked_mul(iterations)
                        != Some(integer(output, key)?)
                    {
                        return Err(format!("timing {key} work mismatch"));
                    }
                }
            }
        }
        Ok(())
    };
    let mut timed_work = None;
    if let Some(clock) = &clock {
        if text(clock, "status")? != "measured" || integer(clock, "iterations")? < 1 {
            return Err("timing is not measured".into());
        }
        if !validation::iterations_valid(contract, field(clock, "iterations")?) {
            return Err("iterations do not match the contract".into());
        }
        timing::validate(
            clock,
            &timing::Expected {
                worker_hashes: &workers,
                input_sha256: Some(field(timed, "input_sha256")?),
                minimum_pairs: minimum.max(GATE_PAIRS as i128),
                work: &timing::Work {
                    fields: fields.clone(),
                    own: vec![],
                },
                require_linux: linux,
                module_absence: true,
                ceiling: Some(ceiling),
            },
        )
        .map_err(|error| format!("timing: {error}"))?;
        if let Some(outputs) = expected.get("expected_time_outputs")
            && !super::acceptance::timing_outputs_match(clock, outputs)
        {
            return Err("measurements do not match expected_time_outputs".into());
        }
        let output = work(
            field(field(&array(clock, "samples")?[0], "reference")?, "data")?,
            &fields,
        )?;
        let iterations = integer(clock, "iterations")?;
        if memory.is_none() {
            check_work(&output, iterations)?;
        }
        timed_work = Some((output, iterations));
    }
    let mut row = timed.clone();
    if let Some(memory) = memory {
        let memory_raw = samples(&memory, minimum, text(&memory, "status")? != "qualified")?;
        memory_binding(&memory, identity, &workers, linux)?;
        let selection = super::memory::Selection::for_case(timed);
        if let Some(outputs) = expected.get("expected_memory_outputs")
            && !super::acceptance::selected_memory_outputs_match(&memory, outputs, selection)
        {
            return Err("measurements do not match expected_memory_outputs".into());
        }
        let output = work(
            selection
                .reference_work(&memory_raw[0])
                .ok_or("missing reference memory work")?,
            &fields,
        )?;
        if let Some((timed_output, iterations)) = timed_work {
            for key in ["packets", "bytes"] {
                if integer(&output, key)?.checked_mul(iterations)
                    != Some(integer(&timed_output, key)?)
                {
                    return Err(format!("timing/memory {key} work mismatch"));
                }
            }
        }
        for sample in memory_raw {
            if members(sample)?.len() != selection.roles().len() {
                return Err("memory samples do not match the scheduled observers".into());
            }
            for role in selection.roles() {
                require_same(
                    &work(field(field(sample, role)?, "work")?, &fields)?,
                    &output,
                    "memory output",
                )?;
            }
        }
        check_work(&output, 1)?;
        put(&mut row, "memory", memory)?;
    }
    Ok(row)
}

fn current_workers(root: &Path, identity: &Value, contract: &Value) -> R<()> {
    require_same(
        identity,
        &json::read(&root.join("target/harness/identity.json"))?,
        "current memory build identity",
    )?;
    validation::current_workers(root, identity)?;
    if let Some(name) = contract
        .get("candidate")
        .and_then(Value::as_str)
        .filter(|n| *n != "media-worker")
    {
        validation::candidate_worker(root, identity, name)?;
    }
    Ok(())
}

fn additional_case<'a>(report: &'a Value, memory: &Value, binding: &Binding<'_>) -> R<&'a Value> {
    if shape(report)? != Shape::External {
        return Err("additional report must contain one external case".into());
    }
    require_same(
        field(report, "identity")?,
        field(memory, "identity")?,
        "additional report source/build identity",
    )?;
    require_same(
        field(report, "target")?,
        field(memory, "target")?,
        "additional report target",
    )?;
    if hash(report, "contract_sha256")? != binding.contract_sha {
        return Err("additional report contract hash mismatch".into());
    }
    for (key, expected) in [
        ("contract_id", text(binding.contract, "id")?),
        ("corpus_manifest_sha256", binding.manifest_sha),
    ] {
        if report.get(key).is_some() && text(report, key)? != expected {
            return Err(format!("additional report {key} mismatch"));
        }
    }
    field(report, "case")
}

fn validate_additional(
    rows: &[Value],
    binding: &Binding<'_>,
    identity: &Value,
    target: &str,
    ceiling: f64,
) -> R<()> {
    let mut seen = BTreeSet::new();
    for row in rows {
        let id = text(row, "id")?;
        if !seen.insert(id) {
            return Err(format!("duplicate additional case {id}"));
        }
        let wrapper = Value::object([("case", row.clone())]);
        let expected = binding.expected(&wrapper, Shape::External)?;
        membership(&expected, std::slice::from_ref(row))?;
        join_case(
            row,
            row,
            &expected[0],
            binding.contract,
            identity,
            target,
            ceiling,
            &super::bounds::Unmeasured::default(),
        )?;
    }
    Ok(())
}

fn exit_code(verdict: &Value, rows: &[Value], requirement: Option<&str>) -> i32 {
    if requirement.is_some_and(|key| verdict.get(key) != Some(&Value::Bool(true))) {
        return 2;
    }
    i32::from(
        rows.is_empty()
            || rows.iter().any(|r| {
                r.get("parity")
                    .and_then(|p| p.get("status"))
                    .and_then(Value::as_str)
                    != Some("pass")
            }),
    )
}

/// Grade already collected reports. Invalid joins are errors; a required gate
/// that remains open returns 2, and a recorded parity failure, or a case whose runtime,
/// heap or total RSS bound fails without the contract listing it in `open_bounds`,
/// returns 1.
pub fn main(root: &Path, args: &[String]) -> R<i32> {
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        println!("{USAGE}");
        return Ok(0);
    }
    let root = root.canonicalize().map_err(|error| error.to_string())?;
    let root = root.as_path();
    let options = Options::parse(root, args)?;
    super::report::create_output(&options.output)?;
    // A grade binds the gate results recorded beside the reports it joins.
    let collections = [&options.timing, &options.memory]
        .map(|report| report.parent().unwrap_or(Path::new(".")).to_path_buf());
    let key = if collections.iter().all(|d| reuse::read(d).is_ok()) {
        let reports = [options.timing.as_path(), options.memory.as_path()];
        let mut files = vec![options.contract.as_path(), options.manifest.as_path()];
        files.extend(options.additional.iter().map(PathBuf::as_path));
        let settings = Value::object([
            (
                "requirement",
                options.requirement.map_or(Value::Null, Value::str),
            ),
            (
                "isobmff_part",
                options
                    .isobmff_part
                    .map_or(Value::Null, |(i, n)| Value::str(format!("{}/{n}", i + 1))),
            ),
        ]);
        Some(
            reuse::Key::new(root, "grade", settings, &files, &[])?
                .untimed()
                .with_inputs(&reports)?,
        )
    } else {
        None
    };
    let join = || {
        if collections.iter().any(|d| reuse::was_reused(d)) {
            return Err(
                "a reused collection has no report to grade: measure both with --full".into(),
            );
        }
        grade(root, &options)
    };
    match key {
        Some(key) => reuse::step(root, &options.output, &key, options.full, join),
        None => join(),
    }
}

/// Join and grade the two reports.
fn grade(root: &Path, options: &Options) -> R<i32> {
    let (timed, timing_sha) = document(&options.timing)?;
    let (memory, memory_sha) = document(&options.memory)?;
    let (contract, contract_sha) = document(&options.contract)?;
    let (manifest, manifest_sha) = super::manifest::read(root, &options.manifest)?;
    // Open cases scoped to targets are those of the target the reports measured.
    let open = super::report::open_cases(&contract, &manifest, text(&timed, "target")?)?;
    let binding = Binding {
        contract: &contract,
        manifest: &manifest,
        contract_sha: &contract_sha,
        manifest_sha: &manifest_sha,
    };
    binding.registered(root)?;
    let kind = binding.headers(&timed, &memory)?;
    control_proofs(
        root,
        &contract,
        &manifest,
        (&options.timing, &timed),
        (&options.memory, &memory),
    )?;
    let identity = field(&memory, "identity")?;
    let target = text(&memory, "target")?;
    let unmeasured = super::bounds::Unmeasured::read(&contract, &manifest, target)?;
    let pinned = gate_build::pinned_release(root)?;
    println!("{}", gate_build::describe(identity, &pinned));
    let refusals = gate_build::refusals(identity, &pinned);
    if (options.requirement.is_some() || options.isobmff_part.is_some()) && !refusals.is_empty() {
        return Err(format!("not the gate build: {}", refusals.join("; ")));
    }
    current_workers(root, identity, &contract)?;
    let expected = binding.expected(&timed, kind)?;
    let timed_rows = membership(&expected, report_rows(&timed)?)?;
    let memory_rows = membership(&expected, report_rows(&memory)?)?;
    let selection = part_selection(root, &contract, &timed, &memory, options.isobmff_part)?;
    aliases(&expected, &timed_rows, selection.is_some())?;
    aliases(&expected, &memory_rows, selection.is_some())?;
    let mut grade_members: Vec<_> = expected.iter().collect();
    if let Some(selection) = &selection {
        grade_members.clear();
        for spec in &expected {
            let id = text(spec, "id")?;
            if text(spec, "outcome")? == "refusal"
                || usize::try_from(integer(spec, "index")?)
                    .is_ok_and(|index| selection.contains(&index))
            {
                grade_members.push(spec);
                continue;
            }
            let (a, b) = (timed_rows[id], memory_rows[id]);
            unmeasured_output(a, b).map_err(|error| format!("{id}: {error}"))?;
            selected(a, spec, &contract, identity)?;
        }
    }
    let policy = super::policy::Policy::load(root)?;
    let ceiling = policy.runtime.ceiling;
    let combine = |spec: &Value| -> R<Value> {
        let id = text(spec, "id")?;
        let clock = expanded_case(&timed, &options.timing, timed_rows[id])?;
        let measured = expanded_case(&memory, &options.memory, memory_rows[id])?;
        if open.iter().any(|listed| listed == id) {
            return open_case(&clock, &measured).map_err(|e| format!("{id}: {e}"));
        }
        for (bound, cause) in unmeasured.causes(id) {
            println!("{id}: {bound} not measured: {cause}");
        }
        join_case(
            &clock,
            &measured,
            spec,
            &contract,
            identity,
            target,
            ceiling,
            &unmeasured,
        )
        .map_err(|e| format!("{id}: {e}"))
    };
    let mut additional = match memory.get("additional_cases") {
        Some(Value::Array(rows)) => rows.clone(),
        None => Vec::new(),
        _ => return Err("additional_cases must be an array".into()),
    };
    let mut inputs = Vec::new();
    for path in &options.additional {
        let (report, digest) = document(path)?;
        additional.push(additional_case(&report, &memory, &binding)?.clone());
        inputs.push(Value::object([
            ("path", Value::str(path.display().to_string())),
            ("sha256", Value::str(digest)),
        ]));
    }
    if kind == Shape::External && !additional.is_empty() {
        return Err("an external-vector grade cannot contain additional cases".into());
    }
    if selection.is_some() && !additional.is_empty() {
        return Err("an ISO BMFF part cannot contain additional cases".into());
    }
    validate_additional(&additional, &binding, identity, target, ceiling)?;
    let derived;
    let selected_contract = if kind == Shape::External {
        derived = external_contract(&contract, text(&expected[0], "id")?)?;
        &derived
    } else if selection.is_some() {
        let mut part_contract = contract.clone();
        put(
            &mut part_contract,
            "required_profiles",
            Value::strings(
                grade_members
                    .iter()
                    .map(|spec| text(spec, "profile"))
                    .collect::<R<Vec<_>>>()?,
            ),
        )?;
        derived = part_contract;
        &derived
    } else {
        &contract
    };
    let coverage = match memory.get("fate_coverage") {
        Some(Value::Array(rows)) => rows.as_slice(),
        None => &[],
        _ => return Err("fate_coverage must be an array".into()),
    };
    let mut join_error = None;
    let mut bounded = Vec::new();
    let mut verdict = acceptance_iter(
        root,
        selected_contract,
        grade_members.iter().copied().map(|spec| {
            combine(spec)
                .inspect(|row| bounded.extend(super::bounds::case_bounds(row)))
                .inspect_err(|error| {
                    join_error = Some(error.clone());
                })
        }),
        identity,
        target,
        coverage,
        &additional,
    );
    if let Some(error) = join_error {
        return Err(error);
    }
    let part_pass = if selection.is_some() {
        let passed = verdict.get("slice_accepted") == Some(&Value::Bool(true));
        put(&mut verdict, "accepted", Value::Bool(false))?;
        put(&mut verdict, "slice_accepted", Value::Bool(false))?;
        put(&mut verdict, "scope", Value::str("isobmff-part"))?;
        put(&mut verdict, "part_accepted", Value::Bool(passed))?;
        Some(passed)
    } else {
        None
    };
    let mut report = without(&memory, &["cases", "case"])?;
    for (key, value) in [
        ("collection_status", Value::str("complete")),
        ("contract_id", field(&contract, "id")?.clone()),
        ("corpus_manifest_sha256", Value::str(manifest_sha)),
        ("additional_cases", Value::Array(additional)),
        ("acceptance", verdict.clone()),
        ("case_storage", Value::str("inline")),
        (
            "timing_environment",
            timed.get("environment").cloned().unwrap_or(Value::Null),
        ),
        (
            "grading_inputs",
            Value::object([
                (
                    "timing",
                    Value::object([
                        ("path", Value::str(options.timing.display().to_string())),
                        ("sha256", Value::str(timing_sha)),
                    ]),
                ),
                (
                    "memory",
                    Value::object([
                        ("path", Value::str(options.memory.display().to_string())),
                        ("sha256", Value::str(memory_sha)),
                    ]),
                ),
                ("additional_reports", Value::Array(inputs)),
            ]),
        ),
    ] {
        put(&mut report, key, value)?;
    }
    let destination = options.output.join("report.json");
    match kind {
        Shape::Corpus => super::report::write_observed_case_report(
            &options.output,
            &report,
            grade_members.iter().copied().map(combine),
        )?,
        Shape::External => {
            put(&mut report, "case", combine(&expected[0])?)?;
            super::report::publish_json(&destination, &json::to_python(&report, Some(2)))?;
        }
    }
    let mut code = exit_code(&verdict, report_rows(&memory)?, options.requirement);
    let label = text(&contract, "id")?;
    if !super::report::ratchet(label, report_rows(&timed)?, &open) {
        code = code.max(1);
    }
    // Each case's runtime, heap and total RSS bounds, as the codec bounds are graded.
    let open_bounds = super::bounds::open_bounds(&contract, &manifest, target)?;
    let (lines, failed) = super::bounds::verdict(label, &bounded, &open_bounds, policy);
    for line in &lines {
        println!("{line}");
    }
    if failed {
        code = code.max(1);
    }
    if let Some(passed) = part_pass {
        code = code.max(i32::from(!passed) * 2);
        println!(
            "ISO BMFF part {}: {} measured labels, part accepted: {}",
            text(&memory, "part")?,
            selection.as_ref().map_or(0, BTreeSet::len),
            passed
        );
    } else {
        println!(
            "{} cases; slice accepted: {}; accepted: {}; {}",
            expected.len(),
            verdict.get("slice_accepted") == Some(&Value::Bool(true)),
            verdict.get("accepted") == Some(&Value::Bool(true)),
            destination.display()
        );
    }
    Ok(code)
}

#[cfg(test)]
mod tests;
