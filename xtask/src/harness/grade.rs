//! Join separate timing and memory collections without changing their evidence.
//! The memory build identity remains current while the existing acceptance
//! evaluator checks the raw samples and the original kernel proof.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use super::acceptance::acceptance_iter;
use super::comparison::reference_summary;
use super::gate_build;
use super::timing::{self, GATE_PAIRS};
use super::validation::{
    self, array, external_contract, field, hash, members, put, require_same, selected_operation,
    text,
};
use crate::checks::read_json;
use crate::json::{self, Value};
use crate::sha256;

pub const USAGE: &str =
    "usage: cargo xtask grade TIMING MEMORY --contract PATH --manifest PATH --output DIR
    [--additional-report EXTERNAL-GRADED-REPORT] [--require-slice-acceptance|--require-acceptance]";

type R<T> = Result<T, String>;

struct Options {
    timing: PathBuf,
    memory: PathBuf,
    contract: PathBuf,
    manifest: PathBuf,
    output: PathBuf,
    additional: Vec<PathBuf>,
    requirement: Option<&'static str>,
}

impl Options {
    fn parse(root: &Path, args: &[String]) -> R<Self> {
        let [timing, memory, rest @ ..] = args else {
            return Err("both timing and memory reports are required".into());
        };
        let (mut contract, mut manifest, mut output) = (None, None, None);
        let mut additional = Vec::new();
        let mut requirement = None;
        let mut rest = rest.iter();
        while let Some(flag) = rest.next() {
            match flag.as_str() {
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
        Ok(Self {
            timing: root.join(timing),
            memory: root.join(memory),
            contract: contract.ok_or("--contract is required")?,
            manifest: manifest.ok_or("--manifest is required")?,
            output: output.ok_or("--output is required")?,
            additional,
            requirement,
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

fn document(path: &Path) -> R<(Value, String)> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value =
        json::parse(std::str::from_utf8(&bytes).map_err(|e| format!("{}: {e}", path.display()))?)
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

fn shape(report: &Value) -> R<Shape> {
    match (report.get("cases"), report.get("case")) {
        (Some(Value::Array(_)), None) => {
            if text(report, "collection_status")? != "complete" {
                return Err("collection is not complete".into());
            }
            Ok(Shape::Corpus)
        }
        (None, Some(Value::Object(_))) => {
            // External-vector reports contain one complete case. Their
            // collection_status field is optional.
            if report.get("collection_status").is_some()
                && text(report, "collection_status")? != "complete"
            {
                return Err("external collection is not complete".into());
            }
            Ok(Shape::External)
        }
        _ => Err("report must contain either cases or one external case".into()),
    }
}

fn rows(report: &Value, shape: Shape) -> R<&[Value]> {
    match shape {
        Shape::Corpus => array(report, "cases"),
        Shape::External => Ok(std::slice::from_ref(field(report, "case")?)),
    }
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
            "cpus",
        ] {
            same_field(timed, memory, key)?;
        }
        same_field(
            timed.get("environment").unwrap_or(&Value::Null),
            memory.get("environment").unwrap_or(&Value::Null),
            "cpus",
        )?;
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
fn aliases(expected: &[Value], actual: &BTreeMap<&str, &Value>) -> R<()> {
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
            || row.get("sampled_as") != Some(alias)
            || row.get("sampled_profile") != prior.get("profile")
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
    if pairs < minimum.max(GATE_PAIRS as i128) || pairs != raw.len() as i128 {
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
        &without(timed, &["timing", "memory"])?,
        &without(measured, &["timing", "memory"])?,
        "case fields/parity",
    )?;
    if super::acceptance::parity_passes(timed) {
        return Err("listed open case now passes; remove it from open_cases".into());
    }
    let mut row = without(timed, &["timing", "memory"])?;
    put(&mut row, "open_case", Value::Bool(true))?;
    Ok(row)
}

fn join_case(
    timed: &Value,
    measured: &Value,
    expected: &Value,
    contract: &Value,
    identity: &Value,
    target: &str,
) -> R<Value> {
    require_same(
        &without(timed, &["timing", "memory"])?,
        &without(measured, &["timing", "memory"])?,
        "case fields/parity",
    )?;
    let workers = selected(timed, expected, contract, identity)?;
    if expected.get("outcome").and_then(Value::as_str) == Some("refusal") {
        if [timed, measured].iter().any(|row| {
            ["timing", "memory"]
                .iter()
                .any(|key| row.get(key).is_some_and(|value| *value != Value::Null))
        }) {
            return Err("a registered refusal cannot contain successful measurements".into());
        }
        return Ok(timed.clone());
    }
    let fields = super::comparison::work_fields(contract)?;
    let clock = field(timed, "timing")?;
    let memory = field(measured, "memory")?;
    if let Some(previous) = timed.get("memory").filter(|value| **value != Value::Null) {
        require_same(previous, memory, "retained memory measurement")?;
    }
    if let Some(previous) = measured
        .get("timing")
        .filter(|value| **value != Value::Null)
    {
        require_same(previous, clock, "retained timing measurement")?;
    }
    let minimum = integer(contract, "minimum_pairs")?;
    let memory_raw = samples(memory, minimum, text(memory, "status")? != "qualified")?;
    if text(clock, "status")? != "measured" || integer(clock, "iterations")? < 1 {
        return Err("timing is not measured".into());
    }
    if !validation::iterations_valid(contract, field(clock, "iterations")?) {
        return Err("iterations do not match the contract".into());
    }
    for measurement in [clock, memory] {
        require_same(
            field(measurement, "input_sha256")?,
            field(timed, "input_sha256")?,
            "measurement input hash",
        )?;
    }
    let linux = target.contains("linux")
        || identity
            .get("host_platform")
            .and_then(|h| h.get("system"))
            .and_then(Value::as_str)
            == Some("Linux");
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
        },
    )
    .map_err(|error| format!("timing: {error}"))?;
    memory_binding(memory, identity, &workers, linux)?;
    // The validator has shown both workers do this work in every pair.
    let timed_output = work(
        field(field(&array(clock, "samples")?[0], "reference")?, "data")?,
        &fields,
    )?;
    let output = work(
        field(field(&memory_raw[0], "reference_heap")?, "work")?,
        &fields,
    )?;
    // Timing folds every iteration into one result; memory folds one operation.
    // Counts scale with iterations. Digests are compared within each phase,
    // whose complete operation, input, workers and parity were bound above.
    for key in ["packets", "bytes"] {
        if integer(&output, key)?.checked_mul(integer(clock, "iterations")?)
            != Some(integer(&timed_output, key)?)
        {
            return Err(format!("timing/memory {key} work mismatch"));
        }
    }
    for sample in memory_raw {
        for role in [
            "candidate_heap",
            "candidate_rss",
            "reference_heap",
            "reference_rss",
        ] {
            require_same(
                &work(field(field(sample, role)?, "work")?, &fields)?,
                &output,
                "memory output",
            )?;
        }
    }
    let parity = field(timed, "parity")?;
    for expected in [
        expected.get("expected"),
        parity.get("expected"),
        parity.get("candidate"),
        parity.get("reference"),
    ]
    .into_iter()
    .flatten()
    {
        require_same(&work(expected, &fields)?, &output, "expected output")?;
    }
    let mut row = timed.clone();
    put(&mut row, "memory", memory.clone())?;
    Ok(row)
}

fn current_workers(root: &Path, identity: &Value, contract: &Value) -> R<()> {
    require_same(
        identity,
        &read_json(&root.join("target/harness/identity.json"))?,
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
        join_case(row, row, &expected[0], binding.contract, identity, target)?;
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
/// that remains open returns 2, and a recorded parity failure returns 1.
pub fn main(root: &Path, args: &[String]) -> R<i32> {
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        println!("{USAGE}");
        return Ok(0);
    }
    let options = Options::parse(root, args)?;
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
    let pinned = gate_build::pinned_release(root)?;
    println!("{}", gate_build::describe(identity, &pinned));
    let refusals = gate_build::refusals(identity, &pinned);
    if options.requirement.is_some() && !refusals.is_empty() {
        return Err(format!("not the gate build: {}", refusals.join("; ")));
    }
    current_workers(root, identity, &contract)?;
    let expected = binding.expected(&timed, kind)?;
    let timed_rows = membership(&expected, rows(&timed, kind)?)?;
    let memory_rows = membership(&expected, rows(&memory, kind)?)?;
    aliases(&expected, &timed_rows)?;
    aliases(&expected, &memory_rows)?;
    let combine = |spec: &Value| -> R<Value> {
        let id = text(spec, "id")?;
        let clock = expanded_case(&timed, &options.timing, timed_rows[id])?;
        let measured = expanded_case(&memory, &options.memory, memory_rows[id])?;
        if open.iter().any(|listed| listed == id) {
            return open_case(&clock, &measured).map_err(|e| format!("{id}: {e}"));
        }
        join_case(&clock, &measured, spec, &contract, identity, target)
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
    validate_additional(&additional, &binding, identity, target)?;
    let derived;
    let selected_contract = if kind == Shape::External {
        derived = external_contract(&contract, text(&expected[0], "id")?)?;
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
    let verdict = acceptance_iter(
        root,
        selected_contract,
        expected.iter().map(|spec| {
            combine(spec).inspect_err(|error| {
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
    // Refuse replacement so an earlier failed grade and its inputs stay intact.
    fs::create_dir_all(&options.output)
        .map_err(|e| format!("{}: {e}", options.output.display()))?;
    let destination = options.output.join("report.json");
    match kind {
        Shape::Corpus => super::report::write_case_report(
            &options.output,
            &report,
            expected.iter().map(combine),
        )?,
        Shape::External => {
            put(&mut report, "case", combine(&expected[0])?)?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
                .map_err(|e| format!("{}: {e}", destination.display()))?;
            file.write_all(json::to_python(&report, Some(2)).as_bytes())
                .map_err(|e| format!("{}: {e}", destination.display()))?;
        }
    }
    let mut code = exit_code(&verdict, rows(&memory, kind)?, options.requirement);
    if !super::report::ratchet(text(&contract, "id")?, rows(&timed, kind)?, &open) {
        code = code.max(1);
    }
    println!(
        "{} cases; slice accepted: {}; accepted: {}; {}",
        expected.len(),
        verdict.get("slice_accepted") == Some(&Value::Bool(true)),
        verdict.get("accepted") == Some(&Value::Bool(true)),
        destination.display()
    );
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::comparison::reference_record;

    #[test]
    fn ordinary_contracts_do_not_require_functional_control_reports() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut ordinary = 0;
        for feature in fs::read_dir(root.join("harness/features")).unwrap() {
            let contract = feature.unwrap().path().join("contract.json");
            if !contract.is_file() {
                continue;
            }
            let contract = read_json(&contract).unwrap();
            if contract.get("functional_controls").is_some() {
                continue;
            }
            control_proofs(
                Path::new("unused"),
                &contract,
                &Value::Null,
                (Path::new("timing"), &Value::Null),
                (Path::new("memory"), &Value::Null),
            )
            .unwrap();
            ordinary += 1;
        }
        assert!(ordinary >= 8);
    }

    #[test]
    fn progressive_grading_requires_functional_proof_before_numeric_gates() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let feature = root.join("harness/features/progressive-ts188");
        let contract = read_json(&feature.join("contract.json")).unwrap();
        let manifest = read_json(&feature.join("corpus.json")).unwrap();
        let error = control_proofs(
            root,
            &contract,
            &manifest,
            (Path::new("timing"), &Value::Null),
            (Path::new("memory"), &Value::Null),
        )
        .unwrap_err();
        assert!(error.contains("timing functional controls: missing functional controls"));
    }

    fn digest(byte: char) -> String {
        std::iter::repeat_n(byte, 64).collect()
    }

    fn object(fields: &[(&str, Value)]) -> Value {
        Value::object(fields.iter().cloned())
    }

    fn change(value: &mut Value, path: &[&str], key: &str, new: Value) {
        let mut node = value;
        for step in path {
            let Value::Object(fields) = node else {
                panic!("object at {step}");
            };
            node = &mut fields.iter_mut().find(|(key, _)| key == step).unwrap().1;
        }
        put(node, key, new).unwrap();
    }

    fn identity() -> Value {
        object(&[
            ("schema", Value::int(2)),
            ("rustc", Value::str("compiler record")),
            ("compiler", Value::str("reference compiler record")),
            ("compiler_command", Value::strings(["cc", "reference.c"])),
            ("candidate_command", Value::strings(["cargo", "build"])),
            ("build_environment", object(&[])),
            ("cargo_configuration", Value::Array(vec![])),
            (
                "selected_graph",
                object(&[("sha256", Value::str(digest('d')))]),
            ),
            ("source_revision", Value::str("reference revision")),
            (
                "reference_build",
                object(&[("configuration", Value::str("reference settings"))]),
            ),
            (
                "binaries",
                object(&[
                    ("candidate", Value::str(digest('a'))),
                    ("reference", Value::str(digest('b'))),
                ]),
            ),
            (
                "instrumentation",
                object(&[
                    ("observer_sha256", Value::str(digest('c'))),
                    ("probe_sha256", Value::str(digest('d'))),
                ]),
            ),
        ])
    }

    // These rows exercise joining only. The memory status is unqualified and
    // there is no observer/kernel proof or runtime interval that could pass a gate.
    struct Fixture {
        timed: Value,
        measured: Value,
        expected: Value,
        contract: Value,
        identity: Value,
    }

    impl Fixture {
        fn new() -> Self {
            let identity = identity();
            let contract = object(&[
                ("id", Value::str("matroska-demux")),
                ("minimum_pairs", Value::int(30)),
            ]);
            let expected = object(&[
                ("id", Value::str("case")),
                ("profile", Value::str("case")),
                ("sha256", Value::str(digest('e'))),
            ]);
            let output = object(&[
                ("packets", Value::int(2)),
                ("bytes", Value::int(40)),
                ("digest", Value::int(7)),
            ]);
            let mut observations = Vec::new();
            let mut observe = |stage: String| {
                observations.push(object(&[
                    ("stage", Value::str(stage)),
                    ("state", object(&[("status", Value::str("not-applicable"))])),
                ]));
            };
            observe("before-timing-session".into());
            for role in ["candidate", "reference"] {
                observe(format!("before-warmup:{role}"));
                observe(format!("after-warmup:{role}"));
            }
            // An idle host's CPU record, valid whether or not Linux evidence is required.
            let cpu = object(&[(
                super::super::acceptance::LINUX_CPU_KEY,
                object(&[
                    ("status", Value::str("recorded")),
                    ("boot_id", Value::str("boot")),
                    ("cpus", Value::strings(["cpu0"])),
                    ("ticks_per_second", Value::int(100)),
                    ("counters", Value::Array(vec![Value::int(0); 8])),
                ]),
            )]);
            let mut timed_raw = Vec::new();
            for pair in 0..30 {
                let mut data = output.clone();
                put(&mut data, "packets", Value::int(400)).unwrap();
                put(&mut data, "bytes", Value::int(8000)).unwrap();
                put(&mut data, "digest", Value::int(19)).unwrap();
                put(&mut data, "iterations", Value::int(200)).unwrap();
                put(&mut data, "elapsed_ns", Value::int(100)).unwrap();
                let worker = object(&[("status", Value::str("ok")), ("data", data)]);
                timed_raw.push(object(&[
                    ("pair", Value::int(pair)),
                    ("order", Value::strings(["candidate", "reference"])),
                    ("candidate", worker.clone()),
                    ("reference", worker),
                    ("cpu_before", cpu.clone()),
                    ("cpu_after", cpu.clone()),
                ]));
                for role in ["candidate", "reference"] {
                    observe(format!("before-pair:{pair}:{role}"));
                    observe(format!("after-pair:{pair}:{role}"));
                }
            }
            let row = object(&[
                ("id", Value::str("case")),
                ("profile", Value::str("case")),
                ("input_sha256", Value::str(digest('e'))),
                (
                    "reference",
                    reference_summary(&reference_record(&identity, None).unwrap()),
                ),
                (
                    "operation",
                    object(&[
                        ("name", Value::str("container-demux-v1")),
                        ("seek_target_ns", Value::Null),
                        ("view", Value::str("source-frames-ffmpeg9-clock-v1")),
                    ]),
                ),
                (
                    "parity",
                    object(&[
                        ("status", Value::str("pass")),
                        ("source_timing_status", Value::str("pass")),
                    ]),
                ),
                (
                    "retained_record",
                    object(&[("source", Value::str("complete"))]),
                ),
            ]);
            let mut timed = row.clone();
            put(
                &mut timed,
                "timing",
                object(&[
                    ("status", Value::str("measured")),
                    ("pairs", Value::int(30)),
                    ("iterations", Value::int(200)),
                    ("seed", Value::int(0)),
                    (
                        "ratio",
                        timing::interval_value(
                            &crate::stats::ratio_interval(&[(100.0, 100.0); 30], 0, 4000).unwrap(),
                        ),
                    ),
                    ("input_sha256", Value::str(digest('e'))),
                    (
                        "worker_hashes",
                        field(&identity, "binaries").unwrap().clone(),
                    ),
                    (
                        "warmups",
                        Value::Array(
                            ["candidate", "reference"]
                                .map(|role| {
                                    object(&[
                                        ("worker", Value::str(role)),
                                        ("result", field(&timed_raw[0], role).unwrap().clone()),
                                    ])
                                })
                                .into(),
                        ),
                    ),
                    ("samples", Value::Array(timed_raw)),
                    ("timing_module_policy", Value::str(timing::MODULE_POLICY)),
                    ("timing_module_observations", Value::Array(observations)),
                    (
                        "rejected_contaminated_attempts",
                        Value::Array(vec![object(&[
                            ("status", Value::str("contaminated")),
                            (
                                "samples",
                                Value::Array(vec![Value::str("raw failed sample")]),
                            ),
                        ])]),
                    ),
                ]),
            )
            .unwrap();
            let memory_sample = Value::object(
                [
                    "candidate_heap",
                    "candidate_rss",
                    "reference_heap",
                    "reference_rss",
                ]
                .map(|key| (key, object(&[("work", output.clone())]))),
            );
            let mut measured = row;
            put(
                &mut measured,
                "memory",
                object(&[
                    ("status", Value::str("observer-unqualified")),
                    ("pairs", Value::int(30)),
                    ("input_sha256", Value::str(digest('e'))),
                    ("samples", Value::Array(vec![memory_sample; 30])),
                    (
                        "certificate",
                        object(&[
                            ("status", Value::str("unqualified")),
                            (
                                "worker_hashes",
                                field(&identity, "binaries").unwrap().clone(),
                            ),
                            ("observer_sha256", Value::str(digest('c'))),
                            ("probe_sha256", Value::str(digest('d'))),
                        ]),
                    ),
                ]),
            )
            .unwrap();
            Self {
                timed,
                measured,
                expected,
                contract,
                identity,
            }
        }

        fn join(&self) -> R<Value> {
            join_case(
                &self.timed,
                &self.measured,
                &self.expected,
                &self.contract,
                &self.identity,
                "unqualified-test",
            )
        }

        fn standard_linux() -> Self {
            let mut fixture = Self::new();
            let mut memory = field(&fixture.measured, "memory").unwrap().clone();
            change(
                &mut memory,
                &["certificate"],
                "platform",
                Value::str("Linux"),
            );
            change(
                &mut memory,
                &["certificate"],
                "kernel_rss_required",
                Value::Bool(false),
            );
            let mut samples = array(&memory, "samples").unwrap().to_vec();
            for sample in &mut samples {
                for role in ["candidate_rss", "reference_rss"] {
                    change(
                        sample,
                        &[role],
                        "observer",
                        object(&[("kernel_rss_requested", Value::int(0))]),
                    );
                }
            }
            put(&mut memory, "samples", Value::Array(samples)).unwrap();
            put(&mut fixture.measured, "memory", memory).unwrap();
            let mut clock = field(&fixture.timed, "timing").unwrap().clone();
            let mut observations = array(&clock, "timing_module_observations")
                .unwrap()
                .to_vec();
            for observation in &mut observations {
                change(
                    observation,
                    &["state"],
                    "status",
                    Value::str("module-absent"),
                );
            }
            put(
                &mut clock,
                "timing_module_observations",
                Value::Array(observations),
            )
            .unwrap();
            put(&mut fixture.timed, "timing", clock).unwrap();
            put(
                &mut fixture.identity,
                "host_platform",
                object(&[("system", Value::str("Linux"))]),
            )
            .unwrap();
            fixture
        }
    }

    #[test]
    fn complete_membership_rejects_missing_duplicate_extra_and_changed_inputs() {
        let fixture = Fixture::new();
        let required = [fixture.expected.clone()];
        assert!(membership(&required, std::slice::from_ref(&fixture.timed)).is_ok());
        assert!(
            membership(&required, &[])
                .unwrap_err()
                .contains("missing case")
        );
        assert!(
            membership(&required, &[fixture.timed.clone(), fixture.timed.clone()])
                .unwrap_err()
                .contains("duplicate")
        );
        let mut extra = fixture.timed.clone();
        put(&mut extra, "id", Value::str("extra")).unwrap();
        assert!(
            membership(&required, &[extra])
                .unwrap_err()
                .contains("unexpected")
        );
        let mut changed = fixture.timed.clone();
        put(&mut changed, "input_sha256", Value::str(digest('f'))).unwrap();
        assert!(
            membership(&required, &[changed])
                .unwrap_err()
                .contains("input hash")
        );
        assert!(
            membership(
                &[fixture.expected.clone(), fixture.expected],
                &[fixture.timed]
            )
            .unwrap_err()
            .contains("duplicate manifest")
        );
    }

    #[test]
    fn every_feature_manifest_keeps_all_cases_including_nonrequired_profiles() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut matroska = 0;
        for (feature, count) in [
            ("matroska-demux", 12),
            ("matroska-header-removal", 4),
            ("matroska-zlib", 10),
            ("matroska-cues-preroll", 8),
            ("matroska-colour", 33),
            ("matroska-segment-metadata", 48),
            ("matroska-startup-seek", 44),
            ("hls-segments", 71),
            ("packet-framing", 125),
            ("isobmff-open-index", 1020),
            ("font-outlines", 53),
            ("font-hinting-none", 159),
            ("font-hinting-modes", 954),
            ("matroska-audio-clocks", 10),
            ("mp4-forward", 39),
            ("ts-program", 35),
            ("raw-annex-b", 7),
            ("mpegts-pes", 97),
            ("subtitle-pgs", 317),
            ("subtitle-dvb", 229),
            ("subtitle-dvd", 223),
            ("subtitle-cc", 205),
            ("subtitle-teletext", 70),
            ("subtitle-pgs-capsules", 229),
            ("subtitle-text", 275),
        ] {
            let (manifest, manifest_sha) = super::super::manifest::read(
                root,
                &root.join(format!("harness/features/{feature}/corpus.json")),
            )
            .unwrap();
            let (contract, contract_sha) =
                document(&root.join(format!("harness/features/{feature}/contract.json"))).unwrap();
            let binding = Binding {
                contract: &contract,
                manifest: &manifest,
                contract_sha: &contract_sha,
                manifest_sha: &manifest_sha,
            };
            binding.registered(root).unwrap();
            let expected = array(&manifest, "cases").unwrap();
            let actual: Vec<_> = expected
                .iter()
                .rev()
                .map(|row| {
                    let mut report = object(&[
                        ("id", row.get("id").unwrap().clone()),
                        ("profile", row.get("profile").unwrap().clone()),
                        ("input_sha256", row.get("sha256").unwrap().clone()),
                    ]);
                    if let Some(entities) = row.get("entities") {
                        put(&mut report, "input_entities", entities.clone()).unwrap();
                    }
                    report
                })
                .collect();
            assert_eq!(
                membership(expected, &actual).unwrap().len(),
                count,
                "{feature}"
            );
            assert!(membership(expected, &actual[1..]).is_err(), "{feature}");
            if let Some(index) = actual
                .iter()
                .position(|row| row.get("input_entities").is_some())
            {
                let mut changed = actual.clone();
                put(&mut changed[index], "input_entities", Value::Array(vec![])).unwrap();
                assert!(membership(expected, &changed).is_err(), "{feature}");
            }
            let mut pilot = manifest.clone();
            put(&mut pilot, "cases", Value::Array(expected[1..].to_vec())).unwrap();
            let pilot_sha = sha256::hex(json::to_python(&pilot, Some(2)).as_bytes());
            let pilot_binding = Binding {
                manifest: &pilot,
                manifest_sha: &pilot_sha,
                ..binding
            };
            assert!(pilot_binding.registered(root).is_err(), "{feature}");
            let mut changed_contract = contract.clone();
            put(&mut changed_contract, "minimum_pairs", Value::int(31)).unwrap();
            let changed_sha = sha256::hex(json::to_python(&changed_contract, Some(2)).as_bytes());
            let changed_binding = Binding {
                contract: &changed_contract,
                contract_sha: &changed_sha,
                manifest: &manifest,
                manifest_sha: &manifest_sha,
            };
            assert!(changed_binding.registered(root).is_err(), "{feature}");
            if feature.starts_with("matroska-")
                && !matches!(feature, "matroska-startup-seek" | "matroska-audio-clocks")
            {
                matroska += count;
            }
        }
        let contract =
            read_json(&root.join("harness/features/matroska-segment-metadata/contract.json"))
                .unwrap();
        assert_eq!(
            matroska
                + array(&contract, "required_additional_vectors")
                    .unwrap()
                    .len(),
            116
        );
    }

    #[test]
    fn aliases_require_registered_identical_operations_and_separate_traces() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let manifest =
            read_json(&root.join("harness/features/isobmff-open-index/corpus.json")).unwrap();
        let cases = array(&manifest, "cases").unwrap();
        let expected = [cases[56].clone(), cases[248].clone()];
        let mut actual: Vec<_> = expected
            .iter()
            .map(|spec| {
                let mut row = spec.clone();
                put(
                    &mut row,
                    "input_sha256",
                    field(spec, "sha256").unwrap().clone(),
                )
                .unwrap();
                for key in ["candidate_trace", "reference_trace"] {
                    put(
                        &mut row,
                        key,
                        object(&[("stdout_sha256", Value::str(digest('1')))]),
                    )
                    .unwrap();
                }
                row
            })
            .collect();
        put(&mut actual[1], "sampled_as", Value::int(56)).unwrap();
        put(
            &mut actual[1],
            "sampled_profile",
            field(&expected[0], "profile").unwrap().clone(),
        )
        .unwrap();
        assert!(aliases(&expected, &membership(&expected, &actual).unwrap()).is_ok());

        let mut altered = actual.clone();
        put(&mut altered[1], "sampled_as", Value::int(55)).unwrap();
        assert!(aliases(&expected, &membership(&expected, &altered).unwrap()).is_err());
        let mut altered = actual.clone();
        change(
            &mut altered[1],
            &["candidate_trace"],
            "stdout_sha256",
            Value::str(digest('2')),
        );
        assert!(aliases(&expected, &membership(&expected, &altered).unwrap()).is_err());
        let mut altered = expected.clone();
        change(
            &mut altered[1],
            &["operation"],
            "arguments",
            Value::strings(["1", "0", "memory", "3"]),
        );
        assert!(aliases(&altered, &membership(&altered, &actual).unwrap()).is_err());
        put(&mut actual[0], "sampled_as", Value::int(0)).unwrap();
        assert!(aliases(&expected, &membership(&expected, &actual).unwrap()).is_err());
    }

    /// The open-case ratchet: a listed failing case is graded on parity alone, a
    /// listed case that passes is refused, a listed id outside the corpus is
    /// refused, and an unlisted failing case still needs its measurements.
    #[test]
    fn open_cases_are_graded_on_parity_and_only_shrink() {
        let mut fixture = Fixture::new();
        let failing = object(&[
            ("status", Value::str("mismatch")),
            ("source_timing_status", Value::str("pass")),
        ]);
        for row in [&mut fixture.timed, &mut fixture.measured] {
            *row = without(row, &["timing", "memory"]).unwrap();
            put(row, "parity", failing.clone()).unwrap();
        }
        let graded = open_case(&fixture.timed, &fixture.measured).unwrap();
        assert_eq!(graded.get("open_case"), Some(&Value::Bool(true)));
        assert_eq!(graded.get("parity"), Some(&failing));
        assert!(graded.get("timing").is_none() && graded.get("memory").is_none());
        assert!(fixture.join().unwrap_err().contains("missing timing"));

        let passing = Fixture::new();
        let error = open_case(&passing.timed, &passing.measured).unwrap_err();
        assert!(error.contains("remove it from open_cases"), "{error}");

        let mut other = fixture.measured.clone();
        put(&mut other, "parity", Value::Null).unwrap();
        assert!(open_case(&fixture.timed, &other).is_err());

        let contract = |id: &str| {
            json::parse_strict(&format!(
                r#"{{"pending_module_requirements": ["reader"], "open_cases": [{{"id": "{id}", "requirement": "reader"}}]}}"#
            ))
            .unwrap()
        };
        let manifest = json::parse_strict(r#"{"cases": [{"id": "a"}]}"#).unwrap();
        assert_eq!(
            super::super::report::open_cases(&contract("a"), &manifest, "t").unwrap(),
            ["a"]
        );
        assert!(super::super::report::open_cases(&contract("gone"), &manifest, "t").is_err());
    }

    #[test]
    fn explicit_refusals_join_without_successful_samples() {
        let mut fixture = Fixture::new();
        put(&mut fixture.expected, "outcome", Value::str("refusal")).unwrap();
        for row in [&mut fixture.timed, &mut fixture.measured] {
            *row = without(row, &["timing", "memory"]).unwrap();
            put(row, "outcome", Value::str("refusal")).unwrap();
        }
        let joined = fixture.join().unwrap();
        assert!(joined.get("timing").is_none());
        assert!(joined.get("memory").is_none());
        put(&mut fixture.measured, "memory", object(&[])).unwrap();
        assert!(fixture.join().unwrap_err().contains("refusal"));
    }

    #[test]
    fn only_an_added_kernel_proof_can_differ_between_builds() {
        let timed = identity();
        let mut memory = timed.clone();
        let pin = object(&[
            ("proof_path", Value::str("original-proof.json")),
            ("proof_sha256", Value::str(digest('f'))),
        ]);
        change(
            &mut memory,
            &["instrumentation"],
            "rss_kernel_bound",
            pin.clone(),
        );
        identities(&timed, &memory).unwrap();
        let mut instrument = memory.clone();
        change(
            &mut instrument,
            &["instrumentation"],
            "observer_sha256",
            Value::str(digest('f')),
        );
        assert!(identities(&timed, &instrument).is_err());
        let mut previous = timed.clone();
        change(&mut previous, &["instrumentation"], "rss_kernel_bound", pin);
        assert!(identities(&previous, &timed).is_err());
        change(
            &mut previous,
            &["instrumentation", "rss_kernel_bound"],
            "proof_sha256",
            Value::str(digest('a')),
        );
        assert!(identities(&previous, &memory).is_err());
    }

    #[test]
    fn source_worker_compiler_environment_and_reference_changes_are_refused() {
        let original = identity();
        for (path, key, replacement) in [
            (vec!["selected_graph"], "sha256", Value::str(digest('f'))),
            (vec!["binaries"], "candidate", Value::str(digest('f'))),
            (vec!["binaries"], "reference", Value::str(digest('f'))),
            (vec![], "rustc", Value::str("different compiler")),
            (
                vec!["build_environment"],
                "RUSTFLAGS",
                Value::str("override"),
            ),
            (vec![], "cargo_configuration", Value::strings(["override"])),
            (
                vec!["reference_build"],
                "configuration",
                Value::str("changed"),
            ),
        ] {
            let mut changed = original.clone();
            change(&mut changed, &path, key, replacement);
            assert!(identities(&original, &changed).is_err(), "{path:?}/{key}");
        }
        let missing = without(&original, &["rustc"]).unwrap();
        assert!(identities(&missing, &missing).is_err());
    }

    #[test]
    fn joining_preserves_all_raw_samples_parity_and_failed_attempts() {
        let fixture = Fixture::new();
        let joined = fixture.join().unwrap();
        assert_eq!(joined.get("timing"), fixture.timed.get("timing"));
        assert_eq!(joined.get("memory"), fixture.measured.get("memory"));
        assert_eq!(joined.get("parity"), fixture.timed.get("parity"));
        assert_eq!(
            joined.get("retained_record"),
            fixture.timed.get("retained_record")
        );
        assert_eq!(
            text(field(&joined, "memory").unwrap(), "status").unwrap(),
            "observer-unqualified"
        );
    }

    #[test]
    fn failed_memory_without_pair_count_is_written_and_cannot_pass() {
        let mut fixture = Fixture::new();
        let mut memory = without(field(&fixture.measured, "memory").unwrap(), &["pairs"]).unwrap();
        put(
            &mut memory,
            "status",
            Value::str("invalid-memory-observation"),
        )
        .unwrap();
        put(&mut fixture.measured, "memory", memory.clone()).unwrap();
        let joined = fixture.join().unwrap();
        assert_eq!(joined.get("memory"), Some(&memory));
        assert!(field(&joined, "memory").unwrap().get("pairs").is_none());
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let verdict = super::super::acceptance::grade_case(
            root,
            &fixture.contract,
            &joined,
            &fixture.identity,
            "unqualified-test",
        );
        assert!(!verdict.memory_pass);
        let acceptance = acceptance_iter(
            root,
            &fixture.contract,
            [Ok(joined.clone())].into_iter(),
            &fixture.identity,
            "unqualified-test",
            &[],
            &[],
        );
        assert_eq!(acceptance.get("accepted"), Some(&Value::Bool(false)));
        assert_eq!(acceptance.get("slice_accepted"), Some(&Value::Bool(false)));
        let output =
            std::env::temp_dir().join(format!("grade-failed-memory-{}", std::process::id()));
        fs::create_dir_all(&output).unwrap();
        super::super::report::write_case_report(
            &output,
            &object(&[("acceptance", acceptance)]),
            [Ok(joined.clone())].into_iter(),
        )
        .unwrap();
        let written = read_json(&output.join("report.json")).unwrap();
        require_same(
            field(&written, "cases").unwrap(),
            &Value::Array(vec![joined]),
            "retained failed cases",
        )
        .unwrap();
        fs::remove_dir_all(output).unwrap();

        for (path, key) in [
            (vec!["memory"], "input_sha256"),
            (vec!["memory", "certificate"], "observer_sha256"),
            (vec!["memory", "certificate", "worker_hashes"], "candidate"),
        ] {
            let mut changed = fixture.measured.clone();
            change(&mut changed, &path, key, Value::str(digest('f')));
            let mut invalid = Fixture::new();
            invalid.measured = changed;
            assert!(invalid.join().is_err(), "{path:?}/{key}");
        }
    }

    #[test]
    fn missing_pair_counts_never_relax_success_or_minimum_sample_guards() {
        let fixture = Fixture::new();
        let mut memory = without(field(&fixture.measured, "memory").unwrap(), &["pairs"]).unwrap();
        assert_eq!(samples(&memory, 30, true).unwrap().len(), 30);
        assert!(samples(&memory, 31, true).is_err());
        assert!(samples(&memory, 30, false).is_err());
        let raw = array(&memory, "samples").unwrap()[..29].to_vec();
        put(&mut memory, "samples", Value::Array(raw)).unwrap();
        assert!(samples(&memory, 30, true).is_err());

        let mut fixture = Fixture::new();
        let mut clock = without(field(&fixture.timed, "timing").unwrap(), &["pairs"]).unwrap();
        assert!(samples(&clock, 30, false).is_err());
        put(&mut clock, "status", Value::str("measured")).unwrap();
        put(&mut fixture.timed, "timing", clock).unwrap();
        assert!(fixture.join().unwrap_err().contains("pairs"));

        let mut fixture = Fixture::new();
        let mut memory = without(field(&fixture.measured, "memory").unwrap(), &["pairs"]).unwrap();
        put(&mut memory, "status", Value::str("qualified")).unwrap();
        put(&mut fixture.measured, "memory", memory).unwrap();
        assert!(fixture.join().unwrap_err().contains("pairs"));
    }

    #[test]
    fn absent_phase_values_allow_separate_collections_to_join() {
        let mut fixture = Fixture::new();
        put(&mut fixture.timed, "memory", Value::Null).unwrap();
        put(&mut fixture.measured, "timing", Value::Null).unwrap();
        assert!(fixture.join().is_ok());
    }

    #[test]
    fn registered_role_proofs_reject_integer_to_float_coercion() {
        let mut fixture = Fixture::new();
        let proof = object(&[
            ("candidate", object(&[("input_bytes", Value::int(747))])),
            ("reference", object(&[("input_bytes", Value::int(747))])),
        ]);
        put(
            &mut fixture.expected,
            "expected_time_outputs",
            proof.clone(),
        )
        .unwrap();
        put(&mut fixture.timed, "expected_time_outputs", proof).unwrap();
        assert!(
            selected(
                &fixture.timed,
                &fixture.expected,
                &fixture.contract,
                &fixture.identity
            )
            .is_ok()
        );
        change(
            &mut fixture.timed,
            &["expected_time_outputs", "reference"],
            "input_bytes",
            Value::float(747.0),
        );
        assert!(
            selected(
                &fixture.timed,
                &fixture.expected,
                &fixture.contract,
                &fixture.identity
            )
            .is_err()
        );
    }

    #[test]
    fn joining_checks_declared_work_fields_without_dropping_the_second_digest() {
        let mut fixture = Fixture::new();
        put(
            &mut fixture.contract,
            "work_fields",
            Value::strings(["digest_b"]),
        )
        .unwrap();
        for (row, section, key, roles, digest) in [
            (
                &mut fixture.timed,
                "timing",
                "data",
                vec!["candidate", "reference"],
                17,
            ),
            (
                &mut fixture.measured,
                "memory",
                "work",
                vec![
                    "candidate_heap",
                    "candidate_rss",
                    "reference_heap",
                    "reference_rss",
                ],
                4,
            ),
        ] {
            let mut samples = array(field(row, section).unwrap(), "samples")
                .unwrap()
                .to_vec();
            for sample in &mut samples {
                for role in &roles {
                    change(sample, &[role, key], "digest_b", Value::int(digest));
                }
            }
            change(row, &[section], "samples", Value::Array(samples));
        }
        let mut warmups = array(field(&fixture.timed, "timing").unwrap(), "warmups")
            .unwrap()
            .to_vec();
        for warmup in &mut warmups {
            change(warmup, &["result", "data"], "digest_b", Value::int(17));
        }
        change(
            &mut fixture.timed,
            &["timing"],
            "warmups",
            Value::Array(warmups),
        );
        put(
            &mut fixture.expected,
            "expected",
            object(&[
                ("packets", Value::int(2)),
                ("bytes", Value::int(40)),
                ("digest", Value::int(7)),
                ("digest_b", Value::int(4)),
            ]),
        )
        .unwrap();
        let joined = fixture.join().unwrap();
        assert_eq!(joined.get("timing"), fixture.timed.get("timing"));
        assert_eq!(joined.get("memory"), fixture.measured.get("memory"));
        let mut samples = array(field(&fixture.measured, "memory").unwrap(), "samples")
            .unwrap()
            .to_vec();
        for sample in &mut samples {
            let Value::Object(entries) = sample else {
                unreachable!()
            };
            for (_, observed) in entries {
                let mut work = field(observed, "work").unwrap().clone();
                let Value::Object(fields) = &mut work else {
                    unreachable!()
                };
                fields.retain(|(k, _)| k != "digest_b");
                put(observed, "work", work).unwrap();
            }
        }
        change(
            &mut fixture.measured,
            &["memory"],
            "samples",
            Value::Array(samples),
        );
        assert!(fixture.join().unwrap_err().contains("digest_b"));
    }

    #[test]
    fn parity_operation_and_worker_selection_must_match_before_joining() {
        for (path, key, value) in [
            (vec!["operation"], "name", Value::str("other operation")),
            (vec!["parity"], "source_timing_status", Value::str("fail")),
            (vec![], "candidate_worker", Value::str("other-worker")),
            (vec![], "reference_worker", Value::str("other-reference")),
            (vec![], "profile", Value::str("other-profile")),
        ] {
            let mut fixture = Fixture::new();
            change(&mut fixture.measured, &path, key, value);
            assert!(fixture.join().unwrap_err().contains("case fields/parity"));
        }
        let mut fixture = Fixture::new();
        for row in [&mut fixture.timed, &mut fixture.measured] {
            change(row, &["operation"], "name", Value::str("other operation"));
        }
        assert!(fixture.join().unwrap_err().contains("contract operation"));
    }

    #[test]
    fn nested_measurement_hashes_and_worker_bindings_cannot_change() {
        for (timed, path, key) in [
            (true, vec!["timing"], "input_sha256"),
            (false, vec!["memory"], "input_sha256"),
            (true, vec!["timing", "worker_hashes"], "candidate"),
            (
                false,
                vec!["memory", "certificate", "worker_hashes"],
                "reference",
            ),
            (false, vec!["memory", "certificate"], "observer_sha256"),
        ] {
            let mut fixture = Fixture::new();
            let row = if timed {
                &mut fixture.timed
            } else {
                &mut fixture.measured
            };
            change(row, &path, key, Value::str(digest('f')));
            assert!(fixture.join().is_err(), "{path:?}/{key}");
        }
    }

    #[test]
    fn matching_but_wrong_workers_are_not_selected_by_rows() {
        let mut fixture = Fixture::new();
        for row in [&mut fixture.timed, &mut fixture.measured] {
            put(row, "candidate_worker", Value::str("other-worker")).unwrap();
        }
        assert!(fixture.join().unwrap_err().contains("candidate worker"));
    }

    #[test]
    fn different_memory_output_and_contract_expected_output_are_refused() {
        let mut fixture = Fixture::new();
        let mut raw = array(field(&fixture.measured, "memory").unwrap(), "samples")
            .unwrap()
            .to_vec();
        change(
            &mut raw[29],
            &["candidate_rss", "work"],
            "digest",
            Value::int(8),
        );
        change(
            &mut fixture.measured,
            &["memory"],
            "samples",
            Value::Array(raw),
        );
        assert!(fixture.join().unwrap_err().contains("memory output"));
        let mut fixture = Fixture::new();
        put(
            &mut fixture.expected,
            "expected",
            object(&[
                ("packets", Value::int(2)),
                ("bytes", Value::int(40)),
                ("digest", Value::int(8)),
            ]),
        )
        .unwrap();
        assert!(fixture.join().unwrap_err().contains("expected output"));
    }

    #[test]
    fn incomplete_raw_samples_and_changed_contract_iterations_are_refused() {
        let mut fixture = Fixture::new();
        change(&mut fixture.measured, &["memory"], "pairs", Value::int(29));
        assert!(fixture.join().unwrap_err().contains("pair count"));
        let mut fixture = Fixture::new();
        put(&mut fixture.contract, "iterations", Value::int(50)).unwrap();
        assert!(fixture.join().unwrap_err().contains("iterations"));
    }

    #[test]
    fn module_observations_cover_warmup_and_every_pair_member() {
        let fixture = Fixture::new();
        let mut clock = field(&fixture.timed, "timing").unwrap().clone();
        let raw = array(&clock, "samples").unwrap().to_vec();
        let mut observations = array(&clock, "timing_module_observations")
            .unwrap()
            .to_vec();
        for observation in &mut observations {
            change(
                observation,
                &["state"],
                "status",
                Value::str("module-absent"),
            );
        }
        put(
            &mut clock,
            "timing_module_observations",
            Value::Array(observations.clone()),
        )
        .unwrap();
        timing::validate_module_absence(&clock, &raw, true).unwrap();
        change(
            &mut observations[64],
            &["state"],
            "status",
            Value::str("module-loaded"),
        );
        put(
            &mut clock,
            "timing_module_observations",
            Value::Array(observations.clone()),
        )
        .unwrap();
        assert!(timing::validate_module_absence(&clock, &raw, true).is_err());
        observations.pop();
        put(
            &mut clock,
            "timing_module_observations",
            Value::Array(observations),
        )
        .unwrap();
        assert!(
            timing::validate_module_absence(&clock, &raw, true)
                .unwrap_err()
                .contains("incomplete")
        );
    }

    #[test]
    fn standard_linux_total_rss_binding_does_not_require_a_kernel_proof() {
        let fixture = Fixture::standard_linux();
        let memory = field(&fixture.measured, "memory").unwrap();
        let workers = field(&fixture.identity, "binaries").unwrap();
        memory_binding(memory, &fixture.identity, workers, true).unwrap();
        let mut without_opt_in = memory.clone();
        put(
            &mut without_opt_in,
            "certificate",
            without(
                field(memory, "certificate").unwrap(),
                &["kernel_rss_required"],
            )
            .unwrap(),
        )
        .unwrap();
        memory_binding(&without_opt_in, &fixture.identity, workers, true).unwrap();
        let row = fixture.join().unwrap();
        assert_eq!(row.get("memory"), Some(memory));
        // A successful binding/join does not qualify this deliberately incomplete
        // observer fixture. The shared acceptance evaluator still grades it.
        assert_eq!(text(memory, "status").unwrap(), "observer-unqualified");
        let mut identity = fixture.identity.clone();
        change(
            &mut identity,
            &["instrumentation"],
            "rss_kernel_bound",
            object(&[("proof_path", Value::str("unused-proof.json"))]),
        );
        memory_binding(memory, &identity, workers, true).unwrap();
    }

    #[test]
    fn standard_linux_total_rss_still_requires_module_absence_during_timing() {
        let mut fixture = Fixture::standard_linux();
        let mut clock = field(&fixture.timed, "timing").unwrap().clone();
        let mut observations = array(&clock, "timing_module_observations")
            .unwrap()
            .to_vec();
        change(
            &mut observations[64],
            &["state"],
            "status",
            Value::str("module-loaded"),
        );
        put(
            &mut clock,
            "timing_module_observations",
            Value::Array(observations),
        )
        .unwrap();
        put(&mut fixture.timed, "timing", clock).unwrap();
        assert!(
            fixture
                .join()
                .unwrap_err()
                .contains("absence during timing")
        );
    }

    #[test]
    fn kernel_rss_observations_cannot_bypass_the_certificate_opt_in() {
        let fixture = Fixture::standard_linux();
        let memory = field(&fixture.measured, "memory").unwrap();
        let workers = field(&fixture.identity, "binaries").unwrap();
        for role in ["candidate_rss", "reference_rss"] {
            let mut changed = memory.clone();
            let mut samples = array(&changed, "samples").unwrap().to_vec();
            change(
                &mut samples[29],
                &[role, "observer"],
                "kernel_rss_requested",
                Value::int(1),
            );
            put(&mut changed, "samples", Value::Array(samples)).unwrap();
            assert!(
                memory_binding(&changed, &fixture.identity, workers, true)
                    .unwrap_err()
                    .contains("kernel RSS proof")
            );
        }
        let mut changed = memory.clone();
        change(
            &mut changed,
            &["certificate"],
            "kernel_rss_required",
            Value::int(1),
        );
        assert!(
            memory_binding(&changed, &fixture.identity, workers, true)
                .unwrap_err()
                .contains("kernel RSS proof")
        );
    }

    #[test]
    fn memory_kernel_pin_must_bind_the_original_proof() {
        let fixture = Fixture::standard_linux();
        let memory = field(&fixture.measured, "memory").unwrap();
        let workers = field(&fixture.identity, "binaries").unwrap();
        let mut memory = memory.clone();
        change(
            &mut memory,
            &["certificate"],
            "kernel_rss_required",
            Value::Bool(true),
        );
        change(
            &mut memory,
            &["certificate"],
            "kernel_rss_certificate",
            object(&[(
                "profile",
                object(&[
                    ("proof_path", Value::str("different-proof.json")),
                    ("proof_sha256", Value::str(digest('f'))),
                ]),
            )]),
        );
        let mut identity = fixture.identity.clone();
        change(
            &mut identity,
            &["instrumentation"],
            "rss_kernel_bound",
            object(&[
                ("proof_path", Value::str("original-proof.json")),
                ("proof_sha256", Value::str(digest('f'))),
            ]),
        );
        assert!(
            memory_binding(&memory, &identity, workers, true)
                .unwrap_err()
                .contains("proof_path")
        );
        change(
            &mut memory,
            &["certificate", "kernel_rss_certificate", "profile"],
            "proof_path",
            Value::str("original-proof.json"),
        );
        change(
            &mut memory,
            &["certificate", "kernel_rss_certificate", "profile"],
            "proof_sha256",
            Value::str(digest('a')),
        );
        assert!(
            memory_binding(&memory, &identity, workers, true)
                .unwrap_err()
                .contains("proof_sha256")
        );
        assert!(memory_binding(&memory, &fixture.identity, workers, true).is_err());
    }

    #[test]
    fn matching_kernel_pin_does_not_replace_the_live_proof() {
        let fixture = Fixture::standard_linux();
        let mut memory = field(&fixture.measured, "memory").unwrap().clone();
        let mut identity = fixture.identity.clone();
        let pin = object(&[
            ("proof_path", Value::str("unavailable-proof.json")),
            ("proof_sha256", Value::str(digest('f'))),
        ]);
        change(
            &mut identity,
            &["instrumentation"],
            "rss_kernel_bound",
            pin.clone(),
        );
        change(
            &mut memory,
            &["certificate"],
            "kernel_rss_required",
            Value::Bool(true),
        );
        change(
            &mut memory,
            &["certificate"],
            "kernel_rss_certificate",
            object(&[("profile", pin)]),
        );
        let mut samples = array(&memory, "samples").unwrap().to_vec();
        for sample in &mut samples {
            for role in ["candidate_rss", "reference_rss"] {
                change(
                    sample,
                    &[role, "observer"],
                    "kernel_rss_requested",
                    Value::int(1),
                );
            }
        }
        put(&mut memory, "samples", Value::Array(samples)).unwrap();
        memory_binding(
            &memory,
            &identity,
            field(&identity, "binaries").unwrap(),
            true,
        )
        .unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let result = super::super::memory::evaluate(
            root,
            array(&memory, "samples").unwrap(),
            30,
            280,
            field(&memory, "certificate").unwrap(),
            None,
        );
        assert_eq!(text(&result, "status").unwrap(), "observer-unqualified");
    }

    #[test]
    fn headers_and_external_reports_bind_contract_manifest_target_and_new_identity() {
        let fixture = Fixture::new();
        let mut contract = fixture.contract.clone();
        put(
            &mut contract,
            "required_additional_vectors",
            Value::Array(vec![object(&[
                ("file", Value::str("external.mkv")),
                ("sha256", Value::str(digest('e'))),
            ])]),
        )
        .unwrap();
        let manifest = object(&[("cases", Value::Array(vec![fixture.expected.clone()]))]);
        let binding = Binding {
            contract: &contract,
            manifest: &manifest,
            contract_sha: &digest('a'),
            manifest_sha: &digest('b'),
        };
        let header = object(&[
            ("schema", Value::int(1)),
            ("identity", fixture.identity.clone()),
            ("target", Value::str("unqualified-test")),
            ("contract_sha256", Value::str(digest('a'))),
            ("contract_id", Value::str("matroska-demux")),
            ("corpus_manifest_sha256", Value::str(digest('b'))),
            ("collection_status", Value::str("complete")),
            ("fate_coverage", Value::Array(vec![])),
            (
                "environment",
                object(&[(
                    "cpus",
                    object(&[("allowed", Value::str("1")), ("online", Value::str("0-1"))]),
                )]),
            ),
            ("cases", Value::Array(vec![fixture.timed])),
        ]);
        assert!(binding.headers(&header, &header).is_ok());
        for (key, value) in [
            ("collection_status", Value::str("partial")),
            ("contract_sha256", Value::str(digest('c'))),
            ("corpus_manifest_sha256", Value::str(digest('c'))),
            ("contract_id", Value::str("other")),
            ("target", Value::str("other")),
            ("fate_coverage", Value::Array(vec![Value::str("changed")])),
        ] {
            let mut changed = header.clone();
            put(&mut changed, key, value).unwrap();
            assert!(binding.headers(&header, &changed).is_err(), "{key}");
        }
        for unchanged in [Value::Bool(false), Value::Null, Value::int(1)] {
            let mut failed = header.clone();
            put(
                &mut failed,
                "summary",
                object(&[("source_and_workers_unchanged", unchanged)]),
            )
            .unwrap();
            assert!(binding.headers(&header, &failed).is_err());
            assert!(binding.headers(&failed, &header).is_err());
        }
        let external = object(&[
            ("id", Value::str("external.mkv")),
            ("profile", Value::str("external.mkv")),
            ("input_sha256", Value::str(digest('e'))),
        ]);
        let mut report = without(
            &header,
            &[
                "cases",
                "collection_status",
                "contract_id",
                "corpus_manifest_sha256",
                "fate_coverage",
            ],
        )
        .unwrap();
        put(&mut report, "case", external.clone()).unwrap();
        put(
            &mut report,
            "cpus",
            field(field(&header, "environment").unwrap(), "cpus")
                .unwrap()
                .clone(),
        )
        .unwrap();
        assert!(binding.headers(&report, &report).is_ok());
        for base in [&header, &report] {
            let parent = if shape(base).unwrap() == Shape::Corpus {
                Some("environment")
            } else {
                None
            };
            for replacement in [
                None,
                Some(Value::Null),
                Some(object(&[("allowed", Value::str("0"))])),
            ] {
                let missing = replacement
                    .as_ref()
                    .is_none_or(|value| *value == Value::Null);
                let mut changed = base.clone();
                let mut record = parent.map_or_else(
                    || changed.clone(),
                    |key| field(&changed, key).unwrap().clone(),
                );
                record = without(&record, &["cpus"]).unwrap();
                if let Some(value) = replacement {
                    put(&mut record, "cpus", value).unwrap();
                }
                match parent {
                    Some(key) => put(&mut changed, key, record).unwrap(),
                    None => changed = record,
                }
                assert!(binding.headers(base, &changed).is_err());
                if missing {
                    assert!(binding.headers(&changed, &changed).is_err());
                }
            }
        }
        let expected = binding.expected(&report, Shape::External).unwrap();
        assert_eq!(
            membership(&expected, rows(&report, Shape::External).unwrap())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            additional_case(&report, &header, &binding).unwrap(),
            &external
        );
        change(
            &mut report,
            &["identity", "selected_graph"],
            "sha256",
            Value::str(digest('f')),
        );
        assert!(additional_case(&report, &header, &binding).is_err());
    }

    #[test]
    fn external_grading_preserves_pending_module_requirements_and_other_fields() {
        let contract = object(&[
            ("required_profiles", Value::strings(["authored"])),
            (
                "required_additional_vectors",
                Value::Array(vec![Value::str("required")]),
            ),
            (
                "pending_module_requirements",
                Value::strings(["FATE pending", "broader scope"]),
            ),
            ("minimum_pairs", Value::int(30)),
        ]);
        let derived = external_contract(&contract, "external.mkv").unwrap();
        assert_eq!(
            without(
                &contract,
                &["required_profiles", "required_additional_vectors"]
            )
            .unwrap(),
            without(
                &derived,
                &["required_profiles", "required_additional_vectors"]
            )
            .unwrap()
        );
        let verdict = object(&[
            ("accepted", Value::Bool(false)),
            ("slice_accepted", Value::Bool(false)),
        ]);
        assert_eq!(exit_code(&verdict, &[], Some("accepted")), 2);
        assert_eq!(exit_code(&verdict, &[], Some("slice_accepted")), 2);
    }

    #[test]
    fn arbitrary_report_paths_and_invalid_cli_are_handled_without_panics() {
        let root = Path::new("/repository");
        let args = [
            "odd timing.name",
            "/elsewhere/memory+data",
            "--contract",
            "contract",
            "--manifest",
            "manifest",
            "--output",
            "out",
        ];
        let args: Vec<_> = args.map(str::to_owned).into();
        let parsed = Options::parse(root, &args).unwrap();
        assert_eq!(parsed.timing, root.join("odd timing.name"));
        assert_eq!(parsed.memory, Path::new("/elsewhere/memory+data"));
        for extra in [
            vec!["--contract"],
            vec!["--output", "--require-acceptance"],
            vec!["--unknown"],
            vec!["--require-acceptance", "--require-slice-acceptance"],
        ] {
            let mut invalid = args.clone();
            invalid.extend(extra.into_iter().map(str::to_owned));
            assert!(Options::parse(root, &invalid).is_err());
        }
        assert!(Options::parse(root, &[]).is_err());
        assert!(document(&root.join("missing report")).is_err());
    }
}
