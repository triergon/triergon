//! The gate build: workers compiled by the toolchain in `rust-toolchain.toml` with no
//! extra compiler configuration, workers and references linked under [`LINK_POLICY`].
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

/// One `key: value` field of `rustc -vV`.
fn field<'a>(compiler: &'a str, key: &str) -> Option<&'a str> {
    compiler
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix(": "))
}

fn release(compiler: &str) -> Option<&str> {
    field(compiler, "release")
}

fn host_of(compiler: &str) -> Result<&str, String> {
    field(compiler, "host").ok_or_else(|| "rustc reports no host".to_owned())
}

/// The gate target on 32-bit ARM: Thumb-2, assuming NEON. The armhf compilers that
/// build the FFmpeg and libass references emit Thumb-2 by default, while rustup's
/// 32-bit ARM host toolchain, [`ARM32_HOST`], emits A32 without NEON.
pub const ARM32_TARGET: &str = "thumbv7neon-unknown-linux-gnueabihf";

/// The host triple of rustup's 32-bit ARM hard-float toolchain, which builds the
/// tooling there but no worker.
const ARM32_HOST: &str = "armv7-unknown-linux-gnueabihf";

/// The host triple of the toolchain cargo resolves in the current directory.
pub fn host() -> Result<&'static str, String> {
    static HOST: std::sync::OnceLock<Result<String, String>> = std::sync::OnceLock::new();
    HOST.get_or_init(|| {
        let compiler = compiler(Path::new("."), None)?;
        host_of(&compiler).map(str::to_owned)
    })
    .as_deref()
    .map_err(Clone::clone)
}

/// The target the workers and the tests are compiled for.
pub fn worker_target() -> Result<&'static str, String> {
    host().map(target_for)
}

fn target_for(host: &str) -> &str {
    if host == ARM32_HOST {
        ARM32_TARGET
    } else {
        host
    }
}

/// The `--target` arguments cargo needs to compile for [`worker_target`]; none where
/// it is the host's, so those builds stay as they are.
pub fn target_args() -> Result<Vec<String>, String> {
    host().map(target_arguments)
}

fn target_arguments(host: &str) -> Vec<String> {
    let target = target_for(host);
    if target == host {
        Vec::new()
    } else {
        vec!["--target".to_owned(), target.to_owned()]
    }
}

/// Where cargo puts release binaries for [`worker_target`], relative to its target
/// directory.
pub fn release_subdir() -> Result<String, String> {
    host().map(release_path)
}

fn release_path(host: &str) -> String {
    let target = target_for(host);
    if target == host {
        "release".to_owned()
    } else {
        format!("{target}/release")
    }
}

/// Where the workers are, relative to the repository root.
pub fn release_dir() -> Result<String, String> {
    Ok(format!("target/{}", release_subdir()?))
}

/// How workers and references link the support libraries of their language runtimes.
/// On a GNU/Linux target both sides take every runtime support library from its static
/// archive: the Rust workers libgcc's unwinder (`libgcc_eh`) and libm, the C and C++
/// references libm, libstdc++ and libgcc. A process then maps only the support code its
/// own libraries use. libc and third-party libraries stay shared on both sides, and so
/// does libm on x86, where glibc's `libm.a` selects its implementations through the
/// static libc and links only into a static program. Elsewhere both sides link as their
/// toolchains do.
pub const LINK_POLICY: &str = "static-runtime-support-v1";

/// Whether [`LINK_POLICY`] changes how a worker or reference for `target` links.
fn static_runtime(target: &str) -> bool {
    target.contains("-linux-gnu")
}

/// Whether glibc's `libm.a` links into a program that uses the shared libc on `target`.
fn static_libm(target: &str) -> bool {
    static_runtime(target)
        && !["x86_64-", "i586-", "i686-"]
            .iter()
            .any(|x86| target.starts_with(x86))
}

/// The arguments `cargo rustc` passes to a worker's own compile, after `--`, to link
/// it under [`LINK_POLICY`]. Placed with the worker's native libraries, ahead of the
/// standard library's `-lgcc_s` and `-lm`, the static archives resolve every unwinder
/// and libm import, and the linker leaves the unneeded shared libraries out.
pub fn worker_link_arguments() -> Result<Vec<String>, String> {
    worker_target().map(worker_link_flags)
}

fn worker_link_flags(target: &str) -> Vec<String> {
    let mut libraries = Vec::new();
    if static_runtime(target) {
        libraries.push("gcc_eh");
    }
    if static_libm(target) {
        libraries.push("m");
    }
    libraries
        .into_iter()
        .flat_map(|library| ["-l".to_owned(), format!("static:-bundle={library}")])
        .collect()
}

/// A reference's link flags under [`LINK_POLICY`]: libm and libstdc++ from their
/// static archives, and libgcc from its own with `-static-libgcc`.
pub fn reference_link_flags(flags: Vec<String>) -> Result<Vec<String>, String> {
    Ok(reference_flags(worker_target()?, flags))
}

fn reference_flags(target: &str, flags: Vec<String>) -> Vec<String> {
    if !static_runtime(target) {
        return flags;
    }
    let libm = static_libm(target);
    let mut linked: Vec<String> = flags
        .into_iter()
        .map(|flag| match flag.as_str() {
            "-lm" if libm => "-l:libm.a".to_owned(),
            "-lstdc++" => "-l:libstdc++.a".to_owned(),
            _ => flag,
        })
        .collect();
    linked.push("-static-libgcc".to_owned());
    linked
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
    ) || name.starts_with("CARGO_PROFILE_RELEASE_")
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
    if configuration
        .get("profile")
        .is_some_and(|profiles| profiles.get("release").is_some())
    {
        keys.push("profile.release".to_owned());
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
        ("link_policy".to_owned(), Value::str(LINK_POLICY)),
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
    match identity.get("link_policy").and_then(Value::as_str) {
        Some(LINK_POLICY) => {}
        Some(found) => reasons.push(format!("link policy {found}, not {LINK_POLICY}")),
        None => reasons.push("records no link policy".to_owned()),
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

/// The CPUs this process may run on and those online on the machine, as Linux lists
/// them; null elsewhere. A bound measured pinned to one CPU is not comparable with one
/// measured unpinned.
pub fn cpus() -> Value {
    let allowed = std::fs::read_to_string("/proc/self/status")
        .ok()
        .as_deref()
        .and_then(allowed_cpus)
        .map_or(Value::Null, Value::str);
    let online = std::fs::read_to_string("/sys/devices/system/cpu/online")
        .map_or(Value::Null, |list| Value::str(list.trim()));
    Value::object([("allowed", allowed), ("online", online)])
}

fn allowed_cpus(status: &str) -> Option<&str> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .map(str::trim)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    const PIN: &str = "1.99.1";

    #[test]
    fn the_host_is_read_from_rustc_or_refused() {
        let version = "rustc 1.99.1\nbinary: rustc\nhost: aarch64-apple-darwin\nrelease: 1.99.1\n";
        assert_eq!(host_of(version), Ok("aarch64-apple-darwin"));
        assert_eq!(release(version), Some("1.99.1"));
        assert_eq!(
            host_of("rustc 1.99.1\nrelease: 1.99.1\n"),
            Err("rustc reports no host".to_owned())
        );
        assert_eq!(field("hostname: x\n", "host"), None);
    }

    #[test]
    fn workers_compile_for_thumb2_on_32_bit_arm_and_for_the_host_elsewhere() {
        assert_eq!(target_for(ARM32_HOST), ARM32_TARGET);
        assert_eq!(
            target_arguments(ARM32_HOST),
            ["--target", "thumbv7neon-unknown-linux-gnueabihf"]
        );
        assert_eq!(
            release_path(ARM32_HOST),
            "thumbv7neon-unknown-linux-gnueabihf/release"
        );
        for host in [
            "aarch64-unknown-linux-gnu",
            "aarch64-apple-darwin",
            "x86_64-unknown-linux-gnu",
        ] {
            assert_eq!(target_for(host), host);
            assert!(target_arguments(host).is_empty());
            assert_eq!(release_path(host), "release");
        }
    }

    /// A record of the gate build with one field replaced.
    fn record(key: &str, value: Option<Value>) -> Value {
        let base = [
            ("rustc", Value::str(format!("rustc\nrelease: {PIN}\n"))),
            ("build_environment", Value::Object(Vec::new())),
            ("cargo_configuration", Value::Array(Vec::new())),
            ("link_policy", Value::str(LINK_POLICY)),
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
            ("link_policy", "records no link policy"),
        ] {
            assert_eq!(refusals(&record(key, None), PIN), [reason]);
        }
    }

    #[test]
    fn a_build_linked_under_another_policy_is_refused() {
        let earlier = record("link_policy", Some(Value::str("toolchain-default")));
        assert_eq!(
            refusals(&earlier, PIN),
            ["link policy toolchain-default, not static-runtime-support-v1"]
        );
    }

    #[test]
    fn gnu_linux_workers_and_references_take_runtime_support_from_static_archives() {
        let flags = ["-L/p", "-lharfbuzz", "-lstdc++", "-lm", "-lz", "-latomic"];
        let flags = || flags.map(String::from).to_vec();
        for target in [ARM32_TARGET, "aarch64-unknown-linux-gnu"] {
            assert_eq!(
                worker_link_flags(target),
                ["-l", "static:-bundle=gcc_eh", "-l", "static:-bundle=m"]
            );
            assert_eq!(
                reference_flags(target, flags()),
                [
                    "-L/p",
                    "-lharfbuzz",
                    "-l:libstdc++.a",
                    "-l:libm.a",
                    "-lz",
                    "-latomic",
                    "-static-libgcc"
                ]
            );
        }
        // glibc's libm.a on x86 links only into a static program: libm stays shared.
        assert_eq!(
            worker_link_flags("x86_64-unknown-linux-gnu"),
            ["-l", "static:-bundle=gcc_eh"]
        );
        assert_eq!(
            reference_flags("x86_64-unknown-linux-gnu", flags()),
            [
                "-L/p",
                "-lharfbuzz",
                "-l:libstdc++.a",
                "-lm",
                "-lz",
                "-latomic",
                "-static-libgcc"
            ]
        );
        for target in ["aarch64-apple-darwin", "aarch64-unknown-linux-musl"] {
            assert!(worker_link_flags(target).is_empty());
            assert_eq!(reference_flags(target, flags()), flags());
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
            "CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_DEBUG",
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
                ("CARGO_PROFILE_TEST_DEBUG", "0"),
                ("CARGO_PROFILE_DEV_OPT_LEVEL", "1"),
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
    fn other_profiles_do_not_reach_a_release_worker() {
        let tree = scratch("profiles");
        fs::create_dir_all(tree.join(".cargo")).unwrap();
        fs::write(
            tree.join(".cargo/config.toml"),
            "[profile.test]\ndebug = 0\n[profile.dev]\nopt-level = 1\n",
        )
        .unwrap();
        let found = configuration(&tree, None);
        fs::remove_dir_all(&tree).unwrap();
        assert_eq!(found, Vec::<String>::new());
    }

    #[test]
    fn the_allowed_cpus_come_from_the_process_status() {
        let status = "Name:\tx\nCpus_allowed:\t02\nCpus_allowed_list:\t1\nMems:\t1\n";
        assert_eq!(allowed_cpus(status), Some("1"));
        assert_eq!(allowed_cpus("Name:\tx\n"), None);
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
