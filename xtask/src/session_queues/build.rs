//! Bind a supplied session worker to the library checkout and the target Cargo built.
use super::sha;
use crate::harness::{build, gate_build};
use crate::json::{self, Value};
use crate::{process, sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

const OVERLAY: [&str; 3] = [
    "Cargo.lock",
    "crates/session/Cargo.toml",
    "crates/session/examples/session_queue/main.rs",
];
const WORKER: build::Candidate = build::Candidate {
    name: "session_queue",
    package: "triergon-session",
    example: Some("session_queue"),
    features: &["codecs"],
    nightly: false,
};

fn overlay(root: &Path) -> Result<Value, String> {
    let mut files = Vec::new();
    for name in OVERLAY {
        files.push((name.to_owned(), Value::str(sha(&root.join(name))?)));
    }
    Ok(Value::Object(files))
}

fn identity_path(worker: &Path) -> PathBuf {
    let mut path = worker.as_os_str().to_owned();
    path.push(".build.json");
    PathBuf::from(path)
}

fn build_dir(dir: &Path, revision: &str, files: &Value) -> PathBuf {
    let source = format!("{revision}\n{}", json::to_python(files, None));
    dir.join("session-queues")
        .join(sha256::hex(source.as_bytes()))
}

fn cargo_command(dir: &Path) -> Result<Vec<String>, String> {
    let mut argv = build::cargo_rustc(&WORKER, dir, true)?;
    argv.extend(gate_build::worker_link_arguments()?);
    Ok(argv)
}

fn linked_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("session_queue-"))
        .is_some_and(|hash| hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
}

// The pinned Cargo retains the linked example (and its .d file) here. Read only
// this source's intermediate directory; the public release path is shared.
fn linked_worker(cache: &Path) -> Result<PathBuf, String> {
    let examples = cache.join(gate_build::release_subdir()?).join("examples");
    let mut binaries = Vec::new();
    for entry in std::fs::read_dir(&examples).map_err(|e| format!("{}: {e}", examples.display()))? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if linked_name(&path) {
            binaries.push(path);
        }
    }
    match binaries.as_slice() {
        [binary] => Ok(binary.clone()),
        _ => Err(format!(
            "expected one linked session worker in {}",
            examples.display()
        )),
    }
}

pub(super) fn stage(root: &Path, tree: &Path, dest: &Path) -> Result<(), String> {
    if dest.exists() || identity_path(dest).exists() {
        return Err(format!(
            "worker or build identity already exists: {}",
            dest.display()
        ));
    }
    let mut git = Command::new("git");
    git.current_dir(tree)
        .args(["status", "--porcelain", "--untracked-files=all", "--", "."]);
    for name in OVERLAY {
        git.arg(format!(":(exclude){name}"));
    }
    if !process::output(&mut git)?.is_empty() {
        return Err("worker checkout has changes outside the workload overlay".into());
    }
    let files = overlay(tree)?;
    if files != overlay(root)? {
        return Err("worker checkout has the wrong workload overlay".into());
    }
    let revision = process::output(
        Command::new("git")
            .current_dir(tree)
            .args(["rev-parse", "HEAD"]),
    )?;
    let build = Value::Object(gate_build::current(tree)?);
    let refusals = gate_build::refusals(&build, &gate_build::pinned_release(root)?);
    if !refusals.is_empty() {
        return Err(refusals.join(", "));
    }
    if build.get("rustc").and_then(Value::as_str) != Some(&gate_build::compiler(root, None)?) {
        return Err("worker checkout selects a different compiler".into());
    }
    let target = gate_build::worker_target()?;
    let dir =
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    let dir = std::path::absolute(dir).map_err(|e| e.to_string())?;
    let cache = build_dir(&dir, revision.trim(), &files);
    let argv = cargo_command(&dir)?;
    process::run(
        Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(tree)
            .env("CARGO_INCREMENTAL", "0")
            .env("CARGO_BUILD_BUILD_DIR", &cache),
    )?;
    let binary = linked_worker(&cache)?;
    std::fs::copy(&binary, dest).map_err(|e| format!("{}: {e}", binary.display()))?;
    let identity = Value::object([
        ("revision", Value::str(revision.trim())),
        ("target", Value::str(target)),
        ("sha256", Value::str(sha(dest)?)),
        ("build_dir", Value::str(cache.display().to_string())),
        ("artifact", Value::str(binary.display().to_string())),
        ("overlay", files),
        ("build", build),
        ("argv", Value::strings(argv)),
    ]);
    json::write(&identity_path(dest), &identity)
}

pub(super) fn verify(root: &Path, worker: &Path) -> Result<Value, String> {
    let identity = json::read(&identity_path(worker))?;
    if identity.get("sha256").and_then(Value::as_str) != Some(&sha(worker)?)
        || identity.get("target").and_then(Value::as_str) != Some(gate_build::worker_target()?)
        || identity.get("overlay") != Some(&overlay(root)?)
    {
        return Err(format!(
            "{}: binary, target or workload differs from the build identity",
            worker.display()
        ));
    }
    let build = identity.get("build").ok_or("missing build record")?;
    let argv = identity
        .get("argv")
        .and_then(Value::as_array)
        .ok_or("missing Cargo command")?;
    let dir = argv
        .windows(2)
        .find(|pair| pair[0].as_str() == Some("--target-dir"))
        .and_then(|pair| pair[1].as_str())
        .ok_or("missing Cargo target directory")?;
    let revision = identity
        .get("revision")
        .and_then(Value::as_str)
        .filter(|r| r.len() == 40 && r.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("missing library revision in worker build")?;
    if identity.get("argv") != Some(&Value::strings(cargo_command(Path::new(dir))?)) {
        return Err("Cargo command differs from the worker build for this target".into());
    }
    let cache = build_dir(
        Path::new(dir),
        revision,
        identity.get("overlay").ok_or("missing workload overlay")?,
    );
    if identity.get("build_dir").and_then(Value::as_str) != cache.to_str() {
        return Err("intermediate build directory differs from the source identity".into());
    }
    let artifact = identity
        .get("artifact")
        .and_then(Value::as_str)
        .map(Path::new)
        .ok_or("missing linked artifact path")?;
    if !linked_name(artifact)
        || artifact.parent()
            != Some(
                cache
                    .join(gate_build::release_subdir()?)
                    .join("examples")
                    .as_path(),
            )
    {
        return Err("linked artifact is outside the recorded source build".into());
    }
    let refusals = gate_build::refusals(build, &gate_build::pinned_release(root)?);
    if !refusals.is_empty() {
        return Err(refusals.join(", "));
    }
    Ok(identity)
}
