//! The gate build: workers compiled by the toolchain in `rust-toolchain.toml` with no
//! extra compiler configuration.
//!
//! Compiler version and flags move a runtime bound by more than a gate's margin, so a
//! gate result only means something for one build. `build-workers` refuses any other
//! build unless asked for a comparison build, and the grader refuses a report that
//! records one. Paired comparisons may run on any build; they name it.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::checks::read_json;
use crate::json::Value;
use crate::{sha256, toml};

/// The compiler release `rust-toolchain.toml` pins.
pub fn pinned_release(root: &Path) -> Result<String, String> {
    let path = root.join("rust-toolchain.toml");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let parsed = toml::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    parsed
        .get("toolchain")
        .and_then(|toolchain| toolchain.get("channel"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("{} names no channel", path.display()))
}

/// `rustc -vV` as cargo would resolve it from `root`. `search_path` replaces `PATH`.
pub fn compiler(root: &Path, search_path: Option<&OsStr>) -> Result<String, String> {
    let mut command = Command::new("rustc");
    command.arg("-vV").current_dir(root);
    if let Some(path) = search_path {
        command.env("PATH", path);
    }
    let output = command.output().map_err(|e| format!("rustc: {e}"))?;
    if !output.status.success() {
        return Err(format!("rustc -vV: {}", output.status));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("rustc -vV: {e}"))
}

fn release(compiler: &str) -> Option<&str> {
    compiler.lines().find_map(|l| l.strip_prefix("release: "))
}

/// Whether an environment variable changes how cargo compiles a worker.
fn compiler_variable(name: &str) -> bool {
    matches!(
        name,
        "RUSTFLAGS"
            | "CARGO_ENCODED_RUSTFLAGS"
            | "RUSTC"
            | "RUSTC_WRAPPER"
            | "RUSTC_WORKSPACE_WRAPPER"
            | "CARGO_BUILD_RUSTC"
            | "CARGO_BUILD_RUSTC_WRAPPER"
            | "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"
            | "CARGO_BUILD_RUSTFLAGS"
            | "CARGO_BUILD_TARGET"
    ) || name.starts_with("CARGO_PROFILE_")
        || (name.starts_with("CARGO_TARGET_") && name != "CARGO_TARGET_DIR")
}

/// The compiler variables in effect, sorted, for `build_environment`.
pub fn environment(variables: impl Iterator<Item = (String, String)>) -> Vec<(String, Value)> {
    let mut found: Vec<(String, Value)> = variables
        .filter(|(name, value)| compiler_variable(name) && !value.is_empty())
        .map(|(name, value)| (name, Value::str(value)))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// Every Cargo configuration file that applies to a build in `root`.
fn configuration_files(root: &Path, cargo_home: Option<&Path>) -> Vec<PathBuf> {
    let directories = root
        .ancestors()
        .map(|directory| directory.join(".cargo"))
        .chain(cargo_home.map(Path::to_path_buf));
    let mut files: Vec<PathBuf> = directories
        .flat_map(|directory| [directory.join("config"), directory.join("config.toml")])
        .filter(|file| file.is_file())
        .collect();
    files.sort();
    files.dedup();
    files
}

/// The keys of one parsed configuration file that change how a worker compiles.
fn overriding_keys(configuration: &Value) -> Vec<String> {
    const BUILD: [&str; 5] = [
        "rustc",
        "rustc-wrapper",
        "rustc-workspace-wrapper",
        "rustflags",
        "target",
    ];
    const TARGET: [&str; 2] = ["rustflags", "linker"];
    let mut keys = Vec::new();
    if let Some(build) = configuration.get("build") {
        for key in BUILD.iter().filter(|key| build.get(key).is_some()) {
            keys.push(format!("build.{key}"));
        }
    }
    if let Some(Value::Object(targets)) = configuration.get("target") {
        for (target, settings) in targets {
            for key in TARGET.iter().filter(|key| settings.get(key).is_some()) {
                keys.push(format!("target.{target}.{key}"));
            }
        }
    }
    if let Some(Value::Object(profiles)) = configuration.get("profile") {
        keys.extend(profiles.iter().map(|(name, _)| format!("profile.{name}")));
    }
    keys
}

/// Cargo configuration in effect that changes how a worker compiles. A file that
/// cannot be read counts, because nothing can then be said about it.
pub fn configuration(root: &Path, cargo_home: Option<&Path>) -> Vec<String> {
    let mut found = Vec::new();
    for file in configuration_files(root, cargo_home) {
        let parsed = std::fs::read_to_string(&file)
            .map_err(|e| e.to_string())
            .and_then(|text| toml::parse(&text));
        match parsed {
            Ok(parsed) => found.extend(
                overriding_keys(&parsed)
                    .into_iter()
                    .map(|key| format!("{}: {key}", file.display())),
            ),
            Err(error) => found.push(format!("{}: unreadable: {error}", file.display())),
        }
    }
    found
}

/// The default Cargo home: `CARGO_HOME`, else `.cargo` in the home directory.
pub fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".cargo")))
}

/// The build record of the current environment, as `identity.json` holds it.
pub fn current(root: &Path) -> Result<Vec<(String, Value)>, String> {
    Ok(vec![
        ("rustc".to_owned(), Value::str(compiler(root, None)?)),
        (
            "build_environment".to_owned(),
            Value::Object(environment(std::env::vars())),
        ),
        (
            "cargo_configuration".to_owned(),
            Value::strings(configuration(root, cargo_home().as_deref())),
        ),
    ])
}

/// Why the build a record describes is not the gate build; empty when it is. A
/// record that does not say counts as not the gate build.
pub fn refusals(identity: &Value, pinned: &str) -> Vec<String> {
    let mut reasons = Vec::new();
    match identity
        .get("rustc")
        .and_then(Value::as_str)
        .and_then(release)
    {
        Some(found) if found == pinned => {}
        Some(found) => reasons.push(format!("rustc {found}, not the pinned {pinned}")),
        None => reasons.push("records no compiler".to_owned()),
    }
    match identity.get("build_environment") {
        Some(Value::Object(variables)) if variables.is_empty() => {}
        Some(Value::Object(variables)) => {
            let names: Vec<&str> = variables.iter().map(|(name, _)| name.as_str()).collect();
            reasons.push(format!("compiler settings: {}", names.join(", ")));
        }
        _ => reasons.push("records no build environment".to_owned()),
    }
    match identity
        .get("cargo_configuration")
        .and_then(Value::as_array)
    {
        Some([]) => {}
        Some(keys) => {
            let keys: Vec<&str> = keys.iter().filter_map(Value::as_str).collect();
            reasons.push(format!("Cargo configuration: {}", keys.join(", ")));
        }
        None => reasons.push("records no Cargo configuration".to_owned()),
    }
    reasons
}

/// One line naming the build a record describes.
pub fn name(identity: &Value) -> String {
    let compiler = identity
        .get("rustc")
        .and_then(Value::as_str)
        .and_then(release)
        .unwrap_or("unknown");
    let mut overrides: Vec<String> = Vec::new();
    if let Some(Value::Object(variables)) = identity.get("build_environment") {
        overrides.extend(
            variables
                .iter()
                .map(|(key, value)| format!("{key}={}", value.as_str().unwrap_or_default())),
        );
    }
    if let Some(keys) = identity
        .get("cargo_configuration")
        .and_then(Value::as_array)
    {
        overrides.extend(keys.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    if overrides.is_empty() {
        format!("rustc {compiler}, default flags")
    } else {
        format!("rustc {compiler}, {}", overrides.join(", "))
    }
}

/// One line naming a record's build and whether it is the gate build.
pub fn describe(identity: &Value, pinned: &str) -> String {
    let reasons = refusals(identity, pinned);
    if reasons.is_empty() {
        format!("build: {} (the gate build)", name(identity))
    } else {
        format!(
            "build: {} (not the gate build: {}); no gate result",
            name(identity),
            reasons.join("; ")
        )
    }
}

/// Stop a build that is not the gate build, unless it is for comparisons only.
pub fn admit(root: &Path, comparison_build: bool) -> Result<(), String> {
    let record = Value::Object(current(root)?);
    admission(&record, &pinned_release(root)?, comparison_build)
}

fn admission(record: &Value, pinned: &str, comparison_build: bool) -> Result<(), String> {
    let reasons = refusals(record, pinned);
    if reasons.is_empty() {
        return Ok(());
    }
    if comparison_build {
        println!("{}", describe(record, pinned));
        return Ok(());
    }
    Err(format!(
        "not the gate build: {}. Remove the override, or pass --comparison-build for a \
         build that can only be used in paired comparisons",
        reasons.join("; ")
    ))
}

fn bound(records: Option<&Value>, sha: &str) -> bool {
    matches!(records, Some(Value::Object(records)) if records
        .iter()
        .any(|(_, record)| record.get("sha256").and_then(Value::as_str) == Some(sha)))
}

/// Why a timed pair given by path cannot produce a gate result: the candidate must be
/// a worker and the reference a comparison reference of the gate build recorded in
/// `target/harness/identity.json`.
pub fn pair_refusals(root: &Path, candidate: &Path, reference: &Path) -> Vec<String> {
    let identity = match read_json(&root.join("target/harness/identity.json")) {
        Ok(identity) => identity,
        Err(error) => return vec![format!("no build identity: {error}")],
    };
    let mut reasons = match pinned_release(root) {
        Ok(pinned) => refusals(&identity, &pinned),
        Err(error) => vec![error],
    };
    let digest = |path: &Path| std::fs::read(path).map(|bytes| sha256::hex(&bytes)).ok();
    if !digest(candidate).is_some_and(|sha| bound(identity.get("additional_candidates"), &sha)) {
        reasons.push(format!(
            "{} is not a worker of the recorded build",
            candidate.display()
        ));
    }
    if !digest(reference).is_some_and(|sha| bound(identity.get("comparison_references"), &sha)) {
        reasons.push(format!(
            "{} is not a reference of the recorded build",
            reference.display()
        ));
    }
    reasons
}

/// The CPUs this process may run on and those the machine has. A bound measured
/// pinned to one CPU is not comparable with one measured unpinned.
pub fn cpus() -> Value {
    let allowed = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
                .map(|list| list.trim().to_owned())
        });
    let online = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    Value::object([
        ("allowed", allowed.map_or(Value::Null, Value::str)),
        ("available", Value::int(online as i64)),
    ])
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    const PIN: &str = "1.99.1";

    /// A record of the gate build with one field replaced.
    fn record(key: &str, value: Option<Value>) -> Value {
        let base = [
            ("rustc", Value::str(format!("rustc\nrelease: {PIN}\n"))),
            ("build_environment", Value::Object(Vec::new())),
            ("cargo_configuration", Value::Array(Vec::new())),
        ];
        Value::Object(
            base.into_iter()
                .filter_map(|(name, field)| {
                    let field = if name == key { value.clone()? } else { field };
                    Some((name.to_owned(), field))
                })
                .collect(),
        )
    }

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("gate-build-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn variables(pairs: &[(&str, &str)]) -> Vec<String> {
        let pairs = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()));
        environment(pairs).into_iter().map(|(k, _)| k).collect()
    }

    #[test]
    fn the_gate_build_is_admitted_and_named() {
        let gate = record("", None);
        assert!(refusals(&gate, PIN).is_empty());
        assert_eq!(
            describe(&gate, PIN),
            "build: rustc 1.99.1, default flags (the gate build)"
        );
    }

    #[test]
    fn another_compiler_is_refused() {
        let nightly = record(
            "rustc",
            Some(Value::str("rustc\nrelease: 1.101.0-nightly\n")),
        );
        assert_eq!(
            refusals(&nightly, PIN),
            ["rustc 1.101.0-nightly, not the pinned 1.99.1"]
        );
        let line = describe(&nightly, PIN);
        assert!(line.starts_with("build: rustc 1.101.0-nightly, default flags"));
        assert!(line.ends_with("no gate result"));
    }

    #[test]
    fn a_compiler_ahead_on_the_search_path_is_seen() {
        let directory = scratch("path");
        let fake = directory.join("rustc");
        fs::write(
            &fake,
            "#!/bin/sh\necho rustc 1.101.0-nightly\necho release: 1.101.0-nightly\n",
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let found = compiler(&directory, Some(directory.as_os_str())).unwrap();
        let reasons = refusals(&record("rustc", Some(Value::str(found))), PIN);
        fs::remove_dir_all(&directory).unwrap();
        assert_eq!(reasons, ["rustc 1.101.0-nightly, not the pinned 1.99.1"]);
    }

    #[test]
    fn a_record_that_does_not_say_is_refused() {
        for (key, reason) in [
            ("rustc", "records no compiler"),
            ("build_environment", "records no build environment"),
            ("cargo_configuration", "records no Cargo configuration"),
        ] {
            assert_eq!(refusals(&record(key, None), PIN), [reason]);
        }
    }

    #[test]
    fn each_compiler_variable_is_recorded_and_refused() {
        for name in [
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "RUSTC",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "CARGO_BUILD_RUSTC",
            "CARGO_BUILD_RUSTC_WRAPPER",
            "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
            "CARGO_BUILD_RUSTFLAGS",
            "CARGO_BUILD_TARGET",
            "CARGO_PROFILE_RELEASE_LTO",
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER",
        ] {
            assert_eq!(variables(&[(name, "x")]), [name]);
            let found = environment([(name.to_owned(), "x".to_owned())].into_iter());
            let reasons = refusals(
                &record("build_environment", Some(Value::Object(found))),
                PIN,
            );
            assert_eq!(reasons, [format!("compiler settings: {name}")]);
        }
    }

    #[test]
    fn variables_that_leave_the_compile_alone_are_not_recorded() {
        assert!(
            variables(&[
                ("CARGO_TARGET_DIR", "/tmp/target"),
                ("CARGO_INCREMENTAL", "0"),
                ("RUSTDOCFLAGS", "-D warnings"),
                ("RUSTFLAGS", ""),
            ])
            .is_empty()
        );
    }

    #[test]
    fn cargo_configuration_that_changes_the_compile_is_refused() {
        for (text, key) in [
            (
                "[build]\nrustflags = [\"-Ctarget-cpu=native\"]\n",
                "build.rustflags",
            ),
            ("[build]\nrustc = \"/opt/rustc\"\n", "build.rustc"),
            (
                "[build]\nrustc-wrapper = \"sccache\"\n",
                "build.rustc-wrapper",
            ),
            (
                "[build]\nrustc-workspace-wrapper = \"w\"\n",
                "build.rustc-workspace-wrapper",
            ),
            (
                "[build]\ntarget = \"aarch64-unknown-linux-musl\"\n",
                "build.target",
            ),
            (
                "[target.aarch64-unknown-linux-gnu]\nrustflags = [\"-Cx\"]\n",
                "target.aarch64-unknown-linux-gnu.rustflags",
            ),
            (
                "[target.aarch64-unknown-linux-gnu]\nlinker = \"lld\"\n",
                "target.aarch64-unknown-linux-gnu.linker",
            ),
            ("[profile.release]\nlto = false\n", "profile.release"),
        ] {
            let tree = scratch("configuration");
            let inner = tree.join("repository");
            fs::create_dir_all(&inner).unwrap();
            fs::create_dir_all(tree.join(".cargo")).unwrap();
            fs::write(tree.join(".cargo/config.toml"), text).unwrap();
            let found = configuration(&inner, None);
            let suffix = format!(": {key}");
            assert_eq!(found.len(), 1, "{text}");
            assert!(found[0].ends_with(&suffix), "{found:?}");
            let reasons = refusals(
                &record("cargo_configuration", Some(Value::strings(found.clone()))),
                PIN,
            );
            assert_eq!(reasons.len(), 1);
            assert!(reasons[0].starts_with("Cargo configuration: "));
            fs::remove_dir_all(&tree).unwrap();
        }
    }

    #[test]
    fn the_cargo_home_is_read_and_harmless_configuration_passes() {
        let tree = scratch("home");
        let home = tree.join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("config.toml"), "[build]\nrustflags = [\"-Cx\"]\n").unwrap();
        let inner = tree.join("repository");
        fs::create_dir_all(inner.join(".cargo")).unwrap();
        fs::write(
            inner.join(".cargo/config.toml"),
            "[alias]\nxtask = \"run --package xtask --quiet --\"\n",
        )
        .unwrap();
        let found = configuration(&inner, Some(&home));
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with(": build.rustflags"));
        assert!(configuration(&inner, None).is_empty());
        fs::remove_dir_all(&tree).unwrap();
    }

    #[test]
    fn unreadable_configuration_is_refused() {
        let tree = scratch("unreadable");
        fs::create_dir_all(tree.join(".cargo")).unwrap();
        fs::write(tree.join(".cargo/config.toml"), "[build\n").unwrap();
        let found = configuration(&tree, None);
        fs::remove_dir_all(&tree).unwrap();
        assert!(found[0].contains("unreadable"), "{found:?}");
    }

    #[test]
    fn a_pair_outside_the_recorded_build_cannot_gate() {
        let tree = scratch("pair");
        fs::create_dir_all(tree.join("target/harness")).unwrap();
        fs::write(
            tree.join("rust-toolchain.toml"),
            format!("[toolchain]\nchannel = \"{PIN}\"\n"),
        )
        .unwrap();
        let (worker, reference, other) = (tree.join("w"), tree.join("r"), tree.join("o"));
        fs::write(&worker, "worker").unwrap();
        fs::write(&reference, "reference").unwrap();
        fs::write(&other, "other").unwrap();
        assert!(pair_refusals(&tree, &worker, &reference)[0].starts_with("no build identity"));

        let digest = |text: &str| sha256::hex(text.as_bytes());
        let mut identity = record("", None);
        if let Value::Object(fields) = &mut identity {
            let entry =
                |sha: String| Value::object([("w", Value::object([("sha256", Value::str(sha))]))]);
            fields.push(("additional_candidates".into(), entry(digest("worker"))));
            fields.push(("comparison_references".into(), entry(digest("reference"))));
        }
        crate::corpus::write_json(&tree.join("target/harness/identity.json"), &identity).unwrap();
        assert!(pair_refusals(&tree, &worker, &reference).is_empty());
        assert_eq!(pair_refusals(&tree, &other, &reference).len(), 1);
        assert_eq!(pair_refusals(&tree, &worker, &other).len(), 1);
        fs::remove_dir_all(&tree).unwrap();
    }

    #[test]
    fn a_refused_build_is_told_how_to_continue() {
        let flagged = record(
            "build_environment",
            Some(Value::object([("RUSTFLAGS", Value::str("-Cx"))])),
        );
        let error = admission(&flagged, PIN, false).unwrap_err();
        assert!(error.starts_with("not the gate build"), "{error}");
        assert!(error.contains("--comparison-build"), "{error}");
        assert!(admission(&flagged, PIN, true).is_ok());
        assert!(admission(&record("", None), PIN, false).is_ok());
    }

    #[test]
    fn the_toolchain_pin_is_read_from_the_repository() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let pinned = pinned_release(root).unwrap();
        assert!(pinned.split('.').all(|part| part.parse::<u32>().is_ok()));
    }
}
