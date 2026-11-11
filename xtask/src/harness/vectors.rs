//! Intact external vectors: the mandatory metadata vector qualified through the
//! same gates as the authored corpus, and individual upstream FATE cases run in
//! the pinned source tree. Samples stay in the ignored cache; running them is
//! local testing, not permission to redistribute them.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::acceptance::acceptance;
use super::comparison::{reference_record, reference_summary};
use super::gate_build;
use super::parity::{self, Run};
use super::provenance::source_identity_error;
use super::report::{PrefixFeature, case_arguments, expected_for, host_target};
use super::timing::{Operation, Work, benchmark};
use crate::checks::read_json;
use crate::corpus::write_json;
use crate::json::Value;

type R<T> = Result<T, String>;

fn sha(path: &Path) -> R<String> {
    super::linux::file_sha(path)
}

fn s<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn same(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => super::diff::equal(x, y),
        (None, None) => true,
        _ => false,
    }
}

fn save(path: &Path, value: &Value) -> R<()> {
    let temporary = path.with_extension("tmp");
    write_json(&temporary, value)?;
    fs::rename(&temporary, path).map_err(|e| e.to_string())
}

fn workers(root: &Path) -> R<(PathBuf, PathBuf, Value)> {
    let identity = read_json(&root.join("target/harness/identity.json"))?;
    let (candidate, reference) = (
        root.join("target/release/media-worker"),
        root.join("target/harness/ffmpeg-worker"),
    );
    let binaries = Value::object([
        ("candidate", Value::str(sha(&candidate)?)),
        ("reference", Value::str(sha(&reference)?)),
    ]);
    if !same(identity.get("binaries"), Some(&binaries)) {
        return Err("worker changed since build identity".into());
    }
    if let Some(error) = source_identity_error(root, &identity) {
        return Err(error);
    }
    Ok((candidate, reference, identity))
}

pub struct ExternalOptions {
    pub output: PathBuf,
    pub pairs: usize,
    pub iterations: u32,
    pub benchmark: bool,
    pub memory: bool,
    /// Time and measure against this worker of another build instead of the
    /// reference. Correctness is still compared with the reference.
    pub baseline_worker: Option<PathBuf>,
}

/// Qualify the mandatory external metadata vector; returns the exit code.
pub fn external_vector(root: &Path, options: &ExternalOptions) -> R<i32> {
    let contract_path = root.join("harness/features/matroska-segment-metadata/contract.json");
    let contract = read_json(&contract_path)?;
    let contract_sha = sha(&contract_path)?;
    let timeout = super::worker::contract_timeout(&contract)?;
    let spec = contract
        .get("required_additional_vectors")
        .and_then(Value::as_array)
        .and_then(|v| v.first())
        .cloned()
        .ok_or("contract names no external vector")?;
    let (candidate, reference, identity) = workers(root)?;
    println!(
        "{}",
        gate_build::describe(&identity, &gate_build::pinned_release(root)?)
    );
    let file = s(&spec, "file").unwrap_or_default().to_owned();
    if Path::new(&file)
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err("external vector path escapes the sample cache".into());
    }
    let path = root.join(".research/fate-samples").join(&file);
    if !path.is_file() || Some(sha(&path)?.as_str()) != s(&spec, "sha256") {
        return Err("required original external vector missing or changed".into());
    }
    fs::create_dir_all(&options.output).map_err(|e| e.to_string())?;
    let operation = Value::object([
        ("name", Value::str("container-demux-v1")),
        ("seek_target_ns", Value::Null),
        ("view", Value::str("catalogue-source-and-native-v1")),
    ]);
    let summary = reference_summary(&reference_record(&identity, None)?);
    let run = Run {
        root,
        candidate: &candidate,
        reference: &reference,
        input: &path,
        raw: options.output.join("raw"),
        timeout,
    };
    let result = parity::catalogue_parity(&run, true, None, false)?;
    let mut case = vec![
        ("id", Value::str(&file)),
        ("profile", Value::str(&file)),
        (
            "input_sha256",
            spec.get("sha256").cloned().unwrap_or(Value::Null),
        ),
        ("rights", spec.get("rights").cloned().unwrap_or(Value::Null)),
        ("reference", summary.clone()),
        ("operation", operation.clone()),
        ("parity", result.clone()),
    ];
    let timed = Operation {
        prefix: "catalogue-".into(),
        arguments: vec![],
        candidate_options: vec![],
        work: Work {
            fields: super::comparison::work_fields(&contract)?,
            own: vec![],
        },
        timeout,
    };
    if s(&result, "status") == Some("pass") {
        let comparison = options.baseline_worker.as_ref().unwrap_or(&reference);
        if options.benchmark {
            case.push((
                "timing",
                benchmark(
                    &candidate,
                    comparison,
                    &path,
                    options.pairs,
                    options.iterations,
                    280,
                    &timed,
                )?,
            ));
        }
        if options.memory {
            case.push((
                "memory",
                super::memory::collect(
                    root,
                    super::memory::WorkerPaths {
                        candidate: &candidate,
                        reference: comparison,
                        registered: options.baseline_worker.is_none(),
                    },
                    &path,
                    options.pairs,
                    280,
                    &timed,
                )?,
            ));
        }
    }
    let case = Value::object(case);
    let derived = super::validation::external_contract(&contract, &file)?;
    let target = match host_target().as_str() {
        t @ ("aarch64-apple-darwin"
        | "aarch64-unknown-linux-gnu"
        | "x86_64-unknown-linux-gnu"
        | "armv7-unknown-linux-gnueabihf") => t.to_owned(),
        _ => "unqualified".to_owned(),
    };
    let mut verdict = acceptance(
        root,
        &derived,
        std::slice::from_ref(&case),
        &identity,
        &target,
        &[],
        &[],
    );
    if let Some(error) = source_identity_error(root, &identity)
        && let Value::Object(fields) = &mut verdict
    {
        for (key, value) in fields.iter_mut() {
            match (key.as_str(), value) {
                ("accepted" | "slice_accepted", v) => *v = Value::Bool(false),
                ("reasons" | "slice_reasons", Value::Array(list)) => list.push(Value::str(&error)),
                _ => {}
            }
        }
    }
    let receipt = Value::object([
        ("schema", Value::int(1)),
        ("identity", identity),
        ("contract_sha256", Value::str(contract_sha)),
        ("case", case),
        ("acceptance", verdict.clone()),
        ("target", Value::str(target)),
        ("cpus", gate_build::cpus()),
        (
            "input_path",
            Value::str(
                fs::canonicalize(&path)
                    .unwrap_or(path)
                    .display()
                    .to_string(),
            ),
        ),
        (
            "origin",
            Value::str(format!("https://fate-suite.ffmpeg.org/{file}")),
        ),
        ("redistribution_qualified", Value::Bool(false)),
    ]);
    save(&options.output.join("report.json"), &receipt)?;
    let passed = s(&result, "status") == Some("pass");
    let slice = verdict.get("slice_accepted") == Some(&Value::Bool(true));
    println!(
        "{file} {} numeric gates {}",
        s(&result, "status").unwrap_or_default(),
        if slice { "True" } else { "False" }
    );
    Ok(i32::from(!passed))
}

/// Upstream cases this runner supports: sample, label, whether short reads apply.
pub const FATE_CASES: [(&str, &str, &str, bool); 4] = [
    (
        "fate-matroska-xiph-lacing",
        "mkv/xiph_lacing.mka",
        "xiph",
        true,
    ),
    (
        "fate-matroska-zero-length-block",
        "mkv/zero_length_block.mks",
        "zero-length",
        false,
    ),
    (
        "fate-matroska-zlib-decompression",
        "mkv/subtitle_zlib.mks",
        "zlib",
        true,
    ),
    (
        "fate-matroska-opus-remux",
        "mkv/codec_delay_opus.mkv",
        "opus",
        true,
    ),
];

/// What a FATE run needs from the host; replaced in tests.
pub trait FateHost {
    fn download(&self, url: &str, limit: usize) -> R<Vec<u8>>;
    /// `make -j2 CASE SAMPLES=...` in the pinned source: exit code, stdout, stderr.
    fn make(&self, source: &Path, case: &str, samples: &Path) -> R<(i64, String, String)>;
    fn parity(&self, run: &Run<'_>, sequential: bool) -> R<Value>;
    fn source_error(&self, root: &Path, identity: &Value) -> Option<String>;
}

pub struct NativeFate;

impl FateHost for NativeFate {
    fn download(&self, url: &str, limit: usize) -> R<Vec<u8>> {
        use crate::fetch::Transport;
        crate::fetch::Network.https(url, "triergon-local-fate-check", limit)
    }

    fn make(&self, source: &Path, case: &str, samples: &Path) -> R<(i64, String, String)> {
        let argv = [
            "make".into(),
            "-j2".into(),
            case.into(),
            format!("SAMPLES={}", samples.display()).into(),
        ];
        let finished = super::worker::run_in(&argv, &[], Some(source), Duration::from_secs(180));
        let finished = finished?.ok_or("upstream FATE case timed out after 180 seconds")?;
        Ok((
            finished.code,
            String::from_utf8_lossy(&finished.stdout).into_owned(),
            String::from_utf8_lossy(&finished.stderr).into_owned(),
        ))
    }

    fn parity(&self, run: &Run<'_>, sequential: bool) -> R<Value> {
        parity::parity(run, sequential, None, false, "")
    }

    fn source_error(&self, root: &Path, identity: &Value) -> Option<String> {
        source_identity_error(root, identity)
    }
}

/// Run one supported upstream case (or only its demux comparison); returns the
/// exit code: the upstream status, or 1 when the demux comparison failed.
pub fn fate_run(root: &Path, case: &str, demux_only: bool, host: &dyn FateHost) -> R<i32> {
    let &(_, sample, label, sequential) = FATE_CASES
        .iter()
        .find(|c| c.0 == case)
        .ok_or_else(|| format!("unsupported FATE case {case}"))?;
    let (candidate, reference) = (
        root.join("target/release/media-worker"),
        root.join("target/harness/ffmpeg-worker"),
    );
    if demux_only && !(candidate.is_file() && reference.is_file()) {
        return Err("Demux comparison requires both candidate and reference workers; run cargo xtask build-workers first".into());
    }
    let cache = root.join(".research/reference");
    let build = read_json(&cache.join("build.json"))?;
    let enabled = fs::read_to_string(cache.join("fate-list.txt")).map_err(|e| e.to_string())?;
    if !demux_only && !enabled.split_whitespace().any(|c| c == case) {
        return Err("Required configured FATE case disabled; rebuild the reference".into());
    }
    let path = root.join(".research/fate-samples").join(sample);
    fs::create_dir_all(path.parent().expect("sample directory")).map_err(|e| e.to_string())?;
    if !path.exists() {
        let data = host.download(
            &format!("https://fate-suite.ffmpeg.org/{sample}"),
            8 * 1024 * 1024,
        )?;
        if data.len() > 8 * 1024 * 1024 {
            return Err("FATE sample size limit".into());
        }
        fs::write(&path, data).map_err(|e| e.to_string())?;
    }
    let sample_sha = sha(&path)?;
    let receipt_path = cache.join(format!("fate-{label}.json"));
    if receipt_path.exists()
        && s(&read_json(&receipt_path)?, "sample_sha256") != Some(sample_sha.as_str())
    {
        return Err("FATE input changed since previous run".into());
    }
    let revision = s(&build, "revision").unwrap_or_default().to_owned();
    let source = cache.join(format!("FFmpeg-{revision}"));
    let (code, stdout, stderr) = if demux_only {
        (0, String::new(), String::new())
    } else {
        host.make(
            &source,
            case,
            path.parent().and_then(Path::parent).expect("sample root"),
        )?
    };
    let reference_status = if demux_only {
        "not-run-demux-only"
    } else if code == 0 {
        "pass"
    } else {
        "fail"
    };
    let mut receipt = vec![
        ("schema", Value::int(1)),
        ("case", Value::str(case)),
        ("revision", Value::str(&revision)),
        ("sample", Value::str(sample)),
        ("sample_sha256", Value::str(&sample_sha)),
        (
            "origin",
            Value::str(format!("https://fate-suite.ffmpeg.org/{sample}")),
        ),
        (
            "rights",
            Value::str("public external vector; redistribution unreviewed"),
        ),
        ("reference_status", Value::str(reference_status)),
        ("upstream_case_executed", Value::Bool(!demux_only)),
        ("exit_code", Value::int(code)),
        ("stdout", Value::str(stdout)),
        ("stderr", Value::str(stderr)),
        (
            "candidate_status",
            Value::str("not tested by upstream FATE"),
        ),
        (
            "upstream_ffmpeg_sha256",
            Value::str(sha(&source.join("ffmpeg"))?),
        ),
        ("reference_build", build),
    ];
    let mut demux_status = None;
    if candidate.exists() && reference.exists() {
        let identity = read_json(&root.join("target/harness/identity.json"))?;
        let fresh = || -> R<bool> {
            let binaries = Value::object([
                ("candidate", Value::str(sha(&candidate)?)),
                ("reference", Value::str(sha(&reference)?)),
            ]);
            Ok(host.source_error(root, &identity).is_none()
                && same(identity.get("binaries"), Some(&binaries)))
        };
        if !fresh()? {
            return Err("Stale adapter identity; rebuild before evaluating FATE vectors".into());
        }
        let run = Run {
            root,
            candidate: &candidate,
            reference: &reference,
            input: &path,
            raw: root.join(format!(".research/fate-candidate-{label}")),
            timeout: super::worker::DEFAULT_TIMEOUT,
        };
        let comparison = host.parity(&run, sequential)?;
        let status = s(&comparison, "status").unwrap_or_default().to_owned();
        receipt.push(("candidate_demux_comparison", comparison));
        if let Some(slot) = receipt.iter_mut().find(|(k, _)| *k == "candidate_status") {
            slot.1 = Value::str(format!(
                "demux portion {status}; not complete upstream FATE execution"
            ));
        }
        receipt.push(("adapter_identity", identity.clone()));
        if !fresh()? {
            return Err("Adapter/source changed during FATE comparison".into());
        }
        demux_status = Some(status);
    }
    write_json(&receipt_path, &Value::object(receipt))?;
    println!("{reference_status} {}", receipt_path.display());
    Ok(if code != 0 {
        code as i32
    } else {
        i32::from(demux_status.is_some_and(|s| s != "pass"))
    })
}

/// The SHA-256 of every raw candidate trace, with each parity verdict. A worker
/// that registered features run through a worker operation prefix is traced over
/// every case of those features; any other worker over all recorded Matroska
/// cases, the mandatory external vector and the selected FATE vectors. Two records
/// are equivalent only when every call matches.
pub fn trace_identities(
    root: &Path,
    candidate: &Path,
    output: &Path,
    compare: Option<&Path>,
    corpora: &BTreeMap<String, PathBuf>,
) -> R<i32> {
    let candidate =
        fs::canonicalize(candidate).map_err(|e| format!("{}: {e}", candidate.display()))?;
    let name = candidate
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let features = PrefixFeature::registered(root, &name, corpora)?;
    let identity = read_json(&root.join("target/harness/identity.json"))?;
    let native = root.join("target/harness/ffmpeg-worker");
    let scratch = std::env::temp_dir().join(format!("triergon-traces-{}", std::process::id()));
    let started = std::time::Instant::now();
    super::worker::record_calls(&candidate);
    let mut verdicts = Vec::new();
    let result = if features.is_empty() {
        matroska_traces(
            root,
            &candidate,
            &identity,
            &native,
            &scratch,
            &mut verdicts,
        )
    } else {
        prefix_traces(
            root,
            &candidate,
            &features,
            &identity,
            &scratch,
            &mut verdicts,
        )
    };
    let calls = super::worker::stop_recording();
    let _ = fs::remove_dir_all(&scratch);
    result?;
    let elapsed = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    let record = Value::object([
        ("schema", Value::int(1)),
        ("candidate_sha256", Value::str(sha(&candidate)?)),
        ("verdicts", Value::Object(verdicts.clone())),
        ("calls", Value::Array(calls.clone())),
        ("elapsed_s", Value::float(elapsed)),
    ]);
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(output, crate::json::to_python(&record, Some(1)) + "\n")
        .map_err(|e| e.to_string())?;
    let passed = verdicts
        .iter()
        .filter(|(_, v)| v.as_str() == Some("pass"))
        .count();
    println!(
        "{} cases, {passed} pass, {} candidate traces",
        verdicts.len(),
        calls.len()
    );
    if let Some(compare) = compare {
        let before = read_json(compare)?;
        let earlier = before
            .get("calls")
            .and_then(Value::as_array)
            .unwrap_or_default();
        let changed: Vec<&Value> = earlier
            .iter()
            .zip(&calls)
            .filter(|(a, b)| !super::diff::equal(a, b))
            .filter_map(|(_, b)| b.get("argv"))
            .collect();
        let same_verdicts = before
            .get("verdicts")
            .is_some_and(|v| super::diff::equal(v, &Value::Object(verdicts)));
        if !same_verdicts || earlier.len() != calls.len() || !changed.is_empty() {
            let shown: Vec<String> = changed
                .iter()
                .take(10)
                .map(|a| crate::json::to_python(a, None))
                .collect();
            println!("NOT EQUIVALENT [{}]", shown.join(", "));
            return Ok(1);
        }
        println!("byte-identical to {}", compare.display());
    }
    Ok(0)
}

/// The candidate's traces over the recorded Matroska cases, the mandatory external
/// vector and the selected FATE vectors.
fn matroska_traces(
    root: &Path,
    candidate: &Path,
    identity: &Value,
    native: &Path,
    scratch: &Path,
    verdicts: &mut Vec<(String, Value)>,
) -> R<()> {
    use super::comparison::rule_for;
    use super::parity::Comparison;
    const SLICES: [(&str, &str, &str); 6] = [
        (
            "harness/features/matroska-demux/corpus.json",
            "harness/features/matroska-demux/contract.json",
            ".research/fixtures",
        ),
        (
            "harness/features/matroska-header-removal/corpus.json",
            "harness/features/matroska-header-removal/contract.json",
            ".research/header-fixtures",
        ),
        (
            "harness/features/matroska-zlib/corpus.json",
            "harness/features/matroska-zlib/contract.json",
            ".research/zlib-fixtures",
        ),
        (
            "harness/features/matroska-cues-preroll/corpus.json",
            "harness/features/matroska-cues-preroll/contract.json",
            ".research/seek-fixtures",
        ),
        (
            "harness/features/matroska-colour/corpus.json",
            "harness/features/matroska-colour/contract.json",
            ".research/colour-fixtures",
        ),
        (
            "harness/features/matroska-segment-metadata/corpus.json",
            "harness/features/matroska-segment-metadata/contract.json",
            ".research/catalogue-fixtures",
        ),
    ];
    for (manifest, contract_path, corpus) in SLICES {
        let contract = read_json(&root.join(contract_path))?;
        let id = s(&contract, "id").unwrap_or_default().to_owned();
        let timeout = super::worker::contract_timeout(&contract)?;
        for case in read_json(&root.join(manifest))?
            .get("cases")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            let rule = rule_for(case, identity, Some(&id))?;
            let reference =
                root.join(s(&reference_record(identity, rule)?, "path").unwrap_or_default());
            let path = root.join(corpus).join(s(case, "file").unwrap_or_default());
            if Some(sha(&path)?.as_str()) != s(case, "sha256") {
                return Err(format!("input changed: {}", path.display()));
            }
            let run = Run {
                root,
                candidate,
                reference: &reference,
                input: &path,
                raw: scratch.join(s(case, "id").unwrap_or_default()),
                timeout,
            };
            let comparison = rule.map(|rule| Comparison {
                rule,
                native_reference: native,
            });
            let content = id == "matroska-zlib";
            let result = if id == "matroska-segment-metadata" {
                parity::catalogue_parity(&run, true, comparison.as_ref(), content)?
            } else if id == "matroska-colour" {
                parity::colour_parity(&run, true, comparison.as_ref(), content)?
            } else if case.get("seek_target_ns").is_none_or(|v| *v == Value::Null) {
                parity::parity(&run, true, comparison.as_ref(), content, "")?
            } else {
                parity::seek_parity(&run, true, comparison.as_ref(), content)?
            };
            verdicts.push((
                format!("{id}/{}", s(case, "id").unwrap_or_default()),
                result.get("status").cloned().unwrap_or(Value::Null),
            ));
        }
    }
    let contract =
        read_json(&root.join("harness/features/matroska-segment-metadata/contract.json"))?;
    let hdr = contract
        .get("required_additional_vectors")
        .and_then(Value::as_array)
        .and_then(|v| v.first())
        .cloned()
        .unwrap_or(Value::Null);
    let file = s(&hdr, "file").unwrap_or_default();
    let path = root.join(".research/fate-samples").join(file);
    if Some(sha(&path)?.as_str()) != s(&hdr, "sha256") {
        return Err("HDR vector changed".into());
    }
    let run = Run {
        root,
        candidate,
        reference: native,
        input: &path,
        raw: scratch.join("hdr"),
        timeout: super::worker::contract_timeout(&contract)?,
    };
    verdicts.push((
        format!("hdr/{file}"),
        parity::catalogue_parity(&run, true, None, false)?
            .get("status")
            .cloned()
            .unwrap_or(Value::Null),
    ));
    for (_, sample, _, sequential) in [FATE_CASES[0], FATE_CASES[1], FATE_CASES[2], FATE_CASES[3]] {
        let path = root.join(".research/fate-samples").join(sample);
        let stem = Path::new(sample)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let run = Run {
            root,
            candidate,
            reference: native,
            input: &path,
            raw: scratch.join(stem),
            timeout: super::worker::DEFAULT_TIMEOUT,
        };
        verdicts.push((
            format!("fate/{sample}"),
            parity::parity(&run, sequential, None, false, "")?
                .get("status")
                .cloned()
                .unwrap_or(Value::Null),
        ));
    }
    Ok(())
}

/// The candidate's traces over every case of each feature that runs it through a
/// worker operation prefix.
fn prefix_traces(
    root: &Path,
    candidate: &Path,
    features: &[PrefixFeature],
    identity: &Value,
    scratch: &Path,
    verdicts: &mut Vec<(String, Value)>,
) -> R<()> {
    let target = host_target();
    for feature in features {
        let reference = feature.reference_worker(root, identity)?;
        for case in &feature.cases {
            let id = s(case, "id").unwrap_or_default();
            let path = feature.verified_input(case)?;
            let run = Run {
                root,
                candidate,
                reference: &reference,
                input: &path,
                raw: scratch.join(&feature.id).join(id),
                timeout: feature.timeout,
            };
            let expected = expected_for(case, &target).unwrap_or(&Value::Null);
            let result = parity::feed_parity(
                &run,
                &feature.prefix,
                &case_arguments(case),
                expected,
                &feature.work_fields,
                feature.per_target,
            )?;
            verdicts.push((
                format!("{}/{id}", feature.id),
                result.get("status").cloned().unwrap_or(Value::Null),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn prefix_traces_require_the_reference_and_secondary_input_pins() {
        let root = crate::walk::scratch("prefix-traces");
        let corpus = root.join("relocated");
        fs::create_dir(&corpus).unwrap();
        fs::write(root.join("reference"), b"reference").unwrap();
        fs::write(corpus.join("first"), b"first").unwrap();
        let input = crate::sha256::hex(b"first");
        let secondary = crate::sha256::hex(b"original");
        let identity = |pin: &str| {
            crate::json::parse(&format!(
                r#"{{"comparison_references":{{"reference":{{"path":"reference","sha256":"{pin}"}}}}}}"#,
            ))
            .unwrap()
        };
        for (prefix, inputs, mismatch) in [
            (
                "ts-",
                format!(
                    r#""arguments":["188","1","next"],"entities":[{{"file":"first","sha256":"{input}"}},{{"file":"next","sha256":"{secondary}"}}]"#,
                ),
                "entity-hash-mismatch",
            ),
            (
                "sub-",
                format!(r#""companions":[{{"file":"next","sha256":"{secondary}"}}]"#),
                "companion-hash-mismatch",
            ),
        ] {
            let case = crate::json::parse(&format!(
                r#"{{"id":"case","file":"first","sha256":"{input}",{inputs}}}"#,
            ))
            .unwrap();
            let features = [PrefixFeature {
                id: "feature".into(),
                prefix: prefix.into(),
                candidate: "candidate".into(),
                reference: "reference".into(),
                contract: root.join("contract.json"),
                manifest: root.join("corpus.json"),
                cases: vec![case],
                corpus: corpus.clone(),
                work_fields: vec![],
                timeout: super::super::worker::DEFAULT_TIMEOUT,
                per_target: false,
            }];
            let run = |identity: &Value| {
                prefix_traces(
                    &root,
                    &root.join("candidate"),
                    &features,
                    identity,
                    &root.join("raw"),
                    &mut Vec::new(),
                )
                .unwrap_err()
            };
            fs::write(corpus.join("next"), b"changed").unwrap();
            assert_eq!(
                run(&identity(&"0".repeat(64))),
                "reference changed since build identity; rebuild"
            );
            assert_eq!(
                run(&identity(&crate::sha256::hex(b"reference"))),
                format!("feature/case: {mismatch}")
            );
        }
        assert!(!root.join("raw").exists());
        fs::remove_dir_all(root).unwrap();
    }

    struct Fake {
        parity: Option<&'static str>,
        make: Option<(i64, &'static str)>,
        stale: bool,
        calls: RefCell<Vec<String>>,
    }

    impl FateHost for Fake {
        fn download(&self, _: &str, _: usize) -> R<Vec<u8>> {
            Err("no network in tests".into())
        }

        fn make(&self, _: &Path, case: &str, _: &Path) -> R<(i64, String, String)> {
            self.calls.borrow_mut().push(format!("make {case}"));
            let (code, out) = self.make.ok_or("make not expected")?;
            Ok((code, out.into(), String::new()))
        }

        fn parity(&self, run: &Run<'_>, sequential: bool) -> R<Value> {
            self.calls.borrow_mut().push(format!(
                "parity {} {sequential}",
                run.input.file_name().unwrap().to_string_lossy()
            ));
            Ok(Value::object([(
                "status",
                Value::str(self.parity.ok_or("parity not expected")?),
            )]))
        }

        fn source_error(&self, _: &Path, _: &Value) -> Option<String> {
            self.stale.then(|| "changed-source".to_owned())
        }
    }

    fn fake(parity: Option<&'static str>, make: Option<(i64, &'static str)>) -> Fake {
        Fake {
            parity,
            make,
            stale: false,
            calls: RefCell::new(vec![]),
        }
    }

    fn setup(with_workers: bool) -> PathBuf {
        let root = crate::walk::scratch("fate-run");
        let cache = root.join(".research/reference");
        fs::create_dir_all(cache.join("FFmpeg-frozen-fixture")).unwrap();
        fs::write(
            cache.join("build.json"),
            r#"{"revision": "frozen-fixture"}"#,
        )
        .unwrap();
        fs::write(
            cache.join("fate-list.txt"),
            "fate-matroska-xiph-lacing\nfate-matroska-opus-remux\n",
        )
        .unwrap();
        fs::write(
            cache.join("FFmpeg-frozen-fixture/ffmpeg"),
            "unit-fixture-only-oracle",
        )
        .unwrap();
        fs::create_dir_all(root.join(".research/fate-samples/mkv")).unwrap();
        for name in ["xiph_lacing.mka", "codec_delay_opus.mkv"] {
            fs::write(
                root.join(".research/fate-samples/mkv").join(name),
                "unit-fixture-only-input",
            )
            .unwrap();
        }
        if with_workers {
            fs::create_dir_all(root.join("target/release")).unwrap();
            fs::create_dir_all(root.join("target/harness")).unwrap();
            fs::write(root.join("target/release/media-worker"), "candidate").unwrap();
            fs::write(root.join("target/harness/ffmpeg-worker"), "reference").unwrap();
            let identity = format!(
                r#"{{"binaries": {{"candidate": "{}", "reference": "{}"}}}}"#,
                crate::sha256::hex(b"candidate"),
                crate::sha256::hex(b"reference")
            );
            fs::write(root.join("target/harness/identity.json"), identity).unwrap();
        }
        root
    }

    fn receipt(root: &Path, label: &str) -> Value {
        read_json(&root.join(format!(".research/reference/fate-{label}.json"))).unwrap()
    }

    #[test]
    fn demux_only_needs_both_workers_and_runs_nothing_upstream() {
        let root = setup(false);
        let host = fake(None, None);
        assert!(
            fate_run(&root, "fate-matroska-xiph-lacing", true, &host)
                .unwrap_err()
                .contains("candidate and reference workers")
        );
        assert!(host.calls.borrow().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upstream_only_runs_make_and_records_no_candidate_test() {
        let root = setup(false);
        let host = fake(None, Some((0, "native FATE output")));
        assert_eq!(
            fate_run(&root, "fate-matroska-xiph-lacing", false, &host).unwrap(),
            0
        );
        let r = receipt(&root, "xiph");
        assert_eq!(r.get("upstream_case_executed"), Some(&Value::Bool(true)));
        assert_eq!(
            s(&r, "candidate_status"),
            Some("not tested by upstream FATE")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_demux_comparison_uses_the_original_vector_and_keeps_failures() {
        let root = setup(true);
        let host = fake(Some("pass"), None);
        assert_eq!(
            fate_run(&root, "fate-matroska-opus-remux", true, &host).unwrap(),
            0
        );
        assert_eq!(*host.calls.borrow(), ["parity codec_delay_opus.mkv true"]);
        let r = receipt(&root, "opus");
        assert_eq!(s(&r, "reference_status"), Some("not-run-demux-only"));
        assert!(
            s(&r, "candidate_status")
                .unwrap()
                .contains("not complete upstream FATE execution")
        );
        assert!(
            s(&r, "rights")
                .unwrap()
                .contains("redistribution unreviewed")
        );
        fs::remove_file(root.join(".research/reference/fate-opus.json")).unwrap();
        assert_eq!(
            fate_run(
                &root,
                "fate-matroska-opus-remux",
                true,
                &fake(Some("mismatch"), None)
            )
            .unwrap(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_inputs_stale_workers_and_upstream_failures_fail_closed() {
        let root = setup(true);
        fs::write(
            root.join(".research/reference/fate-xiph.json"),
            r#"{"sample_sha256": "changed-input"}"#,
        )
        .unwrap();
        assert!(
            fate_run(
                &root,
                "fate-matroska-xiph-lacing",
                true,
                &fake(Some("pass"), None)
            )
            .unwrap_err()
            .contains("FATE input changed")
        );
        fs::remove_file(root.join(".research/reference/fate-xiph.json")).unwrap();
        let mut stale = fake(Some("pass"), None);
        stale.stale = true;
        assert!(
            fate_run(&root, "fate-matroska-xiph-lacing", true, &stale)
                .unwrap_err()
                .contains("Stale adapter identity")
        );
        assert!(stale.calls.borrow().is_empty());
        assert_eq!(
            fate_run(
                &root,
                "fate-matroska-xiph-lacing",
                false,
                &fake(Some("pass"), Some((7, "")))
            )
            .unwrap(),
            7
        );
        fs::remove_dir_all(root).unwrap();
    }
}
