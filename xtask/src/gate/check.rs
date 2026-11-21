//! `gate check`: every file a shard's preflight reads is committed, generated, fetched
//! with a pin or held by a kept evidence set.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::json::{self, Value};
use crate::sha256;

use super::plan::{capture_cases, items, report_features};
use super::{R, TARGETS, select, shards};

/// Where a host gets an input from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Source {
    Committed,
    Generated,
    Fetched,
    Kept,
}

/// What a gate host holds once it is prepared, known without a host: the
/// committed tree, what `fixtures` and `framing fixtures` generate from it, the
/// generated manifests, the vectors `fetch-vectors` fetches with their pins, and
/// the files of the kept evidence sets unpacked at the repository root.
pub(super) struct Tree {
    root: PathBuf,
    committed: BTreeSet<String>,
    /// The generated fixtures, under their repository-relative paths.
    generated: Option<PathBuf>,
    fetched: BTreeMap<String, String>,
    /// Path to SHA-256.
    pub(super) kept: BTreeMap<String, String>,
    hashes: BTreeMap<PathBuf, String>,
}

/// Where `check` generates the fixtures.
const CHECK_DIRECTORY: &str = "target/gate-check";

impl Tree {
    fn load(root: &Path) -> R<Self> {
        let generated = root.join(CHECK_DIRECTORY);
        if generated.exists() {
            fs::remove_dir_all(&generated).map_err(|e| format!("{}: {e}", generated.display()))?;
        }
        let paths = crate::corpus::Paths {
            root: root.to_path_buf(),
            out: generated.clone(),
        };
        crate::corpus::generate_all(&paths)?;
        crate::framing_bench::generate(&generated.join(".research/fixtures-packet-framing"))?;
        crate::framing_views::dts::gate_fixtures(root, &generated)?;
        let mut tree = Self::committed(root)?;
        tree.generated = Some(generated);
        for pin in crate::fetch::pins(root)? {
            tree.fetched
                .insert(format!("{}/{}", pin.directory, pin.file), pin.sha256);
        }
        let fate = json::read(&root.join("harness/fate/samples.json"))?;
        if let Some(samples) = fate.get("samples").and_then(Value::as_array) {
            for sample in samples {
                if let (Some(path), Some(pin)) = (
                    sample.get("path").and_then(Value::as_str),
                    sample.get("sha256").and_then(Value::as_str),
                ) && path.starts_with("dts-corpus/")
                {
                    tree.fetched
                        .insert(format!(".research/{path}"), pin.to_owned());
                }
            }
        }
        tree.kept = kept_files(root)?;
        Ok(tree)
    }

    /// The committed tree alone.
    pub(super) fn committed(root: &Path) -> R<Self> {
        let output = Command::new("git")
            .args(["ls-files", "-z"])
            .current_dir(root)
            .output()
            .map_err(|e| format!("git ls-files: {e}"))?;
        if !output.status.success() {
            return Err("git ls-files failed".into());
        }
        Ok(Self {
            root: root.to_path_buf(),
            committed: output
                .stdout
                .split(|&b| b == 0)
                .filter(|path| !path.is_empty())
                .map(|path| String::from_utf8_lossy(path).into_owned())
                .collect(),
            generated: None,
            fetched: BTreeMap::new(),
            kept: BTreeMap::new(),
            hashes: BTreeMap::new(),
        })
    }

    /// The manifest a host reads at `path`: the committed file, or the one a
    /// host's prepare generates there.
    fn manifest(&self, path: &Path) -> R<Value> {
        let relative = self.relative(path)?;
        if self.committed.contains(&relative) {
            return json::read(path);
        }
        generated_manifest(&self.root, path)
            .unwrap_or_else(|| Err(format!("{relative}: no committed or generated manifest")))
    }

    fn relative(&self, path: &Path) -> R<String> {
        let inside = path
            .strip_prefix(&self.root)
            .map_err(|_| format!("{}: outside the repository", path.display()))?;
        let mut parts = Vec::new();
        for part in inside.components() {
            match part {
                std::path::Component::Normal(part) => parts.push(part.to_string_lossy()),
                std::path::Component::CurDir => {}
                _ => return Err(format!("{}: escapes the repository", path.display())),
            }
        }
        Ok(parts.join("/"))
    }

    fn hash(&mut self, path: PathBuf) -> R<String> {
        if let Some(hash) = self.hashes.get(&path) {
            return Ok(hash.clone());
        }
        let hash = sha256::file_hex(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        self.hashes.insert(path, hash.clone());
        Ok(hash)
    }

    /// Where a host gets `input` from, or why it has no such file.
    fn source(&mut self, input: &crate::harness::preflight::Input) -> R<Source> {
        self.resolve(input).map(|(source, _)| source)
    }

    /// Where a host gets `input` from and its SHA-256, which is known before a host's
    /// prepare but for a file whose manifest that prepare generates.
    fn resolve(&mut self, input: &crate::harness::preflight::Input) -> R<(Source, Option<String>)> {
        let relative = self.relative(&input.path)?;
        let generated = self
            .generated
            .as_ref()
            .map(|directory| directory.join(&relative))
            .filter(|path| path.is_file());
        let (source, hash) = if self.committed.contains(&relative) {
            (Source::Committed, self.hash(self.root.join(&relative))?)
        } else if let Some(path) = generated {
            (Source::Generated, self.hash(path)?)
        } else if let Some(pin) = self.fetched.get(&relative) {
            (Source::Fetched, pin.clone())
        } else if let Some(hash) = self.kept.get(&relative) {
            (Source::Kept, hash.clone())
        } else if generated_manifest(&self.root, &input.path).is_some() {
            return Ok((Source::Generated, None));
        } else {
            return Err(format!(
                "{relative}: not committed, not generated or fetched by a host's prepare, \
                 and in no kept evidence set"
            ));
        };
        match &input.sha256 {
            Some(pin) if *pin != hash => Err(format!(
                "{relative}: pinned as {} but {source:?} as {}",
                &pin[..pin.len().min(12)],
                &hash[..hash.len().min(12)]
            )),
            _ => Ok((source, Some(hash))),
        }
    }

    /// The problems of preflight ARGS: inputs a host would not have.
    pub(super) fn problems(&mut self, args: &[String]) -> (Vec<Source>, Vec<String>) {
        let root = self.root.clone();
        let inputs = {
            let tree = &*self;
            crate::harness::preflight::inputs(&root, args, &|path| tree.manifest(path))
        };
        let mut sources = Vec::new();
        let mut problems = Vec::new();
        match inputs {
            Ok(inputs) => {
                for input in &inputs {
                    match self.source(input) {
                        Ok(source) => sources.push(source),
                        Err(problem) => problems.push(problem),
                    }
                }
            }
            Err(error) => problems.push(error),
        }
        (sources, problems)
    }
}

/// The manifest a host's prepare generates at `path`: beside a generated
/// feature's contract, or where `COMMAND manifest` writes it.
fn generated_manifest(root: &Path, path: &Path) -> Option<R<Value>> {
    crate::harness::manifest::generated(root, path)
        .or_else(|| crate::generators::written(root, path))
}

/// The files of the kept evidence sets that unpack at the repository root, by
/// their listing in the evidence index.
pub(super) fn kept_files(root: &Path) -> R<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    for set in &crate::evidence::sets(root)? {
        let text = |key| set.get(key).and_then(Value::as_str);
        if text("retention") != Some("retained") || text("unpacked_at") != Some("repository") {
            continue;
        }
        for file in set
            .get("files")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            let field = |key| file.get(key).and_then(Value::as_str);
            if let (Some(path), Some(hash)) = (field("path"), field("sha256")) {
                files.insert(path.to_owned(), hash.to_owned());
            }
        }
    }
    Ok(files)
}

/// `gate check`: every shard's preflight reads only what a prepared host has.
pub(super) fn check(
    root: &Path,
    count: usize,
    target: &str,
    selection: Option<&[String]>,
) -> R<i32> {
    let all = select(
        items(
            &report_features(root)?,
            "OUT",
            false,
            target,
            "target/release",
            &capture_cases(root)?,
        ),
        selection,
    )?;
    let mut tree = Tree::load(root)?;
    let mut failed = 0;
    for (index, shard) in shards(all, count).iter().enumerate() {
        let label = format!("shard {}/{count}", index + 1);
        let mut sources = Vec::new();
        let mut problems = Vec::new();
        for item in shard {
            let (found, missing) = tree.problems(&item.preflight);
            sources.extend(found);
            problems.extend(missing.into_iter().map(|p| format!("{}: {p}", item.name)));
        }
        // The shard's arguments together, as `gate preflight --shard` passes them.
        let combined: Vec<String> = shard.iter().flat_map(|i| i.preflight.clone()).collect();
        if let Err(error) = crate::harness::preflight::inputs(root, &combined, &|_| Ok(Value::Null))
        {
            problems.push(format!("the shard's preflight arguments: {error}"));
        }
        if problems.is_empty() {
            let of = |source| sources.iter().filter(|&&s| s == source).count();
            println!(
                "ok   {label}: {} inputs: {} committed, {} generated, {} fetched, {} kept",
                sources.len(),
                of(Source::Committed),
                of(Source::Generated),
                of(Source::Fetched),
                of(Source::Kept)
            );
        } else {
            failed += problems.len();
            for problem in &problems {
                println!("FAIL {label}: {problem}");
            }
        }
    }
    if failed == 0 {
        println!("gate check passed: {count} shards");
        Ok(0)
    } else {
        println!("gate check failed: {failed} problems");
        Ok(1)
    }
}

/// The SHA-256 of every file a gate item on any of [`TARGETS`] reads and of every
/// committed file, each with a path it is found at. An input whose manifest a host's
/// prepare generates is hashed where that prepare put it, so on a prepared host every
/// input is known; elsewhere such an input is an error.
pub fn input_hashes(root: &Path) -> R<BTreeMap<String, String>> {
    let (hashes, unknown) = gate_inputs(root, true)?;
    debug_assert_eq!(unknown, 0);
    Ok(hashes)
}

/// The SHA-256 of every file a gate item reads on either target or this checkout
/// commits that is known without a prepared host, with the same paths as
/// [`input_hashes`], and how many inputs only a host's prepare makes.
pub fn input_pins(root: &Path) -> R<(BTreeMap<String, String>, usize)> {
    gate_inputs(root, false)
}

fn gate_inputs(root: &Path, prepared: bool) -> R<(BTreeMap<String, String>, usize)> {
    let mut tree = Tree::load(root)?;
    let mut hashes = BTreeMap::new();
    for relative in tree.committed.clone() {
        let path = root.join(&relative);
        if path.is_file() && !path.is_symlink() {
            hashes.insert(tree.hash(path)?, relative);
        }
    }
    let capture = capture_cases(root)?;
    let mut unknown = BTreeSet::new();
    let reports = report_features(root)?;
    for target in TARGETS {
        for item in items(&reports, "OUT", false, target, "target/release", &capture) {
            let inputs = crate::harness::preflight::inputs(root, &item.preflight, &|path| {
                tree.manifest(path)
            })?;
            for input in &inputs {
                let relative = tree.relative(&input.path)?;
                let hash = match tree.resolve(input)? {
                    (_, Some(hash)) => hash,
                    (_, None) if input.path.is_file() => tree.hash(input.path.clone())?,
                    (_, None) if !prepared => {
                        unknown.insert(relative);
                        continue;
                    }
                    (_, None) => {
                        return Err(format!(
                            "{}: {relative} is generated by a host's prepare; \
                             check on a prepared host",
                            item.name
                        ));
                    }
                };
                hashes.entry(hash).or_insert(relative);
            }
        }
    }
    Ok((hashes, unknown.len()))
}
