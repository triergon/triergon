//! The gate build: workers compiled by the toolchain in `rust-toolchain.toml`, or by the
//! pinned nightly ([`crate::NIGHTLY`]) for a worker marked to use it, with no extra
//! compiler configuration; workers and references linked under [`LINK_POLICY`].
//!
//! Compiler version and flags move a runtime bound by more than a gate's margin, so a
//! gate result only means something for one build. `build-workers` refuses any other
//! build unless asked for a comparison build, and the grader refuses a report that
//! records one. Paired comparisons may run on any build; they name it.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::json::{self, Value};
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

/// `rustc -vV` of the rustup toolchain `toolchain`, run from `root`.
pub fn toolchain_compiler(root: &Path, toolchain: &str) -> Result<String, String> {
    let output = Command::new("rustc")
        .args([&format!("+{toolchain}"), "-vV"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("rustc +{toolchain}: {e}"))?;
    if !output.status.success() {
        return Err(format!("rustc +{toolchain} -vV: {}", output.status));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("rustc +{toolchain} -vV: {e}"))
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

/// How workers and references link. On a GNU/Linux target both sides take their
/// toolchains' runtime support libraries from their static archives: the Rust workers
/// libgcc's unwinder (`libgcc_eh`), the C and C++ references libgcc, libstdc++ and
/// libatomic. A process then maps only the toolchain support code its own libraries use.
/// The C library, libc with libm, and third-party libraries stay shared on both sides.
/// A Rust worker with a committed trained order is linked there by the pinned
/// toolchain's `lld` with its functions in that order, then module order
/// ([`ordered_link_arguments`]); the other workers and the references link as before.
/// Elsewhere both sides link as their toolchains do.
pub const LINK_POLICY: &str = "static-toolchain-runtime-trained-order-v1";

/// Whether [`LINK_POLICY`] changes how a worker or reference for `target` links.
fn static_runtime(target: &str) -> bool {
    target.contains("-linux-gnu")
}

/// Whether [`LINK_POLICY`] links the workers for `target` with `lld`, those with a
/// trained order in it ([`ordered_link_arguments`]).
pub fn orders_links(target: &str) -> bool {
    static_runtime(target)
}

/// The arguments `cargo rustc` passes to a worker's own compile, after `--`, to link
/// it under [`LINK_POLICY`]. Placed with the worker's native libraries, ahead of the
/// standard library's `-lgcc_s`, libgcc's static unwinder resolves every unwinder
/// import, and the linker leaves the unneeded `libgcc_s` out.
pub fn worker_link_arguments() -> Result<Vec<String>, String> {
    worker_target().map(worker_link_flags)
}

fn worker_link_flags(target: &str) -> Vec<String> {
    if static_runtime(target) {
        vec!["-l".to_owned(), "static:-bundle=gcc_eh".to_owned()]
    } else {
        Vec::new()
    }
}

/// The arguments `cargo rustc` adds to [`worker_link_arguments`], after `--`, to link a
/// worker under [`LINK_POLICY`] with the pinned toolchain's own `lld`, through the
/// `gcc-ld` directory it ships for `-fuse-ld=lld`; with `order`, a symbol ordering
/// file ([`super::link_order::symbol_order`]), its functions are laid out in that
/// order. None where the policy leaves linking to the toolchain.
pub fn ordered_link_arguments(root: &Path, order: Option<&Path>) -> Result<Vec<String>, String> {
    let target = worker_target()?;
    if !static_runtime(target) {
        return Ok(Vec::new());
    }
    let output = Command::new("rustc")
        .args(["--print", "sysroot"])
        .current_dir(root)
        .output()
        .map_err(|error| format!("rustc: {error}"))?;
    if !output.status.success() {
        return Err(format!("rustc --print sysroot: {}", output.status));
    }
    let sysroot = String::from_utf8(output.stdout).map_err(|error| format!("rustc: {error}"))?;
    let linker = Path::new(sysroot.trim())
        .join("lib/rustlib")
        .join(host()?)
        .join("bin/gcc-ld");
    if !linker.join("ld.lld").is_file() {
        return Err(format!(
            "the pinned toolchain has no {}",
            linker.join("ld.lld").display()
        ));
    }
    Ok(ordered_link_flags(&linker, order))
}

fn ordered_link_flags(linker: &Path, order: Option<&Path>) -> Vec<String> {
    let mut arguments = vec![
        format!("link-arg=-B{}", linker.display()),
        "link-arg=-fuse-ld=lld".to_owned(),
    ];
    if let Some(order) = order {
        arguments.push(format!(
            "link-arg=-Wl,--symbol-ordering-file={}",
            order.display()
        ));
        // A local symbol listed once may name several functions; lld would report
        // the duplicates it orders by the first.
        arguments.push("link-arg=-Wl,--no-warn-symbol-ordering".to_owned());
    }
    arguments
        .into_iter()
        .flat_map(|argument| ["-C".to_owned(), argument])
        .collect()
}

/// A reference's link flags under [`LINK_POLICY`]: libstdc++ and libatomic from their
/// static archives, and libgcc from its own with `-static-libgcc`.
pub fn reference_link_flags(flags: Vec<String>) -> Result<Vec<String>, String> {
    Ok(reference_flags(worker_target()?, flags))
}

fn reference_flags(target: &str, flags: Vec<String>) -> Vec<String> {
    if !static_runtime(target) {
        return flags;
    }
    let mut linked: Vec<String> = flags
        .into_iter()
        .map(|flag| match flag.as_str() {
            "-lstdc++" => "-l:libstdc++.a".to_owned(),
            "-latomic" => "-l:libatomic.a".to_owned(),
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
    if let Some(Value::Object(candidates)) = identity.get("additional_candidates") {
        for (name, record) in candidates {
            if let Some(reason) = toolchain_refusal(name, record, pinned) {
                reasons.push(reason);
            }
        }
    }
    reasons
}

/// Why the compiler a worker's record names is not the one the gate build gives that
/// worker: the pinned release, or the pinned nightly for a worker marked to use it.
fn toolchain_refusal(name: &str, record: &Value, pinned: &str) -> Option<String> {
    let Ok(spec) = super::build::candidate(name) else {
        return Some(format!("{name} is no measurement worker"));
    };
    let expected = spec.toolchain(pinned);
    let Some(toolchain) = record.get("toolchain").and_then(Value::as_str) else {
        return Some(format!("{name} records no toolchain"));
    };
    if toolchain != expected {
        return Some(format!("{name} built by {toolchain}, not {expected}"));
    }
    let found = record
        .get("rustc")
        .and_then(Value::as_str)
        .and_then(release);
    let matches = |release: &str| {
        if spec.nightly {
            release.ends_with("-nightly")
        } else {
            release == pinned
        }
    };
    match found {
        Some(release) if matches(release) => None,
        Some(release) => Some(format!("{name} built by rustc {release}, not {expected}")),
        None => Some(format!("{name} records no compiler")),
    }
}

/// Why the build `identity.json` records is not the gate build: [`refusals`], and
/// workers not linked as the training manifest and the committed trained orders give
/// ([`super::link_training::link_order_refusals`]).
pub fn identity_refusals(root: &Path, identity: &Value, pinned: &str) -> Vec<String> {
    let mut reasons = refusals(identity, pinned);
    reasons.extend(super::link_training::link_order_refusals(root, identity));
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
    let mut line = if overrides.is_empty() {
        format!("rustc {compiler}, default flags")
    } else {
        format!("rustc {compiler}, {}", overrides.join(", "))
    };
    if let Some(Value::Object(candidates)) = identity.get("additional_candidates") {
        let other: Vec<String> = candidates
            .iter()
            .filter_map(|(name, record)| {
                let toolchain = record.get("toolchain").and_then(Value::as_str)?;
                (Some(toolchain) != release(identity.get("rustc")?.as_str()?))
                    .then(|| format!("{name} by {toolchain}"))
            })
            .collect();
        if !other.is_empty() {
            line.push_str(&format!("; {}", other.join(", ")));
        }
    }
    line
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
    workers_refusals(root, &[candidate], reference)
}

/// [`pair_refusals`] for a measurement whose workloads are split among `candidates`:
/// each must be a worker of the recorded build.
pub fn workers_refusals(root: &Path, candidates: &[&Path], reference: &Path) -> Vec<String> {
    let identity = match json::read(&root.join("target/harness/identity.json")) {
        Ok(identity) => identity,
        Err(error) => return vec![format!("no build identity: {error}")],
    };
    let mut reasons = match pinned_release(root) {
        Ok(pinned) => identity_refusals(root, &identity, &pinned),
        Err(error) => vec![error],
    };
    let digest = |path: &Path| std::fs::read(path).map(|bytes| sha256::hex(&bytes)).ok();
    for candidate in candidates {
        if !digest(candidate).is_some_and(|sha| bound(identity.get("additional_candidates"), &sha))
        {
            reasons.push(format!(
                "{} is not a worker of the recorded build",
                candidate.display()
            ));
        }
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

/// The CPUs this process may use, as Linux lists them in `Cpus_allowed_list`;
/// `None` where the system does not say.
pub fn allowed_cpu_list() -> Option<Vec<usize>> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    parse_cpu_list(allowed_cpus(&status)?)
}

/// A Linux CPU list such as `1-3,7` as CPU numbers.
fn parse_cpu_list(list: &str) -> Option<Vec<usize>> {
    let mut cpus = Vec::new();
    for part in list.split(',').filter(|part| !part.is_empty()) {
        let (first, last) = part.split_once('-').unwrap_or((part, part));
        let first: usize = first.trim().parse().ok()?;
        let last: usize = last.trim().parse().ok()?;
        if first > last {
            return None;
        }
        cpus.extend(first..=last);
    }
    Some(cpus)
}

fn allowed_cpus(status: &str) -> Option<&str> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .map(str::trim)
}

#[cfg(test)]
mod tests;
