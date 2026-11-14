//! Evidence reuse: a gate command takes the result of an evidence set `main`
//! cites instead of measuring again when everything that can change its
//! measurement is the same.
//!
//! Every gate command ([`super::report`]'s timing and memory collections, `grade`,
//! `isobmff-bench`, `codecs time`, `capture qualify` and `external-vector`) derives a
//! [`Key`] before it measures: its command and settings, the files that define its
//! cases, the bytes of every worker it runs and of their shared libraries, the
//! allocation observer and probe, the measuring and grading code, the
//! toolchains, the gate target and the host class. It records the key and its
//! exit status in `gate-result.json` in its output directory. `evidence upload`
//! lists those results in the set's index entry, and a set's results count once
//! the set is kept, which happens when a result of it is cited. A later run of
//! the same command with the same key prints the cited set, records a
//! `gate-result.json` that names it as reused, and exits with its status;
//! `--full` measures instead.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::json::{self, Value};
use crate::sha256;

type R<T> = Result<T, String>;

const KEY_SCHEMA: &str = "gate-measurement-key-v1";
const RESULT_SCHEMA: &str = "gate-result-v1";
/// The file each gate command writes in its output directory.
pub const RESULT: &str = "gate-result.json";

/// Everything a gate command's result depends on beyond its own settings and
/// the build's shared inputs ([`super::provenance::shared_inputs`]): the code
/// that measures, records and grades, the oracles, the reviewed input lists and
/// the observers it loads. Any change re-measures every feature.
const CODE: [&str; 14] = [
    "xtask/src",
    "harness/oracles.py",
    "harness/fate/samples.json",
    "harness/fonts/corpus.json",
    "xtask/Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "harness/policy.json",
    "harness/allocations.c",
    "harness/allocations-linux.c",
    "harness/memory-check.c",
    "harness/portable-memory-check.c",
    "harness/rss-bound",
    "harness/rss-bound-api.h",
];

/// What a gate command's result depends on, and its SHA-256.
pub struct Key {
    pub sha256: String,
    pub components: Value,
}

impl Key {
    /// The key of `command` run with `settings` over `files` (contracts,
    /// manifests and other documents that define its cases) and `workers`.
    pub fn new(
        root: &Path,
        command: &str,
        settings: Value,
        files: &[&Path],
        workers: &[(&str, &Path)],
    ) -> R<Self> {
        let files = files
            .iter()
            .map(|path| Ok((relative(root, path), Value::str(file_sha(path)?))))
            .collect::<R<Vec<_>>>()?;
        let workers = workers
            .iter()
            .map(|(role, path)| Ok(((*role).to_owned(), worker(path)?)))
            .collect::<R<Vec<_>>>()?;
        let harness = root.join("target/harness");
        let observer = ["allocations.so", "allocations.dylib"]
            .into_iter()
            .map(|name| harness.join(name))
            .find(|path| path.is_file());
        let components = Value::object([
            ("schema", Value::str(KEY_SCHEMA)),
            ("command", Value::str(command)),
            ("settings", settings),
            ("files", Value::Object(files)),
            ("workers", Value::Object(workers)),
            (
                "instrumentation",
                Value::object([
                    (
                        "observer_sha256",
                        observer.map_or(Ok(Value::Null), |p| file_sha(&p).map(Value::str))?,
                    ),
                    ("probe_sha256", optional_sha(&harness.join("memory-check"))?),
                ]),
            ),
            ("code", code(root)?),
            (
                "toolchain",
                Value::object([
                    ("rustc", Value::str(output("rustc", &["-vV"])?)),
                    ("cc", Value::str(output("cc", &["--version"])?)),
                    ("python", python()?),
                ]),
            ),
            ("target", Value::str(super::report::gate_target()?)),
            ("host", host()?),
        ]);
        Ok(Self {
            sha256: sha256::hex(json::canonical(&components).as_bytes()),
            components,
        })
    }

    /// The key without the CPUs this process may use, for a step that is not
    /// timed (`grade`).
    pub fn untimed(mut self) -> Self {
        if let Value::Object(fields) = &mut self.components
            && let Some((_, Value::Object(host))) = fields.iter_mut().find(|(k, _)| k == "host")
        {
            host.retain(|(k, _)| k != "cpus");
        }
        self.sha256 = sha256::hex(json::canonical(&self.components).as_bytes());
        self
    }

    /// A key that also binds the reports it reads, each with the gate result
    /// recorded beside it: `grade` binds the collections it joins. A measured
    /// report is bound by its bytes, so measuring again with `--full` changes the
    /// key; a reused collection, which has no report, by the cited report's hash.
    pub fn with_inputs(mut self, reports: &[&Path]) -> R<Self> {
        let inputs = reports
            .iter()
            .map(|report| {
                let result = read(report.parent().unwrap_or(Path::new(".")))?;
                let bytes = if report.is_file() {
                    Value::str(file_sha(report)?)
                } else {
                    result.get("report_sha256").cloned().unwrap_or(Value::Null)
                };
                Ok(Value::object([
                    ("key", result.get("key").cloned().unwrap_or(Value::Null)),
                    ("report_sha256", bytes),
                ]))
            })
            .collect::<R<Vec<_>>>()?;
        if let Value::Object(fields) = &mut self.components {
            fields.push(("inputs".into(), Value::Array(inputs)));
        }
        self.sha256 = sha256::hex(json::canonical(&self.components).as_bytes());
        Ok(self)
    }
}

/// The result a cited set holds for `key`: the newest kept set whose index entry
/// lists a result with this key.
pub fn cited(root: &Path, key: &Key) -> R<Option<Value>> {
    let index = crate::checks::read_json(&root.join(crate::evidence::INDEX))?;
    let sets = index
        .get("sets")
        .and_then(Value::as_array)
        .unwrap_or_default();
    Ok(sets.iter().rev().find_map(|set| {
        if set.get("retention").and_then(Value::as_str) != Some("retained") {
            return None;
        }
        let result = set
            .get("results")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .find(|result| result.get("key").and_then(Value::as_str) == Some(&key.sha256))?;
        Some(Value::object([
            ("set", set.get("name").cloned().unwrap_or(Value::Null)),
            (
                "set_sha256",
                set.get("sha256").cloned().unwrap_or(Value::Null),
            ),
            (
                "source_revisions",
                set.get("source_revisions").cloned().unwrap_or(Value::Null),
            ),
            ("path", result.get("path").cloned().unwrap_or(Value::Null)),
            (
                "status",
                result.get("status").cloned().unwrap_or(Value::Null),
            ),
            (
                "report_sha256",
                result.get("report_sha256").cloned().unwrap_or(Value::Null),
            ),
        ]))
    }))
}

/// Run a gate step: take the cited result for `key` unless `full`, else run it
/// and record its status.
pub fn step(
    root: &Path,
    output: &Path,
    key: &Key,
    full: bool,
    run: impl FnOnce() -> R<i32>,
) -> R<i32> {
    if !full && let Some(cited) = cited(root, key)? {
        let status = cited
            .get("status")
            .and_then(Value::as_f64)
            .ok_or("cited result without a status")? as i32;
        println!(
            "reused {} from evidence set {} (status {status}); --full measures it again",
            cited
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            cited.get("set").and_then(Value::as_str).unwrap_or_default(),
        );
        record(output, key, status, Some(cited))?;
        return Ok(status);
    }
    let status = run()?;
    record(output, key, status, None)?;
    Ok(status)
}

/// Record the result of a gate command in `output`. A measured result binds the
/// report it wrote (`report.json`), if any; a reused one, the cited report's.
pub fn record(output: &Path, key: &Key, status: i32, reused: Option<Value>) -> R<()> {
    let report = output.join("report.json");
    let report_sha256 = match &reused {
        Some(cited) => cited.get("report_sha256").cloned().unwrap_or(Value::Null),
        None if report.is_file() => Value::str(file_sha(&report)?),
        None => Value::Null,
    };
    let result = Value::object([
        ("schema", Value::str(RESULT_SCHEMA)),
        ("key", Value::str(&key.sha256)),
        ("status", Value::int(status)),
        ("report_sha256", report_sha256),
        ("reused", reused.unwrap_or(Value::Null)),
        ("components", key.components.clone()),
    ]);
    fs::create_dir_all(output).map_err(|e| format!("{}: {e}", output.display()))?;
    crate::corpus::write_json(&output.join(RESULT), &result)
}

/// The gate result recorded in `directory`.
pub fn read(directory: &Path) -> R<Value> {
    crate::checks::read_json(&directory.join(RESULT))
}

/// Whether `directory` holds a reused result rather than a measurement.
pub fn was_reused(directory: &Path) -> bool {
    read(directory).is_ok_and(|result| result.get("reused").is_some_and(|r| *r != Value::Null))
}

/// The fresh gate results among an evidence set's files, as its index entry
/// lists them.
pub fn listed(entries: &[(String, Vec<u8>)]) -> R<Vec<Value>> {
    let mut results = Vec::new();
    for (path, data) in entries {
        if path.rsplit('/').next() != Some(RESULT) {
            continue;
        }
        let text = std::str::from_utf8(data).map_err(|_| format!("{path}: not UTF-8"))?;
        let result = json::parse_strict(text).map_err(|e| format!("{path}: {e}"))?;
        if result.get("schema").and_then(Value::as_str) != Some(RESULT_SCHEMA) {
            return Err(format!("{path}: not a gate result"));
        }
        if result.get("reused").is_some_and(|r| *r != Value::Null) {
            continue;
        }
        results.push(Value::object([
            ("path", Value::str(path)),
            (
                "command",
                result
                    .get("components")
                    .and_then(|c| c.get("command"))
                    .cloned()
                    .unwrap_or(Value::Null),
            ),
            ("key", result.get("key").cloned().unwrap_or(Value::Null)),
            (
                "status",
                result.get("status").cloned().unwrap_or(Value::Null),
            ),
            (
                "report_sha256",
                result.get("report_sha256").cloned().unwrap_or(Value::Null),
            ),
        ]));
    }
    Ok(results)
}

fn file_sha(path: &Path) -> R<String> {
    sha256::file_hex(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn optional_sha(path: &Path) -> R<Value> {
    if path.is_file() {
        file_sha(path).map(Value::str)
    } else {
        Ok(Value::Null)
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// A probe's output; a probe that cannot run or fails is an error.
fn output(program: &str, args: &[&str]) -> R<String> {
    let probe = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if !probe.status.success() {
        return Err(format!("{program} {}: {}", args.join(" "), probe.status));
    }
    Ok(String::from_utf8_lossy(&probe.stdout).trim().to_owned())
}

/// The Python that runs the oracles and the capture input generator
/// ([`crate::python::interpreter`]), with numpy's version where it has one,
/// probed as they run it.
pub fn python() -> R<Value> {
    let version = output(
        &crate::python::interpreter(),
        &[
            "-c",
            "import sys\ntry:\n import numpy; n = numpy.__version__\nexcept ImportError:\n n = None\nprint(sys.version.split()[0], n)",
        ],
    )?;
    Ok(Value::str(version))
}

/// A worker's bytes and, on Linux, every shared library its loader resolves.
fn worker(path: &Path) -> R<Value> {
    let mut fields = vec![("sha256", Value::str(file_sha(path)?))];
    #[cfg(unix)]
    if cfg!(target_os = "linux") {
        let linked = super::linux::linked_libraries(path, None)?;
        let libraries = linked
            .get("libraries")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .map(|library| {
                (
                    library
                        .get("realpath")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    library.get("sha256").cloned().unwrap_or(Value::Null),
                )
            })
            .collect();
        fields.push(("libraries", Value::Object(libraries)));
    }
    Ok(Value::object(fields))
}

/// The SHA-256 of every file under [`CODE`] and every shared build input, by
/// path, and of them all.
fn code(root: &Path) -> R<Value> {
    let mut files: Vec<PathBuf> = super::provenance::shared_inputs(root)?
        .iter()
        .map(|name| root.join(name))
        .collect();
    let oracles = fs::read_dir(root.join("harness")).map_err(|e| format!("harness: {e}"))?;
    for entry in oracles {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_some_and(|e| e == "py") {
            files.push(path);
        }
    }
    for entry in CODE {
        let path = root.join(entry);
        if path.is_dir() {
            files.extend(
                crate::evidence::files(&path)?
                    .into_iter()
                    .map(|(_, file)| file),
            );
        } else if path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    files.dedup();
    let mut all = sha256::Sha256::default();
    for file in &files {
        let line = format!("{}  {}\n", file_sha(file)?, relative(root, file));
        all.update(line.as_bytes());
    }
    Ok(Value::object([
        ("files", Value::int(files.len() as i64)),
        ("sha256", Value::str(all.finish_hex())),
    ]))
}

/// The host class: architecture and personality, kernel, CPU model, machine
/// type, page size, CPUs online and the CPUs this process may use.
fn host() -> R<Value> {
    let read = |path: &str| fs::read_to_string(path).map_or(Value::Null, |t| Value::str(t.trim()));
    let cpu: Vec<String> = fs::read_to_string("/proc/cpuinfo")
        .unwrap_or_default()
        .lines()
        .filter(|line| {
            [
                "model name",
                "CPU implementer",
                "CPU architecture",
                "CPU variant",
                "CPU part",
                "CPU revision",
                "Features",
                "flags",
            ]
            .iter()
            .any(|key| line.starts_with(key))
        })
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .fold(Vec::new(), |mut lines, line| {
            if !lines.contains(&line) {
                lines.push(line);
            }
            lines
        });
    Ok(Value::object([
        ("architecture", Value::str(std::env::consts::ARCH)),
        ("machine", Value::str(output("uname", &["-m"])?)),
        ("kernel_release", Value::str(output("uname", &["-r"])?)),
        ("cpu", Value::strings(cpu)),
        ("product", read("/sys/devices/virtual/dmi/id/product_name")),
        ("cpus", super::gate_build::cpus()),
        ("page_size", Value::str(output("getconf", &["PAGESIZE"])?)),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(root: &Path, retention: &str, key: &str) {
        fs::create_dir_all(root.join("harness/evidence")).unwrap();
        let set = Value::object([
            ("name", Value::str("run")),
            ("retention", Value::str(retention)),
            ("sha256", Value::str("ab")),
            (
                "results",
                Value::Array(vec![Value::object([
                    ("path", Value::str("benchmark/f/gate-result.json")),
                    ("key", Value::str(key)),
                    ("status", Value::int(1)),
                ])]),
            ),
        ]);
        crate::corpus::write_json(
            &root.join(crate::evidence::INDEX),
            &Value::object([("sets", Value::Array(vec![set]))]),
        )
        .unwrap();
    }

    #[test]
    fn only_kept_sets_are_reused_and_full_measures() {
        let root = crate::walk::scratch("gate-reuse");
        let key = Key {
            sha256: "k".repeat(64),
            components: Value::object([("command", Value::str("report --benchmark"))]),
        };
        let output = root.join("out");
        index(&root, "90 days", &key.sha256);
        let measured = || Ok(7);
        assert_eq!(step(&root, &output, &key, false, measured).unwrap(), 7);
        assert!(!was_reused(&output));
        index(&root, "retained", &key.sha256);
        assert_eq!(
            step(&root, &root.join("full"), &key, true, measured).unwrap(),
            7
        );
        assert!(!was_reused(&root.join("full")));
        assert_eq!(step(&root, &output, &key, false, measured).unwrap(), 1);
        assert!(was_reused(&output));
        let recorded = read(&output).unwrap();
        assert_eq!(
            recorded.get("reused").and_then(|r| r.get("set")),
            Some(&Value::str("run"))
        );
        index(&root, "retained", &"m".repeat(64));
        assert_eq!(
            step(&root, &root.join("other"), &key, false, measured).unwrap(),
            7
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_grade_key_binds_the_reports_it_joins() {
        let root = crate::walk::scratch("gate-grade-key");
        let collection = root.join("timing");
        let key = |sha: &str| Key {
            sha256: sha.repeat(64),
            components: Value::object([("command", Value::str("grade"))]),
        };
        fs::create_dir_all(&collection).unwrap();
        fs::write(collection.join("report.json"), b"{\"pairs\": 30}").unwrap();
        record(&collection, &key("t"), 0, None).unwrap();
        let report = collection.join("report.json");
        let first = key("g").with_inputs(&[&report]).unwrap().sha256;
        assert_eq!(first, key("g").with_inputs(&[&report]).unwrap().sha256);
        // Measuring again (--full) writes other samples under the same key.
        fs::write(&report, b"{\"pairs\": 30, \"again\": true}").unwrap();
        record(&collection, &key("t"), 0, None).unwrap();
        assert_ne!(first, key("g").with_inputs(&[&report]).unwrap().sha256);
        // A reused collection has no report and binds the cited report's hash.
        let reused = root.join("reused");
        let cited = Value::object([("report_sha256", Value::str(file_sha(&report).unwrap()))]);
        record(&reused, &key("t"), 0, Some(cited)).unwrap();
        assert_eq!(
            key("g")
                .with_inputs(&[&reused.join("report.json")])
                .unwrap()
                .sha256,
            key("g").with_inputs(&[&report]).unwrap().sha256
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sets_list_their_fresh_results() {
        let root = crate::walk::scratch("gate-results");
        let key = Key {
            sha256: "k".repeat(64),
            components: Value::object([("command", Value::str("grade"))]),
        };
        record(&root.join("fresh"), &key, 0, None).unwrap();
        record(&root.join("reused"), &key, 0, Some(Value::Object(vec![]))).unwrap();
        let entries: Vec<(String, Vec<u8>)> = ["fresh", "reused"]
            .iter()
            .map(|d| {
                (
                    format!("{d}/{RESULT}"),
                    fs::read(root.join(d).join(RESULT)).unwrap(),
                )
            })
            .chain([("notes.json".to_owned(), b"{}".to_vec())])
            .collect();
        let listed = listed(&entries).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].get("command"), Some(&Value::str("grade")));
        assert_eq!(listed[0].get("status"), Some(&Value::int(0)));
        fs::remove_dir_all(root).unwrap();
    }
}
