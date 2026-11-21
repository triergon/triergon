//! `gate test`: the workspace tests for the gate target in one profile, reused from a
//! kept evidence set when everything they read is the same.

use std::path::Path;
use std::process::Command;

use crate::json::Value;
use crate::sha256;

use super::{R, git, owned, status};

/// The test inputs outside the repository that crate tests read when present.
const TEST_INPUTS: [&str; 3] = [
    ".research/fate-samples",
    ".research/flac-generated",
    ".research/flac-test-files",
];

/// Runs the workspace tests in one profile, or takes the result of a kept set
/// for the same key: every tracked file as the working tree holds it, untracked
/// files that are not ignored, the test inputs of [`TEST_INPUTS`], and what every
/// gate key holds (the measuring code, toolchains, target and host class).
///
/// Release tests build in [`RELEASE_TESTS_TARGET_DIR`]: in the gate build's own
/// directory, their feature set would rebuild its workers, and steps measured after
/// them would time binaries the gate build did not make.
pub(super) fn test(root: &Path, output: &Path, profile: &str, full: bool) -> R<i32> {
    let mut command = owned(&["test", "--workspace", "--locked", "--profile", profile]);
    command.extend(crate::harness::gate_build::target_args()?);
    if profile == "release" {
        command.extend(owned(&["--target-dir", RELEASE_TESTS_TARGET_DIR]));
    }
    // The tests that read the evidence index, which the key leaves out, run apart
    // from the reused run and every time.
    let mut index = command.clone();
    index[1..2].clone_from_slice(&owned(&["-p"]));
    index.insert(2, "xtask".into());
    index.extend(owned(&["--bin", "xtask", "--", "--exact"]));
    index.extend(INDEX_TESTS.iter().map(|&name| name.to_owned()));
    command.push("--".into());
    for name in INDEX_TESTS {
        command.extend(owned(&["--skip", name]));
    }
    let inputs = TEST_INPUTS
        .iter()
        .map(|dir| Ok(((*dir).to_owned(), Value::str(tree_digest(&root.join(dir))?))))
        .collect::<R<Vec<_>>>()?;
    let settings = Value::object([
        ("cargo", Value::strings(command.clone())),
        ("sources", Value::str(sources_digest(root)?)),
        ("inputs", Value::Object(inputs)),
    ]);
    let key = crate::harness::reuse::Key::new(root, "gate test", settings, &[], &[])?;
    let cargo = |args: &[String]| {
        status(
            Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
                .args(args)
                .env("CARGO_INCREMENTAL", "0")
                .current_dir(root),
        )
    };
    let reused = crate::harness::reuse::step(root, output, &key, full, || cargo(&command))?;
    let read = cargo(&index)?;
    Ok(if reused != 0 { reused } else { read })
}

/// The target directory of the release-profile tests, apart from the gate build's.
const RELEASE_TESTS_TARGET_DIR: &str = "target/gate-tests";

/// The tests that read the evidence index: the index changes when a result is
/// promoted, so a test result's key leaves it out, and these run on every gate test
/// instead of with the reused run.
pub(crate) const INDEX_TESTS: [&str; 3] = [
    "evidence::tests::the_index_lists_every_archived_file_once",
    "gate::tests::a_feature_whose_inputs_are_neither_committed_nor_kept_is_refused",
    "gate::tests::inputs_in_a_kept_set_count_by_its_listing_and_pins",
];

/// What no reused test reads: the evidence index, which changes when a result is
/// promoted and which only [`INDEX_TESTS`] read, and the project documents.
const NOT_TEST_INPUTS: [&str; 2] = ["harness/evidence/", "docs/"];

/// The SHA-256 of every tracked file as the working tree holds it, and of the
/// untracked files git does not ignore, but [`NOT_TEST_INPUTS`].
pub(super) fn sources_digest(root: &Path) -> R<String> {
    let input = |path: &str| !NOT_TEST_INPUTS.iter().any(|p| path.starts_with(p));
    let mut digest = sha256::Sha256::default();
    for line in git(root, &["ls-files", "-s"])?.lines() {
        if line.split_once('\t').is_some_and(|(_, path)| input(path)) {
            digest.update(line.as_bytes());
            digest.update(b"\n");
        }
    }
    let changed = git(root, &["diff", "HEAD", "--name-only", "-z"])?;
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for path in changed
        .split('\0')
        .chain(untracked.split('\0'))
        .filter(|p| !p.is_empty() && input(p))
    {
        let file = root.join(path);
        let sha = if file.is_file() {
            sha256::file_hex(&file).map_err(|e| format!("{}: {e}", file.display()))?
        } else {
            "absent".to_owned()
        };
        digest.update(format!("{sha}  {path}\n").as_bytes());
    }
    Ok(digest.finish_hex())
}

/// The SHA-256 of every file under `dir` by relative path, or `absent`.
pub(super) fn tree_digest(dir: &Path) -> R<String> {
    if !dir.is_dir() {
        return Ok("absent".into());
    }
    let mut digest = sha256::Sha256::default();
    for (relative, path) in crate::evidence::files(dir)? {
        let sha = sha256::file_hex(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        digest.update(format!("{sha}  {relative}\n").as_bytes());
    }
    Ok(digest.finish_hex())
}
