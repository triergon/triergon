//! Progressive AVC/AAC TS188 sessions through the standard comparison harness.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::harness::report::{self, Options};
use crate::harness::{build, comparison, diff, provenance, reuse, worker};
use crate::json::{self, Value};
use crate::sha256;

pub const USAGE: &str = "usage: cargo xtask progressive [--output DIR]
       [--benchmark] [--memory] [--pairs N] [--iterations 50] [--seed N]
       [--baseline-worker FILE] [--require-acceptance | --require-slice-acceptance] [--full]
       [--no-early-stop]

All five functional inputs (ten literal worker outcomes) must pass first.
The three original comparable cases are required for correctness, runtime and memory.
Timing uses 50 complete sessions per sample and at least 30 paired samples.";

/// The feature's directory; its inputs are under `inputs`.
pub const FEATURE: &str = "harness/features/progressive-ts188";
const OPERATION: &str = "ProgressiveAvcAacTsOwnedFeedV1";
const TIMED: [&str; 3] = ["continuous-44100", "reordered-48000", "later-entry-48000"];
const CONTROLS: [&str; 5] = [
    TIMED[0],
    TIMED[1],
    TIMED[2],
    "duplicate-control-44100",
    "legal-duplicate-44100",
];

fn options(root: &Path, args: &[String]) -> Result<Options, String> {
    let mut options = Options::defaults(root);
    options.contract = root.join(FEATURE).join("contract.json");
    options.manifest = root.join(FEATURE).join("corpus.json");
    options.corpus = root.join(FEATURE).join("inputs");
    options.output = root.join(".research/harness-progressive-ts188");
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let mut value = || rest.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--benchmark" => options.benchmark = true,
            "--memory" => options.memory = true,
            "--full" => options.full = true,
            "--no-early-stop" => options.early_stop = false,
            "--require-acceptance" => options.require_acceptance = true,
            "--require-slice-acceptance" => options.require_slice_acceptance = true,
            "--output" => options.output = PathBuf::from(value()?),
            "--baseline-worker" => options.baseline_worker = Some(PathBuf::from(value()?)),
            "--pairs" => options.pairs = value()?.parse().map_err(|_| "invalid pair count")?,
            "--seed" => options.seed = value()?.parse().map_err(|_| "invalid seed")?,
            "--iterations" => {
                if value()? != "50" {
                    return Err("progressive timing requires 50 complete sessions".into());
                }
                options.iterations = Some(50);
            }
            _ => return Err(format!("unknown progressive option: {flag}\n{USAGE}")),
        }
    }
    if (options.benchmark || options.memory) && options.pairs < 30 {
        return Err("progressive measurements require at least 30 pairs".into());
    }
    if options.baseline_worker.is_some()
        && (options.require_acceptance || options.require_slice_acceptance)
    {
        return Err("a baseline comparison is not an acceptance run".into());
    }
    Ok(options)
}

fn rows(manifest: &Value) -> Result<&[Value], String> {
    manifest
        .get("cases")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing cases".into())
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}"))
}

fn digest(path: &Path) -> Result<String, String> {
    fs::read(path)
        .map(|bytes| sha256::hex(&bytes))
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn validate(root: &Path, options: &Options) -> Result<(), String> {
    let contract = json::read(&options.contract)?;
    let manifest = json::read(&options.manifest)?;
    let controls = json::read(&root.join(FEATURE).join("controls.json"))?;
    let timed = rows(&manifest)?;
    let controls = rows(&controls)?;
    if timed.len() != TIMED.len()
        || controls.len() != CONTROLS.len()
        || contract.get("required_profiles") != Some(&Value::strings(TIMED))
        || text(&contract, "functional_controls")? != "controls.json"
    {
        return Err("three comparable cases and all five functional controls are required".into());
    }
    for (case, id) in controls.iter().zip(CONTROLS) {
        let file = format!("{id}.ts");
        if case.get("id").and_then(Value::as_str) != Some(id)
            || case.get("profile").and_then(Value::as_str) != Some(id)
            || case.get("file").and_then(Value::as_str) != Some(file.as_str())
            || case.get("required") != Some(&Value::Bool(true))
        {
            return Err(format!("progressive membership changed at {id}"));
        }
        let path = root.join(FEATURE).join("inputs").join(file);
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if text(case, "sha256")? != sha256::hex(&bytes)
            || case.get("size") != Some(&Value::int(bytes.len() as u64))
        {
            return Err(format!("progressive input changed: {id}"));
        }
        for role in ["candidate", "reference"] {
            let outcome = case
                .get("outcomes")
                .and_then(|v| v.get(role))
                .ok_or("missing outcome")?;
            let exit = i64::from(id == "legal-duplicate-44100" && role == "reference");
            if outcome.get("exit") != Some(&Value::int(exit))
                || text(outcome, "stdout_sha256")?.len() != 64
                || (exit == 1
                    && outcome.get("stderr_contains")
                        != Some(&Value::strings([
                            "reference stage: read progressive packet",
                            "reference: Invalid data found when processing input (-1094995529)",
                        ])))
            {
                return Err(format!("invalid recorded outcome: {id}/{role}"));
            }
        }
    }
    for (case, control) in timed.iter().zip(controls) {
        for field in [
            "id",
            "profile",
            "file",
            "sha256",
            "size",
            "required",
            "arguments",
            "expected",
        ] {
            if case.get(field) != control.get(field) {
                return Err(format!("timing/control {field} differs"));
            }
        }
    }
    let provenance = json::read(&root.join(FEATURE).join("provenance.json"))?;
    for (field, file) in [
        ("controls_sha256", "controls.json"),
        ("timing_corpus_sha256", "corpus.json"),
    ] {
        if text(&provenance, field)? != digest(&root.join(FEATURE).join(file))? {
            return Err(format!("progressive provenance differs: {file}"));
        }
    }
    Ok(())
}

pub fn main(root: &Path, args: &[String]) -> Result<i32, String> {
    if args == ["--help"] {
        println!("{USAGE}");
        return Ok(0);
    }
    let options = options(root, args)?;
    validate(root, &options)?;
    report::create_output(&options.output)?;
    if options.benchmark || options.memory {
        report::create_output(&options.output.join("parity"))?;
    }
    let documents = ["controls.json", "provenance.json"].map(|name| root.join(FEATURE).join(name));
    // The suite has no grade: each measuring collection grades its own bounds.
    let measure = || {
        let code = collect(root, &options, report::collect, check_controls)?;
        let measured = (options.benchmark || options.memory) && code == 0;
        let failed = measured && crate::harness::bounds::collection_verdict(root, &options)?;
        Ok(code.max(i32::from(failed)))
    };
    match report::gate_key(root, &options, &documents)? {
        Some(key) => reuse::step(root, &options.output, &key, options.full, measure),
        None => measure(),
    }
}

fn collect(
    root: &Path,
    selected: &Options,
    mut run: impl FnMut(&Path, &Options, Option<&Value>) -> Result<i32, String>,
    mut controls: impl FnMut(&Path, &Options) -> Result<Option<Value>, String>,
) -> Result<i32, String> {
    let proof = controls(root, selected);
    let mut parity = options(root, &[])?;
    let measuring = selected.benchmark || selected.memory;
    parity.output = if measuring {
        selected.output.join("parity")
    } else {
        selected.output.clone()
    };
    if !measuring {
        parity.require_acceptance = selected.require_acceptance;
        parity.require_slice_acceptance = selected.require_slice_acceptance;
    }
    let parity_code = run(root, &parity, proof.as_ref().ok().and_then(Option::as_ref))?;
    let Some(proof) = proof? else {
        return Ok(1);
    };
    if measuring && parity_code == 0 {
        run(root, selected, Some(&proof))
    } else {
        Ok(parity_code)
    }
}

struct Context {
    candidate: PathBuf,
    reference: PathBuf,
    binding: Value,
}

// The standard parity report checks the registered worker graphs first. Check
// those same bindings on both sides of the literal controls as well.
fn context(root: &Path, selected: &Options) -> Result<Context, String> {
    validate(root, selected)?;
    let identity_path = root.join("target/harness/identity.json");
    let identity = json::read(&identity_path)?;
    let (candidate, reference, reference_record) = bound_workers(root, &identity)?;
    let binding = proof_binding(root, &identity, &reference_record)?;
    Ok(Context {
        candidate,
        reference,
        binding,
    })
}

fn bound_workers(root: &Path, identity: &Value) -> Result<(PathBuf, PathBuf, Value), String> {
    if let Some(error) = provenance::source_identity_error(root, identity) {
        return Err(error);
    }
    let candidate = build::candidate_worker(root, identity, "playback-worker")?;
    let reference_record = comparison::reference_named(identity, "ffmpeg-formats-native")?;
    let reference = root
        .join(text(&reference_record, "path")?)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if !reference.starts_with(
        root.join("target/harness")
            .canonicalize()
            .map_err(|e| e.to_string())?,
    ) || text(&reference_record, "sha256")? != digest(&reference)?
    {
        return Err("progressive reference changed; rebuild".into());
    }
    Ok((candidate, reference, reference_record))
}

fn proof_binding(root: &Path, identity: &Value, reference: &Value) -> Result<Value, String> {
    let candidate = identity
        .get("additional_candidates")
        .and_then(|value| value.get("playback-worker"))
        .ok_or("playback-worker is not in the build identity")?;
    Ok(Value::object([
        (
            "identity_sha256",
            Value::str(sha256::hex(
                (json::to_python(identity, Some(2)) + "\n").as_bytes(),
            )),
        ),
        ("identity", identity.clone()),
        ("candidate_sha256", Value::str(text(candidate, "sha256")?)),
        ("reference", reference.clone()),
        (
            "contract_sha256",
            Value::str(digest(&root.join(FEATURE).join("contract.json"))?),
        ),
        (
            "corpus_manifest_sha256",
            Value::str(digest(&root.join(FEATURE).join("corpus.json"))?),
        ),
        (
            "controls_sha256",
            Value::str(digest(&root.join(FEATURE).join("controls.json"))?),
        ),
    ]))
}

fn require_same(actual: Option<&Value>, expected: &Value, label: &str) -> Result<(), String> {
    if actual.is_some_and(|actual| diff::differences(actual, expected).is_empty()) {
        Ok(())
    } else {
        Err(format!("functional control {label} mismatch"))
    }
}

fn recorded_cases<'a>(
    proof: &'a Value,
    controls: &Value,
    binding: &Value,
) -> Result<&'a [Value], String> {
    for (field, expected) in [
        ("schema", Value::int(1)),
        ("complete", Value::Bool(true)),
        ("passed", Value::Bool(true)),
        ("context_error", Value::Null),
    ] {
        require_same(proof.get(field), &expected, field)?;
    }
    require_same(proof.get("binding"), binding, "source/build binding")?;
    let recorded = rows(proof)?;
    let declared = rows(controls)?;
    if recorded.len() != CONTROLS.len() || declared.len() != CONTROLS.len() {
        return Err("all five functional controls are required".into());
    }
    for ((case, declared), id) in recorded.iter().zip(declared).zip(CONTROLS) {
        require_same(case.get("id"), &Value::str(id), "case membership")?;
        require_same(
            case.get("input_sha256"),
            declared
                .get("sha256")
                .ok_or("missing declared input hash")?,
            "input hash",
        )?;
        let Some(Value::Object(outcomes)) = case.get("outcomes") else {
            return Err("missing functional control outcomes".into());
        };
        if outcomes.len() != 2 {
            return Err("both functional control worker outcomes are required".into());
        }
        for role in ["candidate", "reference"] {
            let outcome = case
                .get("outcomes")
                .and_then(|value| value.get(role))
                .ok_or("missing functional control worker outcome")?;
            let expected = declared
                .get("outcomes")
                .and_then(|value| value.get(role))
                .ok_or("missing declared worker outcome")?;
            for (field, expected) in [
                ("passed", &Value::Bool(true)),
                ("error", &Value::Null),
                ("expected", expected),
                ("exit", expected.get("exit").ok_or("missing declared exit")?),
                (
                    "stdout_sha256",
                    expected
                        .get("stdout_sha256")
                        .ok_or("missing declared stdout hash")?,
                ),
            ] {
                require_same(
                    outcome.get(field),
                    expected,
                    &format!("{id}/{role}/{field}"),
                )?;
            }
        }
    }
    Ok(recorded)
}

fn artifact_path(root: &Path, report_path: &Path, encoded: &str) -> PathBuf {
    let path = Path::new(encoded);
    if path.is_absolute() || root.join(path).is_file() {
        root.join(path)
    } else {
        report_path.parent().unwrap_or(root).join(path)
    }
}

fn check_artifacts(
    root: &Path,
    proof_path: &Path,
    recorded: &[Value],
    controls: &Value,
) -> Result<(), String> {
    for (case, declared) in recorded.iter().zip(rows(controls)?) {
        for role in ["candidate", "reference"] {
            let outcome = case
                .get("outcomes")
                .and_then(|value| value.get(role))
                .ok_or("missing outcome")?;
            let read = |field| -> Result<Vec<u8>, String> {
                let path = artifact_path(root, proof_path, text(outcome, field)?);
                let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                if text(outcome, &format!("{field}_sha256"))? != sha256::hex(&bytes) {
                    return Err(format!(
                        "{}: functional control {field} hash mismatch",
                        path.display()
                    ));
                }
                Ok(bytes)
            };
            let code = match outcome.get("exit") {
                Some(Value::Number(value)) => {
                    value.as_i128().and_then(|value| i64::try_from(value).ok())
                }
                _ => None,
            }
            .ok_or("functional control exit must be an integer")?;
            let finished = worker::Finished {
                code,
                stdout: read("stdout")?,
                stderr: read("stderr")?,
            };
            let expected = declared
                .get("outcomes")
                .and_then(|value| value.get(role))
                .ok_or("missing declared outcome")?;
            check_trace(declared, expected, &finished)?;
        }
    }
    Ok(())
}

/// Verify a progressive collection's own five-case proof before reusing its data.
/// Timing and memory may have different kernel proof pins, so neither uses the
/// other report's identity or the current identity file to attest its controls.
pub(crate) fn validate_report_controls(
    root: &Path,
    report_path: &Path,
    report: &Value,
    contract: &Value,
    manifest: &Value,
) -> Result<(), String> {
    if contract.get("functional_controls").is_none()
        && contract.get("id").and_then(Value::as_str) != Some("progressive-ts188")
    {
        return Ok(());
    }
    let selected = options(root, &[])?;
    validate(root, &selected)?;
    require_same(
        Some(contract),
        &json::read(&selected.contract)?,
        "registered contract",
    )?;
    require_same(
        Some(manifest),
        &json::read(&selected.manifest)?,
        "registered manifest",
    )?;
    let attachment = report
        .get("functional_controls")
        .ok_or("missing functional controls")?;
    require_same(
        attachment.get("passed"),
        &Value::Bool(true),
        "attachment status",
    )?;
    let path = artifact_path(root, report_path, text(attachment, "path")?);
    let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if text(attachment, "sha256")? != sha256::hex(&bytes) {
        return Err("functional control proof hash mismatch".into());
    }
    let proof = json::parse_strict(std::str::from_utf8(&bytes).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let identity = report.get("identity").ok_or("missing report identity")?;
    let reference = comparison::reference_named(identity, "ffmpeg-formats-native")?;
    let binding = proof_binding(root, identity, &reference)?;
    for key in ["contract_sha256", "corpus_manifest_sha256"] {
        require_same(
            report.get(key),
            binding.get(key).ok_or("missing proof binding")?,
            key,
        )?;
    }
    let controls = json::read(&root.join(FEATURE).join("controls.json"))?;
    let recorded = recorded_cases(&proof, &controls, &binding)?;
    check_artifacts(root, &path, recorded, &controls)?;
    bound_workers(root, identity)?;
    Ok(())
}

fn check_trace(case: &Value, expected: &Value, finished: &worker::Finished) -> Result<(), String> {
    if expected.get("exit") != Some(&Value::int(finished.code)) {
        return Err("exit differs from recorded outcome".into());
    }
    if text(expected, "stdout_sha256")? != sha256::hex(&finished.stdout) {
        return Err("literal stdout differs from recorded outcome".into());
    }
    if finished.code == 0 {
        let line = finished
            .stdout
            .strip_suffix(b"\n")
            .ok_or("missing trace footer newline")?
            .rsplit(|byte| *byte == b'\n')
            .next()
            .ok_or("missing trace footer")?;
        let footer = json::parse_strict(std::str::from_utf8(line).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        let work = case.get("expected").ok_or("missing normative work")?;
        let mut fields = vec![
            ("schema", Value::int(1)),
            ("operation", Value::str(OPERATION)),
            ("iterations", Value::int(1)),
            ("elapsed_ns", Value::int(0)),
            ("eof", Value::Bool(true)),
        ];
        for field in ["packets", "bytes", "digest"] {
            fields.push((field, work.get(field).ok_or("missing work field")?.clone()));
        }
        if !diff::differences(&footer, &Value::object(fields)).is_empty() {
            return Err("trace footer differs from normative work".into());
        }
    } else {
        let stderr = std::str::from_utf8(&finished.stderr).map_err(|e| e.to_string())?;
        let diagnostics = expected
            .get("stderr_contains")
            .and_then(Value::as_array)
            .ok_or("missing refusal diagnostics")?;
        for diagnostic in diagnostics {
            if !stderr.contains(diagnostic.as_str().ok_or("invalid refusal diagnostic")?) {
                return Err("reference refusal diagnostics differ".into());
            }
        }
    }
    Ok(())
}

fn trace_outcome(
    case: &Value,
    role: &str,
    argv: &[OsString],
    output: &Path,
) -> Result<Value, String> {
    let expected = case
        .get("outcomes")
        .and_then(|v| v.get(role))
        .ok_or("missing outcome")?;
    let mut fields = vec![("expected", expected.clone())];
    let result = match worker::run(argv, &[], worker::DEFAULT_TIMEOUT) {
        Ok(Some(finished)) => {
            for (suffix, bytes) in [("stdout", &finished.stdout), ("stderr", &finished.stderr)] {
                let path = output.with_extension(suffix);
                fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
                fields.push((suffix, Value::str(path.to_string_lossy())));
            }
            fields.extend([
                ("exit", Value::int(finished.code)),
                ("stdout_sha256", Value::str(sha256::hex(&finished.stdout))),
                ("stderr_sha256", Value::str(sha256::hex(&finished.stderr))),
            ]);
            check_trace(case, expected, &finished)
        }
        Ok(None) => Err("trace worker timed out".into()),
        Err(error) => Err(error),
    };
    fields.push(("passed", Value::Bool(result.is_ok())));
    fields.push(("error", result.err().map_or(Value::Null, Value::str)));
    Ok(Value::object(fields))
}

fn check_controls(root: &Path, selected: &Options) -> Result<Option<Value>, String> {
    let output = selected.output.join("controls");
    report::create_output(&output)?;
    let before = context(root, selected)?;
    let manifest = json::read(&root.join(FEATURE).join("controls.json"))?;
    let mut results = Vec::new();
    let mut passed = true;
    for case in rows(&manifest)? {
        let id = text(case, "id")?;
        let input = selected.corpus.join(text(case, "file")?);
        let mut outcomes = Vec::new();
        for (role, binary) in [
            ("candidate", &before.candidate),
            ("reference", &before.reference),
        ] {
            let mut argv = vec![
                binary.as_os_str().to_owned(),
                OsString::from("progressive-trace"),
                input.as_os_str().to_owned(),
            ];
            for argument in case
                .get("arguments")
                .and_then(Value::as_array)
                .ok_or("missing arguments")?
            {
                argv.push(OsString::from(argument.as_str().ok_or("invalid argument")?));
            }
            let result = trace_outcome(case, role, &argv, &output.join(format!("{id}-{role}")))?;
            let pass = result.get("passed") == Some(&Value::Bool(true));
            passed &= pass;
            println!(
                "progressive control {id}/{role}: {}",
                if pass { "pass" } else { "FAIL" }
            );
            outcomes.push((role, result));
        }
        results.push(Value::object([
            ("id", Value::str(id)),
            ("input_sha256", Value::str(digest(&input)?)),
            ("outcomes", Value::object(outcomes)),
        ]));
    }
    let context_error = context(root, selected)
        .and_then(|after| {
            if diff::equal(&before.binding, &after.binding) {
                Ok(())
            } else {
                Err("control identity changed during precheck".into())
            }
        })
        .err();
    passed &= context_error.is_none();
    let report = Value::object([
        ("schema", Value::int(1)),
        ("binding", before.binding),
        ("cases", Value::Array(results)),
        ("complete", Value::Bool(true)),
        (
            "context_error",
            context_error.map_or(Value::Null, Value::str),
        ),
        ("passed", Value::Bool(passed)),
    ]);
    let path = output.join("report.json");
    report::publish_json(&path, &(json::to_python(&report, Some(2)) + "\n"))?;
    if !passed {
        return Ok(None);
    }
    Ok(Some(Value::object([
        (
            "path",
            Value::str(
                path.canonicalize()
                    .map_err(|e| e.to_string())?
                    .to_string_lossy(),
            ),
        ),
        ("sha256", Value::str(digest(&path)?)),
        ("passed", Value::Bool(true)),
    ])))
}

#[cfg(test)]
mod tests;
