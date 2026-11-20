//! Bind a supplied session worker to the library checkout and the target Cargo built.
use super::sha;
use crate::harness::{build, gate_build};
use crate::json::Value;
use crate::{checks, corpus, prepare};
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

fn receipt_path(worker: &Path) -> PathBuf {
    let mut path = worker.as_os_str().to_owned();
    path.push(".build.json");
    PathBuf::from(path)
}

fn cargo_command(dir: &Path) -> Result<Vec<String>, String> {
    let mut argv = build::cargo_rustc(&WORKER, dir, true)?;
    argv.extend(gate_build::worker_link_arguments()?);
    Ok(argv)
}

pub(super) fn stage(root: &Path, tree: &Path, dest: &Path) -> Result<(), String> {
    if dest.exists() || receipt_path(dest).exists() {
        return Err(format!(
            "worker or receipt already exists: {}",
            dest.display()
        ));
    }
    let mut git = Command::new("git");
    git.current_dir(tree)
        .args(["status", "--porcelain", "--untracked-files=all", "--", "."]);
    for name in OVERLAY {
        git.arg(format!(":(exclude){name}"));
    }
    if !prepare::output(&mut git)?.is_empty() {
        return Err("worker checkout has changes outside the workload overlay".into());
    }
    let files = overlay(tree)?;
    if files != overlay(root)? {
        return Err("worker checkout has the wrong workload overlay".into());
    }
    let revision = prepare::output(
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
    let argv = cargo_command(&dir)?;
    prepare::run(
        Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(tree)
            .env("CARGO_INCREMENTAL", "0"),
    )?;
    let artifact = dir
        .join(gate_build::release_subdir()?)
        .join("examples/session_queue");
    std::fs::copy(&artifact, dest).map_err(|e| format!("{}: {e}", artifact.display()))?;
    let receipt = Value::object([
        ("revision", Value::str(revision.trim())),
        ("target", Value::str(target)),
        ("sha256", Value::str(sha(dest)?)),
        ("overlay", files),
        ("build", build),
        ("argv", Value::strings(argv)),
    ]);
    corpus::write_json(&receipt_path(dest), &receipt)
}

pub(super) fn verify(root: &Path, worker: &Path) -> Result<Value, String> {
    let receipt = checks::read_json(&receipt_path(worker))?;
    if receipt.get("sha256").and_then(Value::as_str) != Some(&sha(worker)?)
        || receipt.get("target").and_then(Value::as_str) != Some(gate_build::worker_target()?)
        || receipt.get("overlay") != Some(&overlay(root)?)
    {
        return Err(format!(
            "{}: binary, target or workload differs from the build receipt",
            worker.display()
        ));
    }
    let build = receipt.get("build").ok_or("missing build record")?;
    let argv = receipt
        .get("argv")
        .and_then(Value::as_array)
        .ok_or("missing Cargo command")?;
    let dir = argv
        .windows(2)
        .find(|pair| pair[0].as_str() == Some("--target-dir"))
        .and_then(|pair| pair[1].as_str())
        .ok_or("missing Cargo target directory")?;
    if receipt.get("argv") != Some(&Value::strings(cargo_command(Path::new(dir))?)) {
        return Err("Cargo command differs from the worker build for this target".into());
    }
    if !receipt
        .get("revision")
        .and_then(Value::as_str)
        .is_some_and(|r| r.len() == 40 && r.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err("missing library revision in worker build".into());
    }
    let refusals = gate_build::refusals(build, &gate_build::pinned_release(root)?);
    if !refusals.is_empty() {
        return Err(refusals.join(", "));
    }
    Ok(receipt)
}
