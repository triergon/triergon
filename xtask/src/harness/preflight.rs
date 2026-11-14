//! `cargo xtask preflight`: check everything a gate run needs before anything is
//! measured, and report every problem at once, so a run never measures for hours
//! and then turns out invalid.
//!
//! It checks that the gate build's workers and references match the build
//! identity; that every case of the selected features names a reference the
//! identity holds and has its input, ordered entities and companions on disk
//! with their pinned hashes; that the ISO BMFF suite's and the codec workloads'
//! inputs match their pins; that the RSS module source has a review for this
//! kernel; and that the RSS module is unloaded, as timing needs it, or, with
//! `--kernel`, loaded under a current proof, as kernel-bound memory needs it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{gate_build, linux, validation};
use crate::checks::read_json;
use crate::json::Value;
use crate::sha256;

pub const USAGE: &str = "usage: cargo xtask preflight [--feature ID]... [--corpus ID=DIR]...
       [--manifest ID=FILE]... [--isobmff] [--codecs WORKLOADS]... [--review FILE] [--kernel]

Check, before a gate run measures anything, that the build identity's workers and
references are current and the gate build; that each --feature's cases name known
references and have every input with its pinned hash (in --corpus, else the
manifest's recorded directory, else .research/fixtures; --manifest replaces the
contract's corpus.json); with --isobmff the ISO BMFF suite's inputs and with
--codecs each workload list's inputs; with --review that the review covers this RSS
module source and kernel; and that the RSS module is unloaded, or with --kernel
loaded under a current proof. Prints every check and exits 1 if any fails.";

type R<T> = Result<T, String>;

#[derive(Default)]
struct Options {
    features: Vec<String>,
    corpora: BTreeMap<String, PathBuf>,
    manifests: BTreeMap<String, PathBuf>,
    isobmff: bool,
    codecs: Vec<PathBuf>,
    review: Option<PathBuf>,
    kernel: bool,
}

impl Options {
    fn parse(root: &Path, args: &[String]) -> R<Self> {
        let mut options = Self::default();
        let mut rest = args.iter();
        while let Some(flag) = rest.next() {
            let mut value = || {
                rest.next()
                    .cloned()
                    .ok_or_else(|| format!("{flag} needs a value"))
            };
            let pair = |map: &mut BTreeMap<String, PathBuf>, value: String| -> R<()> {
                let (id, path) = value
                    .split_once('=')
                    .filter(|(id, path)| !id.is_empty() && !path.is_empty())
                    .ok_or_else(|| format!("{flag} needs ID=PATH"))?;
                if map.insert(id.to_owned(), root.join(path)).is_some() {
                    return Err(format!("{flag} given twice for {id}"));
                }
                Ok(())
            };
            match flag.as_str() {
                "--feature" => options.features.push(value()?),
                "--corpus" => pair(&mut options.corpora, value()?)?,
                "--manifest" => pair(&mut options.manifests, value()?)?,
                "--isobmff" => options.isobmff = true,
                "--codecs" => options.codecs.push(root.join(value()?)),
                "--review" => options.review = Some(root.join(value()?)),
                "--kernel" => options.kernel = true,
                _ => return Err(USAGE.to_owned()),
            }
        }
        for id in options.corpora.keys().chain(options.manifests.keys()) {
            if !options.features.contains(id) {
                return Err(format!("{id} is not a selected --feature"));
            }
        }
        Ok(options)
    }
}

/// The outcome of each check, in order.
#[derive(Default)]
struct Checks(Vec<(String, R<String>)>);

impl Checks {
    fn check(&mut self, name: impl Into<String>, result: R<String>) {
        self.0.push((name.into(), result));
    }

    fn report(&self) -> i32 {
        for (name, result) in &self.0 {
            match result {
                Ok(detail) => println!("ok   {name}: {detail}"),
                Err(error) => println!("FAIL {name}: {error}"),
            }
        }
        let failed = self.0.iter().filter(|(_, r)| r.is_err()).count();
        if failed == 0 {
            println!("preflight passed: {} checks", self.0.len());
            0
        } else {
            println!("preflight failed: {failed} of {} checks", self.0.len());
            1
        }
    }
}

pub fn main(root: &Path, args: &[String]) -> R<i32> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return Ok(0);
    }
    let options = Options::parse(root, args)?;
    let mut checks = Checks::default();
    let identity = read_json(&root.join("target/harness/identity.json"));
    checks.check(
        "build identity",
        identity
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|identity| workers(root, identity)),
    );
    for id in &options.features {
        let result = match &identity {
            Ok(identity) => feature(root, identity, id, &options),
            Err(_) => Err("no build identity".into()),
        };
        checks.check(format!("feature {id}"), result);
    }
    if options.isobmff {
        checks.check("isobmff inputs", crate::isobmff_bench::verify_inputs(root));
    }
    for workloads in &options.codecs {
        checks.check(
            format!("codec workloads {}", workloads.display()),
            codec_inputs(root, workloads),
        );
    }
    if let Some(review) = &options.review {
        checks.check("RSS module review", module_review(root, review));
    }
    checks.check(
        "RSS module",
        if options.kernel {
            kernel_proof(root)
        } else {
            module_absent()
        },
    );
    Ok(checks.report())
}

/// The gate build's workers and every comparison reference match the identity.
fn workers(root: &Path, identity: &Value) -> R<String> {
    let refusals = gate_build::refusals(identity, &gate_build::pinned_release(root)?);
    if !refusals.is_empty() {
        return Err(format!("not the gate build: {}", refusals.join("; ")));
    }
    validation::current_workers(root, identity)?;
    let candidates = validation::members(validation::field(identity, "additional_candidates")?)?;
    for (name, _) in candidates {
        validation::candidate_worker(root, identity, name)?;
    }
    let references = validation::members(validation::field(identity, "comparison_references")?)?;
    Ok(format!(
        "{} candidates and {} references current",
        candidates.len() + 1,
        references.len()
    ))
}

/// Every case of a feature names references the identity holds and has every
/// input it reads with its pinned hash.
fn feature(root: &Path, identity: &Value, id: &str, options: &Options) -> R<String> {
    let directory = root.join("harness/features").join(id);
    let contract = read_json(&directory.join("contract.json"))?;
    let manifest_path = options
        .manifests
        .get(id)
        .cloned()
        .unwrap_or_else(|| directory.join("corpus.json"));
    let (manifest, _) = super::manifest::read(root, &manifest_path)?;
    let corpus = match options.corpora.get(id) {
        Some(corpus) => corpus.clone(),
        None => super::report::recorded_corpus(root, &manifest)
            .unwrap_or_else(|| root.join(".research/fixtures")),
    };
    let prefix = contract.get("worker_operation").and_then(Value::as_str);
    let cases = manifest
        .get("cases")
        .and_then(Value::as_array)
        .unwrap_or_default();
    let mut problems = Vec::new();
    for case in cases {
        let name = case.get("id").and_then(Value::as_str).unwrap_or_default();
        if let Err(error) = validation::selected_operation(case, &contract, identity) {
            problems.push(format!("{name}: {error}"));
            continue;
        }
        match super::report::input_problem(&corpus, case, prefix) {
            Ok(None) => {}
            Ok(Some(status)) => problems.push(format!("{name}: {status}")),
            Err(error) => problems.push(format!("{name}: {error}")),
        }
    }
    if problems.is_empty() {
        Ok(format!("{} cases in {}", cases.len(), corpus.display()))
    } else {
        let shown = problems.len().min(5);
        Err(format!(
            "{} of {} cases: {}{}",
            problems.len(),
            cases.len(),
            problems[..shown].join("; "),
            if problems.len() > shown { "; …" } else { "" }
        ))
    }
}

/// Every workload's input exists with its pinned hash.
fn codec_inputs(root: &Path, workloads: &Path) -> R<String> {
    let list = read_json(workloads)?;
    let entries = list
        .get("workloads")
        .and_then(Value::as_array)
        .unwrap_or_default();
    let mut problems = Vec::new();
    for workload in entries {
        let id = workload
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (Some(path), Some(pin)) = (
            workload.get("path").and_then(Value::as_str),
            workload.get("sha256").and_then(Value::as_str),
        ) else {
            problems.push(format!("{id}: no path or hash"));
            continue;
        };
        match sha256::file_hex(&root.join(path)) {
            Ok(actual) if actual == pin => {}
            Ok(_) => problems.push(format!("{id}: {path} differs from its pin")),
            Err(error) => problems.push(format!("{id}: {path}: {error}")),
        }
    }
    if problems.is_empty() {
        Ok(format!("{} inputs", entries.len()))
    } else {
        Err(problems.join("; "))
    }
}

/// The review names this module source and kernel release as a reviewed upper bound.
fn module_review(root: &Path, review: &Path) -> R<String> {
    let document = read_json(review)?;
    let source = linux::file_sha(&root.join("harness/rss-bound/triergon_rss_bound.c"))?;
    let text = |key: &str| document.get(key).and_then(Value::as_str);
    if text("module_source_sha256") != Some(source.as_str()) {
        return Err(format!("does not cover module source {}", &source[..12]));
    }
    let release = linux::kernel_release();
    if text("kernel_release") != Some(release.as_str()) {
        return Err(format!("does not cover kernel {release}"));
    }
    if text("status") != Some("reviewed-conditional-upper-bound") {
        return Err("not a reviewed upper bound".into());
    }
    Ok(format!("module {} on {release}", &source[..12]))
}

/// Timing needs the module absent.
fn module_absent() -> R<String> {
    let state = super::timing::timing_module_state();
    match state.get("status").and_then(Value::as_str) {
        Some(status @ ("module-absent" | "not-applicable")) => Ok(status.to_owned()),
        status => Err(format!(
            "{}: unload it before timing",
            status.unwrap_or("unknown")
        )),
    }
}

/// Kernel-bound memory needs the module loaded under a current proof.
fn kernel_proof(root: &Path) -> R<String> {
    let identity = read_json(&root.join("target/harness/identity.json"))?;
    let instrumentation = validation::field(&identity, "instrumentation")?;
    let profile = linux::load_kernel_profile(
        root,
        validation::text(instrumentation, "observer_sha256")?,
        validation::text(instrumentation, "probe_sha256")?,
    )?;
    let observer = validation::text(instrumentation, "observer_sha256")?;
    let probe = validation::text(instrumentation, "probe_sha256")?;
    if linux::kernel_profile_current(root, &profile, Some(observer), Some(probe)) {
        Ok("loaded under a current proof".into())
    } else {
        Err(
            "no current proof for the loaded module: run prove.sh and rebuild workers with it"
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    #[test]
    fn corpora_and_manifests_name_selected_features() {
        let root = Path::new("/repo");
        let options = Options::parse(
            root,
            &args(&[
                "--feature",
                "hls-segments",
                "--corpus",
                "hls-segments=harness/fixtures/hls",
            ]),
        )
        .unwrap();
        assert_eq!(
            options.corpora.get("hls-segments"),
            Some(&root.join("harness/fixtures/hls"))
        );
        assert!(Options::parse(root, &args(&["--corpus", "hls-segments=x"])).is_err());
        assert!(Options::parse(root, &args(&["--feature", "a", "--corpus", "a"])).is_err());
        assert!(Options::parse(root, &args(&["--frobnicate"])).is_err());
    }

    #[test]
    fn a_failed_check_fails_the_preflight() {
        let mut checks = Checks::default();
        checks.check("one", Ok("fine".into()));
        assert_eq!(checks.report(), 0);
        checks.check("two", Err("missing".into()));
        assert_eq!(checks.report(), 1);
    }
}
