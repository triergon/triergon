//! Repository tasks: `cargo xtask <command>`.
//!
//! Standard library only, and independent of the library crates, so the checks
//! and generators stay a separate implementation from the code they test.
#![forbid(unsafe_code)]

mod checks;
mod json;
mod walk;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "usage: cargo xtask <command>

commands:
  check          everything CI runs: format, tests, lints, builds and the checks below
  check-simd     clippy and tests of the opt-in nightly HEVC SIMD lane
  check-links    every relative link in the Markdown files resolves
  check-design   harness data files parse and recorded gates keep their values
  check-safety   owned packages only, no build scripts, no unsafe code";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = root();
    let result = match args.first().map(String::as_str) {
        Some("check") => check(&root),
        Some("check-simd") => check_simd(&root),
        Some("check-links") => check_links(&root),
        Some("check-design") => check_design(&root),
        Some("check-safety") => check_safety(&root),
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        _ => Err(USAGE.to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// The workspace root: the parent of this package's manifest directory.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .to_path_buf()
}

type Task = Result<(), String>;

fn check_links(root: &Path) -> Task {
    let errors = checks::links(root);
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    println!("Local Markdown links OK");
    Ok(())
}

fn check_design(root: &Path) -> Task {
    let summary = checks::design(root).map_err(|errors| errors.join("\n"))?;
    println!("{summary}");
    Ok(())
}

fn check_safety(root: &Path) -> Task {
    println!("{}", checks::safety(root)?);
    Ok(())
}

fn cargo(root: &Path, toolchain: Option<&str>, args: &[&str]) -> Task {
    let mut command = Command::new("cargo");
    if let Some(toolchain) = toolchain {
        command.arg(toolchain);
    }
    run(command
        .args(args)
        .env("CARGO_INCREMENTAL", "0")
        .current_dir(root))
}

/// Run with the pinned FFmpeg reference first on `PATH`, as the harness expects.
fn run(command: &mut Command) -> Task {
    let reference = root().join(".research/reference/install/bin");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(std::iter::once(reference).chain(std::env::split_paths(&path)))
        .map_err(|e| e.to_string())?;
    command.env("PATH", path);
    let shown = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    println!("$ {shown}");
    let status = command.status().map_err(|e| format!("{shown}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{shown}: {status}"))
    }
}

/// The opt-in nightly `std::simd` lane of the HEVC decoder must decode exactly as
/// the scalar default does (the conformance tests compare both with FFmpeg digests).
fn check_simd(root: &Path) -> Task {
    let features = "triergon-codec-hevc/simd,triergon-sim/codecs";
    cargo(
        root,
        Some("+nightly"),
        &[
            "clippy",
            "-p",
            "triergon-codec-hevc",
            "-p",
            "triergon-sim",
            "--features",
            features,
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    cargo(
        root,
        Some("+nightly"),
        &[
            "test",
            "-p",
            "triergon-codec-hevc",
            "--features",
            "simd",
            "--release",
            "--locked",
        ],
    )
}

fn check(root: &Path) -> Task {
    cargo(root, None, &["fmt", "--all", "--check"])?;
    cargo(root, None, &["test", "--workspace", "--locked"])?;
    cargo(
        root,
        None,
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    cargo(
        root,
        None,
        &["check", "--workspace", "--no-default-features", "--locked"],
    )?;
    cargo(
        root,
        None,
        &["build", "--workspace", "--release", "--locked"],
    )?;
    check_simd(root)?;
    run(Command::new("python3")
        .args(["-m", "unittest", "discover", "-s", "harness", "-t", "."])
        .current_dir(root))?;
    check_safety(root)?;
    check_design(root)?;
    check_links(root)?;
    run(Command::new("python3")
        .arg("tools/test-core-time.py")
        .current_dir(root))
}
