//! `cargo xtask build-workers`: build the candidate worker, the FFmpeg reference
//! workers and the memory instrumentation, and record the build identity every
//! collection is later checked against (`target/harness/identity.json`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::linux;
use super::provenance::{cargo_metadata, matroska_inputs, selected_graph};
use crate::checks::read_json;
use crate::corpus::write_json;
use crate::json::Value;

type R<T> = Result<T, String>;

fn sha(path: &Path) -> R<String> {
    linux::file_sha(path)
}

fn output(program: &str, args: &[&str], env: &[(&str, String)]) -> R<String> {
    let mut command = Command::new(program);
    command.args(args);
    for (k, v) in env {
        command.env(k, v);
    }
    let out = command.output().map_err(|e| format!("{program}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program} {}: {}\n{}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}

fn run(argv: &[String]) -> R<()> {
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .map_err(|e| format!("{}: {e}", argv[0]))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{}: {status}", argv.join(" ")))
    }
}

/// POSIX shell word splitting of `pkg-config` output (quotes and backslashes).
pub fn shell_words(text: &str) -> R<Vec<String>> {
    let (mut words, mut word, mut active) = (Vec::new(), String::new(), false);
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if active {
                    words.push(std::mem::take(&mut word));
                    active = false;
                }
            }
            '\'' => {
                active = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("unterminated single quote".into()),
                    }
                }
            }
            '"' => {
                active = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('\\' | '"' | '$' | '`')) => word.push(c),
                            Some('\n') => {}
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("unterminated double quote".into()),
                        },
                        Some(c) => word.push(c),
                        None => return Err("unterminated double quote".into()),
                    }
                }
            }
            '\\' => {
                active = true;
                match chars.next() {
                    Some('\n') => {}
                    Some(c) => word.push(c),
                    None => return Err("trailing backslash".into()),
                }
            }
            c => {
                active = true;
                word.push(c);
            }
        }
    }
    if active {
        words.push(word);
    }
    Ok(words)
}

fn strings(argv: &[String]) -> Value {
    Value::strings(argv.iter().cloned())
}

fn instrumentation_commands(
    compiler: &str,
    root: &Path,
    out: &Path,
    linux: bool,
) -> Vec<Vec<String>> {
    let p = |path: PathBuf| path.display().to_string();
    let owned = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if linux {
        let mut observer = owned(&[
            compiler,
            "-std=c11",
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fno-builtin",
            "-Wl,-z,now",
            "-shared",
            "-fPIC",
        ]);
        observer.extend([
            p(root.join("harness/allocations-linux.c")),
            "-o".into(),
            p(out.join("allocations.so")),
            "-ldl".into(),
            "-pthread".into(),
        ]);
        let mut probe = owned(&[
            compiler,
            "-std=c11",
            "-O0",
            "-fno-builtin",
            "-Wall",
            "-Wextra",
            "-Werror",
        ]);
        probe.extend([
            p(root.join("harness/portable-memory-check.c")),
            "-o".into(),
            p(out.join("memory-check")),
            "-ldl".into(),
            "-pthread".into(),
        ]);
        vec![observer, probe]
    } else {
        let mut observer = owned(&[
            compiler,
            "-std=c11",
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-dynamiclib",
        ]);
        observer.extend([
            p(root.join("harness/allocations.c")),
            "-o".into(),
            p(out.join("allocations.dylib")),
        ]);
        let mut probe = owned(&[
            compiler,
            "-O0",
            "-fno-builtin",
            "-Wall",
            "-Wextra",
            "-Werror",
        ]);
        probe.extend([
            p(root.join("harness/memory-check.c")),
            "-o".into(),
            p(out.join("memory-check")),
        ]);
        vec![observer, probe]
    }
}

/// Darwin: the dynamic links of a worker and the OS-provided zlib it uses.
fn darwin_libraries(worker: &Path) -> R<Value> {
    let listing = output("otool", &["-L", &worker.display().to_string()], &[])?;
    let zlib = listing.lines().skip(1).find(|l| l.contains("/libz."));
    let mut info = vec![
        ("status", Value::str("recorded")),
        ("dynamic_links", Value::str(&listing)),
        (
            "os_build",
            Value::str(output("sw_vers", &["-buildVersion"], &[])?.trim()),
        ),
    ];
    if let Some(line) = zlib {
        let path = line.trim().split(" (").next().unwrap_or_default();
        let version = line
            .split("current version ")
            .nth(1)
            .and_then(|v| v.split(')').next())
            .unwrap_or_default();
        info.push((
            "system_zlib",
            Value::object([
                ("path", Value::str(path)),
                ("version", Value::str(version)),
                ("provenance", Value::str("OS shared-cache component identified by path, linked version and OS build; not a standalone content hash")),
            ]),
        ));
    }
    Ok(Value::object(info))
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

/// Measurement workers a contract may name as its candidate besides
/// `media-worker`, with the command that builds each.
pub const ADDITIONAL_CANDIDATES: [&str; 1] = ["playback-worker"];

fn additional_candidates(root: &Path) -> R<Vec<(&'static str, Vec<String>)>> {
    let mut built = Vec::new();
    for name in ADDITIONAL_CANDIDATES {
        let command: Vec<String> = ["cargo", "build", "-p", name, "--release", "--locked"]
            .map(String::from)
            .to_vec();
        let status = Command::new("cargo")
            .args(&command[1..])
            .env("CARGO_INCREMENTAL", "0")
            .current_dir(root)
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("{name} build failed: {status}"));
        }
        built.push((name, command));
    }
    Ok(built)
}

pub fn build(root: &Path) -> R<PathBuf> {
    let linux = cfg!(target_os = "linux");
    let candidate_command: Vec<String> = [
        "cargo",
        "build",
        "-p",
        "media-worker",
        "--release",
        "--locked",
    ]
    .map(String::from)
    .to_vec();
    let status = Command::new("cargo")
        .args(&candidate_command[1..])
        .env("CARGO_INCREMENTAL", "0")
        .current_dir(root)
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("candidate build failed: {status}"));
    }
    let additional = additional_candidates(root)?;
    let out = root.join("target/harness");
    fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let pinned = root.join(".research/reference/build.json");
    let mut env = Vec::new();
    if pinned.exists() {
        env.push((
            "PKG_CONFIG_LIBDIR",
            root.join(".research/reference/install/lib/pkgconfig")
                .display()
                .to_string(),
        ));
    }
    let flags = output(
        "pkg-config",
        &[
            "--static",
            "--cflags",
            "--libs",
            "libavformat",
            "libavcodec",
            "libavutil",
        ],
        &env,
    )?;
    let compiler = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let worker = out.join("ffmpeg-worker");
    let combined = out.join("ffmpeg-combined-worker");
    let mut command: Vec<String> = vec![
        compiler.clone(),
        "-O3".into(),
        "-Wall".into(),
        "-Wextra".into(),
        "-Werror".into(),
        root.join("harness/reference.c").display().to_string(),
    ];
    command.extend(shell_words(&flags)?);
    command.extend(["-o".into(), worker.display().to_string()]);
    run(&command)?;
    let mut combined_command = vec![
        command[0].clone(),
        "-DSAFE_MEDIA_COMBINED_REFERENCE=1".into(),
    ];
    combined_command.extend(command[1..].iter().cloned());
    *combined_command.last_mut().expect("output path") = combined.display().to_string();
    run(&combined_command)?;
    let commands = instrumentation_commands(&compiler, root, &out, linux);
    for compile in &commands {
        run(compile)?;
    }
    let observer = out.join(if linux {
        "allocations.so"
    } else {
        "allocations.dylib"
    });
    let probe = out.join("memory-check");
    let mut instrumentation = vec![
        ("observer_sha256".to_owned(), Value::str(sha(&observer)?)),
        ("probe_sha256".to_owned(), Value::str(sha(&probe)?)),
    ];
    let candidate = root.join("target/release/media-worker");
    let mut libraries: Vec<(String, Value)> = Vec::new();
    if linux {
        for (name, path) in [
            ("candidate", &candidate),
            ("reference", &worker),
            ("combined_reference", &combined),
            ("probe", &probe),
        ] {
            libraries.push((name.into(), linux::linked_libraries(path, None)?));
        }
        let interpreter = libraries[3]
            .1
            .get("interpreter")
            .and_then(Value::as_str)
            .map(str::to_owned);
        libraries.push((
            "observer".into(),
            linux::linked_libraries(&observer, interpreter.as_deref())?,
        ));
        let of = |name: &str| {
            libraries
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .unwrap_or(Value::Null)
        };
        let selected = Value::object([("observer", of("observer")), ("probe", of("probe"))]);
        let sources = Value::object(
            [
                "harness/allocations-linux.c",
                "harness/portable-memory-check.c",
            ]
            .map(|n| (n, Value::str(sha(&root.join(n)).unwrap_or_default()))),
        );
        instrumentation.extend([
            ("platform".into(), Value::str("Linux")),
            ("architecture".into(), Value::str(linux::machine())),
            ("schema".into(), Value::int(2)),
            (
                "observer_path".into(),
                Value::str("target/harness/allocations.so"),
            ),
            ("preload_variable".into(), Value::str("LD_PRELOAD")),
            (
                "linked_library_hashes".into(),
                linux::linked_hashes(&selected),
            ),
            (
                "binding_environment".into(),
                of("probe")
                    .get("binding_environment")
                    .cloned()
                    .unwrap_or(Value::Null),
            ),
            (
                "compiler_commands".into(),
                Value::Array(commands.iter().map(|c| strings(c)).collect()),
            ),
            ("source_hashes".into(), sources),
        ]);
        if let Ok(proof_path) = std::env::var("SAFE_MEDIA_RSS_KERNEL_PROOF") {
            let proof_file = root.join(&proof_path);
            let profile = Value::object([
                ("proof_path", Value::str(&proof_path)),
                ("proof_sha256", Value::str(sha(&proof_file)?)),
                ("proof", read_json(&proof_file)?),
            ]);
            let (observer_sha, probe_sha) = (
                instrumentation[0].1.as_str().map(str::to_owned),
                instrumentation[1].1.as_str().map(str::to_owned),
            );
            if !linux::kernel_profile_current(
                root,
                &profile,
                observer_sha.as_deref(),
                probe_sha.as_deref(),
            ) {
                return Err("requested kernel RSS proof is unavailable or stale".into());
            }
            let bound = Value::object([
                ("proof_path", profile.get("proof_path").cloned().unwrap()),
                (
                    "proof_sha256",
                    profile.get("proof_sha256").cloned().unwrap(),
                ),
            ]);
            instrumentation.push(("rss_kernel_bound".into(), bound));
        }
    }
    let system_libraries = |name: &str, path: &Path| -> R<Value> {
        if linux {
            Ok(linux::library_identity(
                libraries
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v)
                    .unwrap_or(&Value::Null),
            ))
        } else {
            darwin_libraries(path)
        }
    };
    let worker_identity = |path: &Path| -> R<Value> {
        crate::json::parse(&output(
            &path.display().to_string(),
            &["identity", "-"],
            &[],
        )?)
        .map_err(|e| e.to_string())
    };
    let Value::Object(mut identity) = worker_identity(&worker)? else {
        return Err("worker identity is not an object".into());
    };
    let metadata = cargo_metadata(root)?;
    let graph = selected_graph(root, &metadata, "media-worker", &matroska_inputs())?;
    let mut candidates = Vec::new();
    for (name, command) in &additional {
        let path = format!("target/release/{name}");
        candidates.push((
            *name,
            Value::object([
                ("path", Value::str(&path)),
                ("sha256", Value::str(sha(&root.join(&path))?)),
                ("command", strings(command)),
                (
                    "selected_graph",
                    selected_graph(root, &metadata, name, &matroska_inputs())?,
                ),
            ]),
        ));
    }
    let pinned_build = if pinned.exists() {
        Some(read_json(&pinned)?)
    } else {
        None
    };
    let build_environment: Vec<(String, Value)> = {
        let mut vars: Vec<(String, Value)> = std::env::vars()
            .filter(|(k, _)| {
                matches!(
                    k.as_str(),
                    "RUSTFLAGS"
                        | "CARGO_ENCODED_RUSTFLAGS"
                        | "RUSTC"
                        | "RUSTC_WRAPPER"
                        | "CARGO_BUILD_TARGET"
                ) || k.starts_with("CARGO_PROFILE_")
                    || k.starts_with("CARGO_TARGET_")
            })
            .map(|(k, v)| (k, Value::str(v)))
            .collect();
        vars.sort_by(|a, b| a.0.cmp(&b.0));
        vars
    };
    let files_of = |key: &str| -> Value {
        let mut map = Vec::new();
        if key == "source_hashes" {
            for package in graph
                .get("packages")
                .and_then(Value::as_array)
                .unwrap_or_default()
            {
                if let Some(Value::Object(files)) = package.get("files") {
                    map.extend(files.iter().cloned());
                }
            }
        }
        Value::Object(map)
    };
    let archives = if pinned.exists() {
        let lib = root.join(".research/reference/install/lib");
        let mut entries: Vec<PathBuf> = fs::read_dir(&lib)
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "a"))
            .collect();
        entries.sort();
        Value::Object(
            entries
                .iter()
                .map(|p| {
                    Ok((
                        p.file_name().unwrap().to_string_lossy().into_owned(),
                        Value::str(sha(p)?),
                    ))
                })
                .collect::<R<_>>()?,
        )
    } else {
        Value::Object(vec![])
    };
    let combined_record = Value::object([
        ("path", Value::str("target/harness/ffmpeg-combined-worker")),
        ("sha256", Value::str(sha(&combined)?)),
        (
            "kind",
            Value::str("pinned FFmpeg plus native scope-1 combined-encoding restoration"),
        ),
        ("compiler_command", strings(&combined_command)),
        ("worker_identity", worker_identity(&combined)?),
        (
            "system_libraries",
            system_libraries("combined_reference", &combined)?,
        ),
    ]);
    let updates: Vec<(&str, Value)> = vec![
        ("schema", Value::int(2)),
        (
            "reference_kind",
            Value::str(if pinned.exists() {
                "pinned local source build"
            } else {
                "installed development reference; source build not yet qualified"
            }),
        ),
        ("candidate_command", strings(&candidate_command)),
        ("selected_graph", graph.clone()),
        ("additional_candidates", Value::object(candidates)),
        ("build_environment", Value::Object(build_environment)),
        (
            "source_revision",
            pinned_build
                .as_ref()
                .and_then(|b| b.get("revision").cloned())
                .unwrap_or(Value::Null),
        ),
        ("compiler_command", strings(&command)),
        (
            "comparison_rules",
            read_json(&root.join("harness/comparison-rules.json"))?,
        ),
        (
            "comparison_references",
            Value::object([
                (
                    "ffmpeg-native",
                    Value::object([
                        ("path", Value::str("target/harness/ffmpeg-worker")),
                        ("sha256", Value::str(sha(&worker)?)),
                        ("kind", Value::str("pinned native FFmpeg")),
                    ]),
                ),
                ("ffmpeg-combined-native", combined_record),
            ]),
        ),
        ("system_libraries", system_libraries("reference", &worker)?),
        ("instrumentation", Value::Object(instrumentation)),
        (
            "compiler",
            Value::str(output(&compiler, &["--version"], &[])?),
        ),
        (
            "ffmpeg_version",
            Value::str(output("ffmpeg", &["-version"], &[])?),
        ),
        ("rustc", Value::str(output("rustc", &["-Vv"], &[])?)),
        (
            "binaries",
            Value::object([
                ("candidate", Value::str(sha(&candidate)?)),
                ("reference", Value::str(sha(&worker)?)),
            ]),
        ),
        ("source_hashes", files_of("source_hashes")),
        (
            "build_input_hashes",
            graph.get("shared_files").cloned().unwrap_or(Value::Null),
        ),
        (
            "workspace_provenance",
            Value::object([
                (
                    "manifest_sha256",
                    Value::str(sha(&root.join("Cargo.toml"))?),
                ),
                ("lock_sha256", Value::str(sha(&root.join("Cargo.lock"))?)),
            ]),
        ),
        ("reference_archives", archives),
        ("reference_build", pinned_build.unwrap_or(Value::Null)),
        (
            "reference_source_sha256",
            Value::str(sha(&root.join("harness/reference.c"))?),
        ),
        ("lock_sha256", Value::str(sha(&root.join("Cargo.lock"))?)),
        (
            "ffprobe_sha256",
            which("ffprobe").map_or(Ok(Value::Null), |p| sha(&p).map(Value::str))?,
        ),
    ];
    for (key, value) in updates {
        match identity.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => identity.push((key.to_owned(), value)),
        }
    }
    if linux {
        let getconf = |name: &str| {
            output("getconf", &[name], &[])
                .ok()
                .and_then(|v| v.trim().parse::<i64>().ok())
                .map_or(Value::Null, Value::int)
        };
        let tool_hashes = Value::object(["nm", "readelf", "ps"].map(|n| {
            (
                n,
                which(n).map_or(Value::Null, |p| sha(&p).map_or(Value::Null, Value::str)),
            )
        }));
        let platform = Value::object([
            ("system", Value::str("Linux")),
            ("machine", Value::str(linux::machine())),
            ("kernel_release", Value::str(linux::kernel_release())),
            (
                "kernel_version",
                Value::str(output("uname", &["-v"], &[])?.trim()),
            ),
            (
                "libc",
                Value::strings(
                    output("getconf", &["GNU_LIBC_VERSION"], &[])?
                        .split_whitespace()
                        .map(str::to_owned),
                ),
            ),
            ("page_size", getconf("PAGESIZE")),
            ("clock_ticks", getconf("CLK_TCK")),
            ("nm_version", Value::str(output("nm", &["--version"], &[])?)),
            (
                "readelf_version",
                Value::str(output("readelf", &["--version"], &[])?),
            ),
            ("ps_version", Value::str(output("ps", &["--version"], &[])?)),
            ("tool_hashes", tool_hashes),
        ]);
        identity.push(("host_platform".into(), platform));
        identity.push((
            "linked_libraries".into(),
            Value::Object(
                libraries
                    .iter()
                    .map(|(k, v)| (k.clone(), linux::library_identity(v)))
                    .collect(),
            ),
        ));
        identity.push((
            "linux_loader_observations_path".into(),
            Value::str("target/harness/linux-loader-observations.json"),
        ));
        // Every comparison reference binds the candidate and reference closures too.
        let bindings = Value::Object(
            libraries
                .iter()
                .filter(|(k, _)| {
                    matches!(k.as_str(), "candidate" | "reference" | "combined_reference")
                })
                .map(|(k, v)| (k.clone(), linux::library_identity(v)))
                .collect(),
        );
        if let Some((_, Value::Object(references))) = identity
            .iter_mut()
            .find(|(k, _)| k == "comparison_references")
        {
            for (_, record) in references.iter_mut() {
                if let Value::Object(fields) = record {
                    fields.push(("linked_library_bindings".into(), bindings.clone()));
                }
            }
        }
    }
    let path = out.join("identity.json");
    write_json(&path, &Value::Object(identity))?;
    if linux {
        let observations = Value::object([
            ("identity_sha256", Value::str(sha(&path)?)),
            ("raw_loader_observations", Value::Object(libraries)),
        ]);
        write_json(&out.join("linux-loader-observations.json"), &observations)?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_pkg_config_output_like_a_shell() {
        assert_eq!(
            shell_words("-I/x/include  -L'/a b/lib' -lz \"-DX=\\\"1\\\"\" a\\ b\n").unwrap(),
            ["-I/x/include", "-L/a b/lib", "-lz", "-DX=\"1\"", "a b"]
        );
        assert!(shell_words("'open").is_err());
    }

    #[test]
    fn instrumentation_commands_follow_the_platform() {
        let (root, out) = (Path::new("/r"), Path::new("/r/target/harness"));
        let darwin = instrumentation_commands("cc", root, out, false);
        assert!(
            darwin[0].contains(&"-dynamiclib".to_owned())
                && darwin[0].contains(&"/r/harness/allocations.c".to_owned())
        );
        assert!(darwin[1].contains(&"/r/harness/memory-check.c".to_owned()));
        let linux = instrumentation_commands("cc", root, out, true);
        assert!(
            linux[0].contains(&"-Wl,-z,now".to_owned())
                && linux[0].ends_with(&["-ldl".to_owned(), "-pthread".to_owned()])
        );
        assert!(linux[1].contains(&"/r/harness/portable-memory-check.c".to_owned()));
    }
}
