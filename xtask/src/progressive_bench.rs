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
    validate(root, &options)?;
    let code = collect(root, &options, report::run, |root, selected| {
        let code = check_controls(root, selected)?;
        bind_controls(selected)?;
        Ok(code)
    })?;
    bind_controls(&options)?;
    Ok(code)
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
    if let Some(error) = provenance::source_identity_error(root, &identity) {
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
        &provenance::matroska_inputs(),
    )?;
    if text(record, "sha256")? != digest(&candidate)?
        || !record
            .get("selected_graph")
            .is_some_and(|value| diff::equal(value, &graph))
    {
        return Err("playback-worker sources or binary changed; rebuild".into());
    }
    let reference_record = comparison::reference_named(&identity, "ffmpeg-formats-native")?;
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
    let binding = Value::object([
        ("identity_sha256", Value::str(digest(&identity_path)?)),
        ("identity", identity),
        ("candidate_sha256", Value::str(digest(&candidate)?)),
        ("reference", reference_record),
        ("contract_sha256", Value::str(digest(&selected.contract)?)),
        (
            "corpus_manifest_sha256",
            Value::str(digest(&selected.manifest)?),
        ),
        (
            "controls_sha256",
            Value::str(digest(&root.join(FEATURE).join("controls.json"))?),
        ),
    ]);
    Ok(Context {
        candidate,
        reference,
        binding,
    })
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
        if !diff::equal(&footer, &Value::object(fields)) {
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

fn bind_controls(selected: &Options) -> Result<(), String> {
    let controls_path = selected.output.join("controls/report.json");
    let controls = read_json(&controls_path)?;
    let binding = controls.get("binding").ok_or("missing control binding")?;
    for path in [
        selected.output.join("report.json"),
        selected.output.join("parity/report.json"),
    ] {
        if !path.is_file() {
            continue;
        }
        let mut report = read_json(&path)?;
        for key in ["identity", "contract_sha256", "corpus_manifest_sha256"] {
            if !report
                .get(key)
                .zip(binding.get(key))
                .is_some_and(|(a, b)| diff::equal(a, b))
            {
                return Err(format!("{key} differs between parity and controls"));
            }
        }
        if let Value::Object(fields) = &mut report {
            if controls.get("passed") != Some(&Value::Bool(true))
                && let Some((_, Value::Object(verdict))) =
                    fields.iter_mut().find(|(key, _)| key == "acceptance")
            {
                for (key, value) in verdict {
                    match key.as_str() {
                        "accepted" | "slice_accepted" => *value = Value::Bool(false),
                        "reasons" | "slice_reasons" => {
                            if let Value::Array(reasons) = value {
                                reasons.push(Value::str(
                                    "mandatory progressive functional controls failed",
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }
            fields.retain(|(key, _)| key != "functional_controls");
            fields.push((
                "functional_controls".to_owned(),
                Value::object([
                    ("path", Value::str(controls_path.to_string_lossy())),
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
        write_json(&path, &report)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn original_three_timing_cases_and_five_functional_controls_keep_the_gates() {
        let options = options(root(), &[]).unwrap();
        validate(root(), &options).unwrap();
        let contract = read_json(&options.contract).unwrap();
        assert_eq!(contract.get("runtime_ratio_max"), Some(&Value::float(1.2)));
        assert_eq!(
            contract.get("memory_ratio_max_exclusive"),
            Some(&Value::float(1.0))
        );
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
