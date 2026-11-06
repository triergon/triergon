//! Repository tasks: `cargo xtask <command>`.
//!
//! Standard library only, and independent of the library crates, so the checks
//! and generators stay a separate implementation from the code they test.
#![forbid(unsafe_code)]

mod archive;
mod checks;
mod corpus;
mod ebml;
mod evidence;
mod fate;
mod fetch;
mod fuzz;
mod harness;
mod json;
mod prepare;
mod pyrandom;
mod sha256;
mod stats;
mod toml;
mod walk;
mod zlib;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "usage: cargo xtask <command>

commands:
  check          everything CI runs: format, tests, lints, builds and the checks below
  check-simd     clippy and tests of the opt-in nightly HEVC SIMD lane
  check-links    every relative link in the Markdown files resolves
  check-design   harness data files parse and recorded gates keep their values
  check-safety   owned packages only, no build scripts, no unsafe code
  fixtures       generate the authored Matroska corpus into .research/ and its manifests
  fixture        write one corpus variant (see `cargo xtask fixture --help`)
  prepare-ffmpeg [--jobs N] [--profile matroska|formats|video]
                 build the pinned FFmpeg reference into .research/reference[-formats|-video]
  fetch-specs    [--record] cache the specification texts and verify specs/sources.json
  fetch-vectors  cache the required external test vectors and verify their hashes
  external-vector --output DIR [--pairs N] [--iterations N] [--benchmark] [--memory]
                 qualify the mandatory external metadata vector through the same gates
  fate-run       --case NAME [--demux-only]   run one supported upstream FATE case
  trace-identities --output F [--compare F] [--candidate W]
                 record the digest of every candidate trace over all recorded cases
  evidence pack DIR OUT       deterministic .tar.gz of DIR; prints its SHA-256
  evidence fetch SET [DIR]    fetch, verify and unpack an archived evidence set
                              (harness/evidence/index.json; TRIERGON_EVIDENCE_ARCHIVE)
  fuzz           --manifest F --output DIR [--rounds N] [--seed N] [--maximum-input-bytes N]
                 [--maximum-output-bytes N] [--timeout-seconds S]
                 deterministic, replayable mutation campaign through JSON workers
  graph          PACKAGE [FILE]...   print the provenance graph of a workspace package
  build-workers  build the candidate and reference workers and the memory observers, and
                 record their identity in target/harness/identity.json
  report         [--contract F] [--manifest F] [--corpus DIR] [--output DIR] [--fate-cache DIR]
                 [--additional-report F]... [--require-acceptance] [--require-slice-acceptance]
                 [--benchmark] [--memory] [--pairs N] [--iterations N] [--seed N]
                 correctness, paired runtime and memory report of the built workers for one contract
  compare        --candidate W --reference W --input FILE --output DIR
                 [--operation plain|seek|colour|catalogue] [--content] [--no-short-reads]
                 compare one input across a candidate and a reference worker
  fate-inventory --source DIR --output FILE [--configured-list FILE]
                 index FATE definitions as data; nothing is executed";

fn main() -> ExitCode {
    if let Some(code) = without_cargo_library_path() {
        return code;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = root();
    let result = match args.first().map(String::as_str) {
        Some("check") => check(&root),
        Some("check-simd") => check_simd(&root),
        Some("check-links") => check_links(&root),
        Some("check-design") => check_design(&root),
        Some("check-safety") => check_safety(&root),
        Some("fixtures") => fixtures(&root),
        Some("fixture") => fixture(&root, &args[1..]),
        Some("fate-inventory") => fate_inventory(&args[1..]),
        Some("compare") => compare(&root, &args[1..]),
        Some("report") => return report(&root, &args[1..]),
        Some("external-vector") => return external_vector(&root, &args[1..]),
        Some("fate-run") => return fate_run(&root, &args[1..]),
        Some("graph") => graph(&root, &args[1..]),
        Some("fuzz") => return fuzz_campaign(&root, &args[1..]),
        Some("evidence") => evidence(&root, &args[1..]),
        Some("trace-identities") => return trace_identities(&root, &args[1..]),
        Some("build-workers") => {
            harness::build::build(&root).map(|path| println!("{}", path.display()))
        }
        Some("prepare-ffmpeg") => prepare_ffmpeg(&root, &args[1..]),
        Some("fetch-specs") => match &args[1..] {
            [] => fetch::specs(&root, false, &fetch::Network).map(|s| println!("{s}")),
            [flag] if flag == "--record" => {
                fetch::specs(&root, true, &fetch::Network).map(|s| println!("{s}"))
            }
            _ => Err(USAGE.to_owned()),
        },
        Some("fetch-vectors") => {
            fetch::required_vectors(&root, &fetch::Network, fetch::VECTOR_LIMIT)
                .map(|verified| println!("Verified required vectors: {}", verified.join(", ")))
        }
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

/// `cargo run` (and the rustup proxy in front of it) prepends build and
/// toolchain library directories to `LD_LIBRARY_PATH`. Workers and the allocator observer must load exactly as
/// they would from a shell, so run once more without those entries.
fn without_cargo_library_path() -> Option<ExitCode> {
    std::env::var_os("CARGO")?;
    let current = std::env::var_os("LD_LIBRARY_PATH")?;
    let exe = std::env::current_exe().ok()?;
    let toolchains =
        std::env::var_os("RUSTUP_HOME").map(|home| Path::new(&home).join("toolchains"));
    let kept = shell_library_path(&current, exe.parent()?, toolchains.as_deref());
    if kept.as_ref() == Some(&current) {
        return None;
    }
    let mut command = Command::new(&exe);
    command.args(std::env::args_os().skip(1));
    match kept {
        Some(value) => command.env("LD_LIBRARY_PATH", value),
        None => command.env_remove("LD_LIBRARY_PATH"),
    };
    Some(match command.status() {
        Ok(status) => status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .map_or(ExitCode::FAILURE, ExitCode::from),
        Err(error) => {
            eprintln!("{}: {error}", exe.display());
            ExitCode::FAILURE
        }
    })
}

/// `value` without the entries `cargo run` and rustup add: the running
/// binary's output directory and everything below it, toolchain
/// `lib/rustlib/*/lib` directories and anything under rustup's `toolchains`.
/// `None` when nothing remains.
fn shell_library_path(value: &OsStr, built: &Path, toolchains: Option<&Path>) -> Option<OsString> {
    let kept: Vec<PathBuf> = std::env::split_paths(value)
        .filter(|path| {
            !path.starts_with(built)
                && !toolchains.is_some_and(|t| path.starts_with(t))
                && !path.components().any(|c| c.as_os_str() == "rustlib")
        })
        .collect();
    if kept.is_empty() {
        None
    } else {
        std::env::join_paths(kept).ok()
    }
}

fn evidence(root: &Path, args: &[String]) -> Task {
    match args {
        [command, dir, out] if command == "pack" => {
            println!(
                "{}",
                evidence::pack_command(Path::new(dir), Path::new(out))?
            );
            Ok(())
        }
        [command, set, rest @ ..] if command == "fetch" && rest.len() <= 1 => {
            let into = rest.first().map(Path::new);
            println!("{}", evidence::fetch(root, set, into)?);
            Ok(())
        }
        _ => Err("usage: evidence pack DIR OUT | evidence fetch SET [DIR]".into()),
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

fn fixtures(root: &Path) -> Task {
    let paths = corpus::Paths {
        root: root.to_path_buf(),
        out: root.to_path_buf(),
    };
    for line in corpus::generate_all(&paths)? {
        println!("{line}");
    }
    Ok(())
}

fn exit(result: Result<i32, String>) -> ExitCode {
    match result {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(2)
        }
    }
}

fn external_vector(root: &Path, args: &[String]) -> ExitCode {
    let mut options = harness::vectors::ExternalOptions {
        output: PathBuf::new(),
        pairs: 30,
        iterations: 50,
        benchmark: false,
        memory: false,
    };
    let mut rest = args.iter();
    let parsed = (|| -> Task {
        while let Some(flag) = rest.next() {
            let mut value = || {
                rest.next()
                    .cloned()
                    .ok_or_else(|| format!("{flag} needs a value"))
            };
            match flag.as_str() {
                "--output" => options.output = PathBuf::from(value()?),
                "--pairs" => {
                    options.pairs = value()?.parse().map_err(|_| "--pairs: not a number")?;
                }
                "--iterations" => {
                    options.iterations =
                        value()?.parse().map_err(|_| "--iterations: not a number")?;
                }
                "--benchmark" => options.benchmark = true,
                "--memory" => options.memory = true,
                _ => return Err(USAGE.to_owned()),
            }
        }
        if options.output.as_os_str().is_empty() {
            Err("--output is required".into())
        } else {
            Ok(())
        }
    })();
    exit(parsed.and_then(|()| harness::vectors::external_vector(root, &options)))
}

fn fate_run(root: &Path, args: &[String]) -> ExitCode {
    let (mut case, mut demux_only) = ("fate-matroska-xiph-lacing".to_owned(), false);
    let mut rest = args.iter();
    let parsed = (|| -> Task {
        while let Some(flag) = rest.next() {
            match flag.as_str() {
                "--case" => case = rest.next().cloned().ok_or("--case needs a value")?,
                "--demux-only" => demux_only = true,
                _ => return Err(USAGE.to_owned()),
            }
        }
        Ok(())
    })();
    exit(parsed.and_then(|()| {
        harness::vectors::fate_run(root, &case, demux_only, &harness::vectors::NativeFate)
    }))
}

fn trace_identities(root: &Path, args: &[String]) -> ExitCode {
    let (mut output, mut compare, mut candidate) =
        (None, None, root.join("target/release/media-worker"));
    let mut rest = args.iter();
    let parsed = (|| -> Task {
        while let Some(flag) = rest.next() {
            let value = PathBuf::from(rest.next().ok_or_else(|| format!("{flag} needs a value"))?);
            match flag.as_str() {
                "--output" => output = Some(value),
                "--compare" => compare = Some(value),
                "--candidate" => candidate = value,
                _ => return Err(USAGE.to_owned()),
            }
        }
        Ok(())
    })();
    exit(parsed.and_then(|()| {
        let output = output.ok_or("--output is required")?;
        harness::vectors::trace_identities(root, &candidate, &output, compare.as_deref())
    }))
}

fn fuzz_campaign(root: &Path, args: &[String]) -> ExitCode {
    let mut campaign = fuzz::Campaign {
        manifest: PathBuf::new(),
        output: PathBuf::new(),
        rounds: 4,
        seed: 280,
        maximum_input_bytes: 4 << 20,
        maximum_output_bytes: 8 << 20,
        timeout_seconds: 3.0,
    };
    let mut rest = args.iter();
    let parsed = (|| -> Task {
        while let Some(flag) = rest.next() {
            let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
            let number = || {
                value
                    .parse::<u64>()
                    .map_err(|_| format!("{flag}: not a number: {value}"))
            };
            match flag.as_str() {
                "--manifest" => campaign.manifest = PathBuf::from(value),
                "--output" => campaign.output = PathBuf::from(value),
                "--rounds" => campaign.rounds = number()? as usize,
                "--seed" => campaign.seed = number()?,
                "--maximum-input-bytes" => campaign.maximum_input_bytes = number()? as usize,
                "--maximum-output-bytes" => campaign.maximum_output_bytes = number()? as usize,
                "--timeout-seconds" => {
                    campaign.timeout_seconds =
                        value.parse().map_err(|_| format!("{flag}: not a number"))?;
                }
                _ => return Err(USAGE.to_owned()),
            }
        }
        Ok(())
    })();
    exit(parsed.and_then(|()| fuzz::campaign(root, &campaign)))
}

fn graph(root: &Path, args: &[String]) -> Task {
    let (package, files) = args.split_first().ok_or("graph needs a package name")?;
    let metadata = harness::provenance::cargo_metadata(root)?;
    let graph = harness::provenance::selected_graph(root, &metadata, package, files)?;
    println!("{}", json::to_python(&graph, None));
    Ok(())
}

fn report(root: &Path, args: &[String]) -> ExitCode {
    let mut options = harness::report::Options::defaults(root);
    let mut rest = args.iter();
    let parsed = (|| -> Task {
        while let Some(flag) = rest.next() {
            let mut value = || {
                rest.next()
                    .map(PathBuf::from)
                    .ok_or_else(|| format!("{flag} needs a value"))
            };
            match flag.as_str() {
                "--contract" => options.contract = value()?,
                "--manifest" => options.manifest = value()?,
                "--corpus" => options.corpus = value()?,
                "--output" => options.output = value()?,
                "--fate-cache" => options.fate_cache = Some(value()?),
                "--additional-report" => options.additional_reports.push(value()?),
                "--require-acceptance" => options.require_acceptance = true,
                "--require-slice-acceptance" => options.require_slice_acceptance = true,
                "--benchmark" => options.benchmark = true,
                "--memory" => options.memory = true,
                "--pairs" | "--iterations" | "--seed" => {
                    let text = value()?.display().to_string();
                    let number: u64 = text
                        .parse()
                        .map_err(|_| format!("{flag}: not a number: {text}"))?;
                    match flag.as_str() {
                        "--pairs" => options.pairs = number as usize,
                        "--iterations" => {
                            options.iterations =
                                Some(u32::try_from(number).map_err(|e| e.to_string())?);
                        }
                        _ => options.seed = number,
                    }
                }
                _ => return Err(USAGE.to_owned()),
            }
        }
        Ok(())
    })();
    let result = parsed.and_then(|()| harness::report::run(root, &options));
    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(2)
        }
    }
}

fn compare(root: &Path, args: &[String]) -> Task {
    let mut values = std::collections::BTreeMap::new();
    let (mut content, mut sequential) = (false, true);
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        match flag.as_str() {
            "--content" => content = true,
            "--no-short-reads" => sequential = false,
            "--candidate" | "--reference" | "--input" | "--output" | "--operation" => {
                values.insert(
                    flag.clone(),
                    rest.next()
                        .ok_or_else(|| format!("{flag} needs a value"))?
                        .clone(),
                );
            }
            _ => return Err(USAGE.to_owned()),
        }
    }
    let path = |key: &str| {
        values
            .get(key)
            .map(PathBuf::from)
            .ok_or_else(|| format!("{key} is required"))
    };
    let (candidate, reference, input) =
        (path("--candidate")?, path("--reference")?, path("--input")?);
    let run = harness::parity::Run {
        root,
        candidate: &candidate,
        reference: &reference,
        input: &input,
        raw: path("--output")?,
    };
    use harness::parity::{catalogue_parity, colour_parity, parity, seek_parity};
    let result = match values.get("--operation").map_or("plain", String::as_str) {
        "plain" => parity(&run, sequential, None, content, "")?,
        "seek" => seek_parity(&run, sequential, None, content)?,
        "colour" => colour_parity(&run, sequential, None, content)?,
        "catalogue" => catalogue_parity(&run, sequential, None, content)?,
        other => return Err(format!("unknown operation {other}")),
    };
    corpus::write_json(&run.raw.join("result.json"), &result)?;
    println!(
        "{}",
        result
            .get("status")
            .and_then(json::Value::as_str)
            .unwrap_or("?")
    );
    Ok(())
}

fn fate_inventory(args: &[String]) -> Task {
    let (mut source, mut output, mut configured) = (None, None, None);
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = PathBuf::from(rest.next().ok_or_else(|| format!("{flag} needs a value"))?);
        match flag.as_str() {
            "--source" => source = Some(value),
            "--output" => output = Some(value),
            "--configured-list" => configured = Some(value),
            _ => return Err(USAGE.to_owned()),
        }
    }
    let (source, output) = (
        source.ok_or("--source is required")?,
        output.ok_or("--output is required")?,
    );
    let mut report = fate::inventory(&source)?;
    if let Some(list) = configured {
        fate::annotate(
            &mut report,
            &std::fs::read(&list).map_err(|e| format!("{}: {e}", list.display()))?,
        )?;
    }
    corpus::write_json(&output, &report)?;
    let count = report
        .get("cases")
        .and_then(json::Value::as_array)
        .map_or(0, <[_]>::len);
    println!("Indexed {count} identities; none executed or accepted.");
    Ok(())
}

fn prepare_ffmpeg(root: &Path, args: &[String]) -> Task {
    let (mut jobs, mut profile) = (2, prepare::Profile::Matroska);
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--jobs" => jobs = value.parse().map_err(|_| "jobs must be 1..8")?,
            "--profile" if value == "matroska" => profile = prepare::Profile::Matroska,
            "--profile" if value == "formats" => profile = prepare::Profile::Formats,
            "--profile" if value == "video" => profile = prepare::Profile::Video,
            _ => return Err(USAGE.to_owned()),
        }
    }
    println!(
        "{}",
        prepare::prepare(root, jobs, profile, &fetch::Network)?
    );
    Ok(())
}

const FIXTURE_USAGE: &str = "usage: cargo xtask fixture <family> [options] --output FILE

families and options:
  zlib     [--kind stored|fixed|dynamic] [--scope N] [--lace 0-3] [--aac] [--mixed] [--empty]
  header   [--scope N] [--lace] [--aac]
  colour   --mastering-width 4|8 --values V0,...,V9   (empty Colour with one MasteringMetadata)";

/// One corpus variant, for harness tests that need cases beyond the manifests.
fn fixture(root: &Path, args: &[String]) -> Task {
    let family = args.first().map(String::as_str).ok_or(FIXTURE_USAGE)?;
    let mut options = std::collections::BTreeMap::new();
    let mut rest = args[1..].iter();
    while let Some(flag) = rest.next() {
        let name = flag
            .strip_prefix("--")
            .ok_or_else(|| format!("unexpected argument {flag}\n{FIXTURE_USAGE}"))?;
        if name == "help" {
            return Err(FIXTURE_USAGE.to_owned());
        }
        let switch = matches!(
            (family, name),
            ("zlib", "aac" | "mixed" | "empty") | ("header", "lace" | "aac")
        );
        let value = if switch {
            String::new()
        } else {
            rest.next()
                .ok_or_else(|| format!("--{name} needs a value"))?
                .clone()
        };
        options.insert(name.to_owned(), value);
    }
    let number = |key: &str, default: u64| -> Result<u64, String> {
        options.get(key).map_or(Ok(default), |v| {
            v.parse().map_err(|_| format!("--{key}: not a number: {v}"))
        })
    };
    let flag = |key: &str| options.contains_key(key);
    let output = PathBuf::from(options.get("output").ok_or("--output is required")?);
    let source = corpus::Source::load(root)?;
    let data = match family {
        "zlib" => {
            let kind = match options.get("kind").map_or("dynamic", String::as_str) {
                "stored" => corpus::Kind::Stored,
                "fixed" => corpus::Kind::Fixed,
                "dynamic" => corpus::Kind::Dynamic,
                other => return Err(format!("unknown zlib kind {other}")),
            };
            let case = corpus::ZlibCase {
                kind,
                scope: number("scope", 1)?,
                lace: u8::try_from(number("lace", 0)?).map_err(|e| e.to_string())?,
                aac: flag("aac"),
                mixed: flag("mixed"),
                empty: flag("empty"),
            };
            corpus::zlib_case(&source, case)?
        }
        "header" => {
            corpus::header_removal_case(&source, number("scope", 1)?, flag("lace"), flag("aac"))?
        }
        "colour" => {
            let width = number("mastering-width", 8)? as usize;
            let values = options
                .get("values")
                .ok_or("--values is required")?
                .split(',')
                .map(|v| {
                    v.trim()
                        .parse::<f64>()
                        .map_err(|_| format!("--values: not a number: {v}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            corpus::colour_case(
                &source,
                Some(&Vec::new()),
                &corpus::mastering(width, false, &values),
                1,
            )?
        }
        _ => return Err(FIXTURE_USAGE.to_owned()),
    };
    std::fs::write(&output, data).map_err(|e| format!("{}: {e}", output.display()))
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
    let features = "triergon-codec-hevc/simd,codec-worker/simd";
    cargo(
        root,
        Some("+nightly"),
        &[
            "clippy",
            "-p",
            "triergon-codec-hevc",
            "-p",
            "codec-worker",
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
            "test",
            "-p",
            "triergon-mpegts",
            "--features",
            "reference-trace",
            "--locked",
        ],
    )?;
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
    run(Command::new("cargo")
        .args(["doc", "--workspace", "--no-deps", "--locked"])
        .env("CARGO_INCREMENTAL", "0")
        .env("RUSTDOCFLAGS", "-D warnings")
        .current_dir(root))?;
    cargo(
        root,
        None,
        &["build", "--workspace", "--release", "--locked"],
    )?;
    cargo(
        root,
        None,
        &[
            "clippy",
            "-p",
            "triergon-mpegts",
            "--features",
            "reference-trace",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
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

#[cfg(test)]
mod tests {
    use super::shell_library_path;
    use std::ffi::OsStr;
    use std::path::Path;

    #[test]
    fn cargo_library_entries_are_dropped_and_others_kept() {
        let built = Path::new("/w/target/debug");
        let toolchains = Some(Path::new("/h/.rustup/toolchains"));
        let cargo = "/w/target/debug:/w/target/debug/deps:\
                     /h/.rustup/toolchains/t/lib/rustlib/aarch64-unknown-linux-gnu/lib:\
                     /h/.rustup/toolchains/t/lib";
        assert_eq!(
            shell_library_path(OsStr::new(cargo), built, toolchains),
            None
        );
        let mixed = format!("{cargo}:/opt/lib:/usr/local/lib");
        assert_eq!(
            shell_library_path(OsStr::new(&mixed), built, toolchains).unwrap(),
            OsStr::new("/opt/lib:/usr/local/lib")
        );
        let user = OsStr::new("/opt/lib");
        assert_eq!(shell_library_path(user, built, toolchains).unwrap(), user);
        assert_eq!(shell_library_path(user, built, None).unwrap(), user);
    }
}
