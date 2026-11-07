//! Memory measurement: peak live heap and resident-set bounds of one operation,
//! from separate heap and RSS runs under the native allocation observer
//! (DYLD_INSERT_LIBRARIES on Darwin, LD_PRELOAD on Linux). Timing workers never
//! load the observer. A known-allocation probe calibrates the observer first;
//! Linux additionally certifies its link closure and active-window guard (see
//! [`super::linux`]). Unknown or invalid coverage is never a pass.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use super::acceptance::active_cpu;
use super::diff::equal;
use super::linux::{self, py_int};
use super::timing::{Operation, competing_builds, cpu_snapshot, lock};
use super::worker::{self, Finished};
use crate::json::{self, Value};
use crate::pyrandom::PyRandom;
use crate::stats;

type R<T> = Result<T, String>;

const ALLOCATORS_AND_WRITES: [&str; 8] = [
    "malloc",
    "calloc",
    "realloc",
    "free",
    "aligned_alloc",
    "posix_memalign",
    "write",
    "writev",
];

fn s<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    Value::Object(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

fn push(value: &mut Value, key: &str, item: Value) {
    if let Value::Object(fields) = value {
        match fields.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = item,
            None => fields.push((key.to_owned(), item)),
        }
    }
}

/// What the measurement needs from the host; replaced in tests.
pub trait Host {
    fn linux(&self) -> bool;
    /// One observer run of `worker` (`operation FILE ARGS` or the probe's `ARGS`).
    fn observe(
        &self,
        worker: &Path,
        operation: Option<&str>,
        input: Option<&Path>,
        mode: &str,
        arguments: &[String],
        kernel: bool,
    ) -> Value;
    fn linked(&self, worker: &Path, interpreter: Option<&str>) -> R<Value>;
    fn nm(&self, args: &[&str], path: &Path) -> R<String>;
    fn environment(&self) -> R<Value>;
    fn kernel_requested(&self) -> Option<String>;
}

/// The real host, with workers and instrumentation under `root/target/harness`.
pub struct Native<'a> {
    pub root: &'a Path,
}

impl Native<'_> {
    fn library(&self) -> PathBuf {
        self.root.join("target/harness").join(if self.linux() {
            "allocations.so"
        } else {
            "allocations.dylib"
        })
    }
}

impl Host for Native<'_> {
    fn linux(&self) -> bool {
        cfg!(target_os = "linux")
    }

    fn observe(
        &self,
        worker: &Path,
        operation: Option<&str>,
        input: Option<&Path>,
        mode: &str,
        arguments: &[String],
        kernel: bool,
    ) -> Value {
        observe(
            self,
            &self.library(),
            worker,
            operation,
            input,
            mode,
            arguments,
            kernel,
        )
    }

    fn linked(&self, worker: &Path, interpreter: Option<&str>) -> R<Value> {
        #[cfg(unix)]
        {
            linux::linked_libraries(worker, interpreter)
        }
        #[cfg(not(unix))]
        {
            let _ = (worker, interpreter);
            Err("ELF closures need a Unix host".into())
        }
    }

    fn nm(&self, args: &[&str], path: &Path) -> R<String> {
        let output = std::process::Command::new("nm")
            .args(args)
            .arg(path)
            .output()
            .map_err(|e| format!("nm: {e}"))?;
        if !output.status.success() {
            return Err(format!("nm {} failed: {}", path.display(), output.status));
        }
        String::from_utf8(output.stdout).map_err(|e| e.to_string())
    }

    fn environment(&self) -> R<Value> {
        linux::loader_environment()
    }

    fn kernel_requested(&self) -> Option<String> {
        std::env::var("TRIERGON_RSS_KERNEL_BOUND").ok()
    }
}

fn tail(bytes: &[u8], chars: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.chars()
        .rev()
        .take(chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

#[allow(clippy::too_many_arguments)] // One observer run is fully described by these inputs.
fn observe(
    host: &dyn Host,
    library: &Path,
    worker: &Path,
    operation: Option<&str>,
    input: Option<&Path>,
    mode: &str,
    arguments: &[String],
    kernel: bool,
) -> Value {
    let linux = host.linux();
    if !library.exists() {
        return obj(vec![("status", Value::str("unsupported-platform"))]);
    }
    if linux {
        let shown = library.display().to_string();
        let environment = host.environment().and_then(|_| {
            if shown.contains(|c: char| c.is_whitespace() || c == ':') {
                Err("LD_PRELOAD path contains an unescapable separator".into())
            } else {
                Ok(())
            }
        });
        if let Err(error) = environment {
            return obj(vec![
                ("status", Value::str("observer-environment-invalid")),
                ("error", Value::str(error)),
            ]);
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let directory =
        std::env::temp_dir().join(format!("triergon-memory-{}-{nanos}", std::process::id()));
    if let Err(error) = fs::create_dir(&directory) {
        return obj(vec![
            ("status", Value::str("observer-failed")),
            ("error", Value::str(error.to_string())),
        ]);
    }
    let receipt = directory.join("window.json");
    let mut env = vec![
        (
            "TRIERGON_ALLOCATION_REPORT".to_owned(),
            Some(receipt.display().to_string()),
        ),
        ("TRIERGON_MEMORY_MODE".to_owned(), Some(mode.to_owned())),
    ];
    if linux {
        // Inherited opt-in never silently changes heap runs or controls.
        env.push(("TRIERGON_RSS_KERNEL_BOUND".into(), None));
        if kernel {
            if mode != "rss" {
                let _ = fs::remove_dir_all(&directory);
                return obj(vec![
                    ("status", Value::str("observer-environment-invalid")),
                    ("error", Value::str("kernel bound is RSS-only")),
                ]);
            }
            env.push(("TRIERGON_RSS_KERNEL_BOUND".into(), Some("1".into())));
        }
    }
    env.push((
        if linux {
            "LD_PRELOAD"
        } else {
            "DYLD_INSERT_LIBRARIES"
        }
        .into(),
        Some(library.display().to_string()),
    ));
    let mut argv: Vec<OsString> = vec![worker.as_os_str().to_owned()];
    if let Some(operation) = operation {
        argv.push(operation.into());
        argv.push(input.map_or_else(OsString::new, |p| p.as_os_str().to_owned()));
    }
    argv.extend(arguments.iter().map(OsString::from));
    let result = (|| -> Result<Value, String> {
        let Some(Finished {
            code,
            stdout,
            stderr,
        }) = worker::run(&argv, &env, Duration::from_secs(30))?
        else {
            return Err(format!(
                "Command '{}' timed out after 30 seconds",
                argv[0].to_string_lossy()
            ));
        };
        let raw = fs::read(&receipt).ok();
        if code != 0 || raw.is_none() {
            return Ok(if linux {
                obj(vec![
                    ("status", Value::str("observer-failed")),
                    ("exit_code", Value::int(code)),
                    ("stdout", Value::str(String::from_utf8_lossy(&stdout))),
                    ("stderr", Value::str(String::from_utf8_lossy(&stderr))),
                    (
                        "receipt_raw",
                        raw.map_or(Value::Null, |r| Value::str(String::from_utf8_lossy(&r))),
                    ),
                ])
            } else {
                obj(vec![
                    ("status", Value::str("observer-failed")),
                    ("stderr", Value::str(tail(&stderr, 2048))),
                ])
            });
        }
        let data =
            json::parse(&String::from_utf8_lossy(&raw.unwrap())).map_err(|e| e.to_string())?;
        let last = String::from_utf8(stdout)
            .map_err(|e| e.to_string())?
            .lines()
            .last()
            .map(str::to_owned)
            .ok_or("list index out of range")?;
        let output = json::parse(&last).map_err(|e| e.to_string())?;
        let sha = linux::file_sha(library)?;
        if !matches!(output, Value::Object(_)) {
            return Ok(obj(vec![
                ("status", Value::str("observer-invalid")),
                ("receipt", data),
                ("work", output),
            ]));
        }
        if linux && mode == "rss" && s(&data, "status") == Some("unresolved-hwm") {
            return Ok(obj(vec![
                ("status", Value::str("rss-attribution-unresolved")),
                ("receipt", data.clone()),
                ("observer", data),
                ("work", output),
                ("instrument_sha256", Value::str(sha)),
            ]));
        }
        let requested = data.get("kernel_rss_requested").is_some_and(Value::truthy);
        let invalid = !matches!(data, Value::Object(_))
            || py_int(data.get("schema")) != Some(2)
            || s(&data, "status") != Some("measured")
            || s(&data, "mode") != Some(mode)
            || (linux && (!linux::valid_receipt(&data, mode) || requested != kernel));
        if invalid {
            let mut fields = vec![
                ("status", Value::str("observer-invalid")),
                ("receipt", data),
            ];
            if linux {
                fields.extend([("work", output), ("instrument_sha256", Value::str(sha))]);
            }
            return Ok(obj(fields));
        }
        Ok(obj(vec![
            ("status", Value::str("measured")),
            ("observer", data),
            ("work", output),
            ("instrument_sha256", Value::str(sha)),
        ]))
    })();
    let _ = fs::remove_dir_all(&directory);
    result.unwrap_or_else(|error| {
        obj(vec![
            ("status", Value::str("observer-failed")),
            ("error", Value::str(error)),
        ])
    })
}

fn file_sha(path: &Path) -> R<String> {
    linux::file_sha(path)
}

fn probe_errors(heap: &Value, rss: &Value, errors: &mut Vec<String>, linux: bool) {
    let n = |v: &Value, path: &[&str]| {
        path.iter()
            .try_fold(v, |v, k| v.get(k))
            .and_then(|v| py_int(Some(v)))
    };
    let heap_ok = s(heap, "status") == Some("measured")
        && heap
            .get("observer")
            .and_then(|o| o.get("heap_peak_bytes"))
            .zip(heap.get("work").and_then(|w| w.get("expected_peak_bytes")))
            .is_some_and(|(a, b)| equal(a, b))
        && heap
            .get("observer")
            .and_then(|o| o.get("heap_live_end"))
            .zip(heap.get("observer").and_then(|o| o.get("heap_live_start")))
            .is_some_and(|(a, b)| equal(a, b));
    if !heap_ok {
        errors.push("known-allocation peak or release mismatch".into());
    }
    let grew = s(rss, "status") == Some("measured")
        && n(rss, &["observer", "rss_peak_observed"])
            .zip(n(rss, &["observer", "rss_start"]))
            .is_some_and(|(p, st)| p > st);
    if !grew {
        errors.push("known-allocation resident growth not observed".into());
    }
    if linux {
        if s(rss, "status") == Some("rss-attribution-unresolved") {
            errors.push("Linux resident upper-bound attribution unresolved; VmPeak is not a measured RSS HWM".into());
        }
        if s(heap, "status") == Some("measured")
            && !heap
                .get("observer")
                .and_then(|o| o.get("symbol_successes"))
                .and_then(Value::as_array)
                .is_some_and(|v| v.iter().all(|x| py_int(Some(x)).is_some_and(|n| n > 0)))
        {
            errors.push("Linux calibration did not exercise every admitted allocator hook".into());
        }
    }
}

static GUARD_CACHE: Mutex<Vec<(String, Value)>> = Mutex::new(Vec::new());
static KERNEL_CACHE: Mutex<Vec<(String, Value)>> = Mutex::new(Vec::new());

fn cached(cache: &Mutex<Vec<(String, Value)>>, key: &str) -> Option<Value> {
    cache
        .lock()
        .ok()?
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

fn store(cache: &Mutex<Vec<(String, Value)>>, key: String, value: Value) {
    if let Ok(mut entries) = cache.lock() {
        entries.push((key, value));
    }
}

#[cfg(test)]
pub fn clear_caches() {
    for cache in [&GUARD_CACHE, &KERNEL_CACHE] {
        if let Ok(mut entries) = cache.lock() {
            entries.clear();
        }
    }
}

fn harness_file(root: &Path, name: &str) -> PathBuf {
    root.join("target/harness").join(name)
}

/// Calibrate the observer with the known-allocation probe and prove allocator
/// coverage of both selected workers.
pub fn qualification(host: &dyn Host, root: &Path, candidate: &Path, reference: &Path) -> Value {
    if host.linux() {
        linux_qualification(host, root, candidate, reference)
    } else {
        darwin_qualification(host, root, candidate, reference)
    }
}

fn darwin_qualification(host: &dyn Host, root: &Path, candidate: &Path, reference: &Path) -> Value {
    let probe = harness_file(root, "memory-check");
    let library = harness_file(root, "allocations.dylib");
    let heap = host.observe(&probe, None, None, "heap", &[], false);
    let rss = host.observe(&probe, None, None, "rss", &[], false);
    let mut errors = Vec::new();
    probe_errors(&heap, &rss, &mut errors, false);
    let mut symbols = Vec::new();
    for (name, worker) in [("candidate", candidate), ("reference", reference)] {
        match host
            .nm(&["-u"], worker)
            .and_then(|listing| linux::allocator_symbols(&listing, false))
        {
            Ok(parsed) => {
                symbols.push((
                    name.to_owned(),
                    parsed
                        .get("allocator_imports")
                        .cloned()
                        .unwrap_or(Value::Null),
                ));
                let uncovered = parsed.get("uncovered").cloned().unwrap_or(Value::Null);
                if uncovered.truthy() {
                    errors.push(format!(
                        "{name}: uncovered allocator imports {}",
                        python_list(&uncovered)
                    ));
                }
            }
            Err(error) => errors.push(format!(
                "{name}: allocation/link coverage unavailable: {error}"
            )),
        }
    }
    let mut recorded = |path: &Path| match file_sha(path) {
        Ok(sha) => Value::str(sha),
        Err(error) => {
            errors.push(format!("{}: identity unavailable: {error}", path.display()));
            Value::Null
        }
    };
    let identities = [
        ("probe_sha256", recorded(&probe)),
        ("observer_sha256", recorded(&library)),
        (
            "worker_hashes",
            Value::object([
                ("candidate", recorded(candidate)),
                ("reference", recorded(reference)),
            ]),
        ),
    ];
    let status = if errors.is_empty() {
        "qualified-selected-workers"
    } else {
        "unqualified"
    };
    let mut fields = vec![
        ("status", Value::str(status)),
        ("errors", Value::strings(errors)),
        ("probe_heap", heap),
        ("probe_rss", rss),
        ("allocator_imports", Value::Object(symbols)),
    ];
    fields.extend(identities);
    obj(fields)
}

/// Python's `str(list_of_strings)`.
fn python_list(value: &Value) -> String {
    let items: Vec<String> = value
        .as_array()
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(|s| format!("'{s}'"))
        .collect();
    format!("[{}]", items.join(", "))
}

fn linux_qualification(host: &dyn Host, root: &Path, candidate: &Path, reference: &Path) -> Value {
    let probe = harness_file(root, "memory-check");
    let library = harness_file(root, "allocations.so");
    let mut errors: Vec<String> = Vec::new();
    let heap = host.observe(&probe, None, None, "heap", &[], false);
    let rss = host.observe(&probe, None, None, "rss", &[], false);
    probe_errors(&heap, &rss, &mut errors, true);
    let mut symbols = Vec::new();
    let mut details: Vec<(String, Value)> = Vec::new();
    let mut linked = Vec::new();
    let mut records: Vec<Value> = Vec::new();
    let mut guard = Value::Null;
    let mut identities: Vec<(&str, Value)> = Vec::new();
    let mut binding = Value::Object(vec![]);
    let mut exports = Value::Null;
    let mut attempt = || -> R<()> {
        binding = host.environment()?;
        let worker_hashes = Value::object([
            ("candidate", Value::str(file_sha(candidate)?)),
            ("reference", Value::str(file_sha(reference)?)),
        ]);
        identities = vec![
            ("observer_sha256", Value::str(file_sha(&library)?)),
            ("probe_sha256", Value::str(file_sha(&probe)?)),
            ("worker_hashes", worker_hashes),
        ];
        let roots = [
            ("candidate", candidate.to_path_buf()),
            ("reference", reference.to_path_buf()),
            ("probe", probe.clone()),
            ("observer", library.clone()),
        ];
        for (name, path) in &roots[..3] {
            linked.push((name.to_string(), host.linked(path, None)?));
        }
        let interpreter = s(&linked[2].1, "interpreter").map(str::to_owned);
        linked.push((
            "observer".into(),
            host.linked(&library, interpreter.as_deref())?,
        ));
        let linked_value = Value::Object(linked.clone());
        let mut objects: Vec<(String, PathBuf)> = Vec::new();
        // Later spellings of a known path keep its first position.
        let mut add = |path: String, at: PathBuf| match objects.iter_mut().find(|(p, _)| *p == path)
        {
            Some(slot) => slot.1 = at,
            None => objects.push((path, at)),
        };
        for (_, path) in &roots {
            add(canonical(path), path.clone());
        }
        for (_, data) in &linked {
            for row in data
                .get("libraries")
                .and_then(Value::as_array)
                .unwrap_or_default()
            {
                let real = s(row, "realpath").unwrap_or_default().to_owned();
                add(real.clone(), PathBuf::from(real));
            }
        }
        let mut defined = Vec::new();
        for (path, at) in &objects {
            defined.push((
                path.clone(),
                Value::str(host.nm(&["-D", "--defined-only", "--format=posix"], at)?),
            ));
        }
        let library_real = canonical(&library);
        let export_text = defined
            .iter()
            .find(|(p, _)| *p == library_real)
            .and_then(|(_, v)| v.as_str())
            .unwrap_or_default()
            .to_owned();
        exports = Value::str(&export_text);
        let names = linux::symbol_versions(&export_text)?;
        let mut missing: Vec<&str> = ALLOCATORS_AND_WRITES
            .iter()
            .chain(&linux::GUARD_ORDER)
            .copied()
            .filter(|n| !names.contains_key(*n))
            .collect();
        missing.sort_unstable();
        missing.dedup();
        if !missing.is_empty() {
            errors.push(format!(
                "observer: missing exported hooks {}",
                python_list(&Value::strings(missing))
            ));
        }
        let mut parsed_targets = Vec::new();
        for (role, worker) in [("candidate", candidate), ("reference", reference)] {
            let path = canonical(worker);
            let data = linked
                .iter()
                .find(|(k, _)| k == role)
                .map_or(Value::Null, |(_, v)| v.clone());
            let mut targets = vec![path.clone()];
            for row in data
                .get("libraries")
                .and_then(Value::as_array)
                .unwrap_or_default()
            {
                let real = s(row, "realpath").unwrap_or_default().to_owned();
                if !targets.contains(&real) {
                    targets.push(real);
                }
            }
            for target in targets {
                let raw = host.nm(
                    &["-D", "--undefined-only", "--format=posix"],
                    Path::new(&target),
                )?;
                let parsed = linux::allocator_symbols(&raw, true)?;
                details.push((format!("{role}:{target}"), parsed.clone()));
                let owner = Value::object([
                    ("realpath", Value::str(&target)),
                    ("sha256", Value::str(file_sha(Path::new(&target))?)),
                ]);
                if target == path {
                    let imports = parsed
                        .get("allocator_imports")
                        .cloned()
                        .unwrap_or(Value::Null);
                    if !imports.truthy() {
                        errors.push(format!(
                            "{role}: selected worker allocator coverage empty/unproven"
                        ));
                    }
                    symbols.push((role.to_owned(), imports));
                }
                parsed_targets.push((role, target, owner, parsed));
            }
        }
        let probe_undefined = host.nm(&["-D", "--undefined-only", "--format=posix"], &probe)?;
        let bindings = Value::object([
            ("observer_path", Value::str(&library_real)),
            ("probe_path", Value::str(canonical(&probe))),
            ("observer_exports", Value::str(&export_text)),
            ("probe_undefined", Value::str(probe_undefined)),
            ("defined_objects", Value::Object(defined)),
        ]);
        let ids = Value::object(identities.clone());
        let key = linux::digest(&Value::object([
            ("identities", ids),
            ("closure", linux::closure_facts(&linked_value)?),
            ("bindings", bindings.clone()),
            ("environment", binding.clone()),
        ]));
        let observer_sha = identities[0].1.as_str().unwrap_or_default().to_owned();
        let probe_sha = identities[1].1.as_str().unwrap_or_default().to_owned();
        guard = cached(&GUARD_CACHE, &key).unwrap_or(Value::Null);
        let v3 =
            |r: &Value| r.get("observer").and_then(|o| s(o, "observer")) == Some("linux-glibc-v3");
        if guard == Value::Null
            && s(&heap, "status") == Some("measured")
            && s(&rss, "status") == Some("measured")
            && v3(&heap)
            && v3(&rss)
        {
            records = vec![
                Value::object([
                    ("mode", Value::str("heap")),
                    ("case", Value::str("default")),
                    ("observed", heap.clone()),
                ]),
                Value::object([
                    ("mode", Value::str("rss")),
                    ("case", Value::str("default")),
                    ("observed", rss.clone()),
                ]),
            ];
            for mode in ["heap", "rss"] {
                for case in
                    std::iter::once("scope-startup").chain(linux::GUARD_CASES.iter().map(|c| c.0))
                {
                    let observed =
                        host.observe(&probe, None, None, mode, &[case.to_owned()], false);
                    records.push(Value::object([
                        ("mode", Value::str(mode)),
                        ("case", Value::str(case)),
                        ("observed", observed),
                    ]));
                }
            }
            guard = linux::certify(
                &records,
                &linked_value,
                &observer_sha,
                &probe_sha,
                &bindings,
            )?;
            store(&GUARD_CACHE, key, guard.clone());
        }
        if !linux::certificate_valid(
            Some(&guard),
            &linked_value,
            Some(&observer_sha),
            Some(&probe_sha),
        ) {
            errors.push("Linux active-window guard certificate unproven".into());
        }
        for (role, target, owner, parsed) in parsed_targets {
            let reviewed = linux::conditional_coverage(
                &parsed,
                &owner,
                Some(&guard),
                &linked_value,
                Some(&observer_sha),
                Some(&probe_sha),
            );
            let remaining = reviewed
                .get("remaining_uncovered")
                .cloned()
                .unwrap_or(Value::Null);
            let label = format!("{role}:{target}");
            details.retain(|(k, _)| *k != label);
            details.push((label, reviewed));
            if remaining.truthy() {
                errors.push(format!(
                    "{target}: uncovered allocator imports {}",
                    python_list(&remaining)
                ));
            }
        }
        Ok(())
    };
    if let Err(error) = attempt() {
        errors.push(format!("Linux guard/link coverage unavailable: {error}"));
    }
    let requested = host.kernel_requested();
    let kernel_requested = requested.as_deref() == Some("1");
    if !matches!(requested.as_deref(), None | Some("" | "0" | "1")) {
        errors.push("invalid optional kernel RSS request".into());
    }
    let (mut kernel_certificate, mut kernel_records) = (Value::Null, Vec::new());
    if kernel_requested && errors.is_empty() {
        let observer_sha = identities
            .iter()
            .find(|(k, _)| *k == "observer_sha256")
            .and_then(|(_, v)| v.as_str())
            .unwrap_or_default()
            .to_owned();
        let probe_sha = identities
            .iter()
            .find(|(k, _)| *k == "probe_sha256")
            .and_then(|(_, v)| v.as_str())
            .unwrap_or_default()
            .to_owned();
        let mut kernel = || -> R<()> {
            let profile = linux::load_kernel_profile(root, &observer_sha, &probe_sha)?;
            let key = linux::digest(&Value::object([
                ("profile", profile.clone()),
                ("identities", Value::object(identities.clone())),
                (
                    "guard",
                    guard
                        .get("certificate_sha256")
                        .cloned()
                        .unwrap_or(Value::Null),
                ),
            ]));
            kernel_certificate = if let Some(certificate) = cached(&KERNEL_CACHE, &key) {
                certificate
            } else {
                for case in ["default", "writev", "transient"] {
                    let arguments = if case == "default" {
                        vec![]
                    } else {
                        vec![case.to_owned()]
                    };
                    let observed = host.observe(&probe, None, None, "rss", &arguments, true);
                    kernel_records.push(Value::object([
                        ("case", Value::str(case)),
                        ("observed", observed),
                    ]));
                }
                let certificate = linux::certify_kernel(
                    root,
                    &kernel_records,
                    &profile,
                    &observer_sha,
                    &probe_sha,
                )?;
                store(&KERNEL_CACHE, key, certificate.clone());
                certificate
            };
            if !linux::kernel_certificate_valid(
                root,
                Some(&kernel_certificate),
                Some(&observer_sha),
                Some(&probe_sha),
            ) {
                return Err("kernel RSS certificate unavailable or stale".into());
            }
            Ok(())
        };
        if let Err(error) = kernel() {
            errors.push(format!("optional kernel RSS coverage unavailable: {error}"));
        }
    }
    let linked = Value::Object(linked);
    let status = if errors.is_empty() {
        "qualified-selected-workers"
    } else {
        "unqualified"
    };
    let mut fields = vec![
        ("status", Value::str(status)),
        ("errors", Value::strings(errors)),
        ("probe_heap", heap),
        ("probe_rss", rss),
        ("allocator_imports", Value::Object(symbols)),
        ("allocator_import_details", Value::Object(details)),
        ("linked_library_hashes", linux::linked_hashes(&linked)),
        ("linked_libraries", linked),
        ("observer_exports_raw", exports),
        ("binding_environment", binding),
        ("platform", Value::str("Linux")),
        ("architecture", Value::str(linux::machine())),
        ("runtime_guard_required", Value::Bool(true)),
        ("runtime_guard_certificate", guard),
        ("attempted_guard_calibration", Value::Array(records)),
        ("kernel_rss_required", Value::Bool(kernel_requested)),
        ("kernel_rss_certificate", kernel_certificate),
        ("attempted_kernel_calibration", Value::Array(kernel_records)),
    ];
    fields.extend(identities);
    // Keep the collector's field order: linked_libraries before its hashes.
    let order = |k: &str| fields.iter().position(|(f, _)| *f == k);
    if let (Some(a), Some(b)) = (order("linked_library_hashes"), order("linked_libraries")) {
        fields.swap(a, b);
    }
    obj(fields)
}

fn canonical(path: &Path) -> String {
    fs::canonicalize(path).map_or_else(|_| path.display().to_string(), |p| p.display().to_string())
}

/// Whether the certificate or any RSS observation requests kernel measurement.
pub(super) fn kernel_used(certificate: &Value, samples: &[Value]) -> bool {
    certificate
        .get("kernel_rss_required")
        .is_some_and(Value::truthy)
        || samples
            .iter()
            .filter_map(|sample| match sample {
                Value::Object(entries) => Some(entries),
                _ => None,
            })
            .flatten()
            .filter(|(role, _)| role.ends_with("_rss"))
            .filter_map(|(_, sample)| sample.get("observer"))
            .any(|observer| py_int(observer.get("kernel_rss_requested")) == Some(1))
}

/// Recompute the memory verdict of raw samples. Failures return the failing
/// status with the samples and certificate kept.
pub fn evaluate(
    root: &Path,
    samples: &[Value],
    pairs: i128,
    seed: u64,
    certificate: &Value,
    expected_outputs: Option<&Value>,
) -> Value {
    let linux = s(certificate, "platform") == Some("Linux");
    let failed = |status: &str| {
        obj(vec![
            ("status", Value::str(status)),
            ("samples", Value::Array(samples.to_vec())),
            ("certificate", certificate.clone()),
        ])
    };
    let observer = s(certificate, "observer_sha256");
    let probe = s(certificate, "probe_sha256");
    let linked = certificate
        .get("linked_libraries")
        .cloned()
        .unwrap_or(Value::Object(vec![]));
    let guarded = linux && s(certificate, "status") == Some("qualified-selected-workers");
    let guard = certificate.get("runtime_guard_certificate");
    if guarded && !linux::certificate_valid(guard, &linked, observer, probe) {
        return failed("observer-unqualified");
    }
    let n = |v: &Value, k: &str| py_int(v.get(k));
    let (mut rss, mut heaps, mut totals) = (Vec::new(), Vec::new(), Vec::new());
    for row in samples {
        let Value::Object(entries) = row else {
            return failed("invalid-memory-observation");
        };
        if linux {
            let kernel_rows: Vec<&Value> = entries
                .iter()
                .filter(|(k, _)| k.ends_with("_rss"))
                .filter_map(|(_, v)| v.get("observer"))
                .collect();
            if kernel_used(certificate, std::slice::from_ref(row)) {
                let ok = matches!(
                    certificate.get("kernel_rss_required"),
                    Some(Value::Bool(true))
                ) && linux::kernel_certificate_valid(
                    root,
                    certificate.get("kernel_rss_certificate"),
                    observer,
                    probe,
                ) && kernel_rows
                    .iter()
                    .all(|d| n(d, "kernel_rss_requested") == Some(1));
                if !ok {
                    return failed("observer-unqualified");
                }
            }
        }
        if guarded
            && entries.iter().any(|(_, v)| {
                !linux::sample_valid(v.get("observer"), guard, &linked, observer, probe)
            })
        {
            return failed("invalid-memory-observation");
        }
        if linux {
            let mut keys: Vec<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
            keys.sort_unstable();
            let receipts_ok = entries.iter().all(|(k, v)| {
                let mode = k.rsplit('_').next().unwrap_or_default();
                v.get("observer")
                    .is_some_and(|o| linux::valid_receipt(o, mode))
                    && s(v, "instrument_sha256") == observer
            });
            if keys
                != [
                    "candidate_heap",
                    "candidate_rss",
                    "reference_heap",
                    "reference_rss",
                ]
                || !receipts_ok
            {
                return failed("invalid-memory-observation");
            }
        }
        if entries
            .iter()
            .any(|(_, v)| s(v, "status") != Some("measured"))
        {
            return failed("measurement-failed");
        }
        let reference_output = row.get("reference_heap").and_then(|h| h.get("work"));
        if expected_outputs.is_none() && reference_output.is_none() {
            return failed("invalid-memory-observation");
        }
        let work_matches = entries.iter().all(|(key, sample)| {
            let expected = match expected_outputs {
                Some(outputs) => key.split_once('_').and_then(|(role, _)| outputs.get(role)),
                None => reference_output,
            };
            sample
                .get("work")
                .zip(expected)
                .is_some_and(|(a, b)| equal(a, b))
        });
        if !work_matches {
            return failed("unequal-memory-work");
        }
        let (Some(a), Some(b)) = (
            row.get("candidate_rss").and_then(|r| r.get("observer")),
            row.get("reference_rss").and_then(|r| r.get("observer")),
        ) else {
            return failed("invalid-memory-observation");
        };
        let field = |o: &Value, k: &str| n(o, k);
        let values = (|| -> Option<(i128, i128, i128, i128)> {
            let candidate_peak = if linux {
                field(a, "rss_peak_upper_bound_bytes")?
            } else {
                field(a, "hwm_end")?.max(field(a, "rss_peak_observed")?)
            };
            let upper = candidate_peak - field(a, "rss_start")? + 2 * field(a, "page_size")?;
            let lower = field(b, "rss_peak_observed")?
                - field(b, "rss_start")?
                - 2 * field(b, "page_size")?;
            let reference_total = if linux {
                field(b, "rss_peak_observed")?
            } else {
                field(b, "hwm_end")?.max(field(b, "rss_peak_observed")?)
            };
            // The policy's total resident peak: the candidate's ceiling plus two
            // pages against the reference's observed peak minus two pages.
            let total_upper = candidate_peak + 2 * field(a, "page_size")?;
            let total_lower = reference_total - 2 * field(b, "page_size")?;
            Some((upper, lower, total_upper, total_lower))
        })();
        let Some((upper, lower, total_upper, total_lower)) = values else {
            return failed("invalid-memory-observation");
        };
        if total_lower <= 0 || total_upper <= 0 {
            return failed("total-rss-attribution-unresolved");
        }
        rss.push((upper, lower));
        let heap = |k: &str| {
            row.get(k)
                .and_then(|r| r.get("observer"))
                .and_then(|o| n(o, "heap_peak_bytes"))
        };
        let (Some(ch), Some(rh)) = (heap("candidate_heap"), heap("reference_heap")) else {
            return failed("invalid-memory-observation");
        };
        heaps.push((ch, rh));
        totals.push((total_upper, total_lower));
    }
    let interval = |values: &[(i128, i128)]| -> R<Value> {
        let pairs: Vec<(f64, f64)> = values.iter().map(|&(a, b)| (a as f64, b as f64)).collect();
        let i = stats::ratio_interval(&pairs, seed, 4000)?;
        Ok(Value::object([
            ("median", Value::float(i.median)),
            ("lower_95", Value::float(i.lower_95)),
            ("upper_95", Value::float(i.upper_95)),
        ]))
    };
    let (Ok(heap), Ok(total)) = (interval(&heaps), interval(&totals)) else {
        return failed("invalid-memory-observation");
    };
    // The page allowance can make a growth bound nonpositive even when
    // both total resident bounds are valid.
    let growth = interval(&rss).unwrap_or(Value::Null);
    let mut fields = vec![
        ("status", Value::str("qualified")),
        (
            "total_rss_ratio_upper_95",
            total.get("upper_95").cloned().unwrap(),
        ),
        (
            "heap_ratio_upper_95",
            heap.get("upper_95").cloned().unwrap(),
        ),
        (
            "rss_growth_ratio_upper_95",
            growth.get("upper_95").cloned().unwrap_or(Value::Null),
        ),
        ("total_rss_ratio", total),
        ("heap_ratio", heap),
        (
            "heap_lower_in_each_pair",
            Value::Bool(
                heaps
                    .iter()
                    .all(|(candidate, reference)| candidate < reference),
            ),
        ),
        ("rss_growth_ratio", growth),
        ("pairs", Value::int(pairs)),
        ("samples", Value::Array(samples.to_vec())),
        ("certificate", certificate.clone()),
        ("seed", Value::int(seed as i64)),
    ];
    if linux {
        fields.extend([
            ("total_rss_basis", Value::str("graded: candidate resident peak ceiling plus two pages / reference observed resident peak minus two pages; not a VM ratio")),
            ("rss_growth_basis", Value::str("diagnostic: candidate resident ceiling growth plus two pages / reference observed resident growth minus two pages")),
            ("rss_exact_peak_status", Value::str("unresolved")),
        ]);
    }
    obj(fields)
}

struct Invocation<'a> {
    operation: Option<&'a str>,
    input: &'a Path,
    arguments: &'a [String],
    expected_outputs: Option<&'a Value>,
}

/// Measure `pairs` randomly ordered sets of the four observer runs.
#[allow(clippy::too_many_arguments)] // The measured operation is fully described by these inputs.
fn measure(
    host: &dyn Host,
    root: &Path,
    candidate: &Path,
    reference: &Path,
    pairs: usize,
    seed: u64,
    certificate: &Value,
    invocation: Invocation<'_>,
) -> R<Value> {
    if s(certificate, "status") != Some("qualified-selected-workers") {
        return Ok(obj(vec![
            ("status", Value::str("observer-unqualified")),
            ("certificate", certificate.clone()),
        ]));
    }
    let linux = s(certificate, "platform") == Some("Linux");
    if linux && !linux::certificate_current(root, certificate) {
        return Ok(obj(vec![
            ("status", Value::str("instrumentation-changed")),
            ("certificate", certificate.clone()),
        ]));
    }
    let input_sha = file_sha(invocation.input)?;
    let mut rng = PyRandom::new(seed);
    let kernel = certificate
        .get("kernel_rss_required")
        .is_some_and(Value::truthy);
    let mut samples = Vec::new();
    for _ in 0..pairs {
        let mut jobs = [
            ("candidate", "heap"),
            ("candidate", "rss"),
            ("reference", "heap"),
            ("reference", "rss"),
        ];
        rng.shuffle(&mut jobs);
        let mut row = Vec::new();
        for (name, mode) in jobs {
            let worker = if name == "candidate" {
                candidate
            } else {
                reference
            };
            let observed = host.observe(
                worker,
                invocation.operation,
                Some(invocation.input),
                mode,
                invocation.arguments,
                kernel && mode == "rss",
            );
            row.push((format!("{name}_{mode}"), observed));
        }
        samples.push(Value::Object(row));
    }
    let mut result = evaluate(
        root,
        &samples,
        pairs as i128,
        seed,
        certificate,
        invocation.expected_outputs,
    );
    push(&mut result, "input_sha256", Value::str(&input_sha));
    if file_sha(invocation.input)? != input_sha {
        push(&mut result, "status", Value::str("input-changed"));
    }
    if linux && !linux::certificate_current(root, certificate) {
        push(&mut result, "status", Value::str("instrumentation-changed"));
    }
    Ok(result)
}

/// Check the exact worker closure and emit the observer's calibration results.
pub fn admission(root: &Path, candidate: &Path, reference: &Path) -> R<()> {
    if !cfg!(all(
        any(target_arch = "aarch64", target_arch = "arm"),
        target_os = "linux",
        target_env = "gnu"
    )) {
        return Err(
            "memory admission requires native AArch64 or 32-bit ARMv7 Linux with glibc".into(),
        );
    }
    let _lock = lock()?;
    let busy = competing_builds()?;
    if !busy.is_empty() {
        return Err(format!("competing processes: {}", busy.join(", ")));
    }
    let candidate =
        fs::canonicalize(candidate).map_err(|e| format!("{}: {e}", candidate.display()))?;
    let reference =
        fs::canonicalize(reference).map_err(|e| format!("{}: {e}", reference.display()))?;
    let mut certificate = qualification(&Native { root }, root, &candidate, &reference);
    let result = if s(&certificate, "status") != Some("qualified-selected-workers") {
        Err("memory observer refused admission".into())
    } else if !linux::certificate_current(root, &certificate) {
        let reason = "memory observer admission became stale";
        push(&mut certificate, "status", Value::str("unqualified"));
        push(&mut certificate, "errors", Value::strings([reason]));
        Err(reason.into())
    } else {
        Ok(())
    };
    println!("{}", json::canonical(&certificate));
    result
}

/// One memory collection under the measurement lock, rejected (but kept) when
/// tracked processes used CPU meanwhile.
pub fn collect(
    root: &Path,
    candidate: &Path,
    reference: &Path,
    input: &Path,
    pairs: usize,
    seed: u64,
    operation: &Operation,
) -> R<Value> {
    let name = format!("{}memory", operation.prefix);
    let arguments: Vec<String> = operation
        .arguments
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    collect_invocation(
        root,
        candidate,
        reference,
        pairs,
        seed,
        Invocation {
            operation: Some(&name),
            input,
            arguments: &arguments,
            expected_outputs: None,
        },
    )
}

/// Observe workers with explicit arguments and a correctness-checked output per role.
#[allow(clippy::too_many_arguments)] // The command, input and paired measurement settings.
pub(crate) fn collect_command(
    root: &Path,
    candidate: &Path,
    reference: &Path,
    input: &Path,
    pairs: usize,
    seed: u64,
    arguments: &[String],
    expected_outputs: &Value,
) -> R<Value> {
    collect_invocation(
        root,
        candidate,
        reference,
        pairs,
        seed,
        Invocation {
            operation: None,
            input,
            arguments,
            expected_outputs: Some(expected_outputs),
        },
    )
}

fn collect_invocation(
    root: &Path,
    candidate: &Path,
    reference: &Path,
    pairs: usize,
    seed: u64,
    invocation: Invocation<'_>,
) -> R<Value> {
    let _lock = lock()?;
    let busy = competing_builds()?;
    if !busy.is_empty() {
        return Ok(obj(vec![
            ("status", Value::str("contaminated")),
            ("contaminants", Value::strings(busy)),
            ("samples", Value::Array(vec![])),
        ]));
    }
    let host = Native { root };
    let before = cpu_snapshot()?;
    let certificate = qualification(&host, root, candidate, reference);
    let mut result = measure(
        &host,
        root,
        candidate,
        reference,
        pairs,
        seed,
        &certificate,
        invocation,
    )?;
    let after = cpu_snapshot()?;
    let contaminants = active_cpu(&before, &after, false);
    push(&mut result, "cpu_before", before);
    push(&mut result, "cpu_after", after);
    push(
        &mut result,
        "contaminants",
        Value::strings(contaminants.clone()),
    );
    if !contaminants.is_empty() {
        let status = result.get("status").cloned().unwrap_or(Value::Null);
        push(&mut result, "measurement_status", status);
        push(&mut result, "status", Value::str("contaminated"));
    }
    Ok(result)
}

/// Whether a row's recorded memory evidence is complete, bound to the selected
/// workers and instrumentation, and reproduces its recorded ratios.
pub fn evidence_valid(
    root: &Path,
    row: &Value,
    memory: &Value,
    contract: &Value,
    identity: &Value,
    selected: &Value,
    require_linux: bool,
) -> bool {
    let check = || -> Option<bool> {
        let certificate = memory.get("certificate")?;
        let raw = memory.get("samples")?.as_array()?;
        let probe = certificate.get("probe_heap")?;
        let mut covered = s(certificate, "status")? == "qualified-selected-workers"
            && !certificate.get("errors")?.truthy()
            && equal(certificate.get("worker_hashes")?, selected);
        if py_int(identity.get("schema")).unwrap_or(1) >= 2 {
            covered &= !memory.get("contaminants").is_some_and(Value::truthy)
                && active_cpu(
                    memory.get("cpu_before")?,
                    memory.get("cpu_after")?,
                    require_linux,
                )
                .is_empty();
        }
        if let Some(input) = row.get("input_sha256").filter(|v| v.truthy()) {
            covered &= memory.get("input_sha256") == Some(input);
        }
        if let Some(instrumentation) = identity.get("instrumentation").filter(|v| v.truthy()) {
            for key in ["probe_sha256", "observer_sha256"] {
                covered &= certificate.get(key) == instrumentation.get(key);
            }
        }
        let observer = probe.get("observer")?;
        covered &= s(probe, "status")? == "measured"
            && equal(
                observer.get("heap_peak_bytes")?,
                probe.get("work")?.get("expected_peak_bytes")?,
            )
            && equal(
                observer.get("heap_live_start")?,
                observer.get("heap_live_end")?,
            );
        let pairs = py_int(memory.get("pairs"))?;
        let minimum = py_int(contract.get("minimum_pairs"))?;
        if !(covered && raw.len() as i128 == pairs && raw.len() as i128 >= minimum) {
            return Some(false);
        }
        for sample in raw {
            let Value::Object(entries) = sample else {
                return None;
            };
            for (_, v) in entries {
                covered &= v.get("instrument_sha256")? == certificate.get("observer_sha256")?
                    && s(v.get("observer")?, "status")? == "measured";
            }
        }
        let seed = py_int(memory.get("seed")).unwrap_or(280) as u64;
        let recomputed = evaluate(root, raw, pairs, seed, certificate, None);
        let same = [
            "status",
            "total_rss_ratio_upper_95",
            "heap_ratio_upper_95",
            "rss_growth_ratio_upper_95",
            "total_rss_ratio",
            "heap_ratio",
            "rss_growth_ratio",
        ]
        .iter()
        .all(|k| match (recomputed.get(k), memory.get(k)) {
            (Some(a), Some(b)) => equal(a, b),
            (None, None) => true,
            _ => false,
        });
        Some(covered && same)
    };
    check().unwrap_or(false)
}
