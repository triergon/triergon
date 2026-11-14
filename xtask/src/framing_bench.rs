//! Packet-framing scripts through the ordinary paired harness.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::harness::report::{self, Options, PrefixFeature};
use crate::{checks, corpus, json, sha256};
use json::Value;

#[path = "../../harness/features/packet-framing/fixtures.rs"]
mod fixtures;

pub const USAGE: &str = "usage: cargo xtask framing [fixtures] [--output DIR]
       [--benchmark] [--memory] [--pairs N] [--iterations N] [--seed N]
       [--baseline-worker FILE] [--require-acceptance | --require-slice-acceptance]
       cargo xtask framing manifest --record

All 131 cases are required. `fixtures` writes their hash-checked bytes.
The default operation uses the shared report, timing, memory and build-identity
checks. A baseline comparison cannot produce an acceptance result.";

fn options(root: &Path, args: &[String]) -> Result<Options, String> {
    let mut options = Options::defaults(root);
    options.contract = root.join("harness/features/packet-framing/contract.json");
    options.manifest = root.join("harness/features/packet-framing/corpus.json");
    options.corpus = root.join(".research/fixtures-packet-framing");
    options.output = root.join(".research/harness-packet-framing");
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let mut value = || rest.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--output" => options.output = PathBuf::from(value()?),
            "--baseline-worker" => options.baseline_worker = Some(PathBuf::from(value()?)),
            "--benchmark" => options.benchmark = true,
            "--memory" => options.memory = true,
            "--require-acceptance" => options.require_acceptance = true,
            "--require-slice-acceptance" => options.require_slice_acceptance = true,
            "--pairs" => {
                options.pairs = value()?.parse().map_err(|_| "--pairs: integer required")?;
            }
            "--iterations" => {
                let n = value()?
                    .parse()
                    .map_err(|_| "--iterations: integer required")?;
                if !(1..=1_000_000).contains(&n) {
                    return Err("--iterations: expected 1..1000000".into());
                }
                options.iterations = Some(n);
            }
            "--seed" => options.seed = value()?.parse().map_err(|_| "--seed: integer required")?,
            _ => return Err(format!("unknown framing option: {flag}\n{USAGE}")),
        }
    }
    if (options.benchmark || options.memory) && options.pairs < 30 {
        return Err("packet framing requires at least 30 pairs".into());
    }
    if options.baseline_worker.is_some()
        && (options.require_acceptance || options.require_slice_acceptance)
    {
        return Err("a baseline comparison is not an acceptance run".into());
    }
    Ok(options)
}

fn generate(output: &Path) -> Result<(), String> {
    let original = fixtures::load()?;
    let extensions = fixtures::load_extensions()?;
    fs::create_dir_all(output).map_err(|e| format!("{}: {e}", output.display()))?;
    for (id, bytes) in original
        .into_iter()
        .map(|case| (case.id, case.bytes))
        .chain(extensions.into_iter().map(|case| (case.id, case.bytes)))
    {
        let path = output.join(format!("{id}.bsf"));
        fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

fn record_extensions(root: &Path) -> Result<(), String> {
    fixtures::load()?;
    let options = options(root, &[])?;
    let feature = PrefixFeature::load(
        root,
        &options.contract,
        &options.manifest,
        Some(&options.corpus),
    )?;
    let (_, reference) = feature.workers(root)?;
    fs::create_dir_all(&options.corpus).map_err(|e| e.to_string())?;
    let mut cases = feature.cases;
    let inputs = fixtures::extension_inputs()?;
    if cases[fixtures::ORIGINAL_CASE_COUNT..].iter().any(|row| {
        !inputs
            .iter()
            .any(|(id, _)| row.get("id").and_then(Value::as_str) == Some(id.as_str()))
    }) {
        return Err("packet framing manifest has an unexpected additional case".into());
    }
    cases.truncate(fixtures::ORIGINAL_CASE_COUNT);
    for (id, bytes) in inputs {
        let file = format!("{id}.bsf");
        let path = options.corpus.join(&file);
        fs::write(&path, &bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        let output = Command::new(&reference)
            .arg("nal-fold")
            .arg(&path)
            .output()
            .map_err(|e| format!("{id}: {e}"))?;
        if !output.status.success() {
            return Err(format!("{id}: reference exited {}", output.status));
        }
        let stdout = std::str::from_utf8(&output.stdout).map_err(|e| format!("{id}: {e}"))?;
        let observed = json::parse_strict(stdout).map_err(|e| format!("{id}: {e}"))?;
        if observed.get("eof") != Some(&Value::Bool(true))
            || observed.get("iterations") != Some(&Value::int(1))
        {
            return Err(format!("{id}: incomplete reference result"));
        }
        let mut work = Vec::new();
        for field in &feature.work_fields {
            let Some(number @ Value::Number(_)) = observed.get(field) else {
                return Err(format!("{id}: missing reference work field {field}"));
            };
            work.push((field.clone(), number.clone()));
        }
        cases.push(Value::object([
            ("id", Value::str(&id)),
            ("profile", Value::str(&id)),
            ("file", Value::str(file)),
            ("sha256", Value::str(sha256::hex(&bytes))),
            ("rights", Value::str("authored-synthetic")),
            ("group", Value::str("avc-profile-extension")),
            ("required", Value::Bool(true)),
            ("expected", Value::Object(work)),
        ]));
    }
    let mut manifest = checks::read_json(&options.manifest)?;
    let Value::Object(entries) = &mut manifest else {
        return Err("packet framing manifest must be an object".into());
    };
    let (_, rows) = entries
        .iter_mut()
        .find(|(key, _)| key == "cases")
        .ok_or("cases missing")?;
    *rows = Value::Array(cases);
    let temporary = options.manifest.with_extension("json.part");
    corpus::write_json(&temporary, &manifest)?;
    fs::rename(&temporary, &options.manifest).map_err(|e| e.to_string())
}

/// Run the full packet-framing contract without a separate measurement engine.
pub fn main(root: &Path, args: &[String]) -> Result<i32, String> {
    if args == ["--help"] {
        println!("{USAGE}");
        return Ok(0);
    }
    if args == ["manifest", "--record"] {
        record_extensions(root)?;
        return Ok(0);
    }
    if args.first().is_some_and(|s| s == "fixtures") {
        let output = match &args[1..] {
            [] => root.join(".research/fixtures-packet-framing"),
            [flag, path] if flag == "--output" => PathBuf::from(path),
            _ => return Err(USAGE.into()),
        };
        generate(&output)?;
        println!("Packet-framing fixtures: {}", output.display());
        return Ok(0);
    }
    let options = options(root, args)?;
    generate(&options.corpus)?;
    report::run(root, &options)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn reference_frames_every_avc_extension_case() {
        let cases = fixtures::load_extensions().unwrap();
        assert_eq!(cases.len(), 6);
        let initial = cases[0].expected;
        assert_eq!(initial.packets, 1);
        assert!(cases[..5].iter().all(|case| case.expected == initial));
        assert_eq!(cases[5].expected.packets, 3);
    }

    #[test]
    fn all_original_inputs_and_independent_work_folds_are_unchanged() {
        let cases = fixtures::load().unwrap();
        assert_eq!(cases.len(), 125);
        assert_eq!(cases.iter().map(|c| c.bytes.len()).sum::<usize>(), 21_227);
        assert_eq!(
            fixtures::MEMBERSHIP_SHA256,
            "fa3bda4fd8e3fe6d2a6ddb082054f11af0537feb6afbe82a8b46266ff680f3f0"
        );
        // Invalid codec packets and initialization failures remain required;
        // an empty work count still contributes configuration/error/close folds.
        assert!(
            cases
                .iter()
                .any(|c| c.once.packets == 0 && c.once.digest != 0)
        );
        assert!(
            cases
                .iter()
                .any(|c| c.expected.digest_b != c.expected.digest)
        );
    }

    #[test]
    fn report_options_keep_all_cases_and_use_the_shared_harness() {
        let root = Path::new("/repository");
        let o = options(
            root,
            &args(&[
                "--benchmark",
                "--memory",
                "--pairs",
                "100",
                "--iterations",
                "50",
                "--seed",
                "281",
                "--output",
                "/results",
                "--require-slice-acceptance",
            ]),
        )
        .unwrap();
        assert!(o.benchmark && o.memory && o.require_slice_acceptance);
        assert_eq!(o.pairs, 100);
        assert_eq!(o.iterations, Some(50));
        assert_eq!(o.seed, 281);
        assert_eq!(o.output, Path::new("/results"));
        assert_eq!(
            o.contract,
            root.join("harness/features/packet-framing/contract.json")
        );
        assert_eq!(
            o.manifest,
            root.join("harness/features/packet-framing/corpus.json")
        );
        for flags in [
            args(&["--case", "avc-width1"]),
            args(&["--manifest", "subset.json"]),
            args(&["--benchmark", "--pairs", "29"]),
            args(&["--iterations", "0"]),
            args(&["--iterations", "1000001"]),
            args(&["--pairs"]),
            args(&["--baseline-worker", "base", "--require-slice-acceptance"]),
        ] {
            assert!(options(root, &flags).is_err(), "{flags:?}");
        }
    }
}
