//! Content identity of one worker's selected, owned Cargo dependency graph.
//!
//! The graph comes from Cargo's target-filtered resolved metadata, not from
//! lexical guesses: every selected package's files, features and dependency
//! edges, the shared build settings and the explicit harness inputs. A build
//! records it; every later collection recomputes it and fails closed on change.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::json::{self, Value};
use crate::sha256;

/// Explicit inputs shared by container and playback operations. Changing an
/// operation's inputs requires a fresh build identity.
pub fn matroska_inputs() -> Vec<String> {
    let mut names: Vec<String> = [
        "specs/sources.json",
        "harness/fate/index.json",
        "harness/oracles.py",
        "xtask/src/corpus.rs",
        "xtask/src/harness/acceptance.rs",
        "xtask/src/harness/build.rs",
        "xtask/src/harness/comparison.rs",
        "xtask/src/harness/gate_build.rs",
        "xtask/src/harness/grade.rs",
        "xtask/src/harness/diff.rs",
        "xtask/src/harness/linux.rs",
        "xtask/src/harness/memory.rs",
        "xtask/src/harness/oracle.rs",
        "xtask/src/harness/parity.rs",
        "xtask/src/harness/policy.rs",
        "xtask/src/harness/provenance.rs",
        "xtask/src/harness/report.rs",
        "xtask/src/harness/timing.rs",
        "xtask/src/harness/validation.rs",
        "xtask/src/harness/vectors.rs",
        "xtask/src/harness/worker.rs",
        "harness/spec_oracle.py",
        "harness/content_oracle.py",
        "harness/colour_oracle.py",
        "harness/catalogue_oracle.py",
        "harness/reference.c",
        "harness/reference_catalogue.h",
        "harness/reference_colour.h",
        "harness/reference_content_encoding.h",
        "harness/allocations.c",
        "harness/allocations-linux.c",
        "harness/memory-check.c",
        "harness/portable-memory-check.c",
        "harness/rss-bound-api.h",
        "harness/comparison-rules.json",
        "harness/policy.json",
        "harness/fate/map.json",
        "harness/features/hls-segments/measured.json",
        "harness/reference_feed.h",
        "harness/reference_hls.h",
        "harness/reference_font.h",
        "harness/reference_progressive.h",
        "xtask/src/progressive_bench.rs",
        "harness/features/progressive-ts188/provenance.json",
        "harness/features/progressive-ts188/controls.json",
        "harness/features/progressive-ts188/inputs/continuous-44100.ts",
        "harness/features/progressive-ts188/inputs/reordered-48000.ts",
        "harness/features/progressive-ts188/inputs/later-entry-48000.ts",
        "harness/features/progressive-ts188/inputs/duplicate-control-44100.ts",
        "harness/features/progressive-ts188/inputs/legal-duplicate-44100.ts",
        "harness/reference_mp4_range.h",
        "harness/reference_packet_fold.h",
        "harness/reference_matroska_audio.h",
        "harness/reference_mp4_forward.h",
        "harness/reference_ts_program.h",
        "harness/reference_raw_video.h",
        "harness/reference_ts.h",
    ]
    .map(String::from)
    .to_vec();
    for feature in [
        "matroska-demux",
        "matroska-header-removal",
        "matroska-zlib",
        "matroska-cues-preroll",
        "matroska-colour",
        "matroska-segment-metadata",
        "matroska-startup-seek",
        "hls-segments",
        "progressive-ts188",
        "mp4-range",
        "matroska-audio-clocks",
        "mp4-forward",
        "ts-program",
        "raw-annex-b",
        "mpegts-pes",
    ] {
        for file in ["contract", "corpus"] {
            names.push(format!("harness/features/{feature}/{file}.json"));
        }
    }
    // Features whose manifests are generated from their pins and committed inputs.
    names.extend(crate::lanes::provenance_inputs());
    names
}

/// `cargo metadata` for the host target, as the build ran it.
pub fn cargo_metadata(root: &Path) -> Result<Value, String> {
    cargo_metadata_with_features(root, &[])
}

/// Resolved metadata with the same explicitly enabled features as a worker build.
pub(crate) fn cargo_metadata_with_features(
    root: &Path,
    features: &[String],
) -> Result<Value, String> {
    let rustc = Command::new("rustc")
        .arg("-Vv")
        .current_dir(root)
        .output()
        .map_err(|e| format!("rustc: {e}"))?;
    let version = String::from_utf8_lossy(&rustc.stdout).into_owned();
    let host = version
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .ok_or("rustc reports no host")?
        .to_owned();
    let mut command = Command::new("cargo");
    command.args([
        "metadata",
        "--format-version",
        "1",
        "--locked",
        "--offline",
        "--filter-platform",
        &host,
    ]);
    if !features.is_empty() {
        command.arg("--features").arg(features.join(","));
    }
    let output = command
        .env("CARGO_INCREMENTAL", "0")
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo metadata: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    json::parse(&String::from_utf8(output.stdout).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

/// A regular repository file reached without symbolic links or parent steps:
/// its path relative to `root` and its SHA-256.
pub fn file_identity(root: &Path, path: &Path) -> Result<(String, String), String> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let meta = fs::symlink_metadata(&path).map_err(|_| {
        format!(
            "selected input is not a regular repository file: {}",
            path.display()
        )
    })?;
    let canonical = path.canonicalize().map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() || !meta.is_file() || !canonical.starts_with(root) {
        return Err(format!(
            "selected input is not a regular repository file: {}",
            path.display()
        ));
    }
    let relative = lexical_relative(root, &path).ok_or_else(|| {
        format!(
            "selected input has no repository ancestor: {}",
            path.display()
        )
    })?;
    if relative
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(format!(
            "selected input uses parent traversal: {}",
            relative.display()
        ));
    }
    let mut current = root.to_path_buf();
    for part in relative.components() {
        current.push(part);
        if fs::symlink_metadata(&current).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!(
                "selected input traverses a symlink: {}",
                relative.display()
            ));
        }
    }
    let data = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let name = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    Ok((name, sha256::hex(&data)))
}

/// `path` relative to the first ancestor that resolves to `root`.
fn lexical_relative(root: &Path, path: &Path) -> Option<PathBuf> {
    let mut ancestor = path.parent();
    while let Some(dir) = ancestor {
        if dir.canonicalize().ok().as_deref() == Some(root) {
            return path.strip_prefix(dir).ok().map(Path::to_path_buf);
        }
        ancestor = dir.parent();
    }
    None
}

fn package_files(root: &Path, dir: &Path, out: &mut Vec<(String, Value)>) -> Result<(), String> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some("target" | ".git" | "__pycache__" | ".DS_Store")
        ) {
            continue;
        }
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        let path = entry.path();
        if kind.is_symlink() {
            return Err(format!("symlink in selected package: {}", path.display()));
        }
        if kind.is_dir() {
            package_files(root, &path, out)?;
        } else if kind.is_file() {
            let (name, digest) = file_identity(root, &path)?;
            out.push((name, Value::str(digest)));
        }
    }
    Ok(())
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("cargo metadata without {key}"))
}

/// The selected graph of `package` with its SHA-256 (`canonical` JSON).
pub fn selected_graph(
    root: &Path,
    metadata: &Value,
    package: &str,
    extra_files: &[String],
) -> Result<Value, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .unwrap_or_default();
    let nodes = metadata
        .get("resolve")
        .and_then(|r| r.get("nodes"))
        .and_then(Value::as_array)
        .unwrap_or_default();
    let by_id = |id: &str| {
        packages
            .iter()
            .find(|p| p.get("id").and_then(Value::as_str) == Some(id))
    };
    let node_of = |id: &str| {
        nodes
            .iter()
            .find(|n| n.get("id").and_then(Value::as_str) == Some(id))
    };
    let roots: Vec<&str> = packages
        .iter()
        .filter(|p| p.get("name").and_then(Value::as_str) == Some(package))
        .filter_map(|p| p.get("id").and_then(Value::as_str))
        .collect();
    if roots.len() != 1 {
        return Err(format!(
            "selected package must resolve exactly once: {package}"
        ));
    }
    let (mut pending, mut selected) = (vec![roots[0].to_owned()], Vec::<String>::new());
    while let Some(id) = pending.pop() {
        if selected.contains(&id) {
            continue;
        }
        let node = node_of(&id)
            .filter(|_| by_id(&id).is_some())
            .ok_or("selected dependency is missing from Cargo metadata")?;
        selected.push(id);
        for dep in node
            .get("deps")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            pending.push(text(dep, "pkg")?.to_owned());
        }
    }
    selected.sort_by_key(|id| {
        by_id(id)
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    });
    let mut normalized = Vec::new();
    for id in &selected {
        let package = by_id(id).expect("selected package");
        if package.get("source").is_some_and(|s| *s != Value::Null) {
            return Err(format!(
                "unadmitted non-owned dependency: {}",
                text(package, "name")?
            ));
        }
        let manifest = PathBuf::from(text(package, "manifest_path")?);
        let (manifest_name, _) = file_identity(&root, &manifest)?;
        let directory = manifest.parent().expect("manifest directory");
        if directory == root {
            return Err("selected production packages must have their own directory".into());
        }
        for target in package
            .get("targets")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            let kinds = target
                .get("kind")
                .and_then(Value::as_array)
                .unwrap_or_default();
            if kinds
                .iter()
                .any(|k| matches!(k.as_str(), Some("custom-build" | "proc-macro")))
            {
                return Err("selected build-time code needs a separate admission audit".into());
            }
            file_identity(&root, Path::new(text(target, "src_path")?))?;
        }
        let mut files = Vec::new();
        package_files(&root, directory, &mut files)?;
        let node = node_of(id).expect("selected node");
        let mut dependencies = Vec::new();
        for dep in node
            .get("deps")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            let other = by_id(text(dep, "pkg")?).ok_or("dependency package missing")?;
            let (other_manifest, _) =
                file_identity(&root, Path::new(text(other, "manifest_path")?))?;
            dependencies.push((
                text(dep, "name")?.to_owned(),
                other_manifest.clone(),
                Value::object([
                    ("alias", Value::str(text(dep, "name")?)),
                    ("package", Value::str(text(other, "name")?)),
                    ("manifest", Value::str(other_manifest)),
                    (
                        "kinds",
                        dep.get("dep_kinds").cloned().unwrap_or(Value::Null),
                    ),
                ]),
            ));
        }
        dependencies.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        let mut features: Vec<String> = node
            .get("features")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .filter_map(|f| f.as_str().map(str::to_owned))
            .collect();
        features.sort();
        normalized.push(Value::object([
            ("name", Value::str(text(package, "name")?)),
            ("version", Value::str(text(package, "version")?)),
            ("manifest", Value::str(manifest_name)),
            ("features", Value::strings(features)),
            (
                "dependencies",
                Value::Array(dependencies.into_iter().map(|d| d.2).collect()),
            ),
            ("files", Value::Object(files)),
        ]));
    }
    let mut settings = crate::toml::parse(
        &fs::read_to_string(root.join("Cargo.toml")).map_err(|e| e.to_string())?,
    )?;
    if let Value::Object(top) = &mut settings
        && let Some((_, Value::Object(workspace))) = top.iter_mut().find(|(k, _)| k == "workspace")
    {
        workspace.retain(|(k, _)| !matches!(k.as_str(), "members" | "default-members" | "exclude"));
    }
    let mut shared = Vec::new();
    for name in [
        ".cargo/config",
        ".cargo/config.toml",
        "rust-toolchain",
        "rust-toolchain.toml",
    ] {
        let path = root.join(name);
        if fs::symlink_metadata(&path).is_ok() {
            let (name, digest) = file_identity(&root, &path)?;
            shared.push((name, Value::str(digest)));
        }
    }
    for name in extra_files {
        let (name, digest) = file_identity(&root, Path::new(name))?;
        shared.push((name, Value::str(digest)));
    }
    shared.sort_by(|a, b| a.0.cmp(&b.0));
    shared.dedup_by(|a, b| a.0 == b.0);
    let graph = Value::object([
        ("schema", Value::int(1)),
        ("root_package", Value::str(package)),
        ("workspace_settings", settings),
        ("packages", Value::Array(normalized)),
        ("shared_files", Value::Object(shared)),
    ]);
    let digest = sha256::hex(json::canonical(&graph).as_bytes());
    let Value::Object(mut members) = graph else {
        unreachable!()
    };
    members.push(("sha256".into(), Value::str(digest)));
    Ok(Value::Object(members))
}

/// One fail-closed source check for every Matroska collector: `None` when the
/// recorded build identity still describes the checkout.
pub fn source_identity_error(root: &Path, identity: &Value) -> Option<String> {
    let check = || -> Result<Option<String>, String> {
        match identity.get("schema").and_then(Value::as_f64) {
            Some(2.0) => {
                let current = selected_graph(
                    root,
                    &cargo_metadata(root)?,
                    "media-worker",
                    &matroska_inputs(),
                )?;
                let same = identity
                    .get("selected_graph")
                    .is_some_and(|g| super::diff::equal(g, &current));
                Ok((!same).then(|| {
                    "selected source/configuration changed since build identity".to_owned()
                }))
            }
            Some(1.0) => {
                // Historical collections keep the original whole-workspace rule.
                let mut sources = Vec::new();
                for path in crate::walk::files(&root.join("crates"), &|rel| {
                    rel.extension().is_some_and(|e| e == "rs")
                        && rel
                            .parent()
                            .and_then(Path::file_name)
                            .is_some_and(|n| n == "src")
                }) {
                    let relative = path
                        .strip_prefix(root)
                        .expect("under root")
                        .to_string_lossy()
                        .into_owned();
                    sources.push((
                        relative,
                        Value::str(sha256::hex(&fs::read(&path).map_err(|e| e.to_string())?)),
                    ));
                }
                let same_sources = identity
                    .get("source_hashes")
                    .is_some_and(|h| super::diff::equal(h, &Value::Object(sources)));
                let inputs = match identity.get("build_input_hashes") {
                    Some(Value::Object(m)) => m.clone(),
                    _ => return Err("'build_input_hashes'".into()),
                };
                let same_inputs = inputs.iter().all(|(name, digest)| {
                    fs::read(root.join(name))
                        .is_ok_and(|d| Some(sha256::hex(&d).as_str()) == digest.as_str())
                });
                Ok((!(same_sources && same_inputs))
                    .then(|| "source/configuration changed since build identity".to_owned()))
            }
            _ => Ok(Some("unsupported build identity schema".to_owned())),
        }
    };
    match check() {
        Ok(result) => result,
        Err(error) => Some(format!("cannot verify source identity: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_media_worker_graph_binds_its_sources_and_inputs() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let graph = selected_graph(
            root,
            &cargo_metadata(root).unwrap(),
            "media-worker",
            &matroska_inputs(),
        )
        .unwrap();
        let packages = graph.get("packages").and_then(Value::as_array).unwrap();
        assert!(
            packages
                .iter()
                .any(|p| p.get("name").and_then(Value::as_str) == Some("media-worker"))
        );
        let shared = graph.get("shared_files").unwrap();
        assert!(
            shared.get("Cargo.toml").is_none()
                && shared.get("xtask/src/harness/parity.rs").is_some()
                && shared.get("xtask/src/harness/grade.rs").is_some()
                && shared.get("xtask/src/harness/validation.rs").is_some()
        );
        for name in [
            "harness/reference_font.h",
            "harness/reference_progressive.h",
            "xtask/src/progressive_bench.rs",
            "harness/features/progressive-ts188/controls.json",
        ] {
            assert!(shared.get(name).is_some(), "missing source binding: {name}");
        }
        for input in [
            "continuous-44100",
            "reordered-48000",
            "later-entry-48000",
            "duplicate-control-44100",
            "legal-duplicate-44100",
        ] {
            let name = format!("harness/features/progressive-ts188/inputs/{input}.ts");
            assert!(
                shared.get(&name).is_some(),
                "missing source binding: {name}"
            );
        }
        for feature in crate::fonts::FEATURES {
            for file in ["contract", "pins"] {
                let name = format!("harness/features/{}/{file}.json", feature.id);
                assert!(
                    shared.get(&name).is_some(),
                    "missing source binding: {name}"
                );
            }
        }
        assert_eq!(
            graph.get("sha256").and_then(Value::as_str).map(str::len),
            Some(64)
        );
        assert!(
            graph
                .get("workspace_settings")
                .and_then(|s| s.get("workspace"))
                .and_then(|w| w.get("members"))
                .is_none()
        );
    }

    #[test]
    fn rejects_files_outside_the_repository() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap();
        assert!(file_identity(&root, Path::new("/etc/hosts")).is_err());
        assert!(file_identity(&root, Path::new("crates")).is_err());
        assert_eq!(
            file_identity(&root, Path::new("Cargo.toml")).unwrap().0,
            "Cargo.toml"
        );
    }

    #[test]
    fn playback_graph_binds_range_contract_corpus_and_fixture_recipe() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let graph = crate::harness::build::candidate_graph(root, "playback-worker").unwrap();
        let shared = graph.get("shared_files").unwrap();
        for input in [
            "harness/features/mp4-range/contract.json",
            "harness/features/mp4-range/corpus.json",
            "xtask/src/corpus.rs",
        ] {
            let (_, digest) = file_identity(root, Path::new(input)).unwrap();
            assert_eq!(shared.get(input), Some(&Value::str(digest)), "{input}");
        }
    }
}
