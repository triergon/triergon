//! Check the core arithmetic example against the independent integer and Fraction generators.
use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::harness::{manifest, provenance};
use crate::json::{self, Value};
use crate::worker_tests::items;
use crate::{corpus, process, python, sha256};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const USAGE: &str = "cargo xtask core-time [--output DIR] [--profiles debug release] [--toolchain NAME] [--target TRIPLE]";
const BASE: &str = "tools/generate-time-oracle.py";
const EDGE: &str = "tools/generate-time-edge-oracle.py";
const SCOPE: &str =
    "sampled exact arithmetic correctness; no FFmpeg/performance/complete-module qualification";

struct Suite {
    name: &'static str,
    rows: usize,
    vectors: &'static str,
    expected: &'static str,
}

// Identity of the two generators' complete, ordered TSV streams.
const BASE_SUITES: [Suite; 2] = [
    Suite {
        name: "time",
        rows: 34502,
        vectors: "8d66716976f0af005b4497fbc0447466a0dd39551c3eccc6f3c54dd9fb75f55e",
        expected: "5f966dfe448cbce6d5e03bd0dacde552192d924df318e3e7c06a64e07c3d7f79",
    },
    Suite {
        name: "wide",
        rows: 15024,
        vectors: "2a9e6b526fa4e5cb3ffe5cab9439215882aec3bb17b3934ff57f4677b940962e",
        expected: "ff2e542367b0af768d2863bccbe5aa4c2122425f490c25b76ffa5cc277c34037",
    },
];
const EDGE_SUITES: [Suite; 2] = [
    Suite {
        name: "time",
        rows: 1280,
        vectors: "354787e20131dc8eda2dacea3fb232cfa40e025b28079d10c8096c1f3848293c",
        expected: "640e3fb9f21622392602c7d4d542acffcfe09940fff976bd1133617c482bab5e",
    },
    Suite {
        name: "wide",
        rows: 429,
        vectors: "1d5503368ecc03ddba94b4f97865327852e41ec3606e789495b69b517298ab25",
        expected: "aa1688251d1bf0b292c5453b01b71250c048b4dc95f6675fb7c7c9a2866597ad",
    },
];

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string {key}").into())
}

fn check_hash(path: &Path, digest: &str) -> Result<()> {
    if sha256::file_hex(path)? != digest {
        return Err(format!("SHA-256 mismatch: {}", path.display()).into());
    }
    Ok(())
}

fn graph(root: &Path, metadata: &Value) -> Result<Value> {
    let inputs = [
        BASE,
        EDGE,
        "tools/test-core-time.py",
        "xtask/src/core_time.rs",
        "xtask/src/harness/provenance.rs",
    ]
    .map(str::to_owned);
    Ok(provenance::selected_graph(
        root,
        metadata,
        "triergon-core",
        &inputs,
    )?)
}

fn freeze_sources(root: &Path, out: &Path, graph: &Value) -> Result<()> {
    let packages = items(graph, "packages")?;
    for files in packages
        .iter()
        .map(|p| p.get("files"))
        .chain([graph.get("shared_files")])
    {
        let Some(Value::Object(files)) = files else {
            return Err("missing graph files".into());
        };
        for (name, digest) in files {
            let path = out.join("source").join(name);
            fs::create_dir_all(path.parent().ok_or("missing source parent")?)?;
            fs::copy(root.join(name), &path)?;
            check_hash(&path, digest.as_str().ok_or("missing file digest")?)?;
        }
    }
    Ok(())
}

fn generate(root: &Path, generator: &str, dir: &Path) -> Result<()> {
    Ok(process::run(
        Command::new(python::interpreter())
            .current_dir(root)
            .args([generator, "--output"])
            .arg(dir),
    )?)
}

// Publish only a complete v2 corpus. Concurrent first runs verify the winner.
fn edge_corpus(root: &Path) -> Result<PathBuf> {
    let cache = root.join(".research/core-time-edge-oracle");
    let dir = cache.join("v2");
    if !dir.exists() {
        fs::create_dir_all(&cache)?;
        let staging = process::TempDir::new(Some(&cache))?;
        generate(root, EDGE, &staging.0)?;
        match fs::rename(&staging.0, &dir) {
            Ok(()) => return Ok(dir),
            Err(e) if !dir.exists() => return Err(e.into()),
            Err(_) => {}
        }
    }
    generate(root, EDGE, &dir)?;
    Ok(dir)
}

struct Input {
    name: String,
    vectors: Vec<u8>,
    expected: String,
    pin: &'static Suite,
}

// Keep the exact input and expected bytes checked against the manifest for both profiles.
fn inputs(
    dir: &Path,
    prefix: &str,
    suites: &'static [Suite],
    manifest: &Value,
) -> Result<Vec<Input>> {
    let counts = Value::object(suites.iter().map(|s| (s.name, Value::int(s.rows as i128))));
    if manifest.get("row_counts") != Some(&counts) {
        return Err(format!("unexpected corpus coverage: {}", dir.display()).into());
    }
    let mut files = BTreeMap::new();
    for entry in items(manifest, "files")? {
        let name = text(entry, "path")?;
        if files.insert(name, text(entry, "sha256")?).is_some() {
            return Err(format!("duplicate oracle file {name}").into());
        }
    }
    let mut inputs = Vec::new();
    for pin in suites {
        let vectors = format!("vectors-{}.tsv", pin.name);
        let expected = format!("expected-{}.tsv", pin.name);
        if files.remove(vectors.as_str()) != Some(pin.vectors)
            || files.remove(expected.as_str()) != Some(pin.expected)
        {
            return Err(format!("missing or changed TSV identity: {prefix}{}", pin.name).into());
        }
        let vectors = fs::read(dir.join(vectors))?;
        if sha256::hex(&vectors) != pin.vectors {
            return Err(format!("vector-file hash mismatch: {prefix}{}", pin.name).into());
        }
        let expected = fs::read_to_string(dir.join(expected))?;
        if sha256::hex(expected.as_bytes()) != pin.expected {
            return Err(format!("expected-file hash mismatch: {prefix}{}", pin.name).into());
        }
        inputs.push(Input {
            name: format!("{prefix}{}", pin.name),
            vectors,
            expected,
            pin,
        });
    }
    for (name, digest) in files {
        check_hash(&dir.join(name), digest)?;
    }
    Ok(inputs)
}

struct Compiler {
    toolchain: String,
    path: PathBuf,
    version: String,
    target: String,
}

impl Compiler {
    fn resolve(root: &Path, toolchain: &str, target: Option<String>) -> Result<Self> {
        let which = process::output(
            Command::new("rustup")
                .current_dir(root)
                .args(["which", "--toolchain", toolchain, "rustc"])
                .stderr(Stdio::inherit()),
        )?;
        let path = PathBuf::from(which.trim_end());
        let version = process::output(Command::new(&path).arg("-vV"))?;
        let host = version
            .lines()
            .find_map(|l| l.strip_prefix("host: "))
            .ok_or("rustc reports no host")?;
        let target = target.unwrap_or_else(|| host.to_owned());
        Ok(Self {
            toolchain: toolchain.to_owned(),
            path,
            version,
            target,
        })
    }

    fn cargo(&self, root: &Path) -> Command {
        let mut cargo = Command::new("cargo");
        cargo
            .current_dir(root)
            .arg(format!("+{}", self.toolchain))
            .env("CARGO_INCREMENTAL", "0")
            .env("RUSTC", &self.path)
            .env("RUSTC_WRAPPER", "")
            .env("RUSTC_WORKSPACE_WRAPPER", "")
            .stderr(Stdio::inherit());
        cargo
    }

    fn metadata(&self, root: &Path) -> Result<Value> {
        let output = process::output(self.cargo(root).args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--offline",
            "--filter-platform",
            &self.target,
        ]))?;
        Ok(json::parse(&output).map_err(|e| e.to_string())?)
    }

    fn build(&self, root: &Path, profile: &str) -> Result<(PathBuf, Value)> {
        let mut command = self.cargo(root);
        command.args([
            "build",
            "-p",
            "triergon-core",
            "--example",
            "time-oracle",
            "--locked",
            "--message-format=json-render-diagnostics",
            "--target",
            &self.target,
        ]);
        command.args([
            "--profile",
            if profile == "debug" { "dev" } else { profile },
        ]);
        let argv = Value::strings(
            std::iter::once(command.get_program())
                .chain(command.get_args())
                .map(|s| s.to_string_lossy()),
        );
        let messages = process::output(&mut command)?;
        Ok((
            artifact(&messages, &root.join("crates/core/Cargo.toml"))?,
            argv,
        ))
    }
}

fn artifact(messages: &str, manifest: &Path) -> Result<PathBuf> {
    let mut probe = None;
    for line in messages.lines() {
        let event = json::parse(line).map_err(|e| e.to_string())?;
        if text(&event, "reason")? != "compiler-artifact" {
            continue;
        }
        let target = event.get("target").ok_or("Cargo artifact has no target")?;
        if Path::new(text(&event, "manifest_path")?) == manifest
            && text(target, "name")? == "time-oracle"
            && target.get("kind").and_then(Value::as_array) == Some(&[Value::str("example")])
        {
            if probe.is_some() {
                return Err("ambiguous time-oracle executable in Cargo output".into());
            }
            probe = Some(PathBuf::from(text(&event, "executable")?));
        }
    }
    probe.ok_or_else(|| "no time-oracle example executable in Cargo output".into())
}

fn run_probe(probe: &Path, input: &[u8], observed: &Path, stderr: &Path) -> Result<i64> {
    let mut child = Command::new(probe)
        .stdin(Stdio::piped())
        .stdout(File::create(observed)?)
        .stderr(File::create(stderr)?)
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("missing probe stdin")?;
    std::thread::scope(|scope| {
        // Feed in parallel so a probe that stops reading still reaches the deadline.
        let feed = scope.spawn(move || stdin.write_all(input));
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if start.elapsed() < Duration::from_secs(60) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                result => {
                    let _ = child.kill();
                    child.wait()?;
                    result?;
                    return Err(format!(
                        "{} timed out writing {}",
                        probe.display(),
                        observed.display()
                    )
                    .into());
                }
            }
        };
        let written = feed.join().map_err(|_| "probe input thread panicked")?;
        if status.success() {
            written?;
        }
        Ok(process::exit_code(status))
    })
}

fn comparison(expected: &str, actual: &str, code: i64, count: usize) -> (usize, Vec<Value>, bool) {
    if expected == actual {
        let rows = actual.lines().count();
        return (rows, Vec::new(), code == 0 && rows == count);
    }
    let mut left = expected.split_inclusive('\n');
    let mut right = actual.split_inclusive('\n');
    let mut differences = Vec::new();
    let mut rows = 0;
    loop {
        let pair = (left.next(), right.next());
        if pair == (None, None) {
            break;
        }
        rows += 1;
        if pair.0 != pair.1 {
            differences.push(Value::object([
                ("line", Value::int(rows as i128)),
                ("expected", pair.0.map_or(Value::Null, Value::str)),
                ("actual", pair.1.map_or(Value::Null, Value::str)),
            ]));
        }
    }
    (rows, differences, false)
}

fn profile(
    root: &Path,
    out: &Path,
    compiler: &Compiler,
    profile: &str,
    inputs: &[Input],
) -> Result<(Value, bool, bool)> {
    let (artifact, command) = compiler.build(root, profile)?;
    let probe = out.join(format!("{profile}-time-oracle"));
    fs::copy(&artifact, &probe)?;
    let executable = sha256::file_hex(&probe)?;
    // Cargo's artifact must still match the executable used for every row.
    let mut cases = Vec::new();
    let mut passed = true;
    for input in inputs {
        let name = &input.name;
        let observed = out.join(format!("{profile}-actual-{name}.tsv"));
        let stderr = out.join(format!("{profile}-stderr-{name}.txt"));
        let code = run_probe(&probe, &input.vectors, &observed, &stderr)?;
        let actual = fs::read_to_string(observed)?;
        let (rows, differences, ok) = comparison(&input.expected, &actual, code, input.pin.rows);
        passed &= ok;
        println!(
            "{profile}/{name}: {rows} cases, {} mismatches, exit {code}",
            differences.len()
        );
        let digest = if differences.is_empty() {
            input.pin.expected.into()
        } else {
            sha256::hex(actual.as_bytes())
        };
        let case = Value::object([
            ("status", Value::str(if ok { "pass" } else { "mismatch" })),
            ("rows", Value::int(rows as i128)),
            ("returncode", Value::int(code)),
            ("differences", Value::Array(differences)),
            ("expected_sha256", Value::str(input.pin.expected)),
            ("actual_sha256", Value::str(digest)),
            ("stderr_sha256", Value::str(sha256::file_hex(&stderr)?)),
        ]);
        cases.push((name.to_owned(), case));
    }
    let unchanged =
        sha256::file_hex(&artifact)? == executable && sha256::file_hex(&probe)? == executable;
    Ok((
        Value::object([
            ("cases", Value::object(cases)),
            ("probe_sha256", Value::str(executable)),
            ("build_command", command),
            ("cargo_executable", Value::str(artifact.to_string_lossy())),
            ("probe_unchanged", Value::Bool(unchanged)),
        ]),
        passed,
        unchanged,
    ))
}

pub fn main(root: &Path, args: &[String]) -> std::result::Result<(), String> {
    run(root, args).map_err(|e| e.to_string())
}

fn run(root: &Path, args: &[String]) -> Result<()> {
    let mut destination = root.join(".research/core-time-check");
    let mut toolchain = "nightly";
    let mut target = None;
    let mut profiles = vec!["debug", "release"];
    let mut args = args.iter().map(String::as_str).peekable();
    while let Some(arg) = args.next() {
        let (flag, first) = arg
            .split_once('=')
            .map_or((arg, None), |(flag, v)| (flag, Some(v)));
        let mut value = || {
            first
                .or_else(|| args.next())
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag {
            "--output" => destination = PathBuf::from(value()?),
            "--toolchain" => toolchain = value()?,
            "--target" => target = Some(value()?.to_owned()),
            "--profiles" => {
                profiles.clear();
                let first = value()?;
                for profile in std::iter::once(first)
                    .chain(std::iter::from_fn(|| args.next_if(|s| !s.starts_with('-'))))
                {
                    if !matches!(profile, "debug" | "release") {
                        return Err(format!("invalid profile {profile}").into());
                    }
                    if !profiles.contains(&profile) {
                        profiles.push(profile);
                    }
                }
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => return Err(USAGE.into()),
        }
    }
    let root = &root.canonicalize()?;
    let runs = destination.join("runs");
    fs::create_dir_all(&runs)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let out = runs
        .canonicalize()?
        .join(format!("{stamp}-{}", std::process::id()));
    fs::create_dir(&out)?;
    println!("Arithmetic run: {}", out.display());
    let compiler = Compiler::resolve(root, toolchain, target)?;
    let metadata = compiler.metadata(root)?;
    let before = graph(root, &metadata)?;
    freeze_sources(root, &out, &before)?;
    let base = out.join("oracle");
    generate(root, BASE, &base)?;
    let (baseline, baseline_hash) = manifest::read(root, &base.join("manifest.json"))?;
    let mut cases = inputs(&base, "", &BASE_SUITES, &baseline)?;
    let edge = edge_corpus(root)?;
    let (supplement, edge_hash) = manifest::read(root, &edge.join("manifest.json"))?;
    if text(&supplement, "corpus_version")? != "core-time-edge-v2" {
        return Err("unexpected edge corpus version".into());
    }
    cases.extend(inputs(&edge, "edge-", &EDGE_SUITES, &supplement)?);

    let mut unchanged = true;
    let mut passed = true;
    let mut results = Vec::new();
    for name in profiles {
        let (result, ok, same) = profile(root, &out, &compiler, name, &cases)?;
        results.push((name, result));
        passed &= ok;
        unchanged &= same;
    }
    unchanged &= before == graph(root, &compiler.metadata(root)?)?;
    passed &= unchanged;
    let status = if passed { "pass" } else { "fail" };
    let report = Value::object([
        ("schema", Value::int(2)),
        ("status", Value::str(status)),
        ("scope", Value::str(SCOPE)),
        ("selected_graph", before),
        ("source_and_probe_unchanged", Value::Bool(unchanged)),
        ("profiles", Value::object(results)),
        ("platform", Value::str(text(&baseline, "platform")?)),
        (
            "machine",
            Value::str(process::output(Command::new("uname").arg("-m"))?.trim_end()),
        ),
        ("rustc", Value::str(compiler.version)),
        ("rustc_path", Value::str(compiler.path.to_string_lossy())),
        ("toolchain", Value::str(compiler.toolchain)),
        ("target", Value::str(compiler.target)),
        (
            "target_directory",
            Value::str(text(&metadata, "target_directory")?),
        ),
        ("oracle_manifest_sha256", Value::str(baseline_hash)),
        ("edge_manifest_sha256", Value::str(edge_hash)),
        (
            "run_directory",
            Value::str(out.strip_prefix(root).unwrap_or(&out).to_string_lossy()),
        ),
    ]);
    let report_path = out.join("report.json");
    corpus::write_json(&report_path, &report)?;
    let latest = out.join("latest.tmp");
    fs::copy(report_path, &latest)?;
    fs::rename(latest, destination.join("report.json"))?;
    println!("exact arithmetic {status}");
    if passed {
        Ok(())
    } else {
        Err(format!("arithmetic check failed: {}", out.display()).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_include_every_missing_extra_and_partial_line_and_require_success() {
        let expected = "a\nb\n";
        assert!(comparison(expected, expected, 0, 2).2);
        assert!(!comparison(expected, expected, 3, 2).2);
        assert!(!comparison(expected, expected, 0, 3).2);
        for (actual, rows, missing_side) in [
            ("a\n", 2, Some("actual")),
            ("a\nb\nc\n", 3, Some("expected")),
            ("a\nb", 2, None),
        ] {
            let (count, differences, passed) = comparison(expected, actual, 0, 2);
            assert!(!passed);
            assert_eq!(count, rows);
            assert_eq!(differences.len(), 1);
            assert_eq!(differences[0].get("line"), Some(&Value::int(rows as i128)));
            if let Some(key) = missing_side {
                assert_eq!(differences[0].get(key), Some(&Value::Null));
            }
        }
        assert_eq!(comparison(expected, "x\ny\nz\n", 0, 2).1.len(), 3);
    }

    #[test]
    fn failed_edge_generation_removes_its_partial_input() -> Result<()> {
        let temp = process::TempDir::new(None)?;
        let root = &temp.0;
        fs::create_dir(root.join("tools"))?;
        fs::write(
            root.join(EDGE),
            "import pathlib, sys\n(pathlib.Path(sys.argv[-1]) / 'partial.tsv').touch()\nsys.exit(1)\n",
        )?;

        assert!(edge_corpus(root).is_err());
        let cache = root.join(".research/core-time-edge-oracle");
        assert_eq!(fs::read_dir(cache)?.count(), 0);
        Ok(())
    }

    #[test]
    fn both_profiles_receive_the_verified_vectors_after_path_replacement() -> Result<()> {
        let temp = process::TempDir::new(None)?;
        let dir = &temp.0;
        const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        const SUITE: Suite = Suite {
            name: "time",
            rows: 1,
            vectors: ABC,
            expected: ABC,
        };
        let mut files = Vec::new();
        for name in ["vectors-time.tsv", "expected-time.tsv"] {
            fs::write(dir.join(name), b"abc")?;
            files.push(Value::object([
                ("path", Value::str(name)),
                ("sha256", Value::str(ABC)),
            ]));
        }
        let manifest = Value::object([
            ("row_counts", Value::object([("time", Value::int(1))])),
            ("files", Value::Array(files)),
        ]);
        let inputs = inputs(dir, "", &[SUITE], &manifest)?;
        fs::write(dir.join("vectors-time.tsv"), b"replacement")?;
        let input = &inputs[0];
        for profile in ["debug", "release"] {
            let out = dir.join(profile);
            assert_eq!(
                run_probe(
                    Path::new("/bin/cat"),
                    &input.vectors,
                    &out,
                    &dir.join("err")
                )?,
                0
            );
            check_hash(&out, input.pin.vectors)?;
        }
        Ok(())
    }

    #[test]
    fn cargo_build_uses_named_compiler_and_reports_nondefault_target_directory() -> Result<()> {
        let temp = process::TempDir::new(None)?;
        let root = temp.0.canonicalize()?.join("workspace");
        let core = root.join("crates/core");
        fs::create_dir_all(core.join("examples"))?;
        fs::create_dir_all(root.join(".cargo"))?;
        fs::write(
            root.join(".cargo/config.toml"),
            "[build]\ntarget-dir = '../cargo-output'\n",
        )?;
        // A private profile keeps this tiny example disjoint from real builds even
        // when the invoking workspace requires a single shared CARGO_TARGET_DIR.
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers=['crates/core']\nresolver='3'\n[profile.core-time-test]\ninherits='dev'\n",
        )?;
        fs::write(
            core.join("Cargo.toml"),
            "[package]\nname='triergon-core'\nversion='0.0.0'\nedition='2024'\n",
        )?;
        fs::write(
            core.join("examples/time-oracle.rs"),
            r#"fn main() { println!("{}", env!("RUSTC")); }"#,
        )?;
        let name = crate::harness::gate_build::pinned_release(&crate::root())?;
        let compiler = Compiler::resolve(&root, &name, None)?;
        process::run(
            compiler
                .cargo(&root)
                .args(["generate-lockfile", "--offline"]),
        )?;
        let metadata = compiler.metadata(&root)?;
        let target_dir = Path::new(text(&metadata, "target_directory")?);
        let (probe, _) = compiler.build(&root, "core-time-test")?;
        assert!(!probe.starts_with(root.join("target")));
        assert!(probe.starts_with(target_dir));
        let rustc = process::output(&mut Command::new(probe))?;
        assert_eq!(Path::new(rustc.trim_end()), compiler.path);
        assert_eq!(
            compiler.version,
            crate::harness::gate_build::toolchain_compiler(&root, &name)?
        );
        Ok(())
    }
}
