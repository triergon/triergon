//! `cargo xtask preflight`: check everything a gate run needs before anything is
//! measured, and report every problem at once, so a run never measures for hours
//! and then turns out invalid.
//!
//! It checks that the gate build's workers and references match the build
//! identity; that every case of the selected features names a reference the
//! identity holds and has its input, ordered entities and companions on disk
//! with their pinned hashes; that the ISO BMFF suite's and the codec workloads'
//! inputs match their pins, and those of the external vector and capture; that
//! the RSS module source has a review for this kernel; and that the RSS module
//! is unloaded, as timing needs it, or, with `--kernel`, loaded under a current
//! proof on a host with the qualified class's memory, as kernel-bound memory needs
//! it. Anything it cannot establish fails.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{gate_build, linux, validation};
use crate::json::{self, Value};
use crate::sha256;

pub const USAGE: &str = "usage: cargo xtask preflight [--feature ID]... [--corpus ID=DIR]...
       [--manifest ID=FILE]... [--isobmff] [--isobmff-seek] [--isobmff-reader]
       [--codecs WORKLOADS]...
       [--readers LANE,...] [--correctness LANE,...] [--aac-access-units]
       [--external-vector] [--capture CASES] [--reference NAME]...
       [--review FILE | --standard-memory] [--kernel]

Check, before a gate run measures anything, that the build identity's workers and
references are current and the gate build; that each --feature's cases name known
references and have every input with its pinned hash (in --corpus, else the
manifest's recorded directory, else .research/fixtures; --manifest replaces the
contract's corpus.json); with --isobmff the ISO BMFF suite's inputs, with
--isobmff-seek the ISO BMFF seek suite's, with --isobmff-reader the ISO BMFF reader
comparison's, with --codecs each workload list's inputs,
with --readers the reader comparison's inputs of those lanes
(harness/codecs/reader-manifest.json; reader-ogg-seek is reader-ogg's seek cases,
harness/codecs/reader-seeks.json), with --correctness the correctness manifest's
vectors of those lanes (harness/codecs/correctness-manifest.json), with
--external-vector the mandatory external vector and with --capture the capture
cases, their input generator and its numpy Python; that each named reference was built and is current: those --isobmff,
--isobmff-seek, --codecs, --readers, --correctness and --capture measure against,
and any --reference; that the RSS module review covers
this module source and kernel (the committed review,
harness/rss-bound/reviews/SOURCE-RELEASE.json, or --review FILE), unless the run's
memory uses the standard bounds (--standard-memory); and that the RSS module is
unloaded, or with --kernel loaded under a current proof on a host with at least
30 GiB of memory, the class kernel-bound memory is qualified on. A feature
without cases or a workload list without workloads fails. Prints every check and
exits 1 if any fails.";

type R<T> = Result<T, String>;

#[derive(Default)]
struct Options {
    features: Vec<String>,
    corpora: BTreeMap<String, PathBuf>,
    manifests: BTreeMap<String, PathBuf>,
    isobmff: bool,
    isobmff_seek: bool,
    isobmff_reader: bool,
    codecs: Vec<PathBuf>,
    /// Reader comparison lanes.
    readers: Vec<String>,
    /// Correctness lanes.
    correctness: Vec<String>,
    aac_access_units: bool,
    external_vector: bool,
    capture: Option<PathBuf>,
    references: Vec<String>,
    review: Option<PathBuf>,
    standard_memory: bool,
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
                "--isobmff-seek" => options.isobmff_seek = true,
                "--isobmff-reader" => options.isobmff_reader = true,
                "--codecs" => options.codecs.push(root.join(value()?)),
                "--readers" => options
                    .readers
                    .extend(value()?.split(',').map(str::to_owned)),
                "--correctness" => options
                    .correctness
                    .extend(value()?.split(',').map(str::to_owned)),
                "--aac-access-units" => options.aac_access_units = true,
                "--external-vector" => options.external_vector = true,
                "--capture" => options.capture = Some(root.join(value()?)),
                "--reference" => options.references.push(value()?),
                "--review" => options.review = Some(root.join(value()?)),
                "--standard-memory" => options.standard_memory = true,
                "--kernel" => options.kernel = true,
                _ => return Err(USAGE.to_owned()),
            }
        }
        if options.review.is_some() && options.standard_memory {
            return Err("--review names the RSS module review of a kernel-bound run; --standard-memory is a run without it".into());
        }
        if !options.standard_memory && options.review.is_none() {
            options.review = Some(committed_review(root));
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
    let identity = json::read(&root.join("target/harness/identity.json"));
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
    if options.isobmff_seek {
        checks.check(
            "isobmff seek inputs",
            crate::isobmff_seek::verify_inputs(root),
        );
    }
    if options.isobmff_reader {
        checks.check(
            "isobmff reader inputs",
            crate::codecs::isobmff_reader::verify_inputs(root),
        );
    }
    for workloads in &options.codecs {
        checks.check(
            format!("codec workloads {}", workloads.display()),
            codec_inputs(root, workloads),
        );
    }
    if !options.readers.is_empty() {
        checks.check(
            format!("reader inputs {}", options.readers.join(",")),
            reader_files(root, &options.readers).and_then(|files| pinned_inputs(&files)),
        );
    }
    if !options.correctness.is_empty() {
        checks.check(
            format!("correctness inputs {}", options.correctness.join(",")),
            correctness_files(root, &options.correctness).and_then(|files| pinned_inputs(&files)),
        );
    }
    if options.aac_access_units {
        checks.check(
            "configured AAC sources",
            aac_access_unit_files(root).and_then(|files| pinned_inputs(&files)),
        );
    }
    if options.external_vector {
        checks.check("external vector", external_vector(root));
    }
    for name in options.measured_references() {
        let result = match &identity {
            Ok(identity) => built_reference(root, identity, &name),
            Err(_) => Err("no build identity".into()),
        };
        checks.check(format!("reference {name}"), result);
    }
    if let Some(cases) = &options.capture {
        checks.check("capture inputs", capture_inputs(root, cases));
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
    if options.kernel {
        let meminfo =
            std::fs::read_to_string("/proc/meminfo").map_err(|e| format!("/proc/meminfo: {e}"));
        checks.check("host memory", meminfo.and_then(|m| qualified_memory(&m)));
    }
    Ok(checks.report())
}

/// The host class kernel-bound memory gates are qualified on.
const QUALIFIED_HOST: &str = "a c6g.4xlarge with 32 GB";

/// The least `MemTotal` of that class, below its 32 GiB for the kernel's reserve.
const QUALIFIED_MEMORY_BYTES: u64 = 30 << 30;

/// Kernel-bound memory runs only on hosts with the memory of the qualified class:
/// on smaller ones another task's updates void pairs (`docs/quality.md`).
fn qualified_memory(meminfo: &str) -> R<String> {
    let total = meminfo
        .lines()
        .find_map(|line| {
            let kib = line.strip_prefix("MemTotal:")?.trim().strip_suffix("kB")?;
            kib.trim().parse::<u64>().ok()
        })
        .ok_or("/proc/meminfo has no MemTotal")?
        * 1024;
    let gib = |bytes: u64| bytes as f64 / f64::from(1u32 << 30);
    if total >= QUALIFIED_MEMORY_BYTES {
        Ok(format!("{:.1} GiB", gib(total)))
    } else {
        Err(format!(
            "{:.1} GiB is below the {:.0} GiB of {QUALIFIED_HOST}, the class kernel-bound memory gates are qualified on",
            gib(total),
            gib(QUALIFIED_MEMORY_BYTES)
        ))
    }
}

impl Options {
    /// The manifest feature `id`'s cases come from: --manifest, else the
    /// contract's corpus.json.
    fn manifest(&self, root: &Path, id: &str) -> PathBuf {
        self.manifests
            .get(id)
            .cloned()
            .unwrap_or_else(|| root.join("harness/features").join(id).join("corpus.json"))
    }

    /// The directory feature `id`'s case files are in: --corpus, else the
    /// manifest's recorded directory, else .research/fixtures.
    fn corpus(&self, root: &Path, id: &str, manifest: &Value) -> PathBuf {
        match self.corpora.get(id) {
            Some(corpus) => corpus.clone(),
            None => super::report::recorded_corpus(root, manifest)
                .unwrap_or_else(|| root.join(".research/fixtures")),
        }
    }

    /// The references the selected runs other than features measure against,
    /// which name them by path rather than through a contract.
    fn measured_references(&self) -> Vec<String> {
        let mut names = Vec::new();
        if self.isobmff || self.isobmff_seek {
            names.push(crate::isobmff_bench::REFERENCE.to_owned());
        }
        if !self.codecs.is_empty()
            || !self.readers.is_empty()
            || !self.correctness.is_empty()
            || self.aac_access_units
        {
            names.push("ffmpeg-decode-native".to_owned());
        }
        if self.capture.is_some() {
            names
                .extend(["ffmpeg-capture-native", "ffmpeg-capture-small-native"].map(String::from));
        }
        for name in &self.references {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
    }
}

/// A reference the build identity holds and that is on disk with its hash: a
/// profile `build-workers` skipped, because it was not prepared, has none.
fn built_reference(root: &Path, identity: &Value, name: &str) -> R<String> {
    let record = validation::field(validation::field(identity, "comparison_references")?, name)
        .map_err(|_| "not built: prepare its reference and rebuild the workers".to_owned())?;
    let path = root.join(validation::text(record, "path")?);
    if linux::file_sha(&path)? != validation::hash(record, "sha256")? {
        return Err(format!(
            "{} changed since the build identity",
            path.display()
        ));
    }
    Ok(path.display().to_string())
}

/// The gate build's workers and every comparison reference match the identity.
fn workers(root: &Path, identity: &Value) -> R<String> {
    let refusals =
        gate_build::identity_refusals(root, identity, &gate_build::pinned_release(root)?);
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
    let contract = json::read(&root.join("harness/features").join(id).join("contract.json"))?;
    let manifest_path = options.manifest(root, id);
    let (manifest, _) = super::manifest::read(root, &manifest_path)?;
    let corpus = options.corpus(root, id, &manifest);
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
    if cases.is_empty() {
        Err(format!("{} lists no cases", manifest_path.display()))
    } else if problems.is_empty() {
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

/// Each workload of a list: its id, and its input's repository-relative path and
/// pinned hash, or none when it lacks either.
fn workloads(list: &Value) -> Vec<(&str, Option<(&str, &str)>)> {
    list.get("workloads")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .map(|workload| {
            let text = |key| workload.get(key).and_then(Value::as_str);
            (
                text("id").unwrap_or_default(),
                text("path").zip(text("sha256")),
            )
        })
        .collect()
}

/// Every workload's input exists with its pinned hash.
fn codec_inputs(root: &Path, workloads: &Path) -> R<String> {
    let list = json::read(workloads)?;
    let entries = self::workloads(&list);
    let mut problems = Vec::new();
    for (id, input) in &entries {
        let Some((path, pin)) = input else {
            problems.push(format!("{id}: no path or hash"));
            continue;
        };
        match sha256::file_hex(&root.join(path)) {
            Ok(actual) if actual == *pin => {}
            Ok(_) => problems.push(format!("{id}: {path} differs from its pin")),
            Err(error) => problems.push(format!("{id}: {path}: {error}")),
        }
    }
    if entries.is_empty() {
        Err("lists no workloads".into())
    } else if problems.is_empty() {
        Ok(format!("{} inputs", entries.len()))
    } else {
        Err(problems.join("; "))
    }
}

/// The reader comparison's manifest and seek cases.
const READER_MANIFEST: &str = "harness/codecs/reader-manifest.json";
const READER_SEEKS: &str = "harness/codecs/reader-seeks.json";
/// Resolve derived reader lanes to their pinned input vectors.
fn reader_files(root: &Path, lanes: &[String]) -> R<Vec<(PathBuf, Option<String>)>> {
    use crate::codecs::readers::base_lane;
    let mut wanted: Vec<_> = lanes.iter().map(|lane| base_lane(lane)).collect();
    wanted.sort_unstable();
    wanted.dedup();
    let mut files = lane_files(root, READER_MANIFEST, &wanted)?;
    if lanes.iter().any(|lane| lane.ends_with("-seek")) {
        files.insert(1, (root.join(READER_SEEKS), None));
    }
    Ok(files)
}

/// The correctness manifest's vectors of `lanes`, after the manifest.
fn correctness_files(root: &Path, lanes: &[String]) -> R<Vec<(PathBuf, Option<String>)>> {
    use crate::codecs::correctness::{session_vector_lanes, variant_manifest, vector_lane};

    let mut regular = std::collections::BTreeSet::new();
    let mut files = Vec::new();
    for lane in lanes {
        regular.extend(session_vector_lanes(lane));
        if let Some(manifest) = variant_manifest(lane) {
            files.extend(variant_files(root, manifest, lane)?);
        } else {
            regular.insert(vector_lane(lane));
        }
    }
    let mov = regular.remove("aac-mov");
    let regular = regular.into_iter().collect::<Vec<_>>();
    if !regular.is_empty() {
        files.extend(lane_files(
            root,
            crate::codecs::derived::MANIFEST,
            &regular,
        )?);
    }
    if mov {
        files.extend(lane_files(root, AAC_MOV_MANIFEST, &["aac-mov"])?);
    }
    Ok(files)
}

fn variant_files(root: &Path, manifest: &str, lane: &str) -> R<Vec<(PathBuf, Option<String>)>> {
    let mut files = vec![(root.join(manifest), None)];
    let doc = json::read(&root.join(manifest))?;
    let vectors = doc
        .get("vectors")
        .and_then(Value::as_array)
        .ok_or("variant manifest needs vectors")?;
    for vector in vectors
        .iter()
        .filter(|v| v.get("lane").and_then(Value::as_str) == Some(lane))
    {
        vector_files(root, manifest, lane, vector, &mut files)?;
    }
    if files.len() == 1 && crate::codecs::correctness::session_vector_lanes(lane).is_empty() {
        return Err(format!("{manifest}: no {lane} vectors"));
    }
    Ok(files)
}

fn vector_files(
    root: &Path,
    manifest: &str,
    lane: &str,
    vector: &Value,
    files: &mut Vec<(PathBuf, Option<String>)>,
) -> R<()> {
    let field = |key| {
        vector
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{manifest}: {lane} vector without {key}"))
    };
    let (path, pin) = if vector.get("source").is_some() {
        ("source", "source_sha256")
    } else {
        ("path", "sha256")
    };
    files.push((root.join(field(path)?), Some(field(pin)?.to_owned())));
    if vector.get("tail_source").is_some() {
        files.push((
            root.join(field("tail_source")?),
            Some(field("tail_source_sha256")?.to_owned()),
        ));
    }
    Ok(())
}

const AAC_MOV_MANIFEST: &str = "harness/codecs/aac/mov-manifest.json";
const AAC_FEATURE: &str = "harness/features/aac-access-units";

fn aac_access_unit_files(root: &Path) -> R<Vec<(PathBuf, Option<String>)>> {
    let mut files = lane_files(root, AAC_MOV_MANIFEST, &["aac-mov"])?;
    let feature = root.join(AAC_FEATURE);
    let sources = feature.join("sources.json");
    let manifest = json::read(&sources)?;
    files.extend([
        (feature.join("contract.json"), None),
        (feature.join("corpus.json"), None),
        (sources, None),
    ]);
    for source in manifest
        .get("vectors")
        .and_then(Value::as_array)
        .ok_or("no AAC sources")?
    {
        let field = |key| {
            source
                .get(key)
                .and_then(Value::as_str)
                .ok_or("AAC source lacks name or SHA-256")
        };
        files.push((
            feature.join("inputs").join(field("name")?),
            Some(field("sha256")?.to_owned()),
        ));
    }
    Ok(files)
}

/// `manifest` and every vector of `lanes` it lists, with its pin. A lane without
/// vectors fails.
fn lane_files(root: &Path, manifest: &str, lanes: &[&str]) -> R<Vec<(PathBuf, Option<String>)>> {
    let mut files = vec![(root.join(manifest), None)];
    let listed = json::read(&root.join(manifest))?;
    let vectors = listed
        .get("vectors")
        .and_then(Value::as_array)
        .unwrap_or_default();
    for lane in lanes {
        let before = files.len();
        for vector in vectors {
            let text = |key| vector.get(key).and_then(Value::as_str);
            if text("lane") != Some(*lane) {
                continue;
            }
            vector_files(root, manifest, lane, vector, &mut files)?;
        }
        if files.len() == before {
            return Err(format!("{manifest}: no vectors of {lane}"));
        }
    }
    Ok(files)
}

/// Every pinned file exists with its pin.
fn pinned_inputs(files: &[(PathBuf, Option<String>)]) -> R<String> {
    let mut problems = Vec::new();
    let mut vectors = 0;
    for (path, pin) in files {
        let Some(pin) = pin else { continue };
        vectors += 1;
        match sha256::file_hex(path) {
            Ok(actual) if actual == *pin => {}
            Ok(_) => problems.push(format!("{} differs from its pin", path.display())),
            Err(error) => problems.push(format!("{}: {error}", path.display())),
        }
    }
    if problems.is_empty() {
        Ok(format!("{vectors} inputs"))
    } else {
        Err(problems.join("; "))
    }
}

/// The contract that names the mandatory external vectors.
const VECTOR_CONTRACT: &str = "harness/features/matroska-segment-metadata/contract.json";

/// The mandatory external vectors: each path in the sample cache with its pin.
fn external_vectors(root: &Path) -> R<Vec<(PathBuf, String)>> {
    let contract = json::read(&root.join(VECTOR_CONTRACT))?;
    let specs = contract
        .get("required_additional_vectors")
        .and_then(Value::as_array)
        .unwrap_or_default();
    if specs.is_empty() {
        return Err("the contract names no external vector".into());
    }
    specs
        .iter()
        .map(|spec| {
            let text = |key| spec.get(key).and_then(Value::as_str);
            let file = text("file").ok_or("vector without a file")?;
            let pin = text("sha256").ok_or("vector without a hash")?;
            Ok((
                root.join(".research/fate-samples").join(file),
                pin.to_owned(),
            ))
        })
        .collect()
}

/// The mandatory external vector is in the sample cache with its pinned hash.
fn external_vector(root: &Path) -> R<String> {
    let vectors = external_vectors(root)?;
    for (path, pin) in &vectors {
        if sha256::file_hex(path).map_err(|e| format!("{}: {e}", path.display()))? != *pin {
            return Err(format!("{} differs from its pin", path.display()));
        }
    }
    Ok(format!("{} vectors", vectors.len()))
}

/// A file a preflight check reads, with the SHA-256 the check pins it to, if any.
#[derive(Debug)]
pub struct Input {
    pub path: PathBuf,
    pub sha256: Option<String>,
}

/// Every file the checks of `preflight ARGS` read, resolved as those checks
/// resolve them, with `manifest` reading a feature's manifest. The build identity,
/// the references and the RSS module review are the host's and are left out.
pub fn inputs(root: &Path, args: &[String], manifest: &dyn Fn(&Path) -> R<Value>) -> R<Vec<Input>> {
    let options = Options::parse(root, args)?;
    let mut inputs = Vec::new();
    let mut add = |path: PathBuf, sha256: Option<&str>| {
        inputs.push(Input {
            path,
            sha256: sha256.map(str::to_owned),
        });
    };
    for id in &options.features {
        add(
            root.join("harness/features").join(id).join("contract.json"),
            None,
        );
        let path = options.manifest(root, id);
        let document = manifest(&path).map_err(|e| format!("feature {id}: {e}"))?;
        let corpus = options.corpus(root, id, &document);
        add(path, None);
        for case in document
            .get("cases")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            // The pins in the order `case_files` lists the files: the input, then
            // its ordered entities, then its companions.
            let mut pins = vec![case.get("sha256").and_then(Value::as_str)];
            for key in ["entities", "companions"] {
                for input in case.get(key).and_then(Value::as_array).unwrap_or_default() {
                    pins.push(input.get("sha256").and_then(Value::as_str));
                }
            }
            let files = super::report::case_files(&corpus, case)
                .map_err(|e| format!("feature {id}: {e}"))?;
            for (file, pin) in files.into_iter().zip(pins) {
                add(file, pin);
            }
        }
    }
    if options.isobmff {
        for (path, pin) in crate::isobmff_bench::inputs(root)? {
            add(path, Some(&pin));
        }
    }
    if options.isobmff_seek {
        for (path, pin) in crate::isobmff_seek::inputs(root)? {
            add(path, Some(&pin));
        }
    }
    if options.isobmff_reader {
        for (path, pin) in crate::codecs::isobmff_reader::inputs(root)? {
            add(path, Some(&pin));
        }
    }
    for list in &options.codecs {
        add(list.clone(), None);
        for (id, input) in workloads(&json::read(list)?) {
            let (path, pin) =
                input.ok_or_else(|| format!("{}: {id}: no path or hash", list.display()))?;
            add(root.join(path), Some(pin));
        }
    }
    if !options.readers.is_empty() {
        for (path, pin) in reader_files(root, &options.readers)? {
            add(path, pin.as_deref());
        }
    }
    if !options.correctness.is_empty() {
        for (path, pin) in correctness_files(root, &options.correctness)? {
            add(path, pin.as_deref());
        }
    }
    if options.aac_access_units {
        for (path, pin) in aac_access_unit_files(root)? {
            add(path, pin.as_deref());
        }
    }
    if options.external_vector {
        add(root.join(VECTOR_CONTRACT), None);
        for (path, pin) in external_vectors(root)? {
            add(path, Some(&pin));
        }
    }
    if let Some(cases) = &options.capture {
        add(cases.clone(), None);
        add(root.join(CAPTURE_GENERATOR), None);
    }
    Ok(inputs)
}

/// The capture inputs' generator.
const CAPTURE_GENERATOR: &str = "harness/capture/generate_inputs.py";

/// The capture cases exist, and the Python that generates their inputs has numpy.
fn capture_inputs(root: &Path, cases: &Path) -> R<String> {
    let listed = json::read(cases)?;
    let count = listed
        .get("cases")
        .and_then(Value::as_array)
        .map_or(0, <[Value]>::len);
    if count == 0 {
        return Err(format!("{} lists no cases", cases.display()));
    }
    let generator = root.join(CAPTURE_GENERATOR);
    if !generator.is_file() {
        return Err(format!("{} missing", generator.display()));
    }
    let python = super::reuse::python()?;
    let identity = python.as_str().unwrap_or_default();
    if identity.ends_with(" None") {
        return Err(format!(
            "the input generator's Python has no numpy ({identity})"
        ));
    }
    Ok(format!("{count} cases, Python and numpy {identity}"))
}

/// The committed review of the module source in the tree for the running kernel:
/// `harness/rss-bound/reviews/SOURCE-RELEASE.json`, by the first 12 hexadecimal
/// digits of the source's SHA-256 and the kernel release. `prove.sh` takes the same.
pub fn committed_review(root: &Path) -> PathBuf {
    let source = linux::file_sha(&root.join("harness/rss-bound/triergon_rss_bound.c"))
        .unwrap_or_else(|_| "unreadable-module".into());
    let prefix = source.get(..12).unwrap_or(&source);
    root.join(format!(
        "harness/rss-bound/reviews/{prefix}-{}.json",
        linux::kernel_release()
    ))
}

/// The review names this module source and kernel release as a reviewed upper bound.
fn module_review(root: &Path, review: &Path) -> R<String> {
    if !review.is_file() {
        return Err(format!(
            "no review of this module source for this kernel: {}",
            review.display()
        ));
    }
    let document = json::read(review)?;
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
    let identity = json::read(&root.join("target/harness/identity.json"))?;
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
        let parse = |values: &[&str]| {
            let mut all = args(values);
            all.push("--standard-memory".into());
            Options::parse(root, &all)
        };
        let options = parse(&[
            "--feature",
            "hls-segments",
            "--corpus",
            "hls-segments=harness/fixtures/hls",
        ])
        .unwrap();
        assert_eq!(
            options.corpora.get("hls-segments"),
            Some(&root.join("harness/fixtures/hls"))
        );
        assert!(parse(&["--corpus", "hls-segments=x"]).is_err());
        assert!(parse(&["--feature", "a", "--corpus", "a"]).is_err());
        assert!(parse(&["--frobnicate"]).is_err());
    }

    #[test]
    fn a_run_names_its_rss_review_or_its_standard_bounds() {
        let root = Path::new("/repo");
        let committed = Options::parse(root, &args(&["--feature", "a"])).unwrap();
        assert_eq!(committed.review, Some(committed_review(root)));
        assert!(committed_review(root).starts_with(root.join("harness/rss-bound/reviews")));
        let standard = Options::parse(root, &args(&["--standard-memory"])).unwrap();
        assert_eq!(standard.review, None);
        assert!(Options::parse(root, &args(&["--review", "r.json"])).is_ok());
        assert!(Options::parse(root, &args(&["--review", "r.json", "--standard-memory"])).is_err());
    }

    #[test]
    fn the_module_source_in_the_tree_has_a_committed_review() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let source = linux::file_sha(&root.join("harness/rss-bound/triergon_rss_bound.c")).unwrap();
        let reviews = std::fs::read_dir(root.join("harness/rss-bound/reviews")).unwrap();
        let reviewed = reviews.flatten().any(|entry| {
            let review = json::read(&entry.path()).unwrap();
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(&source[..12])
                && review.get("module_source_sha256").and_then(Value::as_str)
                    == Some(source.as_str())
        });
        assert!(
            reviewed,
            "commit the review of module source {}",
            &source[..12]
        );
    }

    #[test]
    fn a_reference_preflight_names_must_have_been_built() {
        let root = crate::walk::scratch("preflight-references");
        std::fs::create_dir_all(root.join("target/harness")).unwrap();
        std::fs::write(root.join("target/harness/ffmpeg-decode-worker"), b"decode").unwrap();
        let identity = Value::object([(
            "comparison_references",
            Value::object([(
                "ffmpeg-decode-native",
                Value::object([
                    ("path", Value::str("target/harness/ffmpeg-decode-worker")),
                    ("sha256", Value::str(sha256::hex(b"decode"))),
                ]),
            )]),
        )]);
        assert!(built_reference(&root, &identity, "ffmpeg-decode-native").is_ok());
        assert!(built_reference(&root, &identity, "ffmpeg-capture-native").is_err());
        std::fs::write(root.join("target/harness/ffmpeg-decode-worker"), b"rebuilt").unwrap();
        assert!(built_reference(&root, &identity, "ffmpeg-decode-native").is_err());
        let mut all = args(&[
            "--codecs",
            "w.json",
            "--capture",
            "c.json",
            "--reference",
            "ffmpeg-decode-native",
        ]);
        all.push("--standard-memory".into());
        assert_eq!(
            Options::parse(Path::new("/repo"), &all)
                .unwrap()
                .measured_references(),
            [
                "ffmpeg-decode-native",
                "ffmpeg-capture-native",
                "ffmpeg-capture-small-native"
            ]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn kernel_bound_memory_needs_the_qualified_host_memory() {
        let meminfo = |kib: u64| format!("MemTotal:       {kib} kB\nMemFree:        1 kB\n");
        // A c6g.4xlarge and a 16 GB host as /proc/meminfo reports them.
        assert!(qualified_memory(&meminfo(32_239_292)).is_ok());
        let error = qualified_memory(&meminfo(16_073_336)).unwrap_err();
        assert!(
            error.contains("15.3 GiB") && error.contains("c6g.4xlarge"),
            "{error}"
        );
        assert!(qualified_memory("MemFree: 1 kB\n").is_err());
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
