//! Progressive AVC/AAC TS188 sessions through the standard comparison harness.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::checks::read_json;
use crate::corpus::write_json;
use crate::harness::report::{self, Options};
use crate::harness::{comparison, diff, provenance, worker};
use crate::json::{self, Value};
use crate::sha256;

pub const USAGE: &str = "usage: cargo xtask progressive [--output DIR]
       [--benchmark] [--memory] [--pairs N] [--iterations 50] [--seed N]
       [--baseline-worker FILE] [--require-acceptance | --require-slice-acceptance]

All five functional inputs (ten literal worker outcomes) must pass first.
The three original comparable cases are required for correctness, runtime and memory.
Timing uses 50 complete sessions per sample and at least 30 paired samples.";

const FEATURE: &str = "harness/features/progressive-ts188";
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
    let contract = read_json(&options.contract)?;
    let manifest = read_json(&options.manifest)?;
    let controls = read_json(&root.join(FEATURE).join("controls.json"))?;
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
    let provenance = read_json(&root.join(FEATURE).join("provenance.json"))?;
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
    let result: Result<i32, String> = (|| {
        validate(root, &options)?;
        let code = collect(root, &options, report::run, |root, selected| {
            let code = check_controls(root, selected)?;
            bind_controls(root, selected)?;
            Ok(code)
        })?;
        bind_controls(root, &options)?;
        Ok(code)
    })();
    if let Err(error) = &result {
        invalidate_controls(&options, error)?;
    }
    result
}

fn collect(
    root: &Path,
    selected: &Options,
    mut run: impl FnMut(&Path, &Options) -> Result<i32, String>,
    mut controls: impl FnMut(&Path, &Options) -> Result<i32, String>,
) -> Result<i32, String> {
    let measuring = selected.benchmark || selected.memory;
    let mut parity = options(root, &[])?;
    parity.output = if measuring {
        selected.output.join("parity")
    } else {
        selected.output.clone()
    };
    let parity_code = run(root, &parity)?;
    let control_code = controls(root, selected)?;
    if parity_code != 0 || control_code != 0 {
        return Ok(1);
    }
    if measuring || selected.require_acceptance || selected.require_slice_acceptance {
        run(root, selected)
    } else {
        Ok(0)
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
    let identity = read_json(&identity_path)?;
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
    let candidate = root.join("target/release/playback-worker");
    let record = identity
        .get("additional_candidates")
        .and_then(|v| v.get("playback-worker"))
        .ok_or("playback-worker is not in the build identity")?;
    let graph = provenance::selected_graph(
        root,
        &provenance::cargo_metadata(root)?,
        "playback-worker",
        &provenance::shared_inputs(root)?,
    )?;
    if text(record, "sha256")? != digest(&candidate)?
        || !record
            .get("selected_graph")
            .is_some_and(|value| diff::differences(value, &graph).is_empty())
    {
        return Err("playback-worker sources or binary changed; rebuild".into());
    }
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
        &read_json(&selected.contract)?,
        "registered contract",
    )?;
    require_same(
        Some(manifest),
        &read_json(&selected.manifest)?,
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
    let controls = read_json(&root.join(FEATURE).join("controls.json"))?;
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

fn check_controls(root: &Path, selected: &Options) -> Result<i32, String> {
    let before = context(root, selected)?;
    let output = selected.output.join("controls");
    fs::create_dir_all(&output).map_err(|e| e.to_string())?;
    let manifest = read_json(&root.join(FEATURE).join("controls.json"))?;
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
    write_json(&output.join("report.json"), &report)?;
    Ok(i32::from(!passed))
}

fn report_paths(selected: &Options) -> [PathBuf; 2] {
    [
        selected.output.join("report.json"),
        selected.output.join("parity/report.json"),
    ]
}

fn bind_controls(root: &Path, selected: &Options) -> Result<(), String> {
    let result = bind_verified_controls(root, selected);
    if let Err(error) = &result {
        invalidate_controls(selected, error)?;
    }
    result
}

fn bind_verified_controls(root: &Path, selected: &Options) -> Result<(), String> {
    let controls_path = selected.output.join("controls/report.json");
    let controls = read_json(&controls_path)?;
    let contract = read_json(&selected.contract)?;
    let manifest = read_json(&selected.manifest)?;
    for path in report_paths(selected) {
        if !path.is_file() {
            continue;
        }
        let mut report = read_json(&path)?;
        if let Value::Object(fields) = &mut report {
            fields.retain(|(key, _)| key != "functional_controls");
            fields.push((
                "functional_controls".to_owned(),
                Value::object([
                    (
                        "path",
                        Value::str(
                            controls_path
                                .canonicalize()
                                .map_err(|e| e.to_string())?
                                .to_string_lossy(),
                        ),
                    ),
                    ("sha256", Value::str(digest(&controls_path)?)),
                    (
                        "passed",
                        controls
                            .get("passed")
                            .cloned()
                            .unwrap_or(Value::Bool(false)),
                    ),
                ]),
            ));
        }
        validate_report_controls(root, &path, &report, &contract, &manifest)?;
        write_json(&path, &report)?;
    }
    Ok(())
}

fn set(value: &mut Value, key: &str, replacement: Value) -> Result<(), String> {
    let Value::Object(fields) = value else {
        return Err("expected an object".into());
    };
    fields.retain(|(name, _)| name != key);
    fields.push((key.to_owned(), replacement));
    Ok(())
}

fn invalidate_controls(selected: &Options, error: &str) -> Result<(), String> {
    let mut errors = Vec::new();
    for path in report_paths(selected) {
        if !path.is_file() {
            continue;
        }
        let update = || -> Result<(), String> {
            let mut report = read_json(&path)?;
            let mut verdict = report
                .get("acceptance")
                .cloned()
                .unwrap_or(Value::Object(Vec::new()));
            if !matches!(verdict, Value::Object(_)) {
                verdict = Value::Object(Vec::new());
            }
            for key in ["accepted", "slice_accepted"] {
                set(&mut verdict, key, Value::Bool(false))?;
            }
            for key in ["reasons", "slice_reasons"] {
                let mut reasons = verdict
                    .get(key)
                    .and_then(Value::as_array)
                    .unwrap_or_default()
                    .to_vec();
                let reason = Value::str(format!(
                    "mandatory progressive functional controls: {error}"
                ));
                if !reasons.contains(&reason) {
                    reasons.push(reason);
                }
                set(&mut verdict, key, Value::Array(reasons))?;
            }
            set(&mut report, "acceptance", verdict)?;
            let mut controls = report
                .get("functional_controls")
                .cloned()
                .unwrap_or(Value::Object(Vec::new()));
            if !matches!(controls, Value::Object(_)) {
                controls = Value::Object(Vec::new());
            }
            set(&mut controls, "passed", Value::Bool(false))?;
            set(&mut report, "functional_controls", controls)?;
            write_json(&path, &report)
        };
        if let Err(error) = update() {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temporary(PathBuf);

    impl Temporary {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "triergon-control-proof-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Temporary {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn change(value: &mut Value, path: &[&str], replacement: Value) {
        let (key, parents) = path.split_last().unwrap();
        let mut node = value;
        for key in parents {
            node = match node {
                Value::Object(fields) => {
                    &mut fields.iter_mut().find(|(name, _)| name == key).unwrap().1
                }
                Value::Array(values) => &mut values[key.parse::<usize>().unwrap()],
                _ => panic!("invalid test path"),
            };
        }
        set(node, key, replacement).unwrap();
    }

    // Functional-proof metadata only: these fixtures have no timing, memory,
    // observer or native gate evidence. Raw traces are checked separately.
    fn proof_fixture(identity: Value) -> (Value, Value, Value) {
        let controls = read_json(&root().join(FEATURE).join("controls.json")).unwrap();
        let reference = comparison::reference_named(&identity, "ffmpeg-formats-native").unwrap();
        let binding = proof_binding(root(), &identity, &reference).unwrap();
        let cases = rows(&controls)
            .unwrap()
            .iter()
            .map(|case| {
                let outcomes = ["candidate", "reference"].map(|role| {
                    let expected = case.get("outcomes").unwrap().get(role).unwrap();
                    (
                        role,
                        Value::object([
                            ("expected", expected.clone()),
                            ("exit", expected.get("exit").unwrap().clone()),
                            (
                                "stdout_sha256",
                                expected.get("stdout_sha256").unwrap().clone(),
                            ),
                            ("stderr_sha256", Value::str(sha256::hex(b""))),
                            ("stdout", Value::str("trace.stdout")),
                            ("stderr", Value::str("trace.stderr")),
                            ("passed", Value::Bool(true)),
                            ("error", Value::Null),
                        ]),
                    )
                });
                Value::object([
                    ("id", case.get("id").unwrap().clone()),
                    ("input_sha256", case.get("sha256").unwrap().clone()),
                    ("outcomes", Value::object(outcomes)),
                ])
            })
            .collect();
        let proof = Value::object([
            ("schema", Value::int(1)),
            ("binding", binding.clone()),
            ("cases", Value::Array(cases)),
            ("complete", Value::Bool(true)),
            ("passed", Value::Bool(true)),
            ("context_error", Value::Null),
        ]);
        (controls, binding, proof)
    }

    fn proof_identity() -> Value {
        Value::object([
            ("source_revision", Value::str("test source binding")),
            ("instrumentation", Value::Object(Vec::new())),
            (
                "additional_candidates",
                Value::object([(
                    "playback-worker",
                    Value::object([
                        ("sha256", Value::str("a".repeat(64))),
                        ("selected_graph", Value::str("test source graph")),
                    ]),
                )]),
            ),
            (
                "comparison_references",
                Value::object([(
                    "ffmpeg-formats-native",
                    Value::object([
                        ("path", Value::str("target/harness/ffmpeg-formats-worker")),
                        ("sha256", Value::str("b".repeat(64))),
                    ]),
                )]),
            ),
        ])
    }

    fn root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn all_ten_declared_outcomes_and_every_control_are_required() {
        let (controls, binding, proof) = proof_fixture(proof_identity());
        recorded_cases(&proof, &controls, &binding).unwrap();
        for index in 0..5 {
            for role in ["candidate", "reference"] {
                for (field, replacement) in [
                    ("passed", Value::Bool(false)),
                    ("passed", Value::int(1)),
                    ("error", Value::str("worker failed")),
                    ("exit", Value::int(9)),
                    ("stdout_sha256", Value::str("c".repeat(64))),
                    ("expected", Value::Null),
                ] {
                    let mut changed = proof.clone();
                    change(
                        &mut changed,
                        &["cases", &index.to_string(), "outcomes", role, field],
                        replacement,
                    );
                    assert!(
                        recorded_cases(&changed, &controls, &binding).is_err(),
                        "{index}/{role}/{field}"
                    );
                }
            }
            for field in ["id", "input_sha256"] {
                let mut changed = proof.clone();
                change(
                    &mut changed,
                    &["cases", &index.to_string(), field],
                    Value::str("changed"),
                );
                assert!(recorded_cases(&changed, &controls, &binding).is_err());
            }
            let mut changed = proof.clone();
            if let Value::Object(fields) = &mut changed {
                let Value::Array(cases) = &mut fields
                    .iter_mut()
                    .find(|(name, _)| name == "cases")
                    .unwrap()
                    .1
                else {
                    panic!()
                };
                cases.remove(index);
            }
            assert!(recorded_cases(&changed, &controls, &binding).is_err());
        }
        for replacement in [
            Value::Object(Vec::new()),
            Value::object([("candidate", Value::Null)]),
            Value::object([
                ("candidate", Value::Null),
                ("reference", Value::Null),
                ("extra", Value::Null),
            ]),
        ] {
            let mut changed = proof.clone();
            change(&mut changed, &["cases", "4", "outcomes"], replacement);
            assert!(recorded_cases(&changed, &controls, &binding).is_err());
        }
        for (field, replacement) in [
            ("complete", Value::Bool(false)),
            ("passed", Value::Bool(false)),
            ("context_error", Value::str("source changed")),
        ] {
            let mut changed = proof.clone();
            set(&mut changed, field, replacement).unwrap();
            assert!(recorded_cases(&changed, &controls, &binding).is_err());
        }
    }

    #[test]
    fn each_collection_uses_its_own_identity_and_worker_bindings() {
        let identity = proof_identity();
        let (controls, timing_binding, timed) = proof_fixture(identity.clone());
        let mut memory_identity = identity;
        change(
            &mut memory_identity,
            &["instrumentation", "rss_kernel_bound"],
            Value::str("test memory proof pin"),
        );
        let (_, memory_binding, memory) = proof_fixture(memory_identity);
        recorded_cases(&timed, &controls, &timing_binding).unwrap();
        recorded_cases(&memory, &controls, &memory_binding).unwrap();
        assert!(recorded_cases(&timed, &controls, &memory_binding).is_err());
        assert!(recorded_cases(&memory, &controls, &timing_binding).is_err());
        for path in [
            vec!["identity_sha256"],
            vec!["candidate_sha256"],
            vec!["controls_sha256"],
            vec!["contract_sha256"],
            vec!["corpus_manifest_sha256"],
            vec!["reference", "sha256"],
            vec!["identity", "source_revision"],
            vec![
                "identity",
                "additional_candidates",
                "playback-worker",
                "selected_graph",
            ],
        ] {
            let mut changed = timed.clone();
            let mut full = vec!["binding"];
            full.extend(path);
            change(&mut changed, &full, Value::str("changed"));
            assert!(recorded_cases(&changed, &controls, &timing_binding).is_err());
        }
    }

    #[test]
    fn duplicate_or_extra_controls_are_not_a_complete_precheck() {
        let (controls, binding, proof) = proof_fixture(proof_identity());
        let mut duplicate = rows(&proof).unwrap().to_vec();
        duplicate[1] = duplicate[0].clone();
        let mut extra = rows(&proof).unwrap().to_vec();
        extra.push(extra[0].clone());
        for cases in [Vec::new(), duplicate, extra] {
            let mut changed = proof.clone();
            set(&mut changed, "cases", Value::Array(cases)).unwrap();
            assert!(recorded_cases(&changed, &controls, &binding).is_err());
        }
    }

    #[test]
    fn raw_control_files_and_their_declared_hashes_are_rechecked() {
        let directory = Temporary::new();
        let (controls, binding, mut proof) = proof_fixture(proof_identity());
        recorded_cases(&proof, &controls, &binding).unwrap();
        let output = directory.0.join("trace.stdout");
        let errors = directory.0.join("trace.stderr");
        fs::write(&output, b"altered trace\n").unwrap();
        fs::write(&errors, b"").unwrap();
        let recorded = rows(&proof).unwrap();
        assert!(
            check_artifacts(
                &directory.0,
                &directory.0.join("report.json"),
                recorded,
                &controls
            )
            .unwrap_err()
            .contains("hash mismatch")
        );
        change(
            &mut proof,
            &["cases", "0", "outcomes", "candidate", "stdout_sha256"],
            Value::str(sha256::hex(b"altered trace\n")),
        );
        assert!(
            check_artifacts(
                &directory.0,
                &directory.0.join("report.json"),
                rows(&proof).unwrap(),
                &controls
            )
            .unwrap_err()
            .contains("literal stdout")
        );
        fs::remove_file(&output).unwrap();
        assert!(
            check_artifacts(
                &directory.0,
                &directory.0.join("report.json"),
                rows(&proof).unwrap(),
                &controls
            )
            .is_err()
        );
    }

    #[test]
    fn missing_or_rehashed_proof_cannot_leave_an_accepted_report() {
        let directory = Temporary::new();
        let mut selected = options(root(), &[]).unwrap();
        selected.output = directory.0.clone();
        // Untrusted stale flags, with no metric evidence or positive gate.
        let stale = Value::object([(
            "acceptance",
            Value::object([
                ("accepted", Value::Bool(true)),
                ("slice_accepted", Value::Bool(true)),
            ]),
        )]);
        for path in report_paths(&selected) {
            write_json(&path, &stale).unwrap();
        }
        assert!(bind_controls(root(), &selected).is_err());
        for path in report_paths(&selected) {
            let report = read_json(&path).unwrap();
            assert_eq!(
                report.get("acceptance").unwrap().get("accepted"),
                Some(&Value::Bool(false))
            );
            assert_eq!(
                report.get("acceptance").unwrap().get("slice_accepted"),
                Some(&Value::Bool(false))
            );
            assert_eq!(
                report.get("functional_controls").unwrap().get("passed"),
                Some(&Value::Bool(false))
            );
        }
        let path = directory.0.join("proof with spaces.json");
        fs::write(&path, b"{}\n").unwrap();
        let report = Value::object([(
            "functional_controls",
            Value::object([
                ("passed", Value::Bool(true)),
                ("path", Value::str(path.to_string_lossy())),
                ("sha256", Value::str("f".repeat(64))),
            ]),
        )]);
        let contract = read_json(&selected.contract).unwrap();
        let manifest = read_json(&selected.manifest).unwrap();
        assert!(
            validate_report_controls(
                root(),
                &directory.0.join("timing.json"),
                &report,
                &contract,
                &manifest
            )
            .unwrap_err()
            .contains("proof hash mismatch")
        );
        let mut failed = report;
        change(
            &mut failed,
            &["functional_controls", "passed"],
            Value::Bool(false),
        );
        assert!(validate_report_controls(root(), &path, &failed, &contract, &manifest).is_err());
    }

    #[test]
    fn early_entrypoint_failure_revokes_stale_acceptance() {
        let directory = Temporary::new();
        let mut selected = options(&directory.0, &[]).unwrap();
        selected.output = directory.0.join("output");
        write_json(
            &selected.output.join("report.json"),
            &Value::object([(
                "acceptance",
                Value::object([("accepted", Value::Bool(true))]),
            )]),
        )
        .unwrap();
        let args = args(&["--output", selected.output.to_str().unwrap()]);
        assert!(main(&directory.0, &args).is_err());
        assert_eq!(
            read_json(&selected.output.join("report.json"))
                .unwrap()
                .get("acceptance")
                .unwrap()
                .get("accepted"),
            Some(&Value::Bool(false))
        );
    }

    #[test]
    fn original_three_timing_cases_and_five_functional_controls_keep_the_gates() {
        let options = options(root(), &[]).unwrap();
        validate(root(), &options).unwrap();
        let contract = read_json(&options.contract).unwrap();
        for field in [
            "runtime_ratio_max",
            "runtime_optimization_ratio_target",
            "memory_ratio_max_exclusive",
            "runtime_policy",
        ] {
            assert!(
                contract.get(field).is_none(),
                "duplicate policy field: {field}"
            );
        }
        assert_eq!(contract.get("iterations"), Some(&Value::int(50)));
        assert_eq!(contract.get("minimum_pairs"), Some(&Value::int(30)));
        assert_eq!(
            contract.get("required_profiles"),
            Some(&Value::strings(TIMED))
        );
        let controls = read_json(&root().join(FEATURE).join("controls.json")).unwrap();
        assert_eq!(rows(&controls).unwrap().len(), 5);
        let refusal = rows(&controls).unwrap()[4]
            .get("outcomes")
            .unwrap()
            .get("reference")
            .unwrap();
        assert_eq!(refusal.get("exit"), Some(&Value::int(1)));
    }

    #[test]
    fn report_options_cannot_narrow_the_work_or_claim_baseline_acceptance() {
        let options = options(
            root(),
            &args(&["--benchmark", "--memory", "--pairs", "100"]),
        )
        .unwrap();
        assert!(options.benchmark && options.memory);
        assert_eq!(options.pairs, 100);
        for flags in [
            args(&["--case", "continuous-44100"]),
            args(&["--manifest", "subset.json"]),
            args(&["--iterations", "1"]),
            args(&["--memory", "--pairs", "29"]),
            args(&["--baseline-worker", "other", "--require-slice-acceptance"]),
        ] {
            assert!(super::options(root(), &flags).is_err(), "{flags:?}");
        }
    }

    #[test]
    fn a_failed_correctness_report_prevents_timing_and_memory_collection() {
        let selected = options(root(), &args(&["--benchmark", "--memory"])).unwrap();
        let mut calls = Vec::new();
        let mut controls = 0;
        let result = collect(
            root(),
            &selected,
            |_, request| {
                calls.push((request.benchmark, request.memory));
                assert_eq!(request.manifest, selected.manifest);
                assert_eq!(request.output, selected.output.join("parity"));
                assert!(!request.require_acceptance && !request.require_slice_acceptance);
                Ok(1)
            },
            |_, _| {
                controls += 1;
                Ok(0)
            },
        );
        assert_eq!(result.unwrap(), 1);
        assert_eq!(calls, [(false, false)]);
        assert_eq!(controls, 1);
    }

    #[test]
    fn complete_correctness_admits_the_selected_standard_report_unchanged() {
        let selected = options(
            root(),
            &args(&["--benchmark", "--memory", "--pairs", "100"]),
        )
        .unwrap();
        let mut calls = Vec::new();
        let result = collect(
            root(),
            &selected,
            |_, request| {
                calls.push((request.benchmark, request.memory));
                assert_eq!(request.manifest, selected.manifest);
                assert_eq!(request.corpus, selected.corpus);
                if request.benchmark {
                    assert_eq!(request.pairs, 100);
                    assert_eq!(request.output, selected.output);
                }
                Ok(0)
            },
            |_, _| Ok(0),
        );
        assert_eq!(result.unwrap(), 0);
        assert_eq!(calls, [(false, false), (true, true)]);
    }

    #[test]
    fn a_failed_functional_control_prevents_all_measurement() {
        let selected = options(root(), &args(&["--benchmark", "--memory"])).unwrap();
        let mut calls = Vec::new();
        let code = collect(
            root(),
            &selected,
            |_, request| {
                calls.push((request.benchmark, request.memory));
                Ok(0)
            },
            |_, _| Ok(1),
        )
        .unwrap();
        assert_eq!(code, 1);
        assert_eq!(calls, [(false, false)]);
    }

    #[test]
    fn plain_correctness_also_requires_the_functional_precheck() {
        let selected = options(root(), &[]).unwrap();
        let mut calls = 0;
        let mut controls = 0;
        let code = collect(
            root(),
            &selected,
            |_, _| {
                calls += 1;
                Ok(0)
            },
            |_, _| {
                controls += 1;
                Ok(1)
            },
        )
        .unwrap();
        assert_eq!((code, calls, controls), (1, 1, 1));
    }

    #[test]
    fn an_incomplete_control_precheck_cannot_start_measurement() {
        let selected = options(root(), &args(&["--benchmark", "--memory"])).unwrap();
        let mut calls = 0;
        let result = collect(
            root(),
            &selected,
            |_, request| {
                assert!(!request.benchmark && !request.memory);
                calls += 1;
                Ok(0)
            },
            |_, _| Err("control worker timed out or could not start".into()),
        );
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }

    fn refusal() -> (Value, worker::Finished) {
        let manifest = read_json(&root().join(FEATURE).join("controls.json")).unwrap();
        let case = rows(&manifest).unwrap()[4].clone();
        // The real partial trace is checked through the public worker in the CLI.
        let finished = worker::Finished {
            code: 1,
            stdout: b"recorded partial trace\n".to_vec(),
            stderr: b"reference stage: read progressive packet\nreference: Invalid data found when processing input (-1094995529)\n".to_vec(),
        };
        (case, finished)
    }

    #[test]
    fn refusal_requires_its_exit_partial_trace_and_specific_diagnostics() {
        let (case, mut finished) = refusal();
        let mut expected = case
            .get("outcomes")
            .unwrap()
            .get("reference")
            .unwrap()
            .clone();
        if let Value::Object(fields) = &mut expected {
            fields.retain(|(key, _)| key != "stdout_sha256");
            fields.push((
                "stdout_sha256".into(),
                Value::str(sha256::hex(&finished.stdout)),
            ));
        }
        check_trace(&case, &expected, &finished).unwrap();
        for exit in [0, 2, -11] {
            finished.code = exit;
            assert!(check_trace(&case, &expected, &finished).is_err());
        }
        finished.code = 1;
        finished.stderr = b"reference: another error\n".to_vec();
        assert!(check_trace(&case, &expected, &finished).is_err());
        let (_, original) = refusal();
        finished.stderr = original.stderr;
        finished.stdout.push(b'x');
        assert!(check_trace(&case, &expected, &finished).is_err());
    }

    #[test]
    fn successful_literal_trace_also_requires_the_complete_normative_footer() {
        let manifest = read_json(&root().join(FEATURE).join("controls.json")).unwrap();
        let case = &rows(&manifest).unwrap()[0];
        let footer = Value::object([
            ("schema", Value::int(1)),
            ("operation", Value::str(OPERATION)),
            ("iterations", Value::int(1)),
            ("elapsed_ns", Value::int(0)),
            ("eof", Value::Bool(true)),
            ("packets", Value::int(294)),
            ("bytes", Value::int(22945)),
            ("digest", Value::int(1_094_976_622_303_096_385u64)),
        ]);
        let stdout = format!("literal trace\n{}\n", json::to_python(&footer, None)).into_bytes();
        let mut finished = worker::Finished {
            code: 0,
            stdout,
            stderr: Vec::new(),
        };
        let expected = Value::object([
            ("exit", Value::int(0)),
            ("stdout_sha256", Value::str(sha256::hex(&finished.stdout))),
        ]);
        check_trace(case, &expected, &finished).unwrap();
        finished.stdout = b"literal trace\n{\"packets\":294}\n".to_vec();
        let expected = Value::object([
            ("exit", Value::int(0)),
            ("stdout_sha256", Value::str(sha256::hex(&finished.stdout))),
        ]);
        assert!(check_trace(case, &expected, &finished).is_err());
    }
}
