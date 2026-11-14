//! All 125 packet-framing scripts through the ordinary paired harness.

use std::fs;
use std::path::{Path, PathBuf};

use crate::harness::report::{self, Options};
use crate::{json, sha256};

#[path = "../../harness/features/packet-framing/fixtures.rs"]
mod fixtures;

pub const USAGE: &str = "usage: cargo xtask framing [fixtures] [--output DIR]
       [--benchmark] [--memory] [--pairs N] [--iterations N] [--seed N]
       [--baseline-worker FILE] [--require-acceptance | --require-slice-acceptance] [--full]
       [--no-early-stop]

All 125 cases are required. `fixtures` writes their original, hash-checked bytes.
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
            "--full" => options.full = true,
            "--no-early-stop" => options.early_stop = false,
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

pub(crate) fn generate(output: &Path) -> Result<(), String> {
    // Validate the entire set before writing any file. No input is inferred or
    // encoded by a platform codec; these are the original authored script bytes.
    let cases = fixtures::load()?;
    fs::create_dir_all(output).map_err(|e| format!("{}: {e}", output.display()))?;
    for case in cases {
        let path = output.join(format!("{}.bsf", case.id));
        fs::write(&path, case.bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// Run the full packet-framing contract without a separate measurement engine.
pub fn main(root: &Path, args: &[String]) -> Result<i32, String> {
    if args == ["--help"] {
        println!("{USAGE}");
        return Ok(0);
    }
    if args.first().is_some_and(|s| s == "fixtures") {
        let output = match &args[1..] {
            [] => root.join(".research/fixtures-packet-framing"),
            [flag, path] if flag == "--output" => PathBuf::from(path),
            _ => return Err(USAGE.into()),
        };
        generate(&output)?;
        println!("125 packet-framing fixtures: {}", output.display());
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
